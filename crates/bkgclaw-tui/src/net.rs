//! Gateway access from the terminal: REST for the session list, WebSocket
//! for everything live. Small on purpose — the interesting logic lives in
//! `state`.

use bkgclaw_gateway::events::ClientMessage;
use futures_util::{SinkExt, StreamExt};

/// One session row as the Sessions overlay shows it.
pub async fn fetch_sessions(base: &str) -> Result<Vec<(String, String)>, String> {
    let url = format!("{base}/api/sessions");
    let payload: serde_json::Value = reqwest::get(&url)
        .await
        .map_err(|e| format!("gateway nicht erreichbar: {e}"))?
        .json()
        .await
        .map_err(|e| format!("antwort keine json: {e}"))?;
    let rows = payload["data"]
        .as_array()
        .ok_or("die sitzungsliste hat eine unerwartete form")?;
    Ok(rows
        .iter()
        .map(|row| {
            let id = row["id"].as_str().unwrap_or("?").to_string();
            let preview = row["preview"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect::<String>();
            (id, preview)
        })
        .collect())
}

/// Create a session; returns its id.
pub async fn create_session(
    base: &str,
    model: Option<String>,
    policy: String,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let payload: serde_json::Value = client
        .post(format!("{base}/api/sessions"))
        .json(&serde_json::json!({ "model": model, "policy": policy }))
        .send()
        .await
        .map_err(|e| format!("gateway nicht erreichbar: {e}"))?
        .json()
        .await
        .map_err(|e| format!("antwort keine json: {e}"))?;
    payload["data"]["session"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "keine sitzungs-id in der antwort".to_string())
}

/// Is a gateway listening here?
pub async fn healthy(base: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(400))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    match client.get(format!("{base}/api/health")).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// The socket's write handle, so the orchestrator can send from its loop
/// while a task pumps the read half.
pub struct Socket {
    writer: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
}

/// Connect to `/ws`, start both pumps, return the writer and the event
/// channel. The caller moves the channel into its select loop.
pub async fn connect(
    base: &str,
    token: Option<String>,
) -> Result<
    (
        Socket,
        tokio::sync::mpsc::UnboundedReceiver<bkgclaw_gateway::Event>,
    ),
    String,
> {
    let ws_url = format!(
        "{}/ws",
        base.replace("http://", "ws://")
            .replace("https://", "wss://")
    );
    let ws_url = match &token {
        Some(token) => format!("{ws_url}?token={token}"),
        None => ws_url,
    };
    let (socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("websocket fehlgeschlagen: {e}"))?;
    let (writer, reader) = socket.split();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

    // Write pump: serialises messages onto the socket.
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            if writer
                .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Read pump: every event goes to the orchestrator unfiltered; the
    // state decides what matters, subscriptions are re-sent on switches.
    tokio::spawn(async move {
        let mut reader = reader;
        while let Some(Ok(message)) = reader.next().await {
            let text = match message {
                tokio_tungstenite::tungstenite::Message::Text(text) => text,
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            let Ok(event) = serde_json::from_str::<bkgclaw_gateway::Event>(text.as_str()) else {
                continue;
            };
            let _ = event_tx.send(event);
        }
    });

    Ok((Socket { writer: out_tx }, event_rx))
}

/// Send a client message without caring about the result.
pub fn send(socket: &Socket, message: ClientMessage) {
    let _ = socket.writer.send(message);
}
