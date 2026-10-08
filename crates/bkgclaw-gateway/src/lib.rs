//! `bkgclaw-gateway` — the OpenClaw-style daemon.
//!
//! One process holds the sessions, runs the agent loops, answers
//! approvals, streams every event over WebSocket and REST, serves the web
//! cockpit — and persists everything as files the operator can read. No
//! SQL, no service mesh, no container: a binary and a home directory.
//!
//! Wire surface:
//!
//! - `WS  /ws` — the interactive channel: subscribe, send, approve, cancel
//! - `GET /api/health` — liveness, unauthenticated, content-free
//! - `GET /api/models` · `GET /api/tools` · `PUT /api/policy`
//! - `GET/POST /api/sessions` · `GET/DELETE /api/sessions/{id}`
//! - `POST /api/sessions/{id}/messages` · `POST /api/sessions/{id}/cancel`
//! - `GET  /api/sessions/{id}/events?since=N` — polling fallback
//! - `POST /api/approvals/{call_id}`
//! - `GET /api/memory` · `GET /api/skills` · `GET /api/tasks`
//! - `GET /` — the built web cockpit when present, a status page otherwise

#![forbid(unsafe_code)]

pub mod engine;
pub mod events;
pub mod routes;
pub mod state;
pub mod wiring;
pub mod ws;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde_json::json;

pub use events::{ApprovalDecision, ClientMessage, Event};
pub use state::{Gateway, SessionSlot};
pub use wiring::{default_breakers, default_chain, model_registry};

/// Everything a caller needs to boot the daemon.
pub struct Config {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
    pub web_dir: Option<PathBuf>,
}

impl Config {
    /// From the environment: `BKGCLAW_GATEWAY_TOKEN`, `BKGCLAW_WEB_DIR`.
    pub fn from_env(host: String, port: u16) -> Self {
        Config {
            host,
            port,
            token: std::env::var("BKGCLAW_GATEWAY_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty()),
            web_dir: std::env::var("BKGCLAW_WEB_DIR")
                .ok()
                .filter(|d| !d.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var("BKGCLAW_GATEWAY_DIR").ok().map(PathBuf::from)),
        }
    }
}

/// The Axum application: routes, auth, state. Public so tests can boot the
/// router against a tempdir-backed gateway without opening a port.
pub fn app(gateway: Arc<Gateway>) -> AxumRouter {
    let authenticated = AxumRouter::new()
        .route("/api/models", get(routes::models))
        .route("/api/tools", get(routes::tools))
        .route("/api/policy", put(routes::set_policy))
        .route(
            "/api/sessions",
            get(routes::sessions).post(routes::create_session),
        )
        .route(
            "/api/sessions/{id}",
            get(routes::get_session).delete(routes::delete_session),
        )
        .route("/api/sessions/{id}/messages", post(routes::send_message))
        .route("/api/sessions/{id}/cancel", post(rest_cancel))
        .route("/api/sessions/{id}/events", get(routes::session_events))
        .route("/api/approvals/{call_id}", post(routes::answer_approval))
        .route("/api/memory", get(routes::memory))
        .route("/api/skills", get(routes::skills))
        .route("/api/tasks", get(routes::tasks))
        .route("/api/evolve", get(routes::evolve_status).post(routes::evolve_run))
        .route("/ws", get(ws::ws_handler))
        .layer(middleware::from_fn_with_state(gateway.clone(), auth))
        .fallback(static_fallback)
        .with_state(gateway.clone());

    AxumRouter::new()
        .route("/api/health", get(routes::health))
        .merge(authenticated)
        .with_state(gateway)
}

/// Bearer-token auth. Open when no token is configured — the localhost
/// binding is the boundary; with a token, header or `?token=` both work
/// (browsers cannot set WebSocket headers).
async fn auth(
    State(gateway): State<Arc<Gateway>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(expected) = gateway.token.clone() else {
        return next.run(request).await;
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            request.uri().query().and_then(|query| {
                query.split('&').find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key == "token").then(|| value.to_string())
                })
            })
        });
    match provided {
        Some(token) if token == expected => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Serve the built web cockpit from `web_dir`, or the built-in status page.
async fn static_fallback(State(gateway): State<Arc<Gateway>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(web_dir) = &gateway.web_dir {
        let base = Path::new(web_dir);
        let candidate = base.join(path);
        // Traversal guard: the served path must stay inside the web dir.
        let inside = candidate
            .canonicalize()
            .ok()
            .and_then(|resolved| {
                base.canonicalize()
                    .ok()
                    .map(|base_resolved| resolved.starts_with(base_resolved))
            })
            .unwrap_or(false);
        if inside && candidate.is_file() {
            let content_type = match candidate.extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "application/javascript",
                Some("css") => "text/css",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                Some("wasm") => "application/wasm",
                Some("json") => "application/json",
                _ => "application/octet-stream",
            };
            let body = std::fs::read(&candidate).unwrap_or_default();
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(axum::body::Body::from(body))
                .expect("static response builds");
        }
    }

    // No built assets: an honest status page instead of a silent 404.
    let page = "<!doctype html><html lang='de'><head><meta charset='utf-8'><title>bkgclaw gateway</title>\
<style>body{font-family:system-ui;background:#101418;color:#d8dee9;padding:3rem;line-height:1.6}\
code{background:#1b2027;padding:2px 6px;border-radius:4px}</style></head><body>\
<h1>bkgclaw gateway</h1>\
<p>Der Daemon läuft. Die Web-Oberfläche ist in diesem Build nicht vorhanden.</p>\
<p>Bauen mit: <code>dx build --release</code> im Verzeichnis <code>crates/bkgclaw-web</code>, \
ausgeliefert aus <code>BKGCLAW_WEB_DIR</code>.</p>\
<p>WebSocket: <code>/ws</code> · REST: <code>/api/health</code>, <code>/api/sessions</code>.</p>\
</body></html>";
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(axum::body::Body::from(page))
        .expect("status page builds")
}

/// `POST /api/sessions/{id}/cancel` — REST twin of the WS cancel.
async fn rest_cancel(
    State(gateway): State<Arc<Gateway>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let Some(slot) = gateway.slot(&id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "ok": false, "error": format!("no session `{id}`") })),
        );
    };
    if engine::cancel_turn(&slot) {
        gateway.publish(Event::Cancelled { session: id });
        (
            StatusCode::OK,
            axum::Json(json!({ "ok": true, "data": { "cancelled": true } })),
        )
    } else {
        (
            StatusCode::CONFLICT,
            axum::Json(json!({ "ok": false, "error": "no turn is running on that session" })),
        )
    }
}

/// Bind and serve until the process is stopped. Never returns on success.
pub async fn serve(gateway: Arc<Gateway>, config: &Config) -> std::io::Result<()> {
    let app = app(gateway);
    let address = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    axum::serve(listener, app).await
}

/// Bind (port 0 picks a free one) and serve in a spawned task, returning
/// the bound address. For embedded gateways — a TUI that brings its own
/// daemon must learn where it landed.
pub async fn serve_bound(
    gateway: Arc<Gateway>,
    host: &str,
    port: u16,
) -> std::io::Result<std::net::SocketAddr> {
    let address = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(bkgclaw_gateway_engine_keep(gateway, listener));
    Ok(bound)
}

async fn bkgclaw_gateway_engine_keep(gateway: Arc<Gateway>, listener: tokio::net::TcpListener) {
    // The engine loop owns the sub-agent channel for this process.
    tokio::spawn(engine::run_engine_loop(gateway.clone()));
    let app = app(gateway);
    let _ = axum::serve(listener, app).await;
}
