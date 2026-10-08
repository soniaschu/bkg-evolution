//! Tasks: markdown files with a status, listed and updated as files.
//!
//! Like skills, tasks live in the workspace (`./.bkgclaw/tasks/`) so they
//! are project-scoped and readable with `cat`. Each task is one file whose
//! frontmatter carries `title` and `status`; the body is the task itself and
//! may be edited by the agent with its ordinary file tools.
//!
//! No database, no full-text index: the statuses a workflow needs are four,
//! and filtering happens in memory over the listing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
    Deferred,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in-progress",
            TaskStatus::Done => "done",
            TaskStatus::Deferred => "deferred",
        }
    }

    /// Parse the frontmatter value. Unknown text is Pending, not an error:
    /// a hand-edited typo must not hide a task.
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "in-progress" | "in_progress" | "inprogress" => TaskStatus::InProgress,
            "done" => TaskStatus::Done,
            "deferred" => TaskStatus::Deferred,
            _ => TaskStatus::Pending,
        }
    }

    pub fn all() -> [TaskStatus; 4] {
        [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Done,
            TaskStatus::Deferred,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// File stem: the stable, human-typed identifier.
    pub slug: String,
    pub title: String,
    pub status: TaskStatus,
    /// Seconds since the epoch of the last status change.
    pub updated_at: u64,
}

/// The task file store for one workspace.
#[derive(Debug, Default)]
pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub fn new(workspace_root: &Path) -> Self {
        TaskStore {
            dir: workspace_root.join("tasks"),
        }
    }

    fn path_of(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.md"))
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Parse a task file's frontmatter. Body is returned too — the listing
    /// keeps only title/status, but the agent may read the full file with
    /// `read_file`.
    fn parse(slug: &str, text: &str) -> Task {
        let mut title = String::new();
        let mut status = TaskStatus::Pending;
        let mut updated_at = 0;
        let mut in_frontmatter = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "---" {
                if in_frontmatter {
                    break;
                }
                in_frontmatter = true;
                continue;
            }
            if !in_frontmatter {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("title:") {
                title = rest.trim().to_string();
            } else if let Some(rest) = trimmed.strip_prefix("status:") {
                status = TaskStatus::parse(rest);
            } else if let Some(rest) = trimmed.strip_prefix("updated:") {
                updated_at = rest.trim().parse().unwrap_or(0);
            }
        }
        Task {
            slug: slug.to_string(),
            title: if title.is_empty() {
                slug.to_string()
            } else {
                title
            },
            status,
            updated_at,
        }
    }

    /// Create a task. Fails when the slug is already taken — a silent
    /// overwrite would eat an existing task's body.
    pub fn add(&self, slug: &str, title: &str, body: &str) -> Result<Task, String> {
        if slug.is_empty()
            || !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "`{slug}` is not a task slug (letters, digits, - and _)"
            ));
        }
        let path = self.path_of(slug);
        if path.exists() {
            return Err(format!("task `{slug}` already exists"));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let text = format!(
            "---\ntitle: {title}\nstatus: pending\nupdated: {}\n---\n\n{body}\n",
            Self::now()
        );
        std::fs::write(&path, &text).map_err(|e| e.to_string())?;
        Ok(Self::parse(slug, &text))
    }

    /// Change a task's status. Rewrites frontmatter, preserves the body —
    /// a status flip must not eat the task description.
    pub fn set_status(&self, slug: &str, status: TaskStatus) -> Result<Task, String> {
        if slug.contains("..") || slug.contains('/') || slug.contains('\\') {
            return Err("invalid task slug".to_string());
        }
        let path = self.path_of(slug);
        let text =
            std::fs::read_to_string(&path).map_err(|_| format!("task `{slug}` does not exist"))?;
        let (frontmatter, body) = match text.split_once("---\n") {
            Some((_, rest)) => match rest.split_once("\n---") {
                Some((fm, body)) => (fm.to_string(), body.trim_start_matches('\n').to_string()),
                None => return Err(format!("task `{slug}` has no frontmatter end")),
            },
            None => return Err(format!("task `{slug}` has no frontmatter")),
        };
        let mut title = String::new();
        for line in frontmatter.lines() {
            if let Some(rest) = line.trim().strip_prefix("title:") {
                title = rest.trim().to_string();
            }
        }
        let new_text = format!(
            "---\ntitle: {title}\nstatus: {}\nupdated: {}\n---\n\n{body}",
            status.as_str(),
            Self::now()
        );
        std::fs::write(&path, new_text).map_err(|e| e.to_string())?;
        // Parse back from disk: the file is the truth, not the string we
        // happened to assemble.
        self.get(slug)
            .ok_or_else(|| "task file vanished while updating".to_string())
    }

    /// List tasks, newest change first. Missing directory = no tasks.
    pub fn list(&self) -> Vec<Task> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut tasks: Vec<Task> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .filter_map(|e| {
                let slug = e.path().file_stem()?.to_string_lossy().to_string();
                let text = std::fs::read_to_string(e.path()).ok()?;
                Some(Self::parse(&slug, &text))
            })
            .collect();
        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.slug.cmp(&b.slug)));
        tasks
    }

    /// Only the tasks with the given status — the "what's open" view.
    pub fn list_by_status(&self, status: TaskStatus) -> Vec<Task> {
        self.list()
            .into_iter()
            .filter(|t| t.status == status)
            .collect()
    }

    pub fn get(&self, slug: &str) -> Option<Task> {
        if slug.contains("..") || slug.contains('/') {
            return None;
        }
        let text = std::fs::read_to_string(self.path_of(slug)).ok()?;
        Some(Self::parse(slug, &text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bkgclaw-tasks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn statuses_round_trip_and_unknown_is_pending() {
        for status in TaskStatus::all() {
            assert_eq!(TaskStatus::parse(status.as_str()), status);
        }
        assert_eq!(TaskStatus::parse("who knows"), TaskStatus::Pending);
    }

    #[test]
    fn a_task_round_trips_and_keeps_its_body_across_status_changes() {
        let root = temp_root("rt");
        let store = TaskStore::new(&root);
        store
            .add(
                "fix-login",
                "Login reparieren",
                "Der Login bricht bei Umlauten.",
            )
            .unwrap();

        store
            .set_status("fix-login", TaskStatus::InProgress)
            .unwrap();
        let task = store.get("fix-login").unwrap();
        assert_eq!(task.status, TaskStatus::InProgress);
        assert_eq!(task.title, "Login reparieren");

        // The body survived the rewrite.
        let text = std::fs::read_to_string(root.join("tasks").join("fix-login.md")).unwrap();
        assert!(
            text.contains("Der Login bricht bei Umlauten."),
            "body lost: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_duplicate_slug_is_refused_not_overwritten() {
        let root = temp_root("dup");
        let store = TaskStore::new(&root);
        store.add("one", "first", "body").unwrap();
        assert!(
            store.add("one", "second", "body").is_err(),
            "the second add must not eat the first"
        );
        assert!(store.get("one").unwrap().title.contains("first"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slugs_are_sanitised_and_traversal_is_blocked() {
        let root = temp_root("slug");
        let store = TaskStore::new(&root);
        assert!(store.add("../escape", "x", "y").is_err());
        assert!(store.add("has space", "x", "y").is_err());
        assert!(store.add("good-slug", "x", "y").is_ok());
        assert!(store.get("../../etc/passwd").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn listing_filters_by_status() {
        let root = temp_root("list");
        let store = TaskStore::new(&root);
        store.add("a", "a", "x").unwrap();
        store.add("b", "b", "x").unwrap();
        store.set_status("b", TaskStatus::Done).unwrap();
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.list_by_status(TaskStatus::Pending).len(), 1);
        assert_eq!(store.list_by_status(TaskStatus::Done).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
