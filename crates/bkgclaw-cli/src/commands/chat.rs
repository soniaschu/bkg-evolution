//! `bkgclaw chat` — the interactive REPL.
//!
//! The scriptable interactive surface: plain stdin/stdout, so it works in
//! pipes and CI exactly like in a terminal. Transcript continuity across
//! turns, slash commands over the same session store the gateway uses,
//! live streaming output, and approvals answered with a single key.

use std::io::{BufRead, Write};

use bkgclaw_core::loop_engine::{StopReason, ToolOutcome, run_agent_turns_with};
use bkgclaw_core::models::Message;
use bkgclaw_core::observer::{LoopObserver, StreamDelta};
use bkgclaw_core::tools::{Decision, Gate, Policy};
use bkgclaw_core::{Budget, Detector, LoopConfig};
use bkgclaw_store::Session;

use bkgclaw_gateway::wiring;

/// Everything the REPL keeps between turns.
pub struct Repl {
    session: Session,
    budget_limit: f64,
    spent: f64,
    prompt: String,
}

/// What one input line resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Send(String),
    Exit,
    /// A slash command, already executed by the caller.
    Handled,
}

/// Dispatch a slash command. Pure: prints nothing, touches no process
/// state beyond what it is given — the REPL loop does the printing.
pub fn handle_slash(line: &str, repl: &mut Repl, chain_names: &[String]) -> Action {
    let (command, rest) = match line.split_once(' ') {
        Some((head, tail)) => (head, tail.trim()),
        None => (line, ""),
    };
    match command {
        "/exit" | "/quit" => Action::Exit,
        "/help" => {
            println!(
                "/help /exit /new /model <id> /models /policy <p> /tools /cost /save /sessions /history"
            );
            println!("— alles andere geht an den Agenten");
            Action::Handled
        }
        "/new" => {
            repl.session = Session::new(repl.session.model.clone(), repl.session.policy.clone());
            repl.spent = 0.0;
            println!("neue Sitzung {}", repl.session.id);
            Action::Handled
        }
        "/model" if rest.is_empty() => {
            println!(
                "modell: {}",
                repl.session.model.as_deref().unwrap_or("(failover-kette)")
            );
            Action::Handled
        }
        "/model" => {
            if chain_names.iter().any(|m| m == rest) {
                repl.session.model = Some(rest.to_string());
                println!("modell: {rest}");
            } else {
                println!(
                    "`{rest}` ist nicht in der Kette: {}",
                    chain_names.join(", ")
                );
            }
            Action::Handled
        }
        "/models" => {
            for name in chain_names {
                println!("  {name}");
            }
            Action::Handled
        }
        "/policy" if rest.is_empty() => {
            println!("policy: {}", repl.session.policy);
            Action::Handled
        }
        "/policy" => {
            let policy = Policy::parse(rest);
            repl.session.policy = policy.as_str().to_string();
            println!("policy: {}", policy.as_str());
            Action::Handled
        }
        "/cost" => {
            let spent = repl.spent;
            println!(
                "${spent:.4} von ${:.4} — sitzung: ${:.4}",
                repl.budget_limit, repl.session.spend_usd
            );
            Action::Handled
        }
        "/save" => {
            match repl.session.save(&bkgclaw_store::home_root()) {
                Ok(()) => println!("gespeichert: {}", repl.session.id),
                Err(e) => println!("speichern fehlgeschlagen: {e}"),
            }
            Action::Handled
        }
        "/sessions" => {
            for summary in Session::list(&bkgclaw_store::home_root()) {
                println!(
                    "  {}  {}  [{} nachrichten]",
                    summary.id, summary.preview, summary.message_count
                );
            }
            Action::Handled
        }
        "/history" => {
            for message in &repl.session.messages {
                match message {
                    Message::User { content } => println!("  du: {}", clip_line(content)),
                    Message::Assistant { content } => println!("  agent: {}", clip_line(content)),
                    Message::ToolResult { content, .. } => {
                        println!("  werkzeug: {}", clip_line(content))
                    }
                    Message::System { .. } => {}
                }
            }
            Action::Handled
        }
        "/tools" => {
            let registry = bkgclaw_core::tools::builtin_tools();
            let policy = Policy::parse(&repl.session.policy);
            let overrides = interactive_overrides(policy);
            for tool in registry.all() {
                let decision = registry
                    .gate(&tool.name, Policy::AllowAll, &overrides)
                    .decision;
                let mode = match decision {
                    Decision::Allow => "auto",
                    Decision::Ask => "frage",
                    Decision::Deny => "nein",
                };
                println!("  {:<18} {:<10} {}", tool.name, tool.risk.as_str(), mode);
            }
            Action::Handled
        }
        _ => Action::Send(line.to_string()),
    }
}

