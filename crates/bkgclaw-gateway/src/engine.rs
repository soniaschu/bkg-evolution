//! The turn engine: one spawned task per turn, one observer per task.
//!
//! Concurrency rules that keep this sane:
//!
//! - **One turn per session at a time.** Two turns on one transcript would
//!   interleave tool calls into nonsense.
//! - **The task works on a clone and commits at the end.** A cancelled or
//!   crashed turn leaves the persisted session exactly as it was.
//! - **Approvals block the turn, not the process.** The loop runs on a
//!   worker thread; the rest of the gateway keeps serving.
//! - **"Always" lands live.** The observer and the slot share one `Arc`'d
//!   set of granted tools, so a grant applies to the very next call.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bkgclaw_core::loop_engine::{LoopConfig, StopReason, ToolOutcome, run_agent_turns_with};
use bkgclaw_core::models::{Message, ModelId};
use bkgclaw_core::observer::LoopObserver;
use bkgclaw_core::prompt::coding_prompt;
use bkgclaw_core::router::Router;
use bkgclaw_core::tools::{Decision, Gate, Policy};
use bkgclaw_core::{Budget, Detector};
use bkgclaw_exec::EngineRequest;
use bkgclaw_store::{Session, render_sections, system_prompt_sections};

use crate::events::{DeltaKind, Event, ToolOutcomeKind};
use crate::state::{Gateway, SessionSlot};

/// How long an approval question may sit unanswered before it counts as no.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);

/// The observer that mirrors one run onto the event bus and asks a human
/// when the gate says Ask.
struct GatewayObserver {
    session: String,
    gateway: Arc<Gateway>,
    /// Shared with the session slot: "always" grants made while this turn
    /// runs apply to the next call in the same turn.
    allowed: Arc<Mutex<HashSet<String>>>,
}

#[async_trait::async_trait]
impl LoopObserver for GatewayObserver {
    fn on_turn_start(&self, index: u32) {
        self.gateway.publish(Event::TurnStart {
            session: self.session.clone(),
            turn: index,
        });
    }

    fn on_model_request(&self, model: &ModelId) {
        self.gateway.publish(Event::ModelRequest {
            session: self.session.clone(),
            model: model.to_string(),
        });
    }

    fn on_delta(&self, delta: bkgclaw_core::StreamDelta) {
        let (kind, text) = match delta {
            bkgclaw_core::StreamDelta::Text(text) => (DeltaKind::Text, text),
            bkgclaw_core::StreamDelta::Reasoning(text) => (DeltaKind::Reasoning, text),
        };
        self.gateway.publish(Event::Delta {
            session: self.session.clone(),
            kind,
            text,
        });
    }

    fn on_assistant_text(&self, text: &str, model: &ModelId) {
        self.gateway.publish(Event::AssistantText {
            session: self.session.clone(),
            text: text.to_string(),
            model: model.to_string(),
        });
    }

    fn on_tool_outcome(&self, outcome: &ToolOutcome) {
        let event = match outcome {
            ToolOutcome::Ran {
                name,
                output,
                truncated,
            } => Event::ToolOutcome {
                session: self.session.clone(),
                name: name.clone(),
                outcome: ToolOutcomeKind::Ran,
                output: clip(output, 600),
                truncated: *truncated,
            },
            ToolOutcome::Denied { name, reason } => Event::ToolOutcome {
                session: self.session.clone(),
                name: name.clone(),
                outcome: ToolOutcomeKind::Denied,
                output: reason.clone(),
                truncated: false,
            },
            ToolOutcome::Failed { name, error } => Event::ToolOutcome {
                session: self.session.clone(),
                name: name.clone(),
                outcome: ToolOutcomeKind::Failed,
                output: error.clone(),
                truncated: false,
            },
        };
        self.gateway.publish(event);
    }

    fn on_turn_end(&self, turn: &bkgclaw_core::loop_engine::Turn) {
        self.gateway.publish(Event::TurnEnd {
            session: self.session.clone(),
            turn: turn.index,
            cost_usd: turn.cost_usd,
            total_tokens: turn.usage.total(),
        });
    }

