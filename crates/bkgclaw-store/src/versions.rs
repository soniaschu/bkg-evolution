//! The built-in version system: content-addressed snapshots of a directory.
//!
//! bkgclaw improving its own code needs the same safety every coding agent
//! needs: *try, verify, roll back.* This is that mechanism, owned end to
//! end — no external git, no service, one blob store and one index.
//!
//! How it works:
//!
//! - **Blobs**: every distinct file content is stored once, named by its
//!   SHA-256. A thousand snapshots of an unchanged tree cost nothing new.
//! - **Index**: `<home>/versions/<key>/index.json` — one entry per
//!   snapshot with its file list (`path → hash`), a note and a timestamp.
//! - **Scope**: snapshots version ONE directory (the sandbox, a repo, a
//!   service dir). The store lives in the home, so the versioned tree
//!   cannot destroy its own history by deleting files.
//! - **Auto mode**: after `init`, the engine snapshots automatically before
//!   every run that can write — the workflow becomes "let the agent work,
//!   roll back if it broke".
//!
//! Explicitly not built: branching, merging, partial staging. Those are
//! collaboration tools; this is a **checkpoint system for an agent** —
//! snapshot, list, changes, diff, restore.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directories that never belong in a snapshot: build output, dependency
/// trees, foreign VCS metadata, the agent's own runtime state.
const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".bkgclaw",
    ".agent-console-state",
    "web-dist",
    "__pycache__",
];

/// One file's entry in a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub hash: String,
    #[serde(default)]
    pub bytes: u64,
}

/// One snapshot in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    /// Seconds since the epoch.
    pub created_at: u64,
    #[serde(default)]
    pub note: String,
    /// `"auto"` when the engine took it, `"manual"` for the human.
    #[serde(default)]
    pub origin: String,
    pub files: Vec<FileEntry>,
}

/// What changed between two states of one path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Created,
    Modified,
    Deleted,
}

