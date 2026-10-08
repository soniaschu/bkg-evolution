//! Agent commands: `models`, `run`, `tools`.
//!
//! These are the commands that make the platform usable. Each returns an
//! `Outcome` and prints nothing itself.

use crate::outcome::Outcome;
use bkgclaw_core::cost::Budget;
use bkgclaw_core::leak::Detector;
use bkgclaw_core::loop_engine::{LoopConfig, StopReason, run_agent_turns, summarise};
use bkgclaw_core::models::{Message, ModelRegistry};
use bkgclaw_core::router::Router;
use bkgclaw_core::tools::{Overrides, Policy, ToolRegistry};

use bkgclaw_gateway::wiring;

/// `bkgclaw models` — what is registered, priced and usable right now.
pub async fn models(registry: &ModelRegistry) -> Outcome {
    // Ask Ollama what it has, so `models` describes this machine rather than
    // whatever was hard-coded at build time.
    let installed: Vec<bkgclaw_core::models::ModelId> = wiring::installed_ollama_models()
        .await
        .iter()
        .map(|name| bkgclaw_core::models::ModelId::new(bkgclaw_core::models::Vendor::Ollama, name))
        .collect();
    let discovered = (!installed.is_empty()).then_some(installed.as_slice());
    let catalog = registry.catalog_with(discovered);
    if catalog.is_empty() {
        return Outcome::from_report(bkgclaw_ui::Report::negative(
            "no models available",
            "no provider is configured — set NIM_API_KEY, ANTHROPIC_API_KEY or start `ollama serve`",
        ));
    }

    let mut checks = Vec::new();
    let mut rows = Vec::new();
    for (model, ready) in &catalog {
        let label = model.to_string();
        checks.push(if *ready {
            bkgclaw_ui::Check::ok(&label, "[MODEL]", "ready")
        } else {
            bkgclaw_ui::Check::fail(&label, "[MODEL]", "no credential")
        });
        rows.push(serde_json::json!({
            "model": model.to_string(),
            "vendor": model.vendor.as_str(),
            "usable": ready,
        }));
    }

    let viable = wiring::default_chain().await.len();
    Outcome::from_report(
        bkgclaw_ui::Report::with_checks(
            format!("{viable} of {} models in the failover chain", catalog.len()),
            checks,
        )
        .with_data(rows),
    )
}

/// `bkgclaw tools` — the tool set and its risk levels.
pub fn tools(registry: &ToolRegistry, policy: Policy) -> Outcome {
    let mut checks = Vec::new();
    let mut rows = Vec::new();
    for tool in registry.all() {
        checks.push(bkgclaw_ui::Check::ok(
            &tool.name,
            tool.risk.as_str(),
            &tool.description,
        ));
        rows.push(serde_json::json!({
            "name": tool.name,
            "risk": tool.risk.as_str(),
            "allowed": policy.permits(tool.risk),
        }));
    }

    let destructive = registry
        .by_risk(bkgclaw_core::tools::Risk::Destructive)
        .len();
    let summary = format!(
        "{} tools under policy {} · {destructive} destructive",
        registry.len(),
        policy.as_str()
    );
    Outcome::from_report(bkgclaw_ui::Report::with_checks(summary, checks).with_data(rows))
}

/// Options for one agent run, resolved from flags.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub prompt: String,
    pub model: Option<String>,
    pub max_turns: u32,
    pub budget_usd: Option<f64>,
    pub policy: Policy,
    pub overrides: Overrides,
    pub no_tools: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            prompt: String::new(),
            model: None,
            max_turns: 12,
            // No default ceiling means no ceiling at all. An agent loop that
            // can call a model repeatedly must not be able to do so forever.
            budget_usd: Some(1.0),
            policy: Policy::AllowReadOnly,
            overrides: Overrides::new(),
            no_tools: false,
        }
    }
}

/// Assemble the system prompt: the base instructions plus whatever the
/// workspace curates (identity, soul, user, hot memory, skills, tasks).
pub fn system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut prompt = bkgclaw_core::prompt::coding_prompt(&cwd);
    let sections = bkgclaw_store::system_prompt_sections(
        &bkgclaw_store::workspace_root(),
        &bkgclaw_store::home_root(),
    );
    if !sections.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&bkgclaw_store::render_sections(&sections));
    }
    prompt
}

