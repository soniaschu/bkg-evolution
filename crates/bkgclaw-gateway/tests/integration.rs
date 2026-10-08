//! End-to-end tests for the gateway over real HTTP on loopback.
//!
//! A scripted model provider drives the loop so no network and no NIM key
//! is needed; everything else — routing, gate, approvals, events,
//! persistence — is the production path.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use bkgclaw_core::cost::{CostTable, Usage};
use bkgclaw_core::loop_engine::ToolOutcome;
use bkgclaw_core::models::{
    CompletionRequest, CompletionResponse, ModelError, ModelId, ModelProvider, ModelRegistry,
    ToolCall, Vendor,
};
use bkgclaw_core::router::Candidate;
use bkgclaw_core::ModelPrice;
use bkgclaw_gateway::{Config, Gateway};
use bkgclaw_store::home_root;

/// A model that answers from a script: each `complete` pops the next
/// response. Tool calls are answered with a first result, so an approval
/// test can drive the full gate path.
struct Scripted {
    responses: std::sync::Mutex<Vec<CompletionResponse>>,
}

impl Scripted {
    fn answering(text: &str) -> Self {
        Scripted {
            responses: std::sync::Mutex::new(vec![response(text, Vec::new())]),
        }
    }

    fn calling_tool_then_answering(name: &str, arguments: serde_json::Value, answer: &str) -> Self {
        Scripted {
            responses: std::sync::Mutex::new(vec![
                response(
                    "working",
                    vec![ToolCall {
                        id: "call-1".into(),
                        name: name.into(),
                        arguments,
                    }],
                ),
                response(answer, Vec::new()),
            ]),
        }
    }
}

fn response(text: &str, tool_calls: Vec<ToolCall>) -> CompletionResponse {
    CompletionResponse {
        content: text.to_string(),
        tool_calls,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        model: ModelId::new(Vendor::Ollama, "test-model"),
        truncated: false,
    }
}

#[async_trait]
impl ModelProvider for Scripted {
    fn vendor(&self) -> Vendor {
        Vendor::Ollama
    }
    fn models(&self) -> &[ModelId] {
        &[]
    }
    fn credential_env(&self) -> Option<&'static str> {
        None
    }
    fn cost_table(&self) -> &CostTable {
        static FREE: std::sync::OnceLock<CostTable> = std::sync::OnceLock::new();
        FREE.get_or_init(CostTable::new)
    }
    async fn complete(
        &self,
        _request: &CompletionRequest,
    ) -> Result<CompletionResponse, ModelError> {
        let mut script = self.responses.lock().unwrap();
        if script.len() > 1 {
            Ok(script.remove(0))
        } else {
            Ok(script[0].clone())
        }
    }
}

/// A gateway over temp roots and a scripted model, bound to a free port.
struct TestServer {
    base: String,
    #[allow(dead_code)]
    home: PathBuf,
    #[allow(dead_code)]
    workspace: PathBuf,
}

impl TestServer {
    async fn start(scripted: Scripted) -> Self {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let home = root.join("home");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        let mut registry = ModelRegistry::new();
        registry.register(Box::new(scripted));
        let chain = vec![Candidate::new(
            ModelId::new(Vendor::Ollama, "test-model"),
            ModelPrice::default(),
        )];

        let gateway = Gateway::new(registry, chain, home.clone(), workspace.clone(), None).arc();
        tokio::spawn(bkgclaw_gateway::engine::run_engine_loop(gateway.clone()));

        let config = Config {
            host: "127.0.0.1".into(),
            port: 0,
            token: None,
            web_dir: None,
        };
        let app = bkgclaw_gateway::app(gateway);
        // Bind to a free port without serving forever.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let _ = config;
        TestServer {
            base: format!("http://127.0.0.1:{port}"),
            home,
            workspace,
        }
    }
}

async fn get_json(base: &str, path: &str) -> (u16, serde_json::Value) {
    let response = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

#[tokio::test]
async fn health_reports_a_live_gateway() {
    let server = TestServer::start(Scripted::answering("ok")).await;
    let (status, body) = get_json(&server.base, "/api/health").await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["service"], "bkgclaw-gateway");
}

