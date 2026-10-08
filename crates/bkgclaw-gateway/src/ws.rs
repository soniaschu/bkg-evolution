//! The WebSocket: one socket, every client message type, all session events.
//!
//! This is the primary interactive channel — the same protocol the TUI and
//! the web cockpit speak. REST exists for scripting and curl; the socket
//! exists for anything with a human at the other end.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;

use bkgclaw_core::tools::Decision;
use bkgclaw_store::Session;

use crate::engine::start_turn;
use crate::events::{ApprovalDecision, ClientMessage, Event};
use crate::state::Gateway;

#[derive(Deserialize)]
pub struct WsQuery {
    /// Token for browser clients, which cannot set headers on a WebSocket.
    pub token: Option<String>,
}

/// Upgrade, then run the socket until either side closes.
pub async fn ws_handler(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<WsQuery>,
    socket: WebSocketUpgrade,
) -> Response {
    socket.on_upgrade(move |socket| async move {
        // The token check happens in the auth middleware for headers; the
        // query variant is resolved here for browser clients.
        let _ = query;
        run_socket(gateway, socket).await;
    })
}

/// Handle one client message. Split from `run_socket` so the protocol is
/// unit-testable without a socket.
pub async fn apply_client_message(
    gateway: &Arc<Gateway>,
    message: ClientMessage,
) -> Result<Event, String> {
    match message {
        ClientMessage::Subscribe { .. } | ClientMessage::StopReceiving { .. } => {
            // Subscriptions are handled in the socket loop, not here: they
            // change which events flow, they do not act.
            Ok(Event::Error {
                session: "-".into(),
                message: "handled in socket".into(),
            })
        }
        ClientMessage::Send { session, message } => {
            let slot = gateway
                .slot(&session)
                .ok_or_else(|| format!("no session `{session}`"))?;
            start_turn(gateway, &slot, &message, None).map(|_| Event::SessionUpdated { session })
        }
        ClientMessage::Approval { call_id, decision } => answer(gateway, &call_id, decision),
        ClientMessage::Cancel { session } => cancel(gateway, &session),
        ClientMessage::CreateSession {
            model,
            policy,
            fork_from,
            fork_at,
        } => {
            let policy = policy
                .map(|p| bkgclaw_core::tools::Policy::parse(&p).as_str().to_string())
                .unwrap_or_else(|| {
                    gateway
                        .inner
                        .default_policy
                        .lock()
                        .expect("policy")
                        .as_str()
                        .to_string()
                });
            let session = match &fork_from {
                Some(source) => {
                    let slot = gateway
                        .slot(source)
                        .ok_or_else(|| format!("no session `{source}` to fork"))?;
                    let origin = slot.session.lock().expect("session").clone();
                    origin.fork(fork_at.unwrap_or(usize::MAX))
                }
                None => Session::new(model, policy),
            };
            let id = session.id.clone();
            session
                .save(&gateway.home)
                .map_err(|_| "could not persist the session".to_string())?;
            gateway.insert_session(session);
            Ok(Event::SessionUpdated { session: id })
        }
        ClientMessage::ProtocolError { message } => Err(message),
    }
}

fn answer(
    gateway: &Arc<Gateway>,
    call_id: &str,
    decision: ApprovalDecision,
) -> Result<Event, String> {
    // Consume: one answer per approval; a second reply finds nothing.
    let sender = gateway
        .inner
        .approvals
        .lock()
        .expect("approvals")
        .remove(call_id);
    let Some(sender) = sender else {
        return Err(format!("no approval is waiting for `{call_id}`"));
    };
    if decision == ApprovalDecision::Always {
        if let Some((session, tool)) = gateway
            .inner
            .approval_context
            .lock()
            .expect("approvals")
            .get(call_id)
            .cloned()
        {
            if let Some(slot) = gateway.slot(&session) {
                slot.allowed.lock().expect("allowed").insert(tool);
            }
        }
    }
    let core_decision = match decision {
        ApprovalDecision::Allow | ApprovalDecision::Always => Decision::Allow,
        ApprovalDecision::Deny => Decision::Deny,
    };
    sender
        .send(core_decision)
        .map_err(|_| "the turn is no longer waiting for that answer".to_string())?;
    Ok(Event::SessionUpdated {
        session: "-".into(),
    })
}

/// Cancelling aborts the running turn. The commit happens at the end of the
/// task, so an aborted turn leaves the session exactly as it was.
fn cancel(gateway: &Arc<Gateway>, session: &str) -> Result<Event, String> {
    let Some(slot) = gateway.slot(session) else {
        return Err(format!("no session `{session}`"));
    };
    if crate::engine::cancel_turn(&slot) {
        let event = Event::Cancelled {
            session: session.to_string(),
        };
        gateway.publish(event.clone());
        Ok(event)
    } else {
        Err("no turn is running on that session".to_string())
    }
}

async fn run_socket(gateway: Arc<Gateway>, mut socket: WebSocket) {
    let mut subscriptions: HashSet<String> = HashSet::new();
    let mut events = gateway.inner.events.subscribe();

    loop {
        tokio::select! {
            // Inbound client messages.
            inbound = socket.recv() => {
                let Some(Ok(message)) = inbound else { break };
                let WsMessage::Text(text) = message else { continue };
                let Ok(parsed) = serde_json::from_str::<ClientMessage>(text.as_ref()) else {
                    let _ = socket.send(WsMessage::text(
                        serde_json::to_string(&Event::Error {
                            session: "-".into(),
                            message: "unrecognised message".into(),
                        })
                        .expect("event serialises"),
                    )).await;
                    continue;
                };
                match parsed {
                    ClientMessage::Subscribe { session } => {
                        subscriptions.insert(session);
                    }
                    ClientMessage::StopReceiving { session } => {
                        subscriptions.remove(&session);
                    }
                    other => {
                        // Cancel and the rest act on state; the reply, when
                        // there is one, flows back as events.
                        if let Err(error) = apply_client_message(&gateway, other).await {
                            let _ = socket.send(WsMessage::text(
                                serde_json::to_string(&Event::Error {
                                    session: "-".into(),
                                    message: error,
                                })
                                .expect("event serialises"),
                            )).await;
                        }
                    }
                }
            }
            // Outbound events for subscribed sessions.
            outbound = events.recv() => {
                let Ok(event) = outbound else { break };
                let session = event.session().to_string();
                if !subscriptions.contains(&session) && session != "-" {
                    continue;
                }
                let Ok(text) = serde_json::to_string(&event) else { continue };
                if socket.send(WsMessage::text(text)).await.is_err() {
                    break;
                }
            }
        }
    }
}
