//! REST and WebSocket access from WASM. Thin on purpose: every function
//! maps one gateway endpoint, nothing more.

use crate::{Cockpit, MemoryRow, SessionRow, SkillRow, TaskRow, TranscriptItem, WireEvent};
use dioxus::prelude::*;
use gloo_net::http::Request;
use serde_json::json;
use std::sync::OnceLock;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// The gateway base: same origin by default (the gateway serves this app),
/// overridable through the URL hash so a browser can point elsewhere.
pub fn resolve_base() -> String {
    let hash = web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default();
    let hash = hash.trim_start_matches('#');
    if hash.starts_with("http") {
        hash.trim_end_matches('/').to_string()
    } else {
        web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default()
    }
}

pub async fn get_json(base: &str, path: &str) -> Result<serde_json::Value, String> {
    Request::get(&format!("{base}{path}"))
        .send()
        .await
        .map_err(|e| format!("nicht erreichbar: {e}"))?
        .json()
        .await
        .map_err(|e| format!("antwort keine json: {e}"))
}

pub async fn post_json(base: &str, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    Request::post(&format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .map_err(|e| format!("{e}"))?
        .send()
        .await
        .map_err(|e| format!("nicht erreichbar: {e}"))?
        .json()
        .await
        .map_err(|e| format!("antwort keine json: {e}"))
}

pub async fn fetch_sessions(base: &str) -> Result<Vec<SessionRow>, String> {
    let payload = get_json(base, "/api/sessions").await?;
    Ok(serde_json::from_value(payload["data"].clone()).unwrap_or_default())
}

pub async fn fetch_models(base: &str) -> Vec<String> {
    let payload = get_json(base, "/api/models").await.unwrap_or(json!({ "data": [] }));
    payload["data"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| row["usable"].as_bool().unwrap_or(false))
                .filter_map(|row| row["model"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub async fn create_session(base: &str, model: Option<String>, policy: String) -> Result<String, String> {
    let payload = post_json(base, "/api/sessions", json!({ "model": model, "policy": policy })).await?;
    payload["data"]["session"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "keine sitzungs-id".to_string())
}

pub async fn fetch_transcript(base: &str, session: &str) -> Vec<TranscriptItem> {
    let payload = get_json(base, &format!("/api/sessions/{session}")).await.unwrap_or(json!({}));
    let messages = payload["data"]["messages"].as_array().cloned().unwrap_or_default();
    messages
        .iter()
        .filter_map(|message| {
            let role = message["role"].as_str()?;
            let content = message["content"].as_str().unwrap_or("").to_string();
            match role {
                "user" => Some(TranscriptItem::User(content)),
                "assistant" => Some(TranscriptItem::Assistant(content)),
                "tool_result" => Some(TranscriptItem::Tool {
                    name: "werkzeug".into(),
                    outcome: "ran".into(),
                    output: content,
                }),
                _ => None,
            }
        })
        .collect()
}

pub async fn fetch_tasks(base: &str) -> Vec<TaskRow> {
    let payload = get_json(base, "/api/tasks").await.unwrap_or(json!({ "data": [] }));
    serde_json::from_value(payload["data"].clone()).unwrap_or_default()
}

pub async fn fetch_memory(base: &str) -> Vec<MemoryRow> {
    let payload = get_json(base, "/api/memory").await.unwrap_or(json!({ "data": [] }));
    serde_json::from_value(payload["data"].clone()).unwrap_or_default()
}

pub async fn fetch_skills(base: &str) -> Vec<SkillRow> {
    let payload = get_json(base, "/api/skills").await.unwrap_or(json!({ "data": [] }));
    serde_json::from_value(payload["data"].clone()).unwrap_or_default()
}

/// Fold one gateway event into the cockpit state.
pub fn apply_event(state: &mut Cockpit, event: &WireEvent) {
    match event {
        WireEvent::TurnStart { .. } => {
            state.busy = true;
        }
        WireEvent::ModelRequest { model, .. } => {
            state.notice = format!("fragt {model}…");
        }
        WireEvent::Delta { kind, text, .. } => match kind {
            crate::DeltaKind::Text => state.live.push_str(text),
            crate::DeltaKind::Reasoning => state.live_reasoning.push_str(text),
        },
        WireEvent::AssistantText { text, .. } => {
            if !state.live_reasoning.is_empty() {
                let reasoning = std::mem::take(&mut state.live_reasoning);
                state.transcript.push(TranscriptItem::Reasoning(reasoning));
            }
            if !text.trim().is_empty() {
                state.transcript.push(TranscriptItem::Assistant(text.clone()));
            }
            state.live.clear();
        }
        WireEvent::ToolOutcome { name, outcome, output, .. } => {
            state.transcript.push(TranscriptItem::Tool {
                name: name.clone(),
                outcome: outcome.clone(),
                output: output.clone(),
            });
        }
        WireEvent::ApprovalRequest { call_id, name, risk, arguments, .. } => {
            state.pending_approval = Some(crate::ApprovalRequest {
                call_id: call_id.clone(),
                name: name.clone(),
                risk: risk.clone(),
                arguments: arguments.clone(),
            });
        }
        WireEvent::TurnEnd { cost_usd, .. } => {
            if let Some(cost) = cost_usd {
                state.spent += cost;
            }
        }
        WireEvent::Stop { .. } => {
            state.busy = false;
            if !state.live.is_empty() {
                let live = std::mem::take(&mut state.live);
                state.transcript.push(TranscriptItem::Assistant(live));
            }
            if !state.live_reasoning.is_empty() {
                let reasoning = std::mem::take(&mut state.live_reasoning);
                state.transcript.push(TranscriptItem::Reasoning(reasoning));
            }
            state.notice = "bereit".into();
        }
        WireEvent::SessionUpdated { session } => {
            if state.pending_new && Some(session.as_str()) != state.session.as_deref() {
                state.session = Some(session.clone());
                state.transcript.clear();
                state.spent = 0.0;
                state.pending_new = false;
                state.live.clear();
                state.live_reasoning.clear();
            }
            state.notice = format!("sitzung {session}");
        }
        WireEvent::Cancelled { .. } => {
            state.busy = false;
            state.live.clear();
            state.live_reasoning.clear();
            state.notice = "abgebrochen".into();
        }
        WireEvent::Error { message, .. } => {
            state.transcript.push(TranscriptItem::Error(message.clone()));
        }
    }
}

/// The page-lifetime WebSocket. WASM is single-threaded, so a global with a
/// leaked onmessage closure is the standard web-sys pattern.
static SOCKET: OnceLock<web_sys::WebSocket> = OnceLock::new();

/// Open (once) and return the socket; events are folded into the signal by
/// the onmessage closure.
pub fn ensure_socket(base: &str, cockpit: dioxus::prelude::Signal<Cockpit>) -> Option<web_sys::WebSocket> {
    // Dioxus 0.7: `write` needs a mutable signal binding.
    if let Some(socket) = SOCKET.get() {
        return Some(socket.clone());
    }
    let ws_base = format!("{}/ws", base.replace("http://", "ws://").replace("https://", "wss://"));
    let socket = web_sys::WebSocket::new(&ws_base).ok()?;

    let mut signal = cockpit.clone();
    let handler = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        let text = match event.data().as_string() { Some(text) => text, None => return };
        let Ok(parsed) = serde_json::from_str::<WireEvent>(&text) else { return };
        let mut state = signal.write();
        // Only events for the active session change the chat; the rest are
        // session bookkeeping a refresh already covers.
        let belongs = match &parsed {
            WireEvent::SessionUpdated { .. } => true,
            other => other.session() == state.session.clone().unwrap_or_default(),
        };
        if belongs {
            apply_event(&mut state, &parsed);
        }
    });
    // `forget`: the closure must outlive this function — it IS the socket's
    // listener for the page lifetime. Standard web-sys practice.
    let handler = handler.into_js_value();
    socket.set_onmessage(Some(handler.unchecked_ref()));
    SOCKET.set(socket.clone()).ok()?;
    Some(socket)
}

/// Send one client message; silent when the socket is gone (the next
/// user action reconnects nothing — the page reloads).
pub fn send(message: &crate::WireClient) {
    if let Some(socket) = SOCKET.get() {
        if let Ok(text) = serde_json::to_string(message) {
            let _ = socket.send_with_str(&text);
        }
    }
}

impl WireEvent {
    fn session(&self) -> String {
        match self {
            WireEvent::TurnStart { session, .. }
            | WireEvent::ModelRequest { session, .. }
            | WireEvent::Delta { session, .. }
            | WireEvent::AssistantText { session, .. }
            | WireEvent::ToolOutcome { session, .. }
            | WireEvent::ApprovalRequest { session, .. }
            | WireEvent::TurnEnd { session, .. }
            | WireEvent::Stop { session, .. }
            | WireEvent::SessionUpdated { session }
            | WireEvent::Cancelled { session }
            | WireEvent::Error { session, .. } => session.clone(),
        }
    }
}