    async fn approve(&self, call: &bkgclaw_core::ToolCall, gate: &Gate) -> Decision {
        // An "always" from earlier in this turn — or a previous one — needs
        // no human again.
        if self.allowed.lock().expect("allowed").contains(&call.name) {
            return Decision::Allow;
        }

        let risk = gate
            .reason
            .split("risk:")
            .nth(1)
            .map(|r| r.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        {
            let mut approvals = self.gateway.inner.approvals.lock().expect("approvals");
            let mut context = self
                .gateway
                .inner
                .approval_context
                .lock()
                .expect("approvals");
            approvals.insert(call.id.clone(), reply_tx);
            context.insert(call.id.clone(), (self.session.clone(), call.name.clone()));
        }
        self.gateway.publish(Event::ApprovalRequest {
            session: self.session.clone(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            risk,
            arguments: call.arguments.clone(),
        });

        // Await, never park: the runtime stays free to serve the very
        // connection that will answer this. Silence past the deadline is a
        // refusal — an unanswered dangerous call must not run.
        let answer = match tokio::time::timeout(APPROVAL_TIMEOUT, reply_rx).await {
            Ok(Ok(answer)) => answer,
            _ => Decision::Deny,
        };
        self.gateway
            .inner
            .approvals
            .lock()
            .expect("approvals")
            .remove(&call.id);
        self.gateway
            .inner
            .approval_context
            .lock()
            .expect("approvals")
            .remove(&call.id);
        answer
    }
    fn on_stop(&self, reason: &StopReason) {
        self.gateway.publish(Event::Stop {
            session: self.session.clone(),
            reason: format!("{reason:?}"),
        });
    }
}

/// Keep an event payload readable: the full output lives in the session
/// transcript; the event is for the live view.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

/// The per-run turn ceiling, re-exported from core — the loop owns it.
pub use bkgclaw_core::loop_engine::max_turns;



/// What to tell the operator when a run stops for a reason other than
/// "answered". Every message says what to do next — a stop without a next
/// step feels like a crash, and it is not one.
pub fn stop_message(reason: &StopReason, turns: usize, ceiling: u32) -> String {
    match reason {
        StopReason::TurnLimit => format!(
            "turn-limit erreicht ({turns} von {ceiling}) — der verlauf bleibt erhalten; \
             sende eine neue nachricht (z. b. »mach weiter«) oder erhöhe BKGCLAW_MAX_TURNS"
        ),
        StopReason::LoopDetected(name) => format!(
            "der agent wiederholte denselben `{name}`-aufruf trotz sperre — lauf gestoppt. \
             der verlauf bleibt; prüfe die letzten werkzeugergebnisse und gib eine konkrete \
             korrektur oder einen anderen auftrag"
        ),
        StopReason::BudgetBlocked =>
            "budget erreicht — die ausgaben der sitzung stehen in der werkzeugleiste; \
             task mit kleinerem umfang oder höherem budget neu starten"
                .to_string(),
        StopReason::ProviderFailed(inner) => format!(
            "kein modell konnte antworten: {inner} — später erneut versuchen"
        ),
        StopReason::Cancelled => "vom operator abgebrochen".to_string(),
        StopReason::Answered => String::new(),
    }
}

/// The result handed back when a turn finishes.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub answer: String,
    pub stop_reason: StopReason,
}

/// Assemble the full prompt for a turn: base instructions, workspace
/// sections, then the transcript.
pub fn build_transcript(
    gateway: &Gateway,
    messages: &[Message],
    new_message: &str,
) -> Vec<Message> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut prompt = coding_prompt(&cwd);
    let sections = system_prompt_sections(&gateway.workspace, &gateway.home);
    if !sections.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&render_sections(&sections));
    }
    let mut transcript = Vec::with_capacity(messages.len() + 2);
    transcript.push(Message::system(prompt));
    transcript.extend(messages.iter().cloned());
    transcript.push(Message::user(new_message));
    transcript
}

