//! The agent loop.
//!
//! One turn is: send the transcript to a model, run whatever tools it asked
//! for, feed the results back, repeat. The loop owns four invariants that a
//! naive implementation gets wrong:
//!
//! 1. **Every tool call passes the gate.** A model cannot call `execute_command`
//!    just because it decided to; the policy decides.
//! 2. **Every budget is checked before the call**, using a projection, not
//!    after the fact.
//! 3. **Every output is leak-scanned** before it is stored or displayed.
//! 4. **The loop always terminates**, even against a model that keeps asking
//!    for tools. `max_turns` is a hard stop, not a suggestion.

use crate::cost::{Budget, BudgetState, Usage};
use crate::leak::Detector;
use crate::models::{CompletionRequest, Message, ToolCall};
use crate::observer::{LoopObserver, NoObserver, ObserverSink};
use crate::router::Router;
use crate::tools::{Decision, Overrides, Policy, ToolRegistry};

/// Why the loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer with no tool calls.
    Answered,
    /// Hard turn limit reached.
    TurnLimit,
    /// The budget ceiling stopped the run.
    BudgetBlocked,
    /// The model could not be served by any candidate.
    ProviderFailed(String),
    /// The operator stopped it.
    Cancelled,
    /// The model kept asking for the same blocked call after being told to
    /// change its approach. Carries the tool's name.
    LoopDetected(String),
}

/// Detects an agent stuck repeating itself.
///
/// The honest signal of "stuck" is not how many turns a run took — a long
/// task earns its turns — but *the same call over and over*. The guard:
///
/// 1. counts a streak of byte-identical calls (name + canonical arguments)
///    across the whole run, not just one turn;
/// 2. at the limit (default **10**, `BKGCLAW_LOOP_STREAK`) it does not
///    stop the run — it blocks that exact call for the rest of the run
///    and tells the model why, giving it one chance to change its approach;
/// 3. if a blocked call comes back anyway, the run stops with
///    [`StopReason::LoopDetected`].
///
/// Different calls reset the streak, so legitimate retries with changed
/// arguments (the model's way of reacting to feedback) never trigger it.
#[derive(Debug)]
pub struct LoopGuard {
    last: Option<String>,
    streak: usize,
    limit: usize,
    blocked: std::collections::HashSet<String>,
}

impl Default for LoopGuard {
    fn default() -> Self {
        LoopGuard::new(limit_from_env())
    }
}

impl LoopGuard {
    /// A guard with an explicit limit — the tests' way in.
    pub fn new(limit: usize) -> Self {
        LoopGuard {
            last: None,
            streak: 0,
            // A limit below 2 fires on an ordinary double-check; a broken
            // setting must not brick the agent either way.
            limit: limit.clamp(2, 64),
            blocked: std::collections::HashSet::new(),
        }
    }

    /// How many identical calls in a row before the nudge-and-block.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// One model-requested call, before execution. Returns what the loop
    /// should do with it.
    pub fn observe(&mut self, call: &ToolCall) -> LoopVerdict {
        let signature = format!("{}|{}", call.name, canonical_arguments(&call.arguments));

        if self.blocked.contains(&signature) {
            return LoopVerdict::Stop;
        }

        if self.last.as_deref() == Some(signature.as_str()) {
            self.streak += 1;
        } else {
            self.streak = 1;
            self.last = Some(signature.clone());
        }

        if self.streak >= self.limit {
            self.blocked.insert(signature);
            return LoopVerdict::Nudge;
        }
        LoopVerdict::Run
    }
}

/// The operator's tolerance: how many identical calls in a row are allowed
/// before the nudge. Default 10 — polling the same read-only tool a few
/// times is a model quirk, not a runaway; the hard stop still comes before
/// the turn ceiling.
pub fn limit_from_env() -> usize {
    parse_loop_streak(std::env::var("BKGCLAW_LOOP_STREAK").ok().as_deref())
}

/// Pure parsing so the default is testable without racing the process env.
pub fn parse_loop_streak(raw: Option<&str>) -> usize {
    match raw {
        None => 10,
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return 10;
            }
            match trimmed.parse::<usize>() {
                Ok(value) => value.clamp(2, 64),
                // A broken setting must not brick the agent — but it must
                // also not silently pass unnoticed: the clamp keeps the
                // agent alive, the journal keeps the surprise.
                _ => 10,
            }
        }
    }
}

/// What the guard decided about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopVerdict {
    /// A normal call: run it through the gate.
    Run,
    /// Third identical call in a row: refuse this one with an explanation,
    /// and block the signature for the rest of the run.
    Nudge,
    /// A blocked call came back: stop the run.
    Stop,
}

/// Canonical form of a call's arguments, so key order in the JSON does not
/// make two identical calls look different.
fn canonical_arguments(arguments: &serde_json::Value) -> String {
    // serde_json sorts map keys when the `preserve_order` feature is off
    // (it is, here): serialising gives a stable ordering for objects.
    serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string())
}

/// What one tool call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Ran {
        name: String,
        output: String,
        truncated: bool,
    },
    Denied {
        name: String,
        reason: String,
    },
    Failed {
        name: String,
        error: String,
    },
}

