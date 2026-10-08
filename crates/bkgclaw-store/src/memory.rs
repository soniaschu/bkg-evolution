//! Long-term memory: a JSON list of entries plus substring search.
//!
//! This is the L2 layer — everything the agent has been told to remember.
//! The L1 layer is plain `MEMORY.md` in the workspace, which the system
//! prompt loads verbatim (see `workspace`); no tool is needed to edit a
//! markdown file the agent can already write.
//!
//! Search here is deliberately keyword-based, not semantic: there is no
//! embedding model behind the NIM gateway, and pretending relevance scores
//! would be invented ranking. Substring over keys, values and tags is
//! honest and testable.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Seconds since the epoch of the last update.
    #[serde(default)]
    pub updated_at: u64,
}

/// The entry list file `<home>/memory.json`.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Vec<MemoryEntry>,
}

impl MemoryStore {
    /// Load from disk; a missing file is an empty store, not an error.
    /// A malformed file is an error — silently discarding memory the
    /// operator believes in would be worse than failing loudly.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("memory.json");
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(MemoryStore::default()),
            Ok(text) => serde_json::from_str(&text)
                .map(|entries: Vec<MemoryEntry>| MemoryStore { entries })
                .map_err(|e| format!("memory.json is malformed: {e}")),
            Err(_) => Ok(MemoryStore::default()),
        }
    }

    pub fn save(&self, root: &Path) -> Result<(), String> {
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let path = root.join("memory.json");
        std::fs::write(
            path,
            serde_json::to_string_pretty(&self.entries).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    pub fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    /// Upsert by key. Updating an existing key keeps one truth per key —
    /// the alternative, a list of contradictory entries, is how memory
    /// stores rot.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>, tags: Vec<String>) {
        let key = key.into();
        let value = value.into();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.key == key) {
            entry.value = value;
            entry.tags = tags;
            entry.updated_at = now;
        } else {
            self.entries.push(MemoryEntry {
                key,
                value,
                tags,
                updated_at: now,
            });
        }
    }

    pub fn get(&self, key: &str) -> Option<&MemoryEntry> {
        self.entries.iter().find(|e| e.key == key)
    }

    /// Case-insensitive substring search across keys, values and tags.
    /// Empty needle finds nothing, not everything — "search for anything"
    /// is a listing, not a search.
    pub fn search(&self, needle: &str) -> Vec<&MemoryEntry> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let needle = needle.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.key.to_lowercase().contains(&needle)
                    || e.value.to_lowercase().contains(&needle)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.key != key);
        before != self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bkgclaw-mem-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn set_is_an_upsert_by_key() {
        let mut store = MemoryStore::default();
        store.set("projekt", "alpha", vec!["work".into()]);
        store.set("projekt", "beta", vec!["work".into()]);
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.get("projekt").unwrap().value, "beta");
    }

    #[test]
    fn search_spans_keys_values_and_tags_case_insensitively() {
        let mut store = MemoryStore::default();
        store.set("deploy-server", "adresse ist geheim", vec!["infra".into()]);
        store.set("Lieblingsfarbe", "blau", vec!["persönlich".into()]);
        assert_eq!(store.search("DEPLOY").len(), 1);
        assert_eq!(store.search("GEHEIM").len(), 1);
        assert_eq!(store.search("Infra").len(), 1);
        assert_eq!(store.search("").len(), 0, "an empty needle finds nothing");
    }

    #[test]
    fn the_store_round_trips_and_survives_restart() {
        let root = temp_root();
        let mut store = MemoryStore::default();
        store.set("k", "v", vec!["t".into()]);
        store.save(&root).unwrap();

        let reloaded = MemoryStore::load(&root).unwrap();
        assert_eq!(reloaded.get("k").unwrap().value, "v");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_file_is_an_empty_store_and_a_broken_file_is_an_error() {
        let root = temp_root();
        assert_eq!(MemoryStore::load(&root).unwrap().entries().len(), 0);
        std::fs::write(root.join("memory.json"), "{ not json").unwrap();
        assert!(
            MemoryStore::load(&root).is_err(),
            "silently dropping memory is worse"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_reports_whether_it_removed() {
        let mut store = MemoryStore::default();
        store.set("a", "1", vec![]);
        assert!(store.remove("a"));
        assert!(!store.remove("a"));
    }
}
