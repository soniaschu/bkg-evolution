//! Sessions: one conversation, one file, restarts cheap.
//!
//! A session is a JSON document in `<home>/sessions/<id>.json` holding the
//! full transcript plus the metadata a client needs to list, resume and
//! budget it. The whole file is rewritten after every turn: transcripts are
//! small, and a partial append protocol would trade correctness for a
//! negligible write cost.
//!
//! Forking — Nerve-style branching — copies the transcript up to a chosen
//! message into a new session. The original is never touched.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use bkgclaw_core::Message;

/// A persisted session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// Seconds since the epoch, because wall-clock strings invite timezone
    /// bugs; clients format for humans.
    pub created_at: u64,
    pub updated_at: u64,
    /// The model this session is pinned to, if any. `None` = failover chain.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub policy: String,
    /// Dollars spent on this session, as measured by the cost tables.
    #[serde(default)]
    pub spend_usd: f64,
    #[serde(default)]
    pub turns: u32,
    pub messages: Vec<Message>,
}

/// What a session listing shows. Not the whole transcript — a list of
/// hundreds of sessions times a full transcript is a slow list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    /// First user message, truncated: enough to recognise the session.
    pub preview: String,
    pub model: Option<String>,
    pub spend_usd: f64,
    pub turns: u32,
}

impl Session {
    /// A fresh session with a unique id. Seconds plus process id plus a
    /// per-process counter: unique across processes (pid) and within one
    /// process (counter), time-ordered for listings.
    pub fn new(model: Option<String>, policy: String) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("s-{now}-{}-{sequence:x}", std::process::id());
        Session {
            id,
            created_at: now,
            updated_at: now,
            model,
            policy,
            spend_usd: 0.0,
            turns: 0,
            messages: Vec::new(),
        }
    }

    /// A fork of this session: a new id, everything else copied up to and
    /// including message `at` (0-based). Out-of-range `at` forks the whole
    /// transcript, because "fork everything" is a valid request and "fork
    /// nothing" is indistinguishable from it in intent.
    pub fn fork(&self, at: usize) -> Self {
        let mut forked = Session::new(self.model.clone(), self.policy.clone());
        let end = at.saturating_add(1).min(self.messages.len());
        forked.messages = self.messages[..end].to_vec();
        // Cost history does not transfer: the fork's future spend is its own.
        forked.spend_usd = 0.0;
        forked.turns = 0;
        forked
    }

    fn path(root: &Path, id: &str) -> PathBuf {
        root.join("sessions").join(format!("{id}.json"))
    }

    /// Persist the whole session. Creating parents here means the first
    /// save bootstraps the store without a separate setup step.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Self::path(root, &self.id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).expect("session serialises"),
        )
    }

    /// Load one session by id. `None` when it does not exist — callers
    /// decide whether that is an error.
    pub fn load(root: &Path, id: &str) -> Option<Self> {
        // Session ids appear in URLs and file names; refusing anything with
        // a separator closes the traversal hole before it opens.
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return None;
        }
        let text = std::fs::read_to_string(Self::path(root, id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every session, newest first — the listing a client shows.
    pub fn list(root: &Path) -> Vec<SessionSummary> {
        let dir = root.join("sessions");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut sessions: Vec<Session> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| {
                let id = entry.path().file_stem()?.to_string_lossy().to_string();
                Session::load(root, &id)
            })
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        sessions
            .into_iter()
            .map(|session| SessionSummary {
                id: session.id,
                created_at: session.created_at,
                updated_at: session.updated_at,
                message_count: session.messages.len(),
                preview: session
                    .messages
                    .iter()
                    .find(|m| matches!(m, Message::User { .. }))
                    .map(|m| match m {
                        Message::User { content } => content.chars().take(80).collect::<String>(),
                        _ => String::new(),
                    })
                    .unwrap_or_else(|| "(no user message)".to_string()),
                model: session.model,
                spend_usd: session.spend_usd,
                turns: session.turns,
            })
            .collect()
    }

    /// Delete a session file. Used by archive/clean flows; returns whether
    /// anything was removed.
    pub fn delete(root: &Path, id: &str) -> bool {
        if id.contains('/') || id.contains('\\') || id.contains("..") {
            return false;
        }
        std::fs::remove_file(Self::path(root, id)).is_ok()
    }

    /// Stamp the update time. Called by the engine after every turn.
    pub fn touch(&mut self) {
        self.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bkgclaw-store-{}-{}",
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
    fn a_session_round_trips_through_disk() {
        let root = temp_root();
        let mut session = Session::new(Some("nim/x".into()), "allow-read-only".into());
        session.messages.push(Message::user("hello"));
        session.messages.push(Message::assistant("hi"));
        session.spend_usd = 0.25;
        session.turns = 1;
        session.save(&root).unwrap();

        let loaded = Session::load(&root, &session.id).expect("saved session loads");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.model.as_deref(), Some("nim/x"));
        assert!((loaded.spend_usd - 0.25).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fork_copies_the_prefix_and_nothing_else() {
        let mut session = Session::new(None, "deny-all".into());
        for i in 0..4 {
            session.messages.push(Message::user(format!("m{i}")));
        }
        session.spend_usd = 5.0;
        session.turns = 9;
        let forked = session.fork(1);
        assert_eq!(forked.messages.len(), 2, "messages 0..=1 fork");
        assert!(
            !forked
                .messages
                .iter()
                .any(|m| matches!(m, Message::User { content } if content == "m2"))
        );
        assert_eq!(forked.spend_usd, 0.0, "a fork does not inherit the bill");
        assert_eq!(forked.turns, 0);
        assert_ne!(forked.id, session.id);
        // The original is untouched.
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.spend_usd, 5.0);
    }

    #[test]
    fn loading_a_traversal_id_finds_nothing() {
        let root = temp_root();
        assert!(Session::load(&root, "../etc/passwd").is_none());
        assert!(Session::load(&root, "a/b").is_none());
        assert!(!Session::delete(&root, "../../home/whatever"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_listing_is_newest_first_with_a_preview() {
        let root = temp_root();
        let old = Session::new(None, "p".into());
        old.save(&root).unwrap();
        let mut new = Session::new(None, "p".into());
        new.messages.push(Message::user("erkannt mich"));
        new.touch();
        new.save(&root).unwrap();

        let list = Session::list(&root);
        assert_eq!(list.len(), 2);
        assert!(list[0].updated_at >= list[1].updated_at, "newest first");
        assert_eq!(list[0].preview, "erkannt mich");
        assert_eq!(list[1].preview, "(no user message)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_session_is_none_not_an_error() {
        let root = temp_root();
        assert!(Session::load(&root, "s-never").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
