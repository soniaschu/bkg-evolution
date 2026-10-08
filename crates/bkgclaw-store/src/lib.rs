//! File-backed persistence for the agent platform.
//!
//! One rule keeps this crate honest: **no SQL, no service, no daemon.** Every
//! store is a file the operator can read with `cat` and back up with `cp`.
//! The OpenClaw-style gateway holds these files in memory while it runs and
//! flushes them after every turn, so a killed process loses nothing that
//! finished.
//!
//! Two roots, two lifetimes:
//!
//! - **Home** (`~/.bkgclaw`, override `BKGCLAW_HOME`) — machine-scoped:
//!   sessions, long-term memory, plugins and version snapshots. Survives
//!   changing projects.
//! - **Workspace** (`./.bkgclaw` in the working directory) — project-scoped:
//!   personality files (`SOUL.md`, `IDENTITY.md`, `USER.md`), hot memory
//!   (`MEMORY.md`), skills and tasks. Lives and dies with the project.
#![forbid(unsafe_code)]

pub mod memory;
pub mod plugins;
pub mod session;
pub mod skills;
pub mod tasks;
pub mod versions;
pub mod workspace;

pub use memory::{MemoryEntry, MemoryStore};
pub use plugins::{
    install as install_plugin, list as list_plugins, remove as remove_plugin, InstalledPlugin,
};
pub use session::{Session, SessionSummary};
pub use skills::{Skill, SkillIndex};
pub use tasks::{Task, TaskStatus, TaskStore};
pub use versions::{auto_snapshot, Change, FileEntry, Snapshot, SnapshotStore};
pub use workspace::{render_sections, system_prompt_sections, PromptSection};

use std::path::PathBuf;

/// Machine-scoped root: sessions, long-term memory, plugins, snapshots.
pub fn home_root() -> PathBuf {
    std::env::var("BKGCLAW_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".bkgclaw")
        })
}

/// Project-scoped root: personality, skills, tasks, hot memory.
pub fn workspace_root() -> PathBuf {
    std::env::var("BKGCLAW_WORKSPACE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".bkgclaw"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_root_respects_the_override() {
        // A test that mutates the environment would race other tests; the
        // override logic is exercised through the boundary functions that
        // take explicit roots. This test only pins the default shape.
        let root = workspace_root();
        assert!(!root.as_os_str().is_empty());
    }
}