/// The store for one versioned directory.
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    /// Open (or create) the store for a target directory. `key` is the
    /// directory's identity in the home — a readable, sanitized path.
    pub fn for_dir(home: &Path, target: &Path) -> Self {
        let key = sanitize_key(target);
        SnapshotStore { root: home.join("versions").join(key) }
    }

    /// Opt in to auto-snapshots: the marker is the store itself.
    pub fn init(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join("blobs"))
    }

    /// Whether `init` ran for this directory — the engine's auto mode
    /// only fires for directories whose owner asked for versioning.
    pub fn is_initialized(&self) -> bool {
        self.root.join("blobs").is_dir()
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn read_index(&self) -> Vec<Snapshot> {
        std::fs::read_to_string(self.index_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_index(&self, snapshots: &[Snapshot]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::write(
            self.index_path(),
            serde_json::to_string_pretty(snapshots).expect("index serialises"),
        )
    }

    /// Take a snapshot of the target directory now.
    pub fn snapshot(&self, target: &Path, note: &str, origin: &str) -> Result<Snapshot, String> {
        if !self.is_initialized() {
            return Err("versions nicht initialisiert — erst `bkgclaw versions init`".to_string());
        }
        if !target.is_dir() {
            return Err(format!("`{}` ist kein verzeichnis", target.display()));
        }

        let mut files = Vec::new();
        let mut walk_error = None;
        collect_files(target, target, 0, &mut files, &mut walk_error);
        if let Some(error) = walk_error {
            return Err(error);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        // Store every distinct blob. A repeated snapshot of an unchanged
        // file reuses the existing blob — content addressing is the whole
        // trick that makes auto-snapshots cheap.
        for entry in &files {
            let blob = self.root.join("blobs").join(&entry.hash);
            if !blob.exists() {
                let content = std::fs::read(target.join(&entry.path))
                    .map_err(|e| format!("{}: {e}", entry.path))?;
                std::fs::write(&blob, content).map_err(|e| e.to_string())?;
            }
        }

        let mut index = self.read_index();
        let snapshot = Snapshot {
            id: next_id(&index),
            created_at: now(),
            note: note.to_string(),
            origin: origin.to_string(),
            files,
        };
        index.push(snapshot.clone());
        self.write_index(&index).map_err(|e| e.to_string())?;
        Ok(snapshot)
    }

    pub fn list(&self) -> Vec<Snapshot> {
        self.read_index()
    }

    pub fn get(&self, id: &str) -> Option<Snapshot> {
        self.read_index().into_iter().find(|s| s.id == id)
    }

    /// The latest snapshot, or none.
    pub fn latest(&self) -> Option<Snapshot> {
        self.read_index().pop()
    }

    /// What differs between two snapshots of the same tree.
    pub fn diff_snapshots(&self, old: &Snapshot, new: &Snapshot) -> Vec<(String, Change)> {
        diff_lists(&old.files, &new.files)
    }

    /// What differs between a snapshot and the directory as it is now.
    pub fn changes(&self, target: &Path, id: &str) -> Result<Vec<(String, Change)>, String> {
        let snapshot = self
            .get(id)
            .ok_or_else(|| format!("kein snapshot `{id}`"))?;
        let current = scan_current(target)?;
        Ok(diff_lists(&snapshot.files, &current))
    }

    /// Restore a snapshot into the target directory. The directory ends up
    /// exactly as the snapshot records it — extras are removed. Before
    /// anything is touched, a safety snapshot of the CURRENT state is
    /// taken, so a wrong restore is itself restorable.
    pub fn restore(&self, target: &Path, id: &str) -> Result<Snapshot, String> {
        let snapshot = self
            .get(id)
            .ok_or_else(|| format!("kein snapshot `{id}`"))?;

        // Safety first: the pre-restore state is itself a snapshot.
        if self.is_initialized() {
            let _ = self.snapshot(target, &format!("sicherung vor restore auf {id}"), "auto");
        }

        for entry in &snapshot.files {
            let blob = self.root.join("blobs").join(&entry.hash);
            let content = std::fs::read(&blob).map_err(|e| format!("blob fehlt: {e}"))?;
            let path = target.join(&entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, content).map_err(|e| format!("{}: {e}", entry.path))?;
        }

        // Remove everything the snapshot does not know — restore means
        // "this state", not "these files on top of whatever is there".
        let current = scan_current(target)?;
        let keep: std::collections::HashSet<&str> =
            snapshot.files.iter().map(|f| f.path.as_str()).collect();
        for entry in current {
            if !keep.contains(entry.path.as_str()) {
                let _ = std::fs::remove_file(target.join(&entry.path));
            }
        }
        Ok(snapshot)
    }
}

/// The engine's hook: snapshot before a run that can write — but only for
/// directories whose owner ran `versions init`. Returns the snapshot id
/// when one was taken.
pub fn auto_snapshot(home: &Path, target: &Path, note: &str) -> Option<String> {
    let store = SnapshotStore::for_dir(home, target);
    if !store.is_initialized() {
        return None;
    }
    store
        .snapshot(target, &format!("auto vor: {}", clip(note, 120)), "auto")
        .ok()
        .map(|s| s.id)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn next_id(index: &[Snapshot]) -> String {
    let n = index.len();
    format!("v{n:04}")
}

/// Readable, filesystem-safe identity for the versioned directory.
fn sanitize_key(target: &Path) -> String {
    let raw = target
        .to_string_lossy()
        .trim_matches('/')
        .replace(['/', ':'], "_");
    let raw = raw.trim_matches('_');
    if raw.is_empty() {
        "root".to_string()
    } else {
        raw.chars().take(120).collect()
    }
}

/// A tiny SHA-256 implementation — no dependency for a hash the store
/// only compares against itself. FIPS-compatible implementation, constants
/// from the standard.
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub fn hex(data: &[u8]) -> String {
        let digest = digest(data);
        let mut out = String::with_capacity(64);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    fn digest(data: &[u8]) -> [u8; 32] {
        let mut state: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        // Padding: 0x80, zeros, 64-bit big-endian bit length.
        let bit_len = (data.len() as u64).wrapping_mul(8);
        let mut message = data.to_vec();
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in message.chunks_exact(64) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut h = state;
            for i in 0..64 {
                let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
                let ch = (h[4] & h[5]) ^ ((!h[4]) & h[6]);
                let temp1 = h[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
                let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
                let temp2 = s0.wrapping_add(maj);
                h[7] = h[6];
                h[6] = h[5];
                h[5] = h[4];
                h[4] = h[3].wrapping_add(temp1);
                h[3] = h[2];
                h[2] = h[1];
                h[1] = h[0];
                h[0] = temp1.wrapping_add(temp2);
            }
            for i in 0..8 {
                state[i] = state[i].wrapping_add(h[i]);
            }
        }

        let mut out = [0u8; 32];
        for (i, word) in state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Walk the directory, hashing every file, ignoring the noise.
fn collect_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<FileEntry>,
    error: &mut Option<String>,
) {
    const MAX_DEPTH: usize = 12;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        if dir == root {
            // The root itself being unreadable is the caller's problem and
            // is validated before we get here; anything else is silent.
            *error = Some(format!("`{}` ist nicht lesbar", root.display()));
        }
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_files(root, &path, depth + 1, out, error);
        } else {
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            out.push(FileEntry {
                path: relative,
                hash: sha256::hex(&content),
                bytes: content.len() as u64,
            });
        }
    }
}

fn scan_current(target: &Path) -> Result<Vec<FileEntry>, String> {
    let mut files = Vec::new();
    let mut error = None;
    collect_files(target, target, 0, &mut files, &mut error);
    if let Some(error) = error {
        return Err(error);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Compare two sorted file lists into per-path changes.
fn diff_lists(old: &[FileEntry], new: &[FileEntry]) -> Vec<(String, Change)> {
    use std::collections::HashMap;
    let old_map: HashMap<&str, &str> = old.iter().map(|f| (f.path.as_str(), f.hash.as_str())).collect();
    let new_map: HashMap<&str, &str> = new.iter().map(|f| (f.path.as_str(), f.hash.as_str())).collect();

    let mut changes = Vec::new();
    for (path, hash) in &new_map {
        match old_map.get(path) {
            None => changes.push((path.to_string(), Change::Created)),
            Some(old_hash) if old_hash != hash => changes.push((path.to_string(), Change::Modified)),
            _ => {}
        }
    }
    for path in old_map.keys() {
        if !new_map.contains_key(path) {
            changes.push((path.to_string(), Change::Deleted));
        }
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bkgclaw-versions-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_tree(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn the_sha256_matches_the_reference_vector() {
        // "abc" — the canonical test vector from FIPS 180-4. If this fails,
        // the blob names are wrong and dedup quietly breaks.
        assert_eq!(
            sha256::hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn snapshot_dedup_means_a_repeat_costs_no_new_blobs() {
        let home = temp("home");
        let dir = temp("tree");
        write_tree(&dir, "a.txt", "inhalt a");
        std::fs::create_dir(dir.join("sub")).unwrap();
        write_tree(&dir, "sub/b.txt", "inhalt b");

        let store = SnapshotStore::for_dir(&home, &dir);
        store.init().unwrap();
        store.snapshot(&dir, "eins", "manual").unwrap();
        store.snapshot(&dir, "zwei", "manual").unwrap();

        let blobs = std::fs::read_dir(home.join("versions").join(sanitize_key(&dir)).join("blobs"))
            .unwrap()
            .count();
        assert_eq!(blobs, 2, "unchanged files must not duplicate blobs");
        assert_eq!(store.list().len(), 2);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changes_and_restore_round_trip_through_every_change_kind() {
        let home = temp("home");
        let dir = temp("tree");
        write_tree(&dir, "keep.txt", "bleibt");
        write_tree(&dir, "change.txt", "alt");
        write_tree(&dir, "delete.txt", "fliegt");

        let store = SnapshotStore::for_dir(&home, &dir);
        store.init().unwrap();
        let baseline = store.snapshot(&dir, "baseline", "manual").unwrap();

        // The "agent" now changes everything.
        write_tree(&dir, "change.txt", "neu");
        write_tree(&dir, "created.txt", "neu dazu");
        std::fs::remove_file(dir.join("delete.txt")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        write_tree(&dir, "target/junk.txt", "soll nicht auffallen");

        let changes = store.changes(&dir, &baseline.id).unwrap();
        assert!(changes.contains(&("change.txt".into(), Change::Modified)));
        assert!(changes.contains(&("created.txt".into(), Change::Created)));
        assert!(changes.contains(&("delete.txt".into(), Change::Deleted)));
        assert!(
            !changes.iter().any(|(path, _)| path.starts_with("target/")),
            "ignored directories are not versioned"
        );

        // Restore brings the baseline back exactly — including removal of
        // the created file, keeping the safety snapshot of the dirty state.
        store.restore(&dir, &baseline.id).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("change.txt")).unwrap(), "alt");
        assert!(dir.join("delete.txt").exists());
        assert!(!dir.join("created.txt").exists(), "restore removes extras");
        // And the safety net: the dirty state was snapshotted before.
        assert!(store.list().iter().any(|s| s.note.contains("vor restore")));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_snapshot_requires_opt_in() {
        let home = temp("home");
        let dir = temp("tree");
        write_tree(&dir, "x.txt", "x");

        assert_eq!(auto_snapshot(&home, &dir, "note"), None, "no init, no snapshot");
        SnapshotStore::for_dir(&home, &dir).init().unwrap();
        assert!(auto_snapshot(&home, &dir, "note").is_some());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_refuses_without_init_and_lists_sensibly() {
        let home = temp("home");
        let dir = temp("tree");
        write_tree(&dir, "x.txt", "x");
        let store = SnapshotStore::for_dir(&home, &dir);
        assert!(store.snapshot(&dir, "nope", "manual").is_err());
        store.init().unwrap();
        let first = store.snapshot(&dir, "erste", "manual").unwrap();
        assert_eq!(first.id, "v0000");
        let second = store.snapshot(&dir, "zweite", "manual").unwrap();
        assert_eq!(second.id, "v0001");
        assert_eq!(store.latest().unwrap().id, "v0001");
        assert!(store.get("v0000").is_some());
        assert!(store.get("v9999").is_none());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
