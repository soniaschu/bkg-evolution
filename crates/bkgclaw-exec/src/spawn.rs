//! Sub-agent plumbing: how a running loop asks the engine for a child.
//!
//! The executor is synchronous and local; sub-agents are neither. A child
//! session is a whole second agent loop with its own transcript, budget and
//! failover chain — only the engine that is already running loops can
//! provide that. So the executor sends a request down a channel and waits
//! for the engine's answer on a oneshot. Without a channel (a bare `bkgclaw
//! run`), the tools refuse honestly instead of pretending.

use serde::{Deserialize, Serialize};

/// What the executor can ask the engine to do with child sessions.
#[derive(Debug)]
pub enum EngineRequest {
    /// Start a child session on a task. The engine answers with the child's
    /// session id immediately; the answer arrives later via `Send`.
    Spawn {
        task: String,
        model: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    /// Send a message to a child session. A finished child returns its last
    /// answer; a running child is queued and the request returns its status.
    Send {
        session: String,
        message: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    /// Steer a running child mid-task.
    Steer {
        session: String,
        instruction: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
}

/// The wire shape of a child as the model sees it in a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildStatus {
    pub session: String,
    pub state: String,
    pub last_answer: Option<String>,
}