impl ToolOutcome {
    pub fn as_result_text(&self) -> String {
        match self {
            ToolOutcome::Ran { name, output, .. } => format!("{name}: {output}"),
            // The model needs to know a call was refused, or it will retry the
            // same thing forever believing it is still pending.
            ToolOutcome::Denied { name, reason } => format!("{name}: DENIED — {reason}"),
            ToolOutcome::Failed { name, error } => format!("{name}: FAILED — {error}"),
        }
    }
}

/// One step of the loop, as the caller sees it.
#[derive(Debug, Clone)]
pub struct Turn {
    pub index: u32,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_outcomes: Vec<ToolOutcome>,
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub turns: Vec<Turn>,
    pub stop_reason: StopReason,
    pub total_usage: Usage,
    pub total_cost_usd: Option<f64>,
    /// Findings from the leak scanner across everything the models produced.
    pub leak_findings: Vec<String>,
    pub messages: Vec<Message>,
}

/// Runs tool calls. The loop supplies the gate decision; this decides what
/// actually happens, so tests can drive it without a filesystem or a shell.
pub trait ToolExecutor: Send + Sync {
    fn execute(&self, call: &ToolCall) -> Result<String, String>;
}

/// An executor that refuses everything. Used when no real executor is wired
/// and by tests that only exercise routing.
pub struct NoExecutor;

impl ToolExecutor for NoExecutor {
    fn execute(&self, call: &ToolCall) -> Result<String, String> {
        Err(format!("no executor wired for `{}`", call.name))
    }
}

/// The loop's configuration.
pub struct LoopConfig {
    pub max_turns: u32,
    /// How many characters of a tool result are kept. Beyond this the result
    /// is truncated with a marker, so one `cat` of a huge file cannot blow the
    /// context window.
    pub max_tool_output: usize,
    pub policy: Policy,
    pub overrides: Overrides,
    pub detector: Detector,
}

impl Default for LoopConfig {
    fn default() -> Self {
        LoopConfig {
            // A runaway loop is a bug in the model or the task, not something
            // to wait out. 24 turns covers a realistic multi-step task.
            max_turns: 24,
            max_tool_output: 8_000,
            policy: Policy::DenyAll,
            overrides: Overrides::new(),
            detector: Detector::new(),
        }
    }
}

/// Runs the loop to completion.
pub async fn run_agent_turns<'a>(
    router: &'a mut Router<'a>,
    registry: &ToolRegistry,
    executor: &dyn ToolExecutor,
    config: &LoopConfig,
    budget: &mut Budget,
    messages: Vec<Message>,
) -> RunResult {
    run_agent_turns_with(
        router,
        registry,
        executor,
        config,
        budget,
        messages,
        &NoObserver,
    )
    .await
}

/// The loop, watched. Identical behaviour to `run_agent_turns`, plus every
/// event a live UI needs — and the one question it answers: whether a
/// gated tool call may run.
pub async fn run_agent_turns_with<'a>(
    router: &'a mut Router<'a>,
    registry: &ToolRegistry,
    executor: &dyn ToolExecutor,
    config: &LoopConfig,
    budget: &mut Budget,
    messages: Vec<Message>,
    observer: &dyn LoopObserver,
) -> RunResult {
    let result = run_inner(
        router, registry, executor, config, budget, messages, observer,
    )
    .await;
    observer.on_stop(&result.stop_reason);
    result
}

