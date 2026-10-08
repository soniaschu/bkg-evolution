//! TUI state: everything the terminal shows, plus the pure functions that
//! change it. No I/O in this module — the keyboard and the WebSocket feed
//! `apply` and `handle_key`, and the tests drive both without a terminal.

use bkgclaw_gateway::events::{ApprovalDecision, ClientMessage, Event};

/// One rendered transcript line. What happened, not how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        name: String,
        outcome: String,
        output: String,
    },
    Status(String),
    Error(String),
    Cancelled,
}

/// A pending approval, as the modal needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub call_id: String,
    pub name: String,
    pub risk: String,
    pub arguments: String,
}

/// What the key handler wants the orchestrator to do.
#[derive(Debug, Clone)]
pub enum Cmd {
    Nothing,
    /// Send this over the WebSocket.
    Ws(ClientMessage),
    /// Refresh the session list from REST.
    RefreshSessions,
    Quit,
}

/// Which overlay is open, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    None,
    Help,
    Sessions,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub session: String,
    pub lines: Vec<Line>,
    /// Deltas of the assistant message currently streaming.
    pub live: String,
    pub live_reasoning: String,
    pub input: String,
    /// Transcript scroll in lines from the bottom.
    pub scroll: u16,
    pub busy: bool,
    pub pending_approval: Option<PendingApproval>,
    pub sessions: Vec<(String, String)>,
    pub modal: Modal,
    pub status: String,
    pub quit: bool,
    pub spent: f64,
    pub model: Option<String>,
    pub policy: String,
    /// Set by `/new` and `/fork`: the next `SessionUpdated` for an unknown
    /// session is the answer, and the TUI adopts it as active.
    pub pending_new_session: bool,
}

impl TuiState {
    pub fn new(session: String, model: Option<String>, policy: String) -> Self {
        TuiState {
            session,
            lines: Vec::new(),
            live: String::new(),
            live_reasoning: String::new(),
            input: String::new(),
            scroll: 0,
            busy: false,
            pending_approval: None,
            sessions: Vec::new(),
            modal: Modal::None,
            status: "bereit".into(),
            quit: false,
            spent: 0.0,
            model,
            policy,
            pending_new_session: false,
        }
    }
}

/// Fold a gateway event into the state. Pure: no printing, no sockets.
pub fn apply(state: &mut TuiState, event: &Event) {
    // Events for other sessions are filtered by the orchestrator; this
    // function may assume the event belongs to `state.session`.
    match event {
        Event::TurnStart { .. } => {
            state.busy = true;
            state.status = "denkt…".into();
            state.scroll = 0;
        }
        Event::ModelRequest { model, .. } => {
            state.status = format!("fragt {model}…");
        }
        Event::Delta { kind, text, .. } => {
            use bkgclaw_gateway::events::DeltaKind;
            match kind {
                DeltaKind::Text => state.live.push_str(text),
                DeltaKind::Reasoning => state.live_reasoning.push_str(text),
            }
        }
        Event::AssistantText { text, .. } => {
            if !state.live_reasoning.is_empty() {
                state
                    .lines
                    .push(Line::Reasoning(std::mem::take(&mut state.live_reasoning)));
            }
            // The final text is authoritative: the loop sends the same
            // content the deltas accumulated. The live buffer is kept only
            // for the degenerate case of a stream that never finished.
            let message = if !text.trim().is_empty() {
                text.clone()
            } else {
                std::mem::take(&mut state.live)
            };
            if !message.trim().is_empty() {
                state.lines.push(Line::Assistant(message));
            }
            state.live.clear();
            state.status = "antwortet…".into();
        }
        Event::ToolOutcome {
            name,
            outcome,
            output,
            ..
        } => {
            let label = match outcome {
                bkgclaw_gateway::events::ToolOutcomeKind::Ran => "ran",
                bkgclaw_gateway::events::ToolOutcomeKind::Denied => "denied",
                bkgclaw_gateway::events::ToolOutcomeKind::Failed => "failed",
            };
            state.lines.push(Line::Tool {
                name: name.clone(),
                outcome: label.to_string(),
                output: output.clone(),
            });
        }
        Event::ApprovalRequest {
            call_id,
            name,
            risk,
            arguments,
            ..
        } => {
            state.pending_approval = Some(PendingApproval {
                call_id: call_id.clone(),
                name: name.clone(),
                risk: risk.clone(),
                arguments: arguments.to_string(),
            });
        }
        Event::TurnEnd { cost_usd, .. } => {
            if let Some(cost) = cost_usd {
                state.spent += cost;
            }
        }
        Event::Stop { .. } => {
            state.busy = false;
            state.status = "bereit".into();
            if !state.live.is_empty() {
                // The stream ended without a final text event; keep what
                // arrived rather than dropping the message.
                state
                    .lines
                    .push(Line::Assistant(std::mem::take(&mut state.live)));
            }
            if !state.live_reasoning.is_empty() {
                state
                    .lines
                    .push(Line::Reasoning(std::mem::take(&mut state.live_reasoning)));
            }
        }
        Event::SessionUpdated { session } => {
            state.status = format!("sitzung {session}");
        }
        Event::Cancelled { .. } => {
            state.busy = false;
            state.lines.push(Line::Cancelled);
            state.live.clear();
            state.live_reasoning.clear();
            state.status = "abgebrochen".into();
        }
        Event::Error { message, .. } => {
            state.lines.push(Line::Error(message.clone()));
        }
        Event::Notice { text, .. } => {
            state.lines.push(Line::Status(text.clone()));
        }
    }
}

