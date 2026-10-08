//! REST handlers. Every endpoint answers JSON; the wire contract is
//! `{"ok": bool, "data": …}` so a client needs one parser, not one per
//! route.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use bkgclaw_core::tools::{Decision, Policy};
use bkgclaw_store::Session;

use crate::engine::start_turn;
use crate::events::{ApprovalDecision, Event};
use crate::state::Gateway;

/// `GET /api/health` — no auth: the answer contains nothing worth guarding.
pub async fn health(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let sessions = bkgclaw_store::Session::list(&gateway.home).len();
    Json(json!({
        "ok": true,
        "service": "bkgclaw-gateway",
        "nim_configured": gateway.has_nim(),
        "models": gateway.usable_models().len(),
        "sessions": sessions,
    }))
}

pub async fn models(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let models: Vec<serde_json::Value> = gateway
        .usable_models()
        .into_iter()
        .map(
            |(model, vendor, usable)| json!({ "model": model, "vendor": vendor, "usable": usable }),
        )
        .collect();
    Json(json!({ "ok": true, "data": models }))
}

pub async fn tools(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let registry = bkgclaw_core::tools::builtin_tools();
    let policy = *gateway.inner.default_policy.lock().expect("policy");
    let overrides = Gateway::interactive_overrides(policy);
    let data: Vec<serde_json::Value> = registry
        .all()
        .iter()
        .map(|tool| {
            // In the gateway, the interactive mapping decides: what would
            // silently run, and what surfaces as a question.
            let decision = registry
                .gate(&tool.name, Policy::AllowAll, &overrides)
                .decision;
            json!({
                "name": tool.name,
                "risk": tool.risk.as_str(),
                "interactive": match decision {
                    Decision::Allow => "auto",
                    Decision::Ask => "ask",
                    Decision::Deny => "deny",
                },
            })
        })
        .collect();
    Json(json!({ "ok": true, "data": data }))
}

#[derive(Deserialize)]
pub struct PolicyBody {
    pub policy: String,
}

pub async fn set_policy(
    State(gateway): State<Arc<Gateway>>,
    Json(body): Json<PolicyBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let policy = Policy::parse(&body.policy);
    // Round-trip through as_str: an unknown policy name must be visible in
    // the echo, not silently coerced.
    *gateway.inner.default_policy.lock().expect("policy") = policy;
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "data": { "policy": policy.as_str() } })),
    )
}

pub async fn sessions(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let data: Vec<serde_json::Value> = Session::list(&gateway.home)
        .into_iter()
        .map(|summary| serde_json::to_value(&summary).expect("summary serialises"))
        .collect();
    Json(json!({ "ok": true, "data": data }))
}

#[derive(Deserialize)]
pub struct CreateSession {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub fork_from: Option<String>,
    #[serde(default)]
    pub fork_at: Option<usize>,
}

pub async fn create_session(
    State(gateway): State<Arc<Gateway>>,
    Json(body): Json<CreateSession>,
) -> (StatusCode, Json<serde_json::Value>) {
    let policy = body
        .policy
        .map(|p| Policy::parse(&p).as_str().to_string())
        .unwrap_or_else(|| {
            gateway
                .inner
                .default_policy
                .lock()
                .expect("policy")
                .as_str()
                .to_string()
        });

    let session = match &body.fork_from {
        Some(source) => {
            let Some(slot) = gateway.slot(source) else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "ok": false, "error": format!("no session `{source}` to fork") })),
                );
            };
            let origin = slot.session.lock().expect("session").clone();
            origin.fork(body.fork_at.unwrap_or(usize::MAX))
        }
        None => Session::new(body.model, policy),
    };
    let id = session.id.clone();
    if session.save(&gateway.home).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": "could not persist the session" })),
        );
    }
    gateway.insert_session(session);
    gateway.publish(Event::SessionUpdated {
        session: id.clone(),
    });
    (
        StatusCode::CREATED,
        Json(json!({ "ok": true, "data": { "session": id } })),
    )
}