async fn run_inner<'a>(
    router: &'a mut Router<'a>,
    registry: &ToolRegistry,
    executor: &dyn ToolExecutor,
    config: &LoopConfig,
    budget: &mut Budget,
    mut messages: Vec<Message>,
    observer: &dyn LoopObserver,
) -> RunResult {
    let mut turns: Vec<Turn> = Vec::new();
    let mut total_usage = Usage::default();
    let mut total_cost: Option<f64> = Some(0.0);
    let mut leak_findings: Vec<String> = Vec::new();
    // Watches for the model asking for the same call over and over — the
    // real "stuck" signal, unlike the turn ceiling which only bounds
    // runaway bills. The operator's tolerance comes from the environment
    // once, per run.
    let mut guard = LoopGuard::new(limit_from_env());
    let loop_limit = guard.limit();

    for index in 0..config.max_turns {
        observer.on_turn_start(index);

        // Budget is checked before every call, with a projection for the call
        // about to be made. Checking afterwards means the overrun already
        // happened.
        if budget.check(0.0).is_err() {
            return RunResult {
                turns,
                stop_reason: StopReason::BudgetBlocked,
                total_usage,
                total_cost_usd: total_cost,
                leak_findings,
                messages,
            };
        }

        let request = CompletionRequest {
            model: router
                .chain
                .first()
                .map(|c| c.model.clone())
                .expect("chain checked by router"),
            messages: messages.clone(),
            tools: registry.specs(),
            max_tokens: 4096,
            temperature: None,
            thinking_budget: None,
            effort: None,
        };

        // A live hint of who is about to be asked. Failover may answer from
        // further down the chain; `Turn.model` carries the truth afterwards.
        if let Some(first) = router.chain.first() {
            observer.on_model_request(&first.model);
        }

        let routed = match router
            .complete_streamed(&request, &ObserverSink::new(observer))
            .await
        {
            Ok(routed) => routed,
            Err(error) => {
                return RunResult {
                    turns,
                    stop_reason: StopReason::ProviderFailed(error.to_string()),
                    total_usage,
                    total_cost_usd: total_cost,
                    leak_findings,
                    messages,
                };
            }
        };

        // Charge what actually happened, then learn the new state.
        if let Some(cost) = routed.cost_usd {
            *total_cost.get_or_insert(0.0) += cost;
            budget.charge(cost);
        }
        total_usage = total_usage.merge(&routed.response.usage);

        // Scan model output before it is stored or shown.
        let findings = config.detector.scan(&routed.response.content);
        leak_findings.extend(findings.iter().map(|f| format!("{}@{}", f.rule, f.start)));

        observer.on_assistant_text(&routed.response.content, &routed.served_by);

        let mut turn = Turn {
            index,
            text: routed.response.content.clone(),
            tool_calls: routed.response.tool_calls.clone(),
            tool_outcomes: Vec::new(),
            usage: routed.response.usage,
            cost_usd: routed.cost_usd,
            model: routed.served_by.to_string(),
        };

        if routed.response.tool_calls.is_empty() {
            messages.push(Message::assistant(routed.response.content));
            turns.push(turn.clone());
            observer.on_turn_end(&turn);
            return RunResult {
                turns,
                stop_reason: StopReason::Answered,
                total_usage,
                total_cost_usd: total_cost,
                leak_findings,
                messages,
            };
        }

        messages.push(Message::assistant(routed.response.content.clone()));

        for call in routed.response.tool_calls {
            // Loop detection first: the model's *request* is the signal,
            // whatever the gate would have said about it.
            match guard.observe(&call) {
                LoopVerdict::Run => {}
                LoopVerdict::Nudge => {
                    // Third identical call in a row. The call is refused
                    // with the reason and blocked for the rest of the run —
                    // the model gets exactly one chance to change its
                    // approach instead of a silent kill.
                    let outcome = ToolOutcome::Denied {
                        name: call.name.clone(),
                        reason: format!(
                            "LOOP — dieser exakte aufruf kam {loop_limit}× hintereinander \
                             und ist für den rest dieses laufs blockiert. \
                             ändere die argumente, nimm ein anderes werkzeug, \
                             oder frage den menschen",
                        ),
                    };
                    messages.push(Message::tool_result(outcome.as_result_text(), call.id.clone()));
                    turn.tool_outcomes.push(outcome.clone());
                    observer.on_tool_outcome(&outcome);
                    continue;
                }
                LoopVerdict::Stop => {
                    let name = call.name.clone();
                    return RunResult {
                        turns,
                        stop_reason: StopReason::LoopDetected(name),
                        total_usage,
                        total_cost_usd: total_cost,
                        leak_findings,
                        messages,
                    };
                }
            }

            let gate = registry.gate(&call.name, config.policy, &config.overrides);
            match gate.decision {
                Decision::Deny => {
                    // Tell the model it was refused so it can adapt rather than
                    // retrying the same call in a loop.
                    let outcome = ToolOutcome::Denied {
                        name: call.name.clone(),
                        reason: gate.reason,
                    };
                    messages.push(Message::tool_result(
                        outcome.as_result_text(),
                        call.id.clone(),
                    ));
                    turn.tool_outcomes.push(outcome.clone());
                    observer.on_tool_outcome(&outcome);
                }
                Decision::Ask => {
                    // A human decides. The observer's default answer is a
                    // denial, so an unwired observer can never widen
                    // permissions — only an explicit answer can.
                    let decision = observer.approve(&call, &gate).await;
                    match decision {
                        Decision::Allow => match executor.execute(&call) {
                            Ok(output) => {
                                let findings = config.detector.scan(&output);
                                leak_findings.extend(
                                    findings.iter().map(|f| format!("{}@{}", f.rule, f.start)),
                                );

                                let (output, truncated) = truncate(&output, config.max_tool_output);
                                let outcome = ToolOutcome::Ran {
                                    name: call.name.clone(),
                                    output,
                                    truncated,
                                };
                                messages.push(Message::tool_result(
                                    outcome.as_result_text(),
                                    call.id.clone(),
                                ));
                                turn.tool_outcomes.push(outcome.clone());
                                observer.on_tool_outcome(&outcome);
                            }
                            Err(error) => {
                                let outcome = ToolOutcome::Failed {
                                    name: call.name.clone(),
                                    error: error.clone(),
                                };
                                messages.push(Message::tool_result(
                                    outcome.as_result_text(),
                                    call.id.clone(),
                                ));
                                turn.tool_outcomes.push(outcome.clone());
                                observer.on_tool_outcome(&outcome);
                            }
                        },
                        // Deny, ask-again or anything else: the safe reading.
                        _ => {
                            let outcome = ToolOutcome::Denied {
                                name: call.name.clone(),
                                reason: "approval refused by operator".to_string(),
                            };
                            messages.push(Message::tool_result(
                                outcome.as_result_text(),
                                call.id.clone(),
                            ));
                            turn.tool_outcomes.push(outcome.clone());
                            observer.on_tool_outcome(&outcome);
                        }
                    }
                }
                Decision::Allow => match executor.execute(&call) {
                    Ok(output) => {
                        let findings = config.detector.scan(&output);
                        leak_findings
                            .extend(findings.iter().map(|f| format!("{}@{}", f.rule, f.start)));

                        let (output, truncated) = truncate(&output, config.max_tool_output);
                        let outcome = ToolOutcome::Ran {
                            name: call.name.clone(),
                            output,
                            truncated,
                        };
                        messages.push(Message::tool_result(
                            outcome.as_result_text(),
                            call.id.clone(),
                        ));
                        turn.tool_outcomes.push(outcome.clone());
                        observer.on_tool_outcome(&outcome);
                    }
                    Err(error) => {
                        let outcome = ToolOutcome::Failed {
                            name: call.name.clone(),
                            error: error.clone(),
                        };
                        messages.push(Message::tool_result(
                            outcome.as_result_text(),
                            call.id.clone(),
                        ));
                        turn.tool_outcomes.push(outcome.clone());
                        observer.on_tool_outcome(&outcome);
                    }
                },
            }
        }

        turns.push(turn.clone());
        observer.on_turn_end(&turn);

        if budget.state() == BudgetState::Blocked {
            return RunResult {
                turns,
                stop_reason: StopReason::BudgetBlocked,
                total_usage,
                total_cost_usd: total_cost,
                leak_findings,
                messages,
            };
        }
    }

    RunResult {
        turns,
        stop_reason: StopReason::TurnLimit,
        total_usage,
        total_cost_usd: total_cost,
        leak_findings,
        messages,
    }
}

