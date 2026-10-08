//! bkgclaw-web — the cockpit in the browser.
//!
//! One client, two channels, the same protocol as the TUI:
//!
//! - REST (`gloo-net`) for the slow facts: sessions, models, tasks, memory,
//!   skills and a session's transcript.
//! - WebSocket (`web-sys`) for the live run: streaming deltas, tool
//!   outcomes, approval requests, cancellation.
//!
//! The UI is deliberately server-thin: no state lives here that the
//! gateway does not already own. Reload the page, and everything is back.

use dioxus::prelude::*;
use serde::Deserialize;

mod net;
pub mod views;

/// Everything the cockpit knows, in one signal bundle. Dioxus signals make
/// the clone-heavy parts of UI state cheap; the gateway stays the truth.
#[derive(Debug, Clone, Default)]
pub struct Cockpit {
    pub base: String,
    pub token: Option<String>,
    pub session: Option<String>,
    pub models: Vec<String>,
    pub sessions: Vec<SessionRow>,
    pub transcript: Vec<TranscriptItem>,
    pub live: String,
    pub live_reasoning: String,
    pub busy: bool,
    pub pending_approval: Option<ApprovalRequest>,
    pub spent: f64,
    pub notice: String,
    pub panel: Panel,
    pub tasks: Vec<TaskRow>,
    pub memory: Vec<MemoryRow>,
    pub skills: Vec<SkillRow>,
    pub input: String,
    pub policy: String,
    /// A session created from this page is adopted when its confirmation
    /// event arrives.
    pub pending_new: bool,
    /// The model chosen for the next session.
    pub chosen_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    Chat,
    Tasks,
    Memory,
    Skills,
}

/// One rendered transcript row.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool { name: String, outcome: String, output: String },
    Error(String),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionRow {
    pub id: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub message_count: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub risk: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRow {
    pub slug: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryRow {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillRow {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    TurnStart { session: String, turn: u32 },
    ModelRequest { session: String, model: String },
    Delta { session: String, kind: DeltaKind, text: String },
    AssistantText { session: String, text: String, model: String },
    ToolOutcome { session: String, name: String, outcome: String, output: String, truncated: bool },
    ApprovalRequest { session: String, call_id: String, name: String, risk: String, arguments: serde_json::Value },
    TurnEnd { session: String, turn: u32, cost_usd: Option<f64>, total_tokens: u64 },
    Stop { session: String, reason: String },
    SessionUpdated { session: String },
    Cancelled { session: String },
    Error { session: String, message: String },
}

#[derive(Debug, Clone, Copy, serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DeltaKind {
    Text,
    Reasoning,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireClient {
    Subscribe { session: String },
    StopReceiving { session: String },
    Send { session: String, message: String },
    Approval { call_id: String, decision: String },
    Cancel { session: String },
    CreateSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        policy: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fork_from: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fork_at: Option<usize>,
    },
}