/// Start a turn on a session. Returns immediately; the caller learns the
/// outcome from events or from `wait`. Errors synchronously only when the
/// session is busy or its model is unusable.
pub fn start_turn(
    gateway: &Arc<Gateway>,
    slot: &Arc<SessionSlot>,
    message: &str,
    wait: Option<tokio::sync::oneshot::Sender<TurnResult>>,
) -> Result<(), String> {
    // Check and claim in one atomic step, or two rapid sends would both pass.
    if slot.busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err("a turn is already running on this session".to_string());
    }

    let session = slot.session.lock().expect("session").clone();
    let session_id = session.id.clone();
    let policy = Policy::parse(&session.policy);
    let overrides = Gateway::interactive_overrides(policy);
    let pinned = session.model.clone();
    let transcript = build_transcript(gateway, &session.messages, message);

    let gateway = gateway.clone();
    let slot_for_handle = slot.clone();
    let engine_tx = gateway.inner.engine_tx.clone();
    let turn_budget = gateway.turn_budget_usd;
    // Owned copies for the spawned task: borrows cannot outlive start_turn.
    let message_note = message.to_string();

    let handle = tokio::spawn(async move {
        // The task owns its own slot reference; the outer function keeps
        // the original to store this handle for cancellation.
        let slot = slot_for_handle;
        // `busy` resets however this task ends — including an abort.
        struct BusyGuard {
            slot: Arc<SessionSlot>,
        }
        impl Drop for BusyGuard {
            fn drop(&mut self) {
                self.slot
                    .busy
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = BusyGuard { slot: slot.clone() };

        // Auto-checkpoint: when the working directory opted into the
        // version system, a run that can write starts from a snapshot —
        // the operator can roll the whole tree back with one command.
        // Read-only runs snapshot nothing: their risk is zero.
        let config_allows_writes = policy.permits(bkgclaw_core::tools::Risk::Mutating)
            || !overrides.is_empty();
        if config_allows_writes {
            if let Some(cwd) = std::env::current_dir().ok() {
                if let Some(id) = bkgclaw_store::auto_snapshot(&gateway.home, &cwd, &message_note) {
                    gateway.publish(Event::Notice {
                        session: session_id.clone(),
                        text: format!("auto-snapshot {id} vor diesem lauf"),
                    });
                }
            }
        }

        let tool_registry = bkgclaw_core::tools::builtin_tools();
        let executor = bkgclaw_exec::LocalExecutor {
            home: gateway.home.clone(),
            workspace: gateway.workspace.clone(),
            exec_timeout: bkgclaw_exec::EXECUTE_TIMEOUT,
            engine: Some(engine_tx),
        };

        let config = LoopConfig {
            max_turns: max_turns(),
            max_tool_output: 8_000,
            policy,
            overrides,
            detector: Detector::new(),
        };

        // A pinned model must be in the chain; a typo would otherwise fail
        // on the first turn of every session.
        let chain = match pinned {
            Some(model) => match bkgclaw_core::ModelId::parse(&model)
                .ok_or_else(|| format!("`{model}` is not vendor/model"))
                .and_then(|id| {
                    gateway
                        .chain
                        .iter()
                        .find(|c| c.model == id)
                        .cloned()
                        .ok_or_else(|| format!("`{model}` is not in the chain"))
                }) {
                Ok(candidate) => vec![candidate],
                Err(error) => {
                    gateway.publish(Event::Error {
                        session: session_id,
                        message: error,
                    });
                    if let Some(wait) = wait {
                        let _ = wait.send(TurnResult {
                            answer: String::new(),
                            stop_reason: StopReason::ProviderFailed("model unusable".into()),
                        });
                    }
                    return;
                }
            },
            None => gateway.chain.clone(),
        };

        let observer = GatewayObserver {
            session: session_id.clone(),
            gateway: gateway.clone(),
            allowed: slot.allowed.clone(),
        };

        let mut router = Router::new(&gateway.registry, chain);
        router.breakers = bkgclaw_core::router::CircuitBreakers::new(2, Duration::from_secs(30));
        let mut budget = Budget::new(Some(turn_budget));

        let run = run_agent_turns_with(
            &mut router,
            &tool_registry,
            &executor,
            &config,
            &mut budget,
            transcript,
            &observer,
        )
        .await;

        let answer = run
            .turns
            .iter()
            .rev()
            .map(|t| t.text.as_str())
            .find(|t| !t.is_empty())
            .unwrap_or("")
            .to_string();
        if run.stop_reason != StopReason::Answered {
            gateway.publish(Event::Error {
                session: session_id.clone(),
                message: stop_message(&run.stop_reason, run.turns.len(), config.max_turns),
            });
        }

        // Commit: the session file is the truth after this. The system
        // prompt is assembled per turn, so it is not persisted.
        {
            let mut stored = slot.session.lock().expect("session");
            stored.messages = run.messages[1..].to_vec();
            if let Some(cost) = run.total_cost_usd {
                stored.spend_usd += cost;
            }
            stored.turns += run.turns.len() as u32;
            stored.touch();
            let _ = stored.save(&gateway.home);
        }
        gateway.publish(Event::SessionUpdated {
            session: session_id,
        });

        if let Some(wait) = wait {
            let _ = wait.send(TurnResult {
                answer,
                stop_reason: run.stop_reason,
            });
        }
    });
    // The handle is what `cancel_turn` aborts. Storing after spawn is safe:
    // a task that already finished leaves an inert handle, and aborting a
    // finished task is a no-op.
    *slot.task.lock().expect("task") = Some(handle);
    Ok(())
}

/// Cancel the running turn on a session. The task is aborted; because the
/// commit happens only at the end of the task, the persisted session keeps
/// exactly its pre-turn state.
pub fn cancel_turn(slot: &Arc<SessionSlot>) -> bool {
    // Claim first: without the flag, a new turn could start between the
    // abort and the reset and immediately be killed by a stale caller.
    if !slot.busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    let handle = slot.task.lock().expect("task").take();
    match handle {
        Some(handle) => {
            handle.abort();
            true
        }
        // No stored handle means the task already finished and reset busy
        // between our claim and this look — nothing to cancel.
        None => {
            slot.busy.store(false, std::sync::atomic::Ordering::SeqCst);
            false
        }
    }
}

/// Run the sub-agent engine: spawn children, answer sends, refuse steering
/// mid-turn honestly.
pub async fn run_engine_loop(gateway: Arc<Gateway>) {
    // Take the receiver out once: the loop owns it for the process's
    // lifetime. A second call finds nothing and returns — there is one
    // engine, not zero, not two.
    let Some(mut receiver) = gateway.inner.engine_rx.lock().expect("engine").take() else {
        return;
    };
    while let Some(request) = receiver.recv().await {
        match request {
            EngineRequest::Spawn { task, model, reply } => {
                let session = Session::new(model, "allow-mutating".to_string());
                let id = session.id.clone();
                if session.save(&gateway.home).is_err() {
                    let _ = reply.send(Err("could not persist the sub-agent session".to_string()));
                    continue;
                }
                let slot = gateway.insert_session(session);
                gateway.publish(Event::SessionUpdated {
                    session: id.clone(),
                });
                // Children run with the same engine: a child may spawn
                // grandchildren through the same channel.
                if let Err(error) = start_turn(&gateway, &slot, &task, None) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let _ = reply.send(Ok(format!(
                    "sub-agent session {id} created and started; ask for its result with sessions_send"
                )));
            }
            EngineRequest::Send {
                session,
                message,
                reply,
            } => {
                let Some(slot) = gateway.slot(&session) else {
                    let _ = reply.send(Err(format!("no session named `{session}`")));
                    continue;
                };
                if slot.busy.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = reply.send(Ok(format!(
                        "session {session} is still working; ask again in a moment"
                    )));
                    continue;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                if let Err(error) = start_turn(&gateway, &slot, &message, Some(tx)) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                match rx.await {
                    Ok(result) => {
                        if result.answer.is_empty() {
                            let _ = reply.send(Ok(format!(
                                "session {session} finished without a final answer ({:?})",
                                result.stop_reason
                            )));
                        } else {
                            let _ = reply.send(Ok(result.answer));
                        }
                    }
                    Err(_) => {
                        let _ = reply.send(Err(format!("session {session} was cancelled")));
                    }
                }
            }
            EngineRequest::Steer {
                session,
                instruction,
                reply,
            } => {
                let Some(slot) = gateway.slot(&session) else {
                    let _ = reply.send(Err(format!("no session named `{session}`")));
                    continue;
                };
                if slot.busy.load(std::sync::atomic::Ordering::SeqCst) {
                    // Mid-turn steering needs a hook inside the running
                    // loop that does not exist. Pretending to steer would be
                    // a lie the parent agent would rely on.
                    let _ = reply.send(Err(
                        "cannot steer mid-turn in this build; send it when the session is idle"
                            .to_string(),
                    ));
                    continue;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                if let Err(error) = start_turn(&gateway, &slot, &instruction, Some(tx)) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let _ = reply.send(match rx.await {
                    Ok(result) => Ok(result.answer),
                    Err(_) => Err(format!("session {session} was cancelled")),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_is_system_then_history_then_the_new_message() {
        let gateway = Gateway::new(
            bkgclaw_core::ModelRegistry::new(),
            Vec::new(),
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
        );
        let history = vec![
            Message::user("erste frage"),
            Message::assistant("erste antwort"),
        ];
        let transcript = build_transcript(&gateway, &history, "zweite frage");
        assert_eq!(transcript.len(), 4);
        assert!(matches!(transcript[0], Message::System { .. }));
        assert!(matches!(transcript[1], Message::User { .. }));
        assert!(matches!(transcript[2], Message::Assistant { .. }));
        assert_eq!(
            transcript[3],
            Message::user("zweite frage"),
            "the new message must be last"
        );
    }

    #[test]
    fn the_system_prompt_includes_workspace_sections_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "Wir nutzen NIM.").unwrap();
        let gateway = Gateway::new(
            bkgclaw_core::ModelRegistry::new(),
            Vec::new(),
            std::env::temp_dir(),
            dir.path().to_path_buf(),
            None,
        );
        let transcript = build_transcript(&gateway, &[], "hi");
        let Message::System { content } = &transcript[0] else {
            panic!("first message must be the system prompt");
        };
        assert!(content.contains("Wir nutzen NIM."));
        assert!(
            content.contains("bkgclaw"),
            "the base prompt is always present"
        );
    }

    #[test]
    fn event_payloads_are_clipped_for_the_live_view() {
        let long = "x".repeat(2000);
        let clipped = clip(&long, 600);
        assert!(
            clipped.chars().count() <= 601,
            "got {}",
            clipped.chars().count()
        );
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("kurz", 600), "kurz");
    }
}