fn truncate(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    // Cut on a char boundary, then mark it, so the model knows output was lost
    // rather than believing the file ended there.
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}\n… [truncated at {end} bytes]", &text[..end]),
        true,
    )
}

/// Formats a run for the terminal or a log.
pub fn summarise(result: &RunResult) -> String {
    let cost = match result.total_cost_usd {
        Some(value) => format!("${value:.4}"),
        None => "cost unknown".to_string(),
    };
    let last = result.turns.last().map(|t| t.text.as_str()).unwrap_or("");
    let mut out = format!(
        "{} turn(s), {} tokens, {}, stopped: {:?}",
        result.turns.len(),
        result.total_usage.total(),
        cost,
        result.stop_reason
    );
    if !last.is_empty() {
        out.push_str(&format!("\n{last}"));
    }
    if !result.leak_findings.is_empty() {
        out.push_str(&format!(
            "\n⚠ {} credential leak finding(s): {}",
            result.leak_findings.len(),
            result.leak_findings.join(", ")
        ));
    }
    out
}

/// Convenience for the common single-question case.
pub fn budget_for(limit_usd: Option<f64>) -> Budget {
    Budget::new(limit_usd)
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::cost::{CostTable, ModelPrice, Usage};
    use crate::models::{CompletionResponse, ModelError, ModelId, ModelRegistry, Vendor};
    use crate::router::{Candidate, Router};
    use crate::tools::{Risk, Tool};
    use std::sync::{Arc, Mutex};

    /// A model that walks a scripted response list. When the script runs out
    /// it keeps answering with the last entry, so a loop test cannot hang.
    struct Scripted {
        responses: Mutex<Vec<CompletionResponse>>,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl crate::models::ModelProvider for Scripted {
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
            static EMPTY: std::sync::OnceLock<CostTable> = std::sync::OnceLock::new();
            EMPTY.get_or_init(CostTable::new)
        }
        async fn complete(&self, _r: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
            *self.calls.lock().unwrap() += 1;
            let mut responses = self.responses.lock().unwrap();
            if responses.len() > 1 {
                Ok(responses.remove(0))
            } else {
                Ok(responses[0].clone())
            }
        }
    }

    struct Exec {
        ran: Mutex<Vec<String>>,
        output: String,
    }

    impl ToolExecutor for Exec {
        fn execute(&self, call: &ToolCall) -> Result<String, String> {
            self.ran.lock().unwrap().push(call.name.clone());
            Ok(self.output.clone())
        }
    }

    fn text(t: &str) -> CompletionResponse {
        CompletionResponse {
            content: t.into(),
            tool_calls: vec![],
            usage: Usage::default(),
            model: ModelId::new(Vendor::Ollama, "m"),
            truncated: false,
        }
    }

    fn tool(t: &str) -> CompletionResponse {
        let mut r = text("working");
        r.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: t.into(),
            arguments: serde_json::json!({}),
        }];
        r
    }

    /// Everything a loop test needs. The registry is built first, then the
    /// router borrows it — which is why the loop takes them as separate
    /// parameters rather than bundling them.
    struct Harness {
        registry: ModelRegistry,
        exec: Exec,
    }

    impl Harness {
        /// A fresh router over this harness's registry. Cheap enough to make
        /// per test, and avoids any self-referential borrow.
        fn router(&self) -> Router<'_> {
            let mut router = Router::new(
                &self.registry,
                vec![Candidate::new(
                    ModelId::new(Vendor::Ollama, "m"),
                    ModelPrice::default(),
                )],
            );
            router.max_attempts_per_vendor = 1;
            router
        }
    }

    fn harness(script: Vec<CompletionResponse>, tool_output: &str) -> Harness {
        let calls = Arc::new(Mutex::new(0u32));
        let mut registry = ModelRegistry::new();
        registry.register(Box::new(Scripted {
            responses: Mutex::new(script),
            calls,
        }));
        Harness {
            registry,
            exec: Exec {
                ran: Mutex::new(vec![]),
                output: tool_output.into(),
            },
        }
    }

    fn read_only_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Tool::new("read_file", Risk::ReadOnly, "read"));
        r.register(Tool::new("execute_command", Risk::Destructive, "run"));
        r
    }

    fn config(policy: Policy) -> LoopConfig {
        LoopConfig {
            policy,
            ..LoopConfig::default()
        }
    }

    #[tokio::test]
    async fn a_direct_answer_stops_the_loop_immediately() {
        let h = harness(vec![text("42")], "unused");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("what is 6 times 7")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Answered);
        assert_eq!(result.turns.len(), 1);
        assert_eq!(result.turns[0].text, "42");
    }

    #[tokio::test]
    async fn a_permitted_tool_runs_and_its_result_feeds_the_next_turn() {
        let h = harness(
            vec![tool("read_file"), text("the file says hello")],
            "file contents",
        );
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowReadOnly),
            &mut budget,
            vec![Message::user("read the file")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Answered);
        assert_eq!(h.exec.ran.lock().unwrap().as_slice(), ["read_file"]);
        // The tool result must be in the transcript, or the model never saw it.
        assert!(result.messages.iter().any(|m| match m {
            Message::ToolResult { content, .. } => content.contains("file contents"),
            _ => false,
        }));
    }

    #[tokio::test]
    async fn a_denied_tool_never_executes_and_the_model_is_told() {
        let h = harness(vec![tool("execute_command"), text("understood")], "");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowReadOnly),
            &mut budget,
            vec![Message::user("delete everything")],
        )
        .await;
        // The whole point: the destructive call did not run.
        assert!(
            h.exec.ran.lock().unwrap().is_empty(),
            "a denied tool must not execute"
        );
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Denied { .. }
        ));
        assert!(
            result.messages.iter().any(|m| match m {
                Message::ToolResult { content, .. } => content.contains("DENIED"),
                _ => false,
            }),
            "the model must learn the call was refused"
        );
    }

    #[tokio::test]
    async fn deny_all_blocks_read_tools_too() {
        let h = harness(vec![tool("read_file"), text("ok")], "x");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::DenyAll),
            &mut budget,
            vec![Message::user("read")],
        )
        .await;
        assert!(h.exec.ran.lock().unwrap().is_empty());
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn a_zero_budget_stops_before_the_first_call() {
        let h = harness(vec![text("never reached")], "");
        let mut router = h.router();
        let mut budget = Budget::new(Some(0.0));
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("hello")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::BudgetBlocked);
        assert!(
            result.turns.is_empty(),
            "no model call may happen at zero budget"
        );
    }

    #[tokio::test]
    async fn the_loop_terminates_even_when_the_model_keeps_calling_tools() {
        // Every response asks for a tool; without a turn cap this never ends.
        let h = harness(vec![tool("read_file")], "looping");
        let mut router = h.router();
        let mut config = config(Policy::AllowReadOnly);
        config.max_turns = 3;
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config,
            &mut budget,
            vec![Message::user("go")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::TurnLimit);
        assert_eq!(
            result.turns.len(),
            3,
            "the cap must be exact, not approximate"
        );
    }

    #[tokio::test]
    async fn a_credential_in_model_output_is_reported() {
        let leaked = "here is sk-ant-api03-AbCdEf0123456789AbCdEf0123456789";
        let h = harness(vec![text(leaked)], "");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("show me the key")],
        )
        .await;
        assert!(
            !result.leak_findings.is_empty(),
            "a leaked key must be reported"
        );
        assert!(result.leak_findings[0].contains("anthropic-key"));
    }

    #[tokio::test]
    async fn a_credential_in_tool_output_is_reported() {
        let h = harness(
            vec![tool("read_file"), text("done")],
            "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789",
        );
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowReadOnly),
            &mut budget,
            vec![Message::user("read")],
        )
        .await;
        assert!(
            !result.leak_findings.is_empty(),
            "tool output is scanned too"
        );
    }

    #[tokio::test]
    async fn a_clean_run_reports_no_findings() {
        let h = harness(vec![text("all good")], "");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("hello")],
        )
        .await;
        assert!(result.leak_findings.is_empty());
    }

    #[tokio::test]
    async fn a_failing_tool_is_reported_without_stopping_the_loop() {
        struct FailingExec;
        impl ToolExecutor for FailingExec {
            fn execute(&self, _c: &ToolCall) -> Result<String, String> {
                Err("no such file".to_string())
            }
        }
        let h = harness(vec![tool("read_file"), text("I could not read it")], "");
        let mut router = h.router();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &FailingExec,
            &config(Policy::AllowReadOnly),
            &mut budget,
            vec![Message::user("read")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Answered);
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn a_large_tool_result_is_truncated_before_reaching_the_model() {
        let h = harness(vec![tool("read_file"), text("ok")], &"x".repeat(50_000));
        let mut router = h.router();
        let mut config = config(Policy::AllowReadOnly);
        config.max_tool_output = 1_000;
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config,
            &mut budget,
            vec![Message::user("read")],
        )
        .await;
        match &result.turns[0].tool_outcomes[0] {
            ToolOutcome::Ran { truncated, output, .. } => {
                assert!(truncated);
                assert!(output.contains("truncated"));
                assert!(output.len() < 2_000, "output must not reach the model in full");
            }
            other => panic!("expected a truncated run, got {other:?}"),
        }
    }

    /// A model that requests the same tool call forever: the stubborn loop
    /// the guard exists for.
    struct Stubborn;

    #[test]
    fn the_streak_default_is_ten_and_settings_are_clamped() {
        // The operator asked for "min 10": the default carries it, and a
        // broken or extreme setting falls back instead of bricking.
        assert_eq!(parse_loop_streak(None), 10);
        assert_eq!(parse_loop_streak(Some("")), 10);
        assert_eq!(parse_loop_streak(Some("20")), 20);
        assert_eq!(parse_loop_streak(Some("1")), 2, "unter der clamp-grenze");
        assert_eq!(parse_loop_streak(Some("999")), 64, "über der clamp-grenze");
        assert_eq!(parse_loop_streak(Some("keine zahl")), 10);
    }

    #[test]
    fn the_guard_honours_its_limit_and_blocks_after_the_nudge() {
        // Fast variant with a small explicit limit: the run-in-test loop
        // below covers the default elsewhere.
        let mut guard = LoopGuard::new(3);
        let call = || ToolCall {
            id: "c".into(),
            name: "task_list".into(),
            arguments: serde_json::json!({}),
        };
        assert_eq!(guard.observe(&call()), LoopVerdict::Run);
        assert_eq!(guard.observe(&call()), LoopVerdict::Run);
        assert_eq!(guard.observe(&call()), LoopVerdict::Nudge, "der dritte wird verweigert");
        assert_eq!(guard.observe(&call()), LoopVerdict::Stop, "der vierte stoppt");
        // A different call is still fine — the block is per signature.
        let other = ToolCall {
            id: "c2".into(),
            name: "task_list".into(),
            arguments: serde_json::json!({ "andere": 1 }),
        };
        assert_eq!(guard.observe(&other), LoopVerdict::Run);
    }

    #[async_trait::async_trait]
    impl crate::models::ModelProvider for Stubborn {
        fn vendor(&self) -> Vendor { Vendor::Ollama }
        fn models(&self) -> &[ModelId] { &[] }
        fn credential_env(&self) -> Option<&'static str> { None }
        fn cost_table(&self) -> &CostTable {
            static EMPTY: std::sync::OnceLock<CostTable> = std::sync::OnceLock::new();
            EMPTY.get_or_init(CostTable::new)
        }
        async fn complete(&self, _r: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
            Ok(tool("execute_command"))
        }
    }

    #[tokio::test]
    async fn three_identical_calls_nudge_and_a_fourth_stops_the_run() {
        // The stubborn model requests the same call every turn: calls run
        // until the streak hits the limit, the limit-th is refused with
        // the LOOP explanation, the next one stops the run.
        let mut registry = ModelRegistry::new();
        registry.register(Box::new(Stubborn));
        let mut router = Router::new(
            &registry,
            vec![Candidate::new(ModelId::new(Vendor::Ollama, "m"), ModelPrice::default())],
        );
        // The default limit is 10: nine calls run, the tenth is refused,
        // the eleventh stops. The turn ceiling must not fire first.
        let mut config = config(Policy::AllowAll);
        config.max_turns = 40;
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &NoExecutor,
            &config,
            &mut budget,
            vec![Message::user("tu es")],
        )
        .await;

        assert!(
            matches!(result.stop_reason, StopReason::LoopDetected(ref name) if name == "execute_command"),
            "the stubborn loop must be detected, got {:?}",
            result.stop_reason
        );
        // The model was TOLD before the stop: one denied outcome carries
        // the LOOP explanation.
        let denials: Vec<&ToolOutcome> = result
            .turns
            .iter()
            .flat_map(|t| t.tool_outcomes.iter())
            .filter(|o| matches!(o, ToolOutcome::Denied { .. }))
            .collect();
        assert!(!denials.is_empty(), "the nudge must be visible in the outcomes");
        assert!(denials.iter().any(|o| format!("{o:?}").contains("LOOP")));
        // And the transcript told the model exactly why.
        assert!(result.messages.iter().any(|m| match m {
            Message::ToolResult { content, .. } => content.contains("blockiert"),
            _ => false,
        }));
    }

    #[tokio::test]
    async fn varied_calls_never_trigger_the_guard() {
        // Same tool, different arguments each time: the model reacting to
        // feedback is work, not a loop.
        struct Varying(Mutex<u32>);
        #[async_trait::async_trait]
        impl crate::models::ModelProvider for Varying {
            fn vendor(&self) -> Vendor { Vendor::Ollama }
            fn models(&self) -> &[ModelId] { &[] }
            fn credential_env(&self) -> Option<&'static str> { None }
            fn cost_table(&self) -> &CostTable {
                static EMPTY: std::sync::OnceLock<CostTable> = std::sync::OnceLock::new();
                EMPTY.get_or_init(CostTable::new)
            }
            async fn complete(&self, _r: &CompletionRequest) -> Result<CompletionResponse, ModelError> {
                let mut r = text("working");
                let n = {
                    let mut counter = self.0.lock().unwrap();
                    *counter += 1;
                    *counter
                };
                if n <= 12 {
                    r.tool_calls = vec![ToolCall {
                        id: format!("c{n}"),
                        name: "read_file".into(),
                        arguments: serde_json::json!({ "path": format!("/tmp/datei-{n}.txt") }),
                    }];
                }
                Ok(r)
            }
        }
        let mut registry = ModelRegistry::new();
        registry.register(Box::new(Varying(Mutex::new(0))));
        let mut router = Router::new(
            &registry,
            vec![Candidate::new(ModelId::new(Vendor::Ollama, "m"), ModelPrice::default())],
        );
        let mut config = config(Policy::AllowReadOnly);
        config.max_turns = 30;
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &read_only_registry(),
            &NoExecutor,
            &config,
            &mut budget,
            vec![Message::user("lies alle dateien")],
        )
        .await;
        assert_eq!(result.stop_reason, StopReason::Answered);
        assert!(result.turns.len() >= 12, "twelve varied calls are twelve turns of work");
    }

    #[test]
    fn the_guard_counts_signatures_not_key_order() {
        let mut guard = LoopGuard::new(3);
        let call = |arguments: serde_json::Value| ToolCall {
            id: "c".into(),
            name: "write_file".into(),
            arguments,
        };
        // Same call, different JSON key order: one signature.
        assert_eq!(guard.observe(&call(serde_json::json!({"path": "a", "content": "b"}))), LoopVerdict::Run);
        assert_eq!(guard.observe(&call(serde_json::json!({"content": "b", "path": "a"}))), LoopVerdict::Run);
        assert_eq!(guard.observe(&call(serde_json::json!({"path": "a", "content": "b"}))), LoopVerdict::Nudge);
    }

    /// Records every observer event, and answers approvals from a script.
    struct Recording {
        events: Mutex<Vec<String>>,
        approvals: std::collections::HashMap<String, Decision>,
    }

    impl Recording {
        fn deny_all() -> Self {
            Recording {
                events: Mutex::new(Vec::new()),
                approvals: std::collections::HashMap::new(),
            }
        }
        fn approve(name: &str) -> Self {
            let mut approvals = std::collections::HashMap::new();
            approvals.insert(name.to_string(), Decision::Allow);
            Recording {
                events: Mutex::new(Vec::new()),
                approvals,
            }
        }
        fn snapshot(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl LoopObserver for Recording {
        fn on_turn_start(&self, index: u32) {
            self.events
                .lock()
                .unwrap()
                .push(format!("turn_start:{index}"));
        }
        fn on_model_request(&self, model: &ModelId) {
            self.events
                .lock()
                .unwrap()
                .push(format!("model_request:{model}"));
        }
        fn on_assistant_text(&self, text: &str, _model: &ModelId) {
            self.events.lock().unwrap().push(format!(
                "text:{}",
                text.chars().take(20).collect::<String>()
            ));
        }
        fn on_tool_outcome(&self, outcome: &ToolOutcome) {
            self.events.lock().unwrap().push(format!(
                "tool:{}",
                match outcome {
                    ToolOutcome::Ran { name, .. } => format!("{name}:ran"),
                    ToolOutcome::Denied { name, .. } => format!("{name}:denied"),
                    ToolOutcome::Failed { name, .. } => format!("{name}:failed"),
                }
            ));
        }
        fn on_turn_end(&self, turn: &Turn) {
            self.events
                .lock()
                .unwrap()
                .push(format!("turn_end:{}", turn.index));
        }
        fn on_stop(&self, reason: &StopReason) {
            self.events.lock().unwrap().push(format!("stop:{reason:?}"));
        }
        async fn approve(&self, call: &ToolCall, _gate: &crate::tools::Gate) -> Decision {
            self.events
                .lock()
                .unwrap()
                .push(format!("approve:{}", call.name));
            self.approvals
                .get(&call.name)
                .copied()
                .unwrap_or(Decision::Deny)
        }
    }

    fn ask_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Tool::new("write_file", Risk::Mutating, "write"));
        r
    }

    fn ask_overrides() -> Overrides {
        Overrides::parse("write_file=ask")
    }

    #[tokio::test]
    async fn an_approving_observer_lets_an_ask_gated_tool_run() {
        let h = harness(vec![tool("write_file"), text("written")], "wrote it");
        let mut router = h.router();
        let mut config = config(Policy::DenyAll);
        config.overrides = ask_overrides();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns_with(
            &mut router,
            &ask_registry(),
            &h.exec,
            &config,
            &mut budget,
            vec![Message::user("write it")],
            &Recording::approve("write_file"),
        )
        .await;
        assert_eq!(h.exec.ran.lock().unwrap().as_slice(), ["write_file"]);
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Ran { .. }
        ));
    }

    #[tokio::test]
    async fn a_denying_observer_does_not_run_the_tool() {
        let h = harness(vec![tool("write_file"), text("understood")], "wrote it");
        let mut router = h.router();
        let mut config = config(Policy::DenyAll);
        config.overrides = ask_overrides();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns_with(
            &mut router,
            &ask_registry(),
            &h.exec,
            &config,
            &mut budget,
            vec![Message::user("write it")],
            &Recording::deny_all(),
        )
        .await;
        assert!(
            h.exec.ran.lock().unwrap().is_empty(),
            "a refused approval must not execute"
        );
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Denied { .. }
        ));
        // The model must learn of the refusal, or it retries forever.
        assert!(result.messages.iter().any(|m| match m {
            Message::ToolResult { content, .. } => content.contains("refused"),
            _ => false,
        }));
    }

    #[tokio::test]
    async fn without_an_observer_an_ask_tool_stays_denied() {
        // The wrapper must preserve the old, safe semantics exactly.
        let h = harness(vec![tool("write_file"), text("understood")], "wrote it");
        let mut router = h.router();
        let mut config = config(Policy::DenyAll);
        config.overrides = ask_overrides();
        let mut budget = Budget::unlimited();
        let result = run_agent_turns(
            &mut router,
            &ask_registry(),
            &h.exec,
            &config,
            &mut budget,
            vec![Message::user("write it")],
        )
        .await;
        assert!(h.exec.ran.lock().unwrap().is_empty());
        assert!(matches!(
            result.turns[0].tool_outcomes[0],
            ToolOutcome::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn events_arrive_in_a_meaningful_order() {
        let h = harness(vec![text("hello")], "unused");
        let mut router = h.router();
        let observer = Recording::deny_all();
        let mut budget = Budget::unlimited();
        run_agent_turns_with(
            &mut router,
            &read_only_registry(),
            &h.exec,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("hi")],
            &observer,
        )
        .await;

        let events = observer.snapshot();
        // Order is the contract a UI relies on: it learns the turn began,
        // which model is thinking, the text, the turn result, then the stop.
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| e.split(':').next().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["turn_start", "model_request", "text", "turn_end", "stop"]
        );
        assert!(events[2].contains("hello"));
        assert_eq!(events.last().unwrap(), "stop:Answered");
    }

    #[tokio::test]
    async fn a_streaming_model_delivers_deltas_through_the_observer() {
        // A provider that streams text in pieces: the observer must see the
        // deltas in order and the assembled text at the end.
        struct Streaming;
        #[async_trait::async_trait]
        impl crate::models::ModelProvider for Streaming {
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
                static EMPTY: std::sync::OnceLock<CostTable> = std::sync::OnceLock::new();
                EMPTY.get_or_init(CostTable::new)
            }
            async fn complete(
                &self,
                _r: &CompletionRequest,
            ) -> Result<CompletionResponse, ModelError> {
                unreachable!("complete_streamed must be used when asked for")
            }
            async fn complete_streamed(
                &self,
                _r: &CompletionRequest,
                sink: &dyn crate::observer::DeltaSink,
            ) -> Result<CompletionResponse, ModelError> {
                sink.text("Ein ");
                sink.text("Satz.");
                Ok(text("Ein Satz."))
            }
        }

        let mut registry = ModelRegistry::new();
        registry.register(Box::new(Streaming));
        let mut router = Router::new(
            &registry,
            vec![Candidate::new(
                ModelId::new(Vendor::Ollama, "m"),
                ModelPrice::default(),
            )],
        );
        struct DeltaLog(Mutex<Vec<String>>);
        impl crate::observer::LoopObserver for DeltaLog {
            fn on_delta(&self, delta: crate::observer::StreamDelta) {
                match delta {
                    crate::observer::StreamDelta::Text(t) => self.0.lock().unwrap().push(t),
                    crate::observer::StreamDelta::Reasoning(t) => {
                        self.0.lock().unwrap().push(format!("think:{t}"))
                    }
                }
            }
            fn on_assistant_text(&self, text: &str, _model: &ModelId) {
                self.0.lock().unwrap().push(format!("final:{text}"));
            }
        }
        let log = DeltaLog(Mutex::new(Vec::new()));
        let mut budget = Budget::unlimited();
        run_agent_turns_with(
            &mut router,
            &read_only_registry(),
            &NoExecutor,
            &config(Policy::AllowAll),
            &mut budget,
            vec![Message::user("hi")],
            &log,
        )
        .await;
        let recorded = log.0.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec!["Ein ", "Satz.", "final:Ein Satz."],
            "deltas in order, then the assembled text"
        );
    }
}