/// One keystroke, folded into state plus a command for the orchestrator.
pub fn handle_key(state: &mut TuiState, key: crossterm::event::KeyEvent) -> Cmd {
    use crossterm::event::{KeyCode, KeyModifiers};

    // The approval modal swallows every key until it is answered: a
    // dangerous call must not be smuggled past a distracted operator by
    // input meant for the chat box.
    if let Some(pending) = state.pending_approval.clone() {
        return match key.code {
            KeyCode::Char('j') | KeyCode::Char('y') => {
                state.pending_approval = None;
                Cmd::Ws(ClientMessage::Approval {
                    call_id: pending.call_id,
                    decision: ApprovalDecision::Allow,
                })
            }
            KeyCode::Char('n') => {
                state.pending_approval = None;
                Cmd::Ws(ClientMessage::Approval {
                    call_id: pending.call_id,
                    decision: ApprovalDecision::Deny,
                })
            }
            KeyCode::Char('i') => {
                state.pending_approval = None;
                Cmd::Ws(ClientMessage::Approval {
                    call_id: pending.call_id,
                    decision: ApprovalDecision::Always,
                })
            }
            _ => Cmd::Nothing,
        };
    }

    match key.code {
        // Ctrl-C: interrupt the running turn, or quit when idle. A second
        // Ctrl-C on an idle screen quits — one clear gesture per intent.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.busy {
                let session = state.session.clone();
                state.busy = false;
                Cmd::Ws(ClientMessage::Cancel { session })
            } else {
                state.quit = true;
                Cmd::Quit
            }
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.quit = true;
            Cmd::Quit
        }
        KeyCode::Esc => {
            state.modal = Modal::None;
            Cmd::Nothing
        }
        KeyCode::F(1) => {
            state.modal = match state.modal {
                Modal::Help => Modal::None,
                _ => Modal::Help,
            };
            Cmd::Nothing
        }
        KeyCode::F(2) => {
            state.modal = match state.modal {
                Modal::Sessions => Modal::None,
                _ => Modal::Sessions,
            };
            Cmd::RefreshSessions
        }
        KeyCode::Up => {
            state.scroll = state.scroll.saturating_add(1);
            Cmd::Nothing
        }
        KeyCode::Down => {
            state.scroll = state.scroll.saturating_sub(1);
            Cmd::Nothing
        }
        KeyCode::Enter => submit(state),
        // Ctrl+J is the newline key: terminals cannot distinguish Enter
        // from Shift+Enter, so the newline gets its own chord.
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.push('\n');
            Cmd::Nothing
        }
        KeyCode::Backspace => {
            if let Some(ch) = state.input.pop() {
                // Multi-byte characters pop as whole chars, not bytes.
                let _ = ch;
            }
            Cmd::Nothing
        }
        KeyCode::Char(ch) => {
            state.input.push(ch);
            Cmd::Nothing
        }
        _ => Cmd::Nothing,
    }
}

