//! `bkgclaw-tui` — the interactive terminal client.
//!
//! Architecture: the TUI is a *client*. It speaks the gateway's WebSocket
//! protocol, exactly like the web cockpit. When no gateway is listening it
//! starts an embedded one in-process — the RustyClaw pattern: one binary,
//! the same protocol either way, zero setup.
//!
//! The loop is three channels: keys (crossterm thread), events (WebSocket
//! read pump), commands (state machine). The state is pure; rendering is
//! pure; this file is the only place anything blocks anything.

#![forbid(unsafe_code)]

pub mod net;
pub mod state;
pub mod ui;

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use bkgclaw_gateway::Gateway;
use bkgclaw_gateway::events::{ClientMessage, Event};

use state::{Cmd, TuiState};

/// Where to find (or start) a gateway.
pub struct TuiConfig {
    /// An explicit URL wins over everything.
    pub gateway_url: Option<String>,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub policy: String,
}

impl TuiConfig {
    pub fn from_env(
        url: Option<String>,
        resume: Option<String>,
        model: Option<String>,
        policy: String,
    ) -> Self {
        TuiConfig {
            gateway_url: url,
            resume,
            model,
            policy,
        }
    }
}

/// Resolve the gateway base URL: flag > env > default > embedded.
async fn resolve_gateway(explicit: Option<String>) -> Result<String, String> {
    if let Some(url) = explicit {
        if net::healthy(&url).await {
            return Ok(url.trim_end_matches('/').to_string());
        }
        return Err(format!(
            "kein gateway auf {url} — erst `bkgclaw gateway` starten"
        ));
    }
    if let Ok(url) = std::env::var("BKGCLAW_GATEWAY_URL") {
        let url = url.trim_end_matches('/').to_string();
        if net::healthy(&url).await {
            return Ok(url);
        }
    }
    let default = "http://127.0.0.1:8787".to_string();
    if net::healthy(&default).await {
        return Ok(default);
    }
    embedded_gateway().await
}

/// Start an in-process gateway on a free port. One binary, zero setup.
async fn embedded_gateway() -> Result<String, String> {
    let registry = bkgclaw_gateway::wiring::model_registry();
    let chain = bkgclaw_gateway::wiring::default_chain().await;
    if chain.is_empty() {
        return Err("kein nutzbares modell — NIM_API_KEY setzen oder ollama starten".into());
    }
    let mut gateway = Gateway::new(
        registry,
        chain,
        bkgclaw_store::home_root(),
        bkgclaw_store::workspace_root(),
        std::env::var("BKGCLAW_GATEWAY_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty()),
    );
    gateway.web_dir = std::env::var("BKGCLAW_WEB_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from);
    let gateway = gateway.arc();
    let address = bkgclaw_gateway::serve_bound(gateway, "127.0.0.1", 0)
        .await
        .map_err(|e| format!("eingebetteter gateway: {e}"))?;
    Ok(format!("http://{address}"))
}

/// Run the TUI. Restores the terminal on every exit path — a crashed
/// terminal is a broken shell, and that is the one failure users never
/// forgive.
pub async fn run(config: TuiConfig) -> Result<(), String> {
    let base = resolve_gateway(config.gateway_url.clone()).await?;

    let session = match &config.resume {
        Some(id) => id.clone(),
        None => net::create_session(&base, config.model.clone(), config.policy.clone()).await?,
    };

    let token = std::env::var("BKGCLAW_GATEWAY_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    let (socket, events) = net::connect(&base, token).await?;
    net::send(
        &socket,
        ClientMessage::Subscribe {
            session: session.clone(),
        },
    );

    let mut state = TuiState::new(session, config.model, config.policy);
    state.status = format!("verbunden: {base}");

    // Keys: crossterm reads on a plain thread, because its reader is
    // blocking and owes nothing to tokio.
    let (key_tx, key_rx) = std_mpsc::channel::<crossterm::event::KeyEvent>();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => {
                    if key_tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, state, base, socket, events, key_rx).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    mut state: TuiState,
    base: String,
    socket: net::Socket,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    key_rx: std_mpsc::Receiver<crossterm::event::KeyEvent>,
) -> Result<(), String> {
    loop {
        if state.quit {
            return Ok(());
        }
        terminal
            .draw(|frame| ui::render(frame, &state))
            .map_err(|e| format!("render: {e}"))?;

        // Keys arrive from a plain thread; drain the channel with a small
        // sleep budget so the loop keeps serving events while keys idle.
        while let Ok(key) = key_rx.try_recv() {
            match state::handle_key(&mut state, key) {
                Cmd::Nothing => {}
                Cmd::Quit => return Ok(()),
                Cmd::RefreshSessions => match net::fetch_sessions(&base).await {
                    Ok(sessions) => state.sessions = sessions,
                    Err(error) => state.status = error,
                },
                Cmd::Ws(message) => net::send(&socket, message),
            }
        }

        if let Ok(event) = events.try_recv() {
            let session = event.session().to_string();
            // A pending /new or /fork answers with a SessionUpdated for the
            // fresh id: adopt it, swap subscriptions, reset the view.
            if state.pending_new_session
                && matches!(event, Event::SessionUpdated { .. })
                && session != state.session
            {
                state.pending_new_session = false;
                net::send(
                    &socket,
                    ClientMessage::StopReceiving {
                        session: state.session.clone(),
                    },
                );
                state.session = session.clone();
                net::send(
                    &socket,
                    ClientMessage::Subscribe {
                        session: session.clone(),
                    },
                );
                state.lines.clear();
                state.spent = 0.0;
                state.live.clear();
                state.live_reasoning.clear();
                state.status = format!("sitzung {session}");
            }
            if session == state.session || matches!(event, Event::SessionUpdated { .. }) {
                state::apply(&mut state, &event);
            }
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