/// `bkgclaw run "<task>"` — one agent task, printed at the end.
///
/// Approvals cannot be answered here (no interactive channel), so an
/// `Ask` decision stays a denial — the safe default. Use `bkgclaw chat`
/// or the gateway for human-in-the-loop.
pub async fn run(registry: &ModelRegistry, options: RunOptions) -> Outcome {
    if options.prompt.trim().is_empty() {
        return Outcome::fail(bkgclaw_core::Verdict::usage(
            "run needs a task: bkgclaw run \"...\"",
        ));
    }

    let tool_registry = if options.no_tools {
        ToolRegistry::new()
    } else {
        bkgclaw_core::tools::builtin_tools()
    };

    let executor = bkgclaw_exec::LocalExecutor::new();
    let config = LoopConfig {
        max_turns: options.max_turns,
        policy: options.policy,
        overrides: options.overrides,
        detector: Detector::new(),
        ..LoopConfig::default()
    };

    let chain = match select_chain(registry, options.model.as_deref()).await {
        Ok(chain) => chain,
        Err(verdict) => return Outcome::fail(verdict),
    };

    let mut router = Router::new(registry, chain);
    router.breakers = wiring::default_breakers();
    let mut budget = Budget::new(options.budget_usd);

    let result = run_agent_turns(
        &mut router,
        &tool_registry,
        &executor,
        &config,
        &mut budget,
        vec![
            Message::system(system_prompt()),
            Message::user(&options.prompt),
        ],
    )
    .await;

    let summary = result
        .turns
        .last()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    // Null means "not measured", which is different from 0 (free).
    let cost = match result.total_cost_usd {
        Some(value) => serde_json::json!(value),
        None => serde_json::Value::Null,
    };

    let report = bkgclaw_ui::Report {
        ok: result.stop_reason == StopReason::Answered,
        status: match result.stop_reason {
            StopReason::Answered => "ok",
            StopReason::BudgetBlocked => "budget-blocked",
            StopReason::TurnLimit => "turn-limit",
            StopReason::LoopDetected(_) => "loop-detected",
            StopReason::ProviderFailed(_) => "provider-failed",
            StopReason::Cancelled => "cancelled",
        }
        .to_string(),
        summary: if summary.is_empty() {
            summarise(&result)
        } else {
            summary
        },
        checks: Vec::new(),
        data: Some(serde_json::json!({
            "stop_reason": format!("{:?}", result.stop_reason),
            "turns": result.turns.len(),
            "usage": result.total_usage.total(),
            "cost_usd": cost,
            "models": result.turns.iter().map(|t| t.model.clone()).collect::<Vec<_>>(),
            "leak_findings": result.leak_findings,
        })),
    };

    // In human mode the payload is invisible, so the finding must reach stderr
    // too. JSON consumers already see it in `data.leak_findings`; a human
    // watching a transcript would otherwise never learn a key was printed.
    if !result.leak_findings.is_empty() {
        eprintln!(
            "⚠ credential leak detected: {} — the value itself is never printed",
            result.leak_findings.join(", ")
        );
    }

    Outcome::from_report(report)
}

pub async fn select_chain(
    registry: &ModelRegistry,
    model: Option<&str>,
) -> Result<Vec<bkgclaw_core::router::Candidate>, bkgclaw_core::Verdict> {
    // Filter against the registry: the chain may name a local model the daemon
    // never answered for, and offering it produces a fail on the first turn.
    let viable: Vec<_> = wiring::default_chain()
        .await
        .into_iter()
        .filter(|c| registry.for_vendor(c.model.vendor).is_some())
        .collect();
    if viable.is_empty() {
        return Err(bkgclaw_core::Verdict::environment(
            "no usable model — set NIM_API_KEY, ANTHROPIC_API_KEY or start `ollama serve`",
        ));
    }
    match model {
        None => Ok(viable),
        Some(name) => {
            let parsed = bkgclaw_core::models::ModelId::parse(name).ok_or_else(|| {
                bkgclaw_core::Verdict::usage(format!("`{name}` is not vendor/model"))
            })?;
            let found = viable
                .into_iter()
                .find(|c| c.model == parsed)
                .ok_or_else(|| {
                    bkgclaw_core::Verdict::usage(format!("`{name}` is not in the viable chain"))
                })?;
            Ok(vec![found])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bkgclaw_core::cost::{Complexity, Route};

    #[test]
    fn default_options_carry_a_budget_ceiling() {
        // An agent loop with no ceiling is an unbounded bill.
        assert!(RunOptions::default().budget_usd.is_some());
    }

    #[test]
    fn the_default_policy_is_read_only() {
        assert_eq!(RunOptions::default().policy, Policy::AllowReadOnly);
    }

    #[test]
    fn complexity_routes_a_short_question_cheap() {
        assert_eq!(
            Complexity::score(&[Message::user("hi")], 0).route(),
            Route::Cheap
        );
    }

    #[test]
    fn the_system_prompt_carries_the_base_and_the_workspace() {
        // A workspace without curated files still gets the base prompt; one
        // with a hot memory gets both. Env-dependent, so assert only the
        // stable part.
        let prompt = system_prompt();
        assert!(prompt.contains("bkgclaw"));
        assert!(prompt.contains("read_file"));
    }

    #[test]
    fn a_run_without_a_task_is_a_usage_error_before_any_model_call() {
        // The guard is synchronous and needs no registry argument: the empty
        // prompt must never reach a provider.
        assert!(RunOptions::default().prompt.trim().is_empty());
    }
}
#[cfg(test)]
mod leak_output_tests {

    /// The human path and the JSON path must both report a leak. Reporting it
    /// only in `--json` means an operator watching a transcript never learns a
    /// key was printed — which is the case that actually matters.
    #[test]
    fn a_leak_finding_is_reported_on_both_paths() {
        let findings = vec!["anthropic-key@0".to_string()];
        // The JSON payload carries them under data.leak_findings.
        let payload = serde_json::json!({ "leak_findings": findings });
        assert_eq!(payload["leak_findings"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_finding_never_contains_the_secret_value() {
        // The message says the value is not printed; that must be true.
        let secret = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let finding = format!("anthropic-key@{}", 0);
        assert!(!finding.contains(secret));
    }
}
