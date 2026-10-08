//! Wire events: one JSON shape for WebSocket, the event log and REST polling.
//!
//! Every event carries the session it belongs to, so one broadcast channel
//! serves every connected client and each filters for its own sessions.

use serde::{Deserialize, Serialize};

/// One event on the wire. Field names are the client contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A turn began on the session.
    TurnStart { session: String, turn: u32 },
    /// A model is being asked. One event per attempt, failovers included.
    ModelRequest { session: String, model: String },
    /// A streaming delta.
    Delta {
        session: String,
        kind: DeltaKind,
        text: String,
    },
    /// The model's final text for this turn.
    AssistantText {
        session: String,
        text: String,
        model: String,
    },
    /// A tool call resolved.
    ToolOutcome {
        session: String,
        name: String,
        outcome: ToolOutcomeKind,
        #[serde(default)]
        output: String,
        #[serde(default)]
        truncated: bool,
    },
    /// The gate asks a human whether a tool may run.
    ApprovalRequest {
        session: String,
        call_id: String,
        name: String,
        risk: String,
        arguments: serde_json::Value,
    },
    /// A turn resolved with its measured cost and token usage.
    TurnEnd {
        session: String,
        turn: u32,
        #[serde(default)]
        cost_usd: Option<f64>,
        #[serde(default)]
        total_tokens: u64,
    },
    /// The run stopped; always the last event of a turn.
    Stop { session: String, reason: String },
    /// The persisted session changed (new message, spend, status).
    SessionUpdated { session: String },
    /// Turn cancelled by the operator.
    Cancelled { session: String },
    /// Something went wrong worth telling a client about.
    Error { session: String, message: String },
    /// Information worth showing that is neither error nor turn data —
    /// e.g. "auto-snapshot v0007 vor diesem lauf".
    Notice { session: String, text: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeltaKind {
    Text,
    Reasoning,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolOutcomeKind {
    Ran,
    Denied,
    Failed,
}

/// What a client may send over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Start receiving events for a session.
    Subscribe {
        session: String,
    },
    StopReceiving {
        session: String,
    },
    /// Run a turn with this message.
    Send {
        session: String,
        message: String,
    },
    /// Answer an approval request. `always` remembers the tool for the
    /// rest of the session.
    Approval {
        call_id: String,
        decision: ApprovalDecision,
    },
    /// Abort the running turn on a session.
    Cancel {
        session: String,
    },
    /// Create a session; the reply arrives as an `Event::SessionUpdated`
    /// carrying the new id in `session`.
    CreateSession {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        policy: Option<String>,
        /// Fork an existing session's transcript up to this message index.
        #[serde(default)]
        fork_from: Option<String>,
        #[serde(default)]
        fork_at: Option<usize>,
    },
    /// A previous WebSocket message failed to apply.
    #[serde(rename_all = "snake_case")]
    ProtocolError {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    Always,
}

impl Event {
    /// Which session this event belongs to — the one field every variant
    /// carries, so one bus can serve every client.
    pub fn session(&self) -> &str {
        match self {
            Event::TurnStart { session, .. }
            | Event::ModelRequest { session, .. }
            | Event::Delta { session, .. }
            | Event::AssistantText { session, .. }
            | Event::ToolOutcome { session, .. }
            | Event::ApprovalRequest { session, .. }
            | Event::TurnEnd { session, .. }
            | Event::Stop { session, .. }
            | Event::SessionUpdated { session }
            | Event::Cancelled { session }
            | Event::Error { session, .. }
            | Event::Notice { session, .. } => session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_the_wire_format() {
        let event = Event::ToolOutcome {
            session: "s-1".into(),
            name: "write_file".into(),
            outcome: ToolOutcomeKind::Ran,
            output: "wrote 3 bytes".into(),
            truncated: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(r#""type":"tool_outcome""#),
            "tagged shape: {json}"
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn client_messages_parse_from_json() {
        let raw = r#"{"type":"approval","call_id":"c1","decision":"always"}"#;
        let message: ClientMessage = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            message,
            ClientMessage::Approval { call_id, decision: ApprovalDecision::Always } if call_id == "c1"
        ));
    }

    #[test]
    fn create_session_defaults_are_optional() {
        let raw = r#"{"type":"create_session"}"#;
        let message: ClientMessage = serde_json::from_str(raw).unwrap();
        match message {
            ClientMessage::CreateSession {
                model,
                policy,
                fork_from,
                fork_at,
            } => {
                assert!(
                    model.is_none() && policy.is_none() && fork_from.is_none() && fork_at.is_none()
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