pub async fn get_session(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match gateway.slot(&id) {
        Some(slot) => {
            let session = slot.session.lock().expect("session").clone();
            let data = serde_json::to_value(&session).expect("session serialises");
            (StatusCode::OK, Json(json!({ "ok": true, "data": data })))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no session `{id}`") })),
        ),
    }
}

pub async fn delete_session(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // A running session is not deletable: its task would commit into a
    // deleted file, which is a lie about what happened.
    if let Some(slot) = gateway.slot(&id) {
        if slot.busy.load(std::sync::atomic::Ordering::SeqCst) {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "ok": false, "error": "the session is running a turn" })),
            );
        }
    }
    if Session::delete(&gateway.home, &id) {
        gateway.inner.sessions.lock().expect("sessions").remove(&id);
        gateway.inner.logs.lock().expect("logs").remove(&id);
        (
            StatusCode::OK,
            Json(json!({ "ok": true, "data": { "deleted": id } })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no session `{id}`") })),
        )
    }
}

#[derive(Deserialize)]
pub struct SendMessage {
    pub message: String,
}

pub async fn send_message(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    Json(body): Json<SendMessage>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(slot) = gateway.slot(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no session `{id}`") })),
        );
    };
    if body.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "an empty message runs nothing" })),
        );
    }
    match start_turn(&gateway, &slot, &body.message, None) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({ "ok": true, "data": { "session": id, "accepted": true } })),
        ),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": error })),
        ),
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    since: Option<usize>,
}

pub async fn session_events(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Json<serde_json::Value> {
    let events = gateway.events_since(&id, query.since.unwrap_or(0));
    let data: Vec<serde_json::Value> = events
        .iter()
        .map(|event| serde_json::to_value(event).expect("event serialises"))
        .collect();
    Json(json!({
        "ok": true,
        "data": data,
        "next_since": query.since.unwrap_or(0) + data.len(),
    }))
}

#[derive(Deserialize)]
pub struct ApprovalBody {
    pub decision: ApprovalDecision,
}

/// `POST /api/approvals/{call_id}` — the REST path to answer an approval;
/// the WebSocket has its own. "always" grants the tool for the session.
pub async fn answer_approval(
    State(gateway): State<Arc<Gateway>>,
    Path(call_id): Path<String>,
    Json(body): Json<ApprovalBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Consume: one answer per approval; a second reply finds nothing.
    let sender = gateway
        .inner
        .approvals
        .lock()
        .expect("approvals")
        .remove(&call_id);
    let Some(sender) = sender else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "no approval is waiting for that call" })),
        );
    };

    let decision = match body.decision {
        ApprovalDecision::Allow | ApprovalDecision::Always => Decision::Allow,
        ApprovalDecision::Deny => Decision::Deny,
    };
    if body.decision == ApprovalDecision::Always {
        if let Some((session, tool)) = gateway
            .inner
            .approval_context
            .lock()
            .expect("approvals")
            .get(&call_id)
            .cloned()
        {
            if let Some(slot) = gateway.slot(&session) {
                slot.allowed.lock().expect("allowed").insert(tool.clone());
                gateway.publish(Event::SessionUpdated { session });
            }
        }
    }
    let delivered = sender.send(decision).is_ok();
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "data": { "delivered": delivered } })),
    )
}
pub async fn memory(State(gateway): State<Arc<Gateway>>) -> (StatusCode, Json<serde_json::Value>) {
    match bkgclaw_store::MemoryStore::load(&gateway.home) {
        Ok(store) => {
            let data: Vec<serde_json::Value> = store
                .entries()
                .iter()
                .map(|entry| serde_json::to_value(entry).expect("entry serialises"))
                .collect();
            (StatusCode::OK, Json(json!({ "ok": true, "data": data })))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": error })),
        ),
    }
}

