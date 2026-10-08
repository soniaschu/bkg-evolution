//! `bkgclaw gateway` — start the OpenClaw-style daemon.
//!
//! One process: the model registry, the failover chain, the session
//! engine, the REST+WebSocket surface, the sub-agent loop. Everything a
//! TUI, a web cockpit or a curl script needs.

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;

pub async fn run(host: String, port: u16, json: bool) -> Outcome {
    let registry = bkgclaw_gateway::wiring::model_registry();
    let chain = bkgclaw_gateway::wiring::default_chain().await;
    if chain.is_empty() {
        return Outcome::fail(Verdict::environment(
            "no usable model — set NIM_API_KEY, ANTHROPIC_API_KEY or start `ollama serve`",
        ));
    }

    let home = bkgclaw_store::home_root();
    let workspace = bkgclaw_store::workspace_root();
    let config = bkgclaw_gateway::Config::from_env(host, port);

    let mut gateway =
        bkgclaw_gateway::Gateway::new(registry, chain, home, workspace, config.token.clone());
    // The built web cockpit is served from here; without the assignment the
    // daemon falls back to its status page.
    gateway.web_dir = config.web_dir.clone();
    let gateway = gateway.arc();
    tokio::spawn(bkgclaw_gateway::engine::run_engine_loop(gateway.clone()));

    let shown = config.token.is_some();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "listening": format!("{}:{}", config.host, config.port),
                "token_required": shown,
                "web": config.web_dir.is_some(),
            })
        );
    } else {
        println!("bkgclaw gateway — http://{}:{}", config.host, config.port);
        println!("  WebSocket: ws://{}:{}/ws", config.host, config.port);
        println!(
            "  REST:      http://{}:{}/api/health",
            config.host, config.port
        );
        if shown {
            println!("  Auth:      Bearer token required (BKGCLAW_GATEWAY_TOKEN)");
        }
        if config.web_dir.is_none() {
            println!("  Web-UI:    nicht gebaut — siehe `bkgclaw web`-Hinweise in der README");
        }
    }

    match bkgclaw_gateway::serve(gateway, &config).await {
        Ok(()) => Outcome::success("gateway stopped"),
        Err(error) => Outcome::fail(Verdict::environment(format!("gateway failed: {error}"))),
    }
}