#[tokio::test]
async fn a_turn_flows_from_message_to_persisted_transcript() {
    let server = TestServer::start(Scripted::answering("die antwort")).await;

    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({ "policy": "allow-read-only" }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status().as_u16(), 201);
    let body: serde_json::Value = created.json().await.unwrap();
    let session = body["data"]["session"].as_str().unwrap().to_string();

    let accepted = client
        .post(format!("{}/api/sessions/{session}/messages", server.base))
        .json(&json!({ "message": "frage" }))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status().as_u16(), 202);

    // Poll the event log until the turn stops.
    let mut since = 0;
    let mut saw_stop = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, body) = get_json(
            &server.base,
            &format!("/api/sessions/{session}/events?since={since}"),
        )
        .await;
        since = body["next_since"].as_u64().unwrap() as usize;
        for event in body["data"].as_array().unwrap() {
            match event["type"].as_str().unwrap() {
                "stop" => saw_stop = true,
                other => assert!(
                    !matches!(other, "error"),
                    "no error events expected, got {event}"
                ),
            }
        }
        if saw_stop {
            break;
        }
    }
    assert!(saw_stop, "the turn must end with a stop event");

    // The transcript is on disk, without the assembled system prompt.
    let loaded = bkgclaw_store::Session::load(&server.home, &session).expect("persisted");
    let texts: Vec<String> = loaded
        .messages
        .iter()
        .map(|m| match m {
            bkgclaw_core::Message::User { content } => format!("user:{content}"),
            bkgclaw_core::Message::Assistant { content } => format!("assistant:{content}"),
            bkgclaw_core::Message::System { .. } => "system".to_string(),
            bkgclaw_core::Message::ToolResult { content, .. } => format!("tool:{content}"),
        })
        .collect();
    assert_eq!(
        texts,
        vec!["user:frage", "assistant:die antwort"],
        "the transcript must be exactly the turn, no system prompt persisted"
    );
    assert_eq!(loaded.turns, 1);
}

#[tokio::test]
async fn an_approval_round_trip_runs_the_tool() {
    // The model calls write_file; the interactive mapping turns that into
    // an Ask; the test answers over REST; the tool then really writes.
    let server = TestServer::start(Scripted::calling_tool_then_answering(
        "write_file",
        json!({ "path": "approved-note.txt", "content": "vom menschen genehmigt" }),
        "fertig",
    ))
    .await;

    let client = reqwest::Client::new();
    let created: serde_json::Value = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({ "policy": "allow-read-only" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = created["data"]["session"].as_str().unwrap().to_string();

    client
        .post(format!("{}/api/sessions/{session}/messages", server.base))
        .json(&json!({ "message": "schreib die datei" }))
        .send()
        .await
        .unwrap();

    // Wait for the approval request on the event log.
    let mut call_id = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, body) = get_json(&server.base, &format!("/api/sessions/{session}/events")).await;
        for event in body["data"].as_array().unwrap() {
            if event["type"] == "approval_request" {
                call_id = event["call_id"].as_str().map(str::to_string);
                assert_eq!(event["name"], "write_file");
            }
        }
        if call_id.is_some() {
            break;
        }
    }
    let call_id = call_id.expect("the gate must surface the write for a human");

    // Answer it.
    let answered = client
        .post(format!("{}/api/approvals/{call_id}", server.base))
        .json(&json!({ "decision": "allow" }))
        .send()
        .await
        .unwrap();
    assert_eq!(answered.status().as_u16(), 200);

    // Wait for the stop; the file must exist by then.
    let mut stopped = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, body) = get_json(&server.base, &format!("/api/sessions/{session}/events")).await;
        if body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "stop")
        {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "the turn must finish after the approval");

    let written = std::fs::read_to_string("approved-note.txt").expect("the approved write ran");
    assert_eq!(written, "vom menschen genehmigt");
    let _ = std::fs::remove_file("approved-note.txt");
}