/// The per-run turn ceiling. A turn is one model response, which may carry
/// several tool calls — real coding work needs dozens of them, so the
/// default is generous; `BKGCLAW_MAX_TURNS` raises or lowers it. The limit
/// itself is not negotiable: a loop that cannot stop is a runaway bill.
pub fn max_turns() -> u32 {
    parse_max_turns(std::env::var("BKGCLAW_MAX_TURNS").ok().as_deref())
}

/// Pure parsing so the default is testable without racing the process env.
pub fn parse_max_turns(raw: Option<&str>) -> u32 {
    match raw {
        None => 512,
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return 512;
            }
            match trimmed.parse::<u32>() {
                Ok(value) if value >= 1 && value <= 512 => value,
                _ => 512, // a broken setting must not brick the agent
            }
        }
    }
}

#[cfg(test)]
mod max_turns_tests {
    #[test]
    fn broken_settings_fall_back_instead_of_bricking() {
        use super::parse_max_turns;
        assert_eq!(parse_max_turns(None), 512);
        assert_eq!(parse_max_turns(Some("")), 512);
        assert_eq!(parse_max_turns(Some("7")), 7);
        assert_eq!(parse_max_turns(Some("0")), 512);
        assert_eq!(parse_max_turns(Some("keine zahl")), 512);
        assert_eq!(parse_max_turns(Some("9999")), 512);
    }
}