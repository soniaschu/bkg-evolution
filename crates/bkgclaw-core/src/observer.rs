//! The loop's eyes: everything a UI needs to show a run as it happens.
//!
//! The loop used to be a black box: it ran, then returned a `RunResult`.
//! A terminal UI and a web cockpit both need the run *while* it runs —
//! which model is thinking, each delta of text as it arrives, each tool
//! call and its outcome — and they need to answer one question mid-loop:
//! "this tool requires approval — allow it?".
//!
//! One trait, three rules:
//!
//! 1. **Every method has a no-op default**, so `NoObserver` exists and the
//!    library keeps working unchanged where no UI is wired.
//! 2. **Approval defaults to `Deny`.** An observer that does not answer
//!    cannot widen permissions; only an explicit human answer can.
//! 3. **The observer never controls the loop.** It watches and answers;
//!    the loop alone enforces turn limits, budget and the tool gate.

use crate::loop_engine::{StopReason, ToolOutcome, Turn};
use crate::models::{ModelId, ToolCall};
use crate::tools::{Decision, Gate};

/// One chunk of a streaming response. Kept as data rather than a method per
/// kind so an observer can store a transcript of what it saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDelta {
    /// Visible answer text.
    Text(String),
    /// Model reasoning, streamed separately by some servers. Shown as
    /// "thinking" by UIs; never mixed into the answer.
    Reasoning(String),
}

/// Receives streaming deltas from a provider. A separate trait from
/// `LoopObserver` because the *provider* pushes into it synchronously from
/// inside an async response body, while the observer is the loop's broader
/// event surface.
pub trait DeltaSink: Send + Sync {
    fn text(&self, delta: &str);
    fn reasoning(&self, _delta: &str) {}
}

/// A sink that drops everything. The default path: no UI, no overhead.
pub struct NoSink;
impl DeltaSink for NoSink {
    fn text(&self, _delta: &str) {}
}

/// Everything a UI can learn about a run, plus the one question it can
/// answer. All methods take `&self`; observers that need interior
/// mutability (a channel, a log) bring their own.
#[async_trait::async_trait]
pub trait LoopObserver: Send + Sync {
    /// A new turn is starting. Index is zero-based.
    fn on_turn_start(&self, _index: u32) {}

    /// The loop is about to ask this model for a completion. Fired once per
    /// attempt, so a failover shows up as two events.
    fn on_model_request(&self, _model: &ModelId) {}

    /// A streaming delta arrived from the model.
    fn on_delta(&self, _delta: StreamDelta) {}

    /// The model produced its final text for this turn. The accumulated
    /// deltas have already been delivered through `on_delta`; this carries
    /// the assembled whole.
    fn on_assistant_text(&self, _text: &str, _model: &ModelId) {}

    /// A tool call resolved. Exactly one event per call, in execution
    /// order, including denials.
    fn on_tool_outcome(&self, _outcome: &ToolOutcome) {}

    /// One full turn resolved, with its usage and cost. Arrives after any
    /// deltas and outcomes of that turn; a UI that tallies spend listens
    /// here rather than re-deriving it from deltas.
    fn on_turn_end(&self, _turn: &Turn) {}

    /// The gate wants a human decision. Called only when the gate decision
    /// is `Decision::Ask`. The default answer is no: an observer that does
    /// not implement approval cannot approve anything.
    ///
    /// Async because the honest implementations wait for a *different*
    /// connection — a WebSocket, an HTTP handler — to answer. A blocking
    /// wait here would park a runtime thread per pending approval, which
    /// is exactly how a gateway deadlocks under load.
    async fn approve(&self, _call: &ToolCall, _gate: &Gate) -> Decision {
        Decision::Deny
    }

    /// The run ended. Always fired exactly once, whatever the reason —
    /// a UI can close its spinner here.
    fn on_stop(&self, _reason: &StopReason) {}
}

/// The observer for callers that want none of this: `run_agent_turns`
/// without a UI. Approval stays denied; nothing is shown.
pub struct NoObserver;

impl LoopObserver for NoObserver {}

/// Forwards streaming deltas into a loop observer. The loop owns one of
/// these per model call so the observer sees deltas in arrival order.
pub struct ObserverSink<'a> {
    observer: &'a dyn LoopObserver,
}

impl<'a> ObserverSink<'a> {
    pub fn new(observer: &'a dyn LoopObserver) -> Self {
        ObserverSink { observer }
    }
}

impl DeltaSink for ObserverSink<'_> {
    fn text(&self, delta: &str) {
        self.observer.on_delta(StreamDelta::Text(delta.to_string()));
    }

    fn reasoning(&self, delta: &str) {
        self.observer
            .on_delta(StreamDelta::Reasoning(delta.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_approval_answer_is_a_denial() {
        // The safety property of the whole observer design: a UI that
        // implements nothing cannot approve anything.
        let observer = NoObserver;
        let call = ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({}),
        };
        let gate = crate::tools::Gate {
            decision: Decision::Ask,
            reason: "risk: mutating".into(),
        };
        assert_eq!(observer.approve(&call, &gate).await, Decision::Deny);
    }
}