/// Enter: an empty input does nothing; a slash command is local; anything
/// else goes to the model as a turn.
fn submit(state: &mut TuiState) -> Cmd {
    let input = std::mem::take(&mut state.input);
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Cmd::Nothing;
    }
    if !trimmed.starts_with('/') {
        state.lines.push(Line::User(trimmed.to_string()));
        let session = state.session.clone();
        return Cmd::Ws(ClientMessage::Send {
            session,
            message: trimmed.to_string(),
        });
    }
    let (command, rest) = match trimmed.split_once(' ') {
        Some((head, tail)) => (head, tail.trim()),
        None => (trimmed, ""),
    };
    match command {
        "/exit" | "/quit" => {
            state.quit = true;
            Cmd::Quit
        }
        "/help" => {
            state.modal = Modal::Help;
            Cmd::Nothing
        }
        "/sessions" => {
            state.modal = Modal::Sessions;
            Cmd::RefreshSessions
        }
        "/new" => {
            state.pending_new_session = true;
            Cmd::Ws(ClientMessage::CreateSession {
                model: state.model.clone(),
                policy: Some(state.policy.clone()),
                fork_from: None,
                fork_at: None,
            })
        }
        "/fork" => {
            let Some(origin) = rest.split_whitespace().next() else {
                state
                    .lines
                    .push(Line::Error("/fork <sitzungs-id> [bei nachricht-nr]".into()));
                return Cmd::Nothing;
            };
            let fork_at = rest
                .split_whitespace()
                .nth(1)
                .and_then(|n| n.parse::<usize>().ok());
            state.pending_new_session = true;
            Cmd::Ws(ClientMessage::CreateSession {
                model: None,
                policy: None,
                fork_from: Some(origin.to_string()),
                fork_at,
            })
        }
        "/model" if rest.is_empty() => {
            state.lines.push(Line::Status(format!(
                "modell: {}",
                state.model.as_deref().unwrap_or("(failover-kette)")
            )));
            Cmd::Nothing
        }
        "/model" => {
            state.model = Some(rest.to_string());
            state.lines.push(Line::Status(format!(
                "modell: {rest} (ab nächster sitzung)"
            )));
            Cmd::Nothing
        }
        "/policy" if !rest.is_empty() => {
            state.policy = rest.to_string();
            state.lines.push(Line::Status(format!(
                "policy: {rest} (ab nächster sitzung)"
            )));
            Cmd::Nothing
        }
        "/cost" => {
            state
                .lines
                .push(Line::Status(format!("${:.4} sitzung", state.spent)));
            Cmd::Nothing
        }
        other => {
            state.lines.push(Line::Error(format!(
                "{other} ist kein befehl — F1 zeigt die Liste"
            )));
            Cmd::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bkgclaw_gateway::events::DeltaKind;
    use crossterm::event::KeyCode;

    fn state() -> TuiState {
        TuiState::new("s-1".into(), None, "allow-read-only".into())
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn a_full_turn_streams_into_the_transcript() {
        let mut s = state();
        apply(
            &mut s,
            &Event::TurnStart {
                session: "s-1".into(),
                turn: 0,
            },
        );
        apply(
            &mut s,
            &Event::ModelRequest {
                session: "s-1".into(),
                model: "nim/x".into(),
            },
        );
        apply(
            &mut s,
            &Event::Delta {
                session: "s-1".into(),
                kind: DeltaKind::Reasoning,
                text: "hm".into(),
            },
        );
        apply(
            &mut s,
            &Event::Delta {
                session: "s-1".into(),
                kind: DeltaKind::Text,
                text: "Gute ".into(),
            },
        );
        apply(
            &mut s,
            &Event::Delta {
                session: "s-1".into(),
                kind: DeltaKind::Text,
                text: "Frage".into(),
            },
        );
        apply(
            &mut s,
            &Event::AssistantText {
                session: "s-1".into(),
                text: "Antwort".into(),
                model: "nim/x".into(),
            },
        );
        apply(
            &mut s,
            &Event::ToolOutcome {
                session: "s-1".into(),
                name: "read_file".into(),
                outcome: bkgclaw_gateway::events::ToolOutcomeKind::Ran,
                output: "inhalt".into(),
                truncated: false,
            },
        );
        apply(
            &mut s,
            &Event::Stop {
                session: "s-1".into(),
                reason: "Answered".into(),
            },
        );

        assert!(!s.busy);
        assert!(s.live.is_empty());
        assert_eq!(s.lines[0], Line::Reasoning("hm".into()));
        // The streamed deltas were replaced by the final text.
        assert_eq!(s.lines[1], Line::Assistant("Antwort".into()));
        assert!(matches!(s.lines[2], Line::Tool { .. }));
    }

    #[test]
    fn typing_and_submitting_sends_the_message() {
        let mut s = state();
        for ch in "hallo welt".chars() {
            handle_key(&mut s, key(KeyCode::Char(ch)));
        }
        let cmd = handle_key(&mut s, key(KeyCode::Enter));
        match cmd {
            Cmd::Ws(ClientMessage::Send { session, message }) => {
                assert_eq!(session, "s-1");
                assert_eq!(message, "hallo welt");
            }
            other => panic!("expected a send, got {other:?}"),
        }
        assert_eq!(s.lines.last(), Some(&Line::User("hallo welt".into())));
        assert!(s.input.is_empty());
    }

    #[test]
    fn the_approval_modal_answers_with_a_decision() {
        let mut s = state();
        apply(
            &mut s,
            &Event::ApprovalRequest {
                session: "s-1".into(),
                call_id: "call-9".into(),
                name: "write_file".into(),
                risk: "mutating".into(),
                arguments: serde_json::json!({ "path": "x" }),
            },
        );
        assert!(s.pending_approval.is_some());
        // While the modal is open, chat keys must not leak into the input.
        handle_key(&mut s, key(KeyCode::Char('x')));
        assert!(s.input.is_empty());
        match handle_key(&mut s, key(KeyCode::Char('i'))) {
            Cmd::Ws(ClientMessage::Approval {
                call_id,
                decision: ApprovalDecision::Always,
            }) => {
                assert_eq!(call_id, "call-9");
            }
            other => panic!("expected an always-approval, got {other:?}"),
        }
        assert!(s.pending_approval.is_none());
    }

    #[test]
    fn ctrl_c_interrupts_a_running_turn_and_quits_when_idle() {
        let mut s = state();
        let ctrl_c = crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        s.busy = true;
        assert!(matches!(
            handle_key(&mut s, ctrl_c),
            Cmd::Ws(ClientMessage::Cancel { .. })
        ));
        s.busy = false;
        assert!(matches!(handle_key(&mut s, ctrl_c), Cmd::Quit));
        assert!(s.quit);
    }

    #[test]
    fn slash_commands_stay_local() {
        let mut s = state();
        for ch in "/help".chars() {
            handle_key(&mut s, key(KeyCode::Char(ch)));
        }
        assert!(matches!(
            handle_key(&mut s, key(KeyCode::Enter)),
            Cmd::Nothing
        ));
        assert_eq!(s.modal, Modal::Help);

        let mut s = state();
        for ch in "/cost".chars() {
            handle_key(&mut s, key(KeyCode::Char(ch)));
        }
        assert!(matches!(
            handle_key(&mut s, key(KeyCode::Enter)),
            Cmd::Nothing
        ));
        assert!(matches!(s.lines.last(), Some(Line::Status(_))));
    }

    #[test]
    fn an_unknown_slash_command_is_an_error_line_not_a_send() {
        let mut s = state();
        for ch in "/unsinn".chars() {
            handle_key(&mut s, key(KeyCode::Char(ch)));
        }
        assert!(matches!(
            handle_key(&mut s, key(KeyCode::Enter)),
            Cmd::Nothing
        ));
        assert!(matches!(s.lines.last(), Some(Line::Error(_))));
    }

    #[test]
    fn ctrl_j_inserts_a_newline_for_multiline_prompts() {
        let mut s = state();
        handle_key(&mut s, key(KeyCode::Char('a')));
        let ctrl_j = crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        handle_key(&mut s, ctrl_j);
        handle_key(&mut s, key(KeyCode::Char('b')));
        assert_eq!(s.input, "a\nb");
    }

    #[test]
    fn a_cancel_event_clears_the_live_buffers() {
        let mut s = state();
        apply(
            &mut s,
            &Event::TurnStart {
                session: "s-1".into(),
                turn: 0,
            },
        );
        apply(
            &mut s,
            &Event::Delta {
                session: "s-1".into(),
                kind: DeltaKind::Text,
                text: "halbe ant".into(),
            },
        );
        apply(
            &mut s,
            &Event::Cancelled {
                session: "s-1".into(),
            },
        );
        assert!(s.live.is_empty());
        assert!(!s.busy);
        assert!(matches!(s.lines.last(), Some(Line::Cancelled)));
    }
}