/// The interactive mapping: in a REPL there is a human, so what a policy
/// would silently deny becomes a question. Same rule the gateway uses,
/// implemented here because the CLI need not depend on the gateway crate.
pub fn interactive_overrides(policy: Policy) -> bkgclaw_core::tools::Overrides {
    let registry = bkgclaw_core::tools::builtin_tools();
    let pairs: Vec<String> = registry
        .all()
        .iter()
        .filter(|tool| !policy.permits(tool.risk))
        .map(|tool| format!("{}=ask", tool.name))
        .collect();
    bkgclaw_core::tools::Overrides::parse(&pairs.join(","))
}

fn clip_line(text: &str) -> String {
    let mut line = text.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 100 {
        line = format!("{}…", line.chars().take(100).collect::<String>());
    }
    line
}

/// The observer that prints a run as it happens and asks on stdin when the
/// gate says Ask. Only used in a terminal REPL; CI runs answer nothing and
/// therefore deny. "Immer" answers grant the tool for the rest of the
/// session through the shared `granted` set.
struct ReplObserver {
    granted: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl ReplObserver {
    fn shared() -> Self {
        ReplObserver {
            granted: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }
}

#[async_trait::async_trait]
impl LoopObserver for ReplObserver {
    fn on_model_request(&self, model: &bkgclaw_core::ModelId) {
        println!("… {model}");
    }
    fn on_delta(&self, delta: StreamDelta) {
        if let StreamDelta::Text(text) = delta {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }
    fn on_assistant_text(&self, text: &str, _model: &bkgclaw_core::ModelId) {
        // Deltas were streamed already; start a new line for the shell.
        if !text.is_empty() {
            println!();
        }
    }
    fn on_tool_outcome(&self, outcome: &ToolOutcome) {
        match outcome {
            ToolOutcome::Ran {
                name,
                output,
                truncated,
            } => {
                let marker = if *truncated { " (gekürzt)" } else { "" };
                println!("  ⚙ {name}{marker}: {}", clip_line(output));
            }
            ToolOutcome::Denied { name, reason } => println!("  ⊘ {name}: {reason}"),
            ToolOutcome::Failed { name, error } => println!("  ✗ {name}: {error}"),
        }
    }
    async fn approve(&self, call: &bkgclaw_core::ToolCall, gate: &Gate) -> Decision {
        if self.granted.lock().unwrap().contains(&call.name) {
            return Decision::Allow;
        }
        println!();
        println!("  ⏸ Freigabe nötig: {} ({})", call.name, gate.reason);
        println!("    argumente: {}", call.arguments);
        print!("  erlauben? [j/n/i(mmer)] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().lock().read_line(&mut answer).unwrap_or(0) == 0 {
            return Decision::Deny; // EOF is not consent
        }
        match answer.trim().to_lowercase().as_str() {
            "j" | "y" | "ja" | "yes" => Decision::Allow,
            "i" | "immer" | "always" => {
                self.granted.lock().unwrap().insert(call.name.clone());
                Decision::Allow
            }
            _ => Decision::Deny,
        }
    }
}

/// `bkgclaw chat`: the loop around stdin.
pub async fn run(
    policy: Policy,
    model: Option<String>,
    resume: Option<String>,
) -> std::io::Result<i32> {
    let registry = wiring::model_registry();
    let chain = wiring::default_chain().await;
    if chain.is_empty() {
        eprintln!("kein nutzbares Modell — NIM_API_KEY setzen oder ollama starten");
        return Ok(3);
    }
    let chain_names: Vec<String> = chain.iter().map(|c| c.model.to_string()).collect();

    let session = match resume {
        Some(id) => match Session::load(&bkgclaw_store::home_root(), &id) {
            Some(session) => {
                println!("sitzung {id} fortgesetzt");
                session
            }
            None => {
                eprintln!("keine sitzung `{id}`");
                return Ok(2);
            }
        },
        None => Session::new(model, policy.as_str().to_string()),
    };

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

    let mut repl = Repl {
        session,
        budget_limit: 2.0,
        spent: 0.0,
        prompt,
    };

    println!(
        "bkgclaw chat — {} · sitzung {} · /help für befehle",
        repl.session.policy, repl.session.id
    );
    let _ = repl.session.save(&bkgclaw_store::home_root());

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF ends the session, like every good REPL
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match handle_slash(trimmed, &mut repl, &chain_names) {
            Action::Exit => break,
            Action::Handled => {}
            Action::Send(message) => {
                let policy = Policy::parse(&repl.session.policy);
                let overrides = interactive_overrides(policy);
                let mut transcript = Vec::with_capacity(repl.session.messages.len() + 2);
                transcript.push(Message::system(repl.prompt.clone()));
                transcript.extend(repl.session.messages.iter().cloned());
                transcript.push(Message::user(&message));

                let full_chain = wiring::default_chain().await;
                let chain = match &repl.session.model {
                    Some(pinned) => match bkgclaw_core::ModelId::parse(pinned)
                        .ok_or_else(|| format!("`{pinned}` ist nicht anbieter/modell"))
                        .and_then(|id| {
                            full_chain
                                .into_iter()
                                .find(|c| c.model == id)
                                .map(|c| vec![c])
                                .ok_or_else(|| format!("`{pinned}` ist nicht in der kette"))
                        }) {
                        Ok(chain) => chain,
                        Err(error) => {
                            println!("  ✗ {error}");
                            continue;
                        }
                    },
                    None => full_chain,
                };

                let executor = bkgclaw_exec::LocalExecutor::new();
                let config = LoopConfig {
                    max_turns: 24,
                    max_tool_output: 8_000,
                    policy,
                    overrides,
                    detector: Detector::new(),
                };
                let mut router = bkgclaw_core::Router::new(&registry, chain);
                router.breakers = wiring::default_breakers();
                let mut budget = Budget::new(Some(repl.budget_limit));

                let observer = ReplObserver::shared();
                let result = run_agent_turns_with(
                    &mut router,
                    &bkgclaw_core::tools::builtin_tools(),
                    &executor,
                    &config,
                    &mut budget,
                    transcript,
                    &observer,
                )
                .await;

                if let Some(cost) = result.total_cost_usd {
                    repl.spent += cost;
                    repl.session.spend_usd += cost;
                }
                repl.session.messages = result.messages[1..].to_vec();
                repl.session.turns += result.turns.len() as u32;
                repl.session.touch();
                let _ = repl.session.save(&bkgclaw_store::home_root());

                if result.stop_reason != StopReason::Answered {
                    println!();
                    println!("  ⏹ {:?}", result.stop_reason);
                }
                if !result.leak_findings.is_empty() {
                    eprintln!("⚠ leck-funde: {}", result.leak_findings.join(", "));
                }
            }
        }
    }

    println!(
        "sitzung {} gespeichert — ${:.4} ausgegeben",
        repl.session.id, repl.spent
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repl() -> Repl {
        Repl {
            session: Session::new(None, "allow-read-only".into()),
            budget_limit: 2.0,
            spent: 0.0,
            prompt: "system".into(),
        }
    }

    fn chain() -> Vec<String> {
        vec!["nim/nvidia/nemotron-3-super-120b-a12b".into()]
    }

    #[test]
    fn exit_and_help_are_handled_not_sent() {
        let mut r = repl();
        assert_eq!(handle_slash("/exit", &mut r, &chain()), Action::Exit);
        assert_eq!(handle_slash("/help", &mut r, &chain()), Action::Handled);
    }

    #[test]
    fn a_known_model_can_be_pinned_and_its_state_shown() {
        let mut r = repl();
        handle_slash(
            "/model nim/nvidia/nemotron-3-super-120b-a12b",
            &mut r,
            &chain(),
        );
        assert_eq!(
            r.session.model.as_deref(),
            Some("nim/nvidia/nemotron-3-super-120b-a12b")
        );
    }

    #[test]
    fn an_unknown_model_is_refused_and_not_pinned() {
        let mut r = repl();
        handle_slash("/model nim/gibtsnicht", &mut r, &chain());
        assert!(r.session.model.is_none());
    }

    #[test]
    fn a_policy_change_round_trips_through_the_session() {
        let mut r = repl();
        handle_slash("/policy allow-mutating", &mut r, &chain());
        assert_eq!(r.session.policy, "allow-mutating");
        handle_slash("/policy ", &mut r, &chain());
        assert_eq!(r.session.policy, "allow-mutating");
    }

    #[test]
    fn plain_text_goes_to_the_model() {
        let mut r = repl();
        assert_eq!(
            handle_slash("schreib mir eine datei", &mut r, &chain()),
            Action::Send("schreib mir eine datei".into())
        );
    }

    #[test]
    fn the_interactive_mapping_asks_before_writes_under_read_only() {
        let registry = bkgclaw_core::tools::builtin_tools();
        let overrides = interactive_overrides(Policy::AllowReadOnly);
        assert_eq!(
            registry
                .gate("write_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Ask
        );
        assert_eq!(
            registry
                .gate("read_file", Policy::AllowAll, &overrides)
                .decision,
            Decision::Allow
        );
    }
}