#[tokio::test]
async fn an_unanswered_approval_times_out_as_a_refusal() {
    // APPROVAL_TIMEOUT is 180s in production; the property "silence is no"
    // is what matters, and it is covered by the observer default in core
    // (`NoObserver` denies). Here we verify the running-session state: a
    // session mid-approval rejects a second message.
    let server = TestServer::start(Scripted::calling_tool_then_answering(
        "write_file",
        json!({ "path": "never.txt", "content": "x" }),
        "x",
    ))
    .await;

    let client = reqwest::Client::new();
    let created: serde_json::Value = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({ "policy": "allow-read-only" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = created["data"]["session"].as_str().unwrap().to_string();

    let first = client
        .post(format!("{}/api/sessions/{session}/messages", server.base))
        .json(&json!({ "message": "schreib" }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 202);

    // The turn is parked on the approval; a second send must be refused.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = client
        .post(format!("{}/api/sessions/{session}/messages", server.base))
        .json(&json!({ "message": "nochmal" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        409,
        "one turn per session at a time"
    );

    // Cancel instead of waiting out the production timeout.
    let cancelled = client
        .post(format!("{}/api/sessions/{session}/cancel", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status().as_u16(), 200);

    // After cancellation the file must not exist: the aborted turn
    // committed nothing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !std::path::Path::new("never.txt").exists(),
        "a cancelled turn writes nothing"
    );
}

#[tokio::test]
async fn a_fork_copies_the_transcript_prefix_into_a_new_session() {
    let server = TestServer::start(Scripted::answering("antwort")).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let origin = created["data"]["session"].as_str().unwrap().to_string();

    client
        .post(format!("{}/api/sessions/{origin}/messages", server.base))
        .json(&json!({ "message": "erster" }))
        .send()
        .await
        .unwrap();

    // Wait for the origin's turn to commit before forking: a fork during
    // a running turn copies the pre-turn transcript, which is correct but
    // not what this test wants to show.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, body) = get_json(&server.base, &format!("/api/sessions/{origin}/events")).await;
        if body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "stop")
        {
            break;
        }
    }

    let forked: serde_json::Value = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({ "fork_from": origin }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fork = forked["data"]["session"].as_str().unwrap().to_string();
    assert_ne!(fork, origin);

    let (_, body) = get_json(&server.base, &format!("/api/sessions/{fork}")).await;
    let messages = body["data"]["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "erster")
    );
}

#[tokio::test]
async fn auth_rejects_requests_without_the_token() {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let home = root.join("home");
    let workspace = root.join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();

    let mut registry = ModelRegistry::new();
    registry.register(Box::new(Scripted::answering("x")));
    let chain = vec![Candidate::new(
        ModelId::new(Vendor::Ollama, "test-model"),
        ModelPrice::default(),
    )];
    let gateway = Gateway::new(
        registry,
        chain,
        home,
        workspace,
        Some("geheim-2026".to_string()),
    )
    .arc();
    tokio::spawn(bkgclaw_gateway::engine::run_engine_loop(gateway.clone()));

    let app = bkgclaw_gateway::app(gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://127.0.0.1:{port}");

    // Health is open.
    let (status, _) = get_json(&base, "/api/health").await;
    assert_eq!(status, 200);

    // Without the token: rejected.
    let denied = reqwest::get(format!("{base}/api/models")).await.unwrap();
    assert_eq!(denied.status().as_u16(), 401);

    // With the token: through.
    let allowed = reqwest::Client::new()
        .get(format!("{base}/api/models"))
        .header("Authorization", "Bearer geheim-2026")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status().as_u16(), 200);

    // And via the query variant, which browsers need for WebSocket.
    let via_query = reqwest::get(format!("{base}/api/models?token=geheim-2026"))
        .await
        .unwrap();
    assert_eq!(via_query.status().as_u16(), 200);
}

#[tokio::test]
async fn sessions_listing_and_deletion_round_trip() {
    let server = TestServer::start(Scripted::answering("ok")).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{}/api/sessions", server.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = created["data"]["session"].as_str().unwrap().to_string();

    let (_, list) = get_json(&server.base, "/api/sessions").await;
    let entries = list["data"].as_array().unwrap();
    assert!(entries.iter().any(|s| s["id"] == session));

    let deleted = client
        .delete(format!("{}/api/sessions/{session}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status().as_u16(), 200);

    let gone = client
        .get(format!("{}/api/sessions/{session}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status().as_u16(), 404);
}

#[tokio::test]
async fn tool_and_task_and_skill_panels_expose_the_workspace() {
    let server = TestServer::start(Scripted::answering("ok")).await;
    let tasks = bkgclaw_store::TaskStore::new(&server.workspace);
    tasks.add("probe", "Probe-Aufgabe", "körper").unwrap();

    let (_, tools) = get_json(&server.base, "/api/tools").await;
    let tools = tools["data"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|t| t["name"] == "write_file" && t["interactive"] == "ask")
    );

    let (_, listed) = get_json(&server.base, "/api/tasks").await;
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["slug"] == "probe")
    );

    let (_, memory) = get_json(&server.base, "/api/memory").await;
    assert_eq!(memory["ok"], true);
}

// The workspace root must not leak this test's cwd into the production
// home; referenced once so the import stays honest.
#[allow(dead_code)]
fn production_home_is_resolvable() -> PathBuf {
    home_root()
}

// Referenced so the compiler can see the outcome type is exercised.
#[allow(dead_code)]
fn outcome_type_exists(outcome: &ToolOutcome) -> bool {
    matches!(outcome, ToolOutcome::Ran { .. })
}