pub async fn skills(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let index = bkgclaw_store::SkillIndex::load(&gateway.workspace, &gateway.home);
    let data: Vec<serde_json::Value> = index
        .skills
        .iter()
        .map(|skill| {
            json!({ "name": skill.name, "description": skill.description, "source": skill.source })
        })
        .collect();
    Json(json!({ "ok": true, "data": data }))
}

/// `GET /api/evolve` — the attempt archive and journal head for the UIs.
pub async fn evolve_status(State(_gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let home = bkgclaw_store::home_root();
    let archive = bkg_evolve::AttemptArchive::new(&home);
    let attempts: Vec<serde_json::Value> = archive
        .all()
        .iter()
        .map(|attempt| {
            serde_json::json!({
                "id": attempt.id,
                "ts": attempt.ts,
                "goal": attempt.goal,
                "branch": attempt.branch,
                "verdict": attempt.verdict.as_str(),
                "tests_passed": attempt.fitness.tests_passed,
                "tests_failed": attempt.fitness.tests_failed,
                "clippy_warnings": attempt.fitness.clippy_warnings,
                "lesson": attempt.lesson,
            })
        })
        .collect();
    let journal = std::fs::read_to_string(bkg_evolve::journal_path()).unwrap_or_default();
    let head: String = journal.lines().take(80).collect::<Vec<_>>().join("\n");
    Json(json!({ "ok": true, "data": { "attempts": attempts, "journal_head": head } }))
}

#[derive(Deserialize)]
pub struct EvolveRunBody {
    pub goal: String,
    /// Working directory of the tree to evolve — must exist and be a cargo
    /// workspace; the daemon refuses to run evolution somewhere else.
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub push: bool,
}

/// `POST /api/evolve` — one evolution cycle, in the background. Progress
/// reaches every client through the event bus as notices; the result lands
/// in the attempt archive this endpoint reads.
pub async fn evolve_run(
    State(gateway): State<Arc<Gateway>>,
    Json(body): Json<EvolveRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dir = body
        .dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if !dir.join("Cargo.toml").is_file() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("`{}` ist kein cargo-workspace", dir.display()) })),
        );
    }
    if body.goal.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "ein evolve-lauf braucht ein ziel" })),
        );
    }

    let goal = body.goal.clone();
    let push = body.push;
    let dir_display = dir.display().to_string();
    let dir_for_task = dir_display.clone();
    let event_gateway = gateway.clone();
    tokio::spawn(async move {
        let options = bkg_evolve::EvolveOptions { push, ..Default::default() };
        for notice in [
            format!("evolve startet: {goal}"),
        ] {
            event_gateway.publish(Event::Notice { session: "-".into(), text: notice });
        }
        let dir = std::path::PathBuf::from(&dir_for_task);
        match bkg_evolve::run_cycle(&dir, &goal, &options).await {
            Ok(result) => {
                event_gateway.publish(Event::Notice {
                    session: "-".into(),
                    text: format!(
                        "evolve {id:03} [{verdict}] {lesson}{pushed}",
                        id = result.id,
                        verdict = result.verdict.as_str(),
                        lesson = result.lesson,
                        pushed = if result.pushed { " — gepusht" } else { "" },
                    ),
                });
            }
            Err(error) => {
                event_gateway.publish(Event::Notice {
                    session: "-".into(),
                    text: format!("evolve fehlgeschlagen: {error}"),
                });
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "data": { "accepted": true, "dir": dir_display } })),
    )
}

pub async fn tasks(State(gateway): State<Arc<Gateway>>) -> Json<serde_json::Value> {
    let store = bkgclaw_store::TaskStore::new(&gateway.workspace);
    let data: Vec<serde_json::Value> = store
        .list()
        .iter()
        .map(|task| {
            json!({
                "slug": task.slug,
                "title": task.title,
                "status": task.status.as_str(),
                "updated_at": task.updated_at,
            })
        })
        .collect();
    Json(json!({ "ok": true, "data": data }))
}
