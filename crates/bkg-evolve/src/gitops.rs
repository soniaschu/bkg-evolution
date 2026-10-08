//! bkgclaw's own git hands.
//!
//! "bkg soll die git selbst verwalten und benutzen" — this module is that,
//! literally: every git operation evolution needs, first-class, with
//! errors that say what failed instead of swallowing exit codes.
//!
//! Design rules:
//!
//! - **The agent never commits.** The engine commits, after the fitness
//!   gate. A model that could commit could write its own report card.
//! - **Branches, not main.** Every attempt gets `evolve/NNN-slug`;
//!   improvements land there. Main moves only by explicit push of a
//!   verified branch.
//! - **Destructive is explicit.** `hard_reset` exists because revert
//!   needs it; it takes the branch it resets and nothing else.

use std::path::Path;
use std::process::Command;

/// One git invocation, run in `dir`, failing with full context.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git nicht ausführbar: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(format!("git {}: {}", args.join(" "), stderr))
}

pub fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// `git init` + a .gitignore that keeps build output and runtime state out
/// of the history — an evolving repo whose every commit contains `target/`
/// would bury its own diffs in noise.
pub fn init(dir: &Path) -> Result<(), String> {
    if !is_repo(dir) {
        git(dir, &["init", "-b", "main"])?;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(
            &gitignore,
            "/target\n/crates/*/target\n/web-dist\n/.bkgclaw\n/.enclave.toml\n/.enclave.local.toml\nCargo.lock.bak\n",
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn current_branch(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Everything the working tree currently differs in, one name per line.
pub fn dirty_files(dir: &Path) -> Vec<String> {
    git(dir, &["status", "--porcelain"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.get(3..).map(str::to_string))
        .collect()
}

pub fn branch(dir: &Path, name: &str) -> Result<(), String> {
    if name.contains("..") || name.contains(' ') || name.starts_with('-') {
        return Err(format!("ungültiger branch-name: `{name}`"));
    }
    git(dir, &["checkout", "-b", name]).map(|_| ())
}

pub fn checkout(dir: &Path, name: &str) -> Result<(), String> {
    git(dir, &["checkout", name]).map(|_| ())
}

pub fn add_all(dir: &Path) -> Result<(), String> {
    git(dir, &["add", "-A"]).map(|_| ())
}

pub fn commit(dir: &Path, message: &str) -> Result<String, String> {
    git(dir, &["commit", "-m", message]).map(|_| "committed".to_string())
}

pub fn head_short(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "--short", "HEAD"])
}

/// The diffstat as a one-line summary: `N files changed, X insertions(+), Y deletions(-)`.
pub fn diff_stat(dir: &Path) -> Result<String, String> {
    let stat = git(dir, &["diff", "--stat", "HEAD"])?;
    Ok(stat.lines().last().unwrap_or("keine änderungen").to_string())
}

/// Reset the working tree to the last commit — the revert path for a
/// failed attempt. Untracked files created by the attempt are removed
/// too; the attempt's branch keeps the full story if forensics need it.
pub fn hard_reset(dir: &Path) -> Result<(), String> {
    git(dir, &["checkout", "--", "."])?;
    // Untracked leftovers are as much part of a failed attempt as edits.
    git(dir, &["clean", "-fd"])?;
    Ok(())
}

pub fn remote_add_origin(dir: &Path, url: &str) -> Result<(), String> {
    // Idempotent: setting an existing remote again is a config update, not
    // an error worth failing an evolution push over.
    let _ = git(dir, &["remote", "remove", "origin"]);
    git(dir, &["remote", "add", "origin", url]).map(|_| ())
}

pub fn push(dir: &Path, remote: &str, branch: &str, set_upstream: bool) -> Result<(), String> {
    if set_upstream {
        git(dir, &["push", "-u", remote, branch])
    } else {
        git(dir, &["push", remote, branch])
    }
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        let dir = dir.join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_branch_commit_and_dirty_tracking_round_trip() {
        let dir = repo("roundtrip");
        init(&dir).unwrap();
        assert!(is_repo(&dir));

        // Baseline first — a branch needs a commit to stand on, exactly
        // like the engine's init_repo does before any evolve branch.
        std::fs::write(dir.join("baseline.txt"), "null").unwrap();
        add_all(&dir).unwrap();
        commit(&dir, "baseline").unwrap();

        std::fs::write(dir.join("src.txt"), "eins").unwrap();
        assert!(dirty_files(&dir).contains(&"src.txt".to_string()));

        branch(&dir, "evolve/0001-test").unwrap();
        add_all(&dir).unwrap();
        let id = commit(&dir, "evolve: erster versuch").unwrap();
        assert_eq!(id, "committed");
        assert!(dirty_files(&dir).is_empty(), "nach commit ist der baum sauber");
        assert_eq!(current_branch(&dir).unwrap(), "evolve/0001-test");

        checkout(&dir, "main").unwrap();
        assert_eq!(current_branch(&dir).unwrap(), "main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hard_reset_removes_edits_and_untracked_files() {
        let dir = repo("reset");
        init(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "original").unwrap();
        add_all(&dir).unwrap();
        commit(&dir, "baseline").unwrap();

        // The failed attempt: edits one file, creates another.
        std::fs::write(dir.join("keep.txt"), "kaputt").unwrap();
        std::fs::write(dir.join("neu.txt"), "leiche").unwrap();
        hard_reset(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "original");
        assert!(!dir.join("neu.txt").exists(), "untracked müll muss weg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_names_are_sanitised() {
        let dir = repo("sanitize");
        init(&dir).unwrap();
        assert!(branch(&dir, "../escape").is_err());
        assert!(branch(&dir, "mit leerzeichen").is_err());
        assert!(branch(&dir, "-b").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_short_gives_a_short_id() {
        let dir = repo("short_id");
        init(&dir).unwrap();
        // Create a file and commit it to have a HEAD.
        std::fs::write(dir.join("test.txt"), "content").unwrap();
        add_all(&dir).unwrap();
        commit(&dir, "initial commit").unwrap();
        let short = head_short(&dir).unwrap();
        assert_eq!(short.len(), 7, "head_short should return exactly 7 characters");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
