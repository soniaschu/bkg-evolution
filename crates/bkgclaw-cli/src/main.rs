//! bkgclaw — command-line interface.
//!
//! This file is deliberately thin. It parses arguments, calls one command
//! module, renders one report, sets the exit code. No business logic, no
//! provider knowledge, no formatting.
//!
//! The structural difference from what it replaces: `main.rs` here is under
//! 250 lines, and every credential flag lives behind `auth`, not smeared across
//! every provider's flag set. A provider added tomorrow appears in
//! `bkgclaw providers` because it was registered, not because 20 flags were
//! duplicated.

mod commands;
mod outcome;
mod providers;

use clap::{Parser, Subcommand};

use bkgclaw_core::tools::{Overrides, Policy};
use bkgclaw_gateway::wiring;
use bkgclaw_ui::{Mode, Report, render};
use outcome::Outcome;

#[derive(Parser)]
#[command(
    name = "bkgclaw",
    version,
    about = "Deploy and manage BKG agent instances",
    propagate_version = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// JSON only on stdout, nothing before or after.
    #[arg(long, global = true)]
    json: bool,

    /// Essential output only.
    #[arg(long, short, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Check the local environment and report every finding.
    Doctor,

    /// List every registered provider and whether it is configured.
    Providers,

    /// Manage credentials.
    #[command(subcommand)]
    Auth(AuthCommand),

    /// Create an instance from a named spec.
    Deploy {
        /// Which provider to use.
        #[arg(long)]
        provider: String,

        /// Size slug, e.g. s-1vcpu-1gb.
        #[arg(long, default_value = "s-1vcpu-1gb")]
        size: String,

        /// Disk size in GB.
        #[arg(long, default_value_t = 25)]
        disk: u32,

        /// Image slug, e.g. ubuntu-24-04.
        #[arg(long, default_value = "ubuntu-24-04")]
        image: String,

        /// Region slug.
        #[arg(long)]
        region: String,

        /// Instance name. Generated when omitted.
        #[arg(long)]
        name: Option<String>,
    },

    /// List instances for a provider.
    Status {
        #[arg(long)]
        provider: String,
    },

    /// Create a snapshot.
    Snapshot {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        id: String,
        /// Label for the snapshot.
        #[arg(long, default_value = "manual")]
        label: String,
    },

    /// Destroy an instance.
    Destroy {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        id: String,
        /// Required. Destroying an instance is not reversible.
        #[arg(long)]
        yes: bool,
    },

    /// Print the resolved exit-code contract.
    ExitCodes,

    /// List every model the router can reach, and whether it is usable.
    Models,

    /// List the agent tools and their risk levels.
    Tools {
        /// Approval policy: deny-all | allow-read-only | allow-mutating | allow-all.
        #[arg(long, default_value = "allow-read-only")]
        policy: String,
    },

    /// Run one agent task and print the result.
    Run {
        /// The task for the agent.
        task: String,

        /// Pin one model instead of using the failover chain.
        #[arg(long)]
        model: Option<String>,

        /// Hard cap on agent turns.
        #[arg(long, default_value_t = 12)]
        max_turns: u32,

        /// Spend ceiling in dollars.
        #[arg(long)]
        budget: Option<f64>,

        /// Approval policy for tools.
        #[arg(long, default_value = "allow-read-only")]
        policy: String,

        /// Per-tool overrides, e.g. write_file=allow,bash=deny.
        #[arg(long, default_value = "")]
        tools: String,

        /// Run without exposing any tools.
        #[arg(long)]
        no_tools: bool,
    },

    /// Interactive session against the failover chain.
    Chat {
        /// Approval policy for tools.
        #[arg(long, default_value = "allow-read-only")]
        policy: String,

        /// Pin one model instead of using the failover chain.
        #[arg(long)]
        model: Option<String>,

        /// Resume a stored session by id.
        #[arg(long)]
        session: Option<String>,
    },

    /// Start the OpenClaw-style gateway daemon (REST + WebSocket).
    Gateway {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Bind port.
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },

    /// The interactive terminal client (connects to or embeds the gateway).
    Tui {
        /// Gateway URL; default resolves env, then 127.0.0.1:8787, then embeds.
        #[arg(long)]
        url: Option<String>,

        /// Resume a stored session by id.
        #[arg(long)]
        session: Option<String>,

        /// Pin one model for this session.
        #[arg(long)]
        model: Option<String>,

        /// Approval policy.
        #[arg(long, default_value = "allow-read-only")]
        policy: String,
    },

    /// Scaffold the workspace (SOUL/IDENTITY/USER/MEMORY, skills/, tasks/).
    Init {
        /// personal or worker.
        mode: String,
    },

    /// Manage plugins: skills and manifests from github or local paths.
    #[command(subcommand)]
    Plugins(PluginsCommand),

    /// The built-in checkpoint system: snapshot, changes, restore.
    #[command(subcommand)]
    Versions(VersionsCommand),

    /// The self-improvement cycle: branch, work, fitness-gate, commit.
    #[command(subcommand)]
    Evolve(EvolveCommand),
}

#[derive(Subcommand)]
enum EvolveCommand {
    /// Initialize the evolution repo (git init + baseline commit + origin).
    Init {
        /// Remote repository, e.g. https://github.com/soniaschu/bkg-evolution.git
        #[arg(long)]
        origin: Option<String>,
    },
    /// Run one evolution cycle against this tree.
    Run {
        /// What to improve.
        goal: String,
        /// Push the attempt branch after recording.
        #[arg(long)]
        push: bool,
        /// Only commit when fitness measurably improved (not neutral).
        #[arg(long)]
        strict: bool,
    },
    /// Show the attempt archive and the last fitness verdicts.
    Attempts,
    /// Measure the current tree's fitness without evolving.
    Fitness,
    /// Push the current branch and the journal to origin.
    Push,
}

#[derive(Subcommand)]
enum VersionsCommand {
    /// Activate versioning for this directory (auto-snapshots before
    /// write-capable agent runs).
    Init,
    /// Take a snapshot now.
    Snapshot {
        /// A note saying what this snapshot captures.
        #[arg(short, long, default_value = "manuell")]
        message: String,
    },
    /// List snapshots.
    List,
    /// What changed since a snapshot (default: the latest).
    Changes {
        /// Snapshot id; defaults to the latest.
        id: Option<String>,
    },
    /// Compare two snapshots.
    Diff {
        a: String,
        b: String,
    },
    /// Restore a snapshot (takes a safety snapshot first).
    Restore {
        id: String,
    },
}

#[derive(Subcommand)]
enum PluginsCommand {
    /// Install a plugin from `github:<owner>/<repo>` or a local path.
    Install {
        /// e.g. github:dhiraj-salian/openclaw-nvidia-speech
        source: String,
    },
    /// List installed plugins.
    List,
    /// Remove an installed plugin.
    Remove {
        plugin: String,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Record which env var holds a provider's credential.
    Login {
        provider: String,
        /// Environment variable holding the credential.
        #[arg(long)]
        env_var: String,
    },
    /// Show configured providers. Never prints a credential.
    Status,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mode = Mode::parse(cli.json, cli.quiet);

    let outcome = dispatch(cli.command, cli.json).await;

    let (status, report, forced_exit) = outcome.into_parts();

    // --json is the only thing allowed on stdout in JSON mode. A failure
    // still renders as JSON there, so a script never has to parse prose.
    match (&status, mode.is_json()) {
        (_, true) | (Ok(_), _) => print!("{}", render(&report, mode)),
        (Err(verdict), false) => eprintln!("{verdict}"),
    }

    let verdict = match status {
        Ok(verdict) => verdict,
        Err(verdict) => verdict,
    };
    std::process::exit(forced_exit.unwrap_or_else(|| verdict.exit_code()));
}

async fn dispatch(command: Command, json_mode: bool) -> Outcome {
    match command {
        Command::Doctor => commands::doctor::run(&providers::registry()).await,
        Command::Providers => commands::providers::run(&providers::registry()),
        Command::Auth(auth) => commands::auth::run(auth),
        Command::Deploy {
            provider,
            size,
            disk,
            image,
            region,
            name,
        } => {
            commands::deploy::run(
                &providers::registry(),
                provider,
                size,
                disk,
                image,
                region,
                name,
            )
            .await
        }
        Command::Status { provider } => {
            commands::status::run(&providers::registry(), provider).await
        }
        Command::Snapshot {
            provider,
            id,
            label,
        } => commands::snapshot::run(&providers::registry(), provider, id, label).await,
        Command::Destroy { provider, id, yes } => {
            commands::destroy::run(&providers::registry(), provider, id, yes).await
        }
        Command::Models => commands::agent::models(&wiring::model_registry()).await,
        Command::Tools { policy } => {
            let parsed = Policy::parse(&policy);
            commands::agent::tools(&bkgclaw_core::tools::builtin_tools(), parsed)
        }
        Command::Run {
            task,
            model,
            max_turns,
            budget,
            policy,
            tools,
            no_tools,
        } => {
            commands::agent::run(
                &wiring::model_registry(),
                commands::agent::RunOptions {
                    prompt: task,
                    model,
                    max_turns,
                    budget_usd: budget,
                    policy: Policy::parse(&policy),
                    overrides: Overrides::parse(&tools),
                    no_tools,
                },
            )
            .await
        }
        Command::Chat {
            policy,
            model,
            session,
        } => {
            let code =
                commands::chat::run(bkgclaw_core::tools::Policy::parse(&policy), model, session)
                    .await
                    .unwrap_or(1);
            Outcome::success("chat finished").with_exit(code)
        }
        Command::Gateway { host, port } => commands::gateway::run(host, port, json_mode).await,
        Command::Tui {
            url,
            session,
            model,
            policy,
        } => {
            let code = bkgclaw_tui::run(bkgclaw_tui::TuiConfig::from_env(
                url, session, model, policy,
            ))
            .await
            .map(|_| 0)
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                3
            });
            Outcome::success("tui finished").with_exit(code)
        }
        Command::Init { mode } => commands::init::run(&mode),
        Command::Plugins(PluginsCommand::Install { source }) => commands::plugins::install(&source),
        Command::Plugins(PluginsCommand::List) => commands::plugins::list(),
        Command::Plugins(PluginsCommand::Remove { plugin }) => commands::plugins::remove(&plugin),
        Command::Versions(VersionsCommand::Init) => commands::versions::init(),
        Command::Versions(VersionsCommand::Snapshot { message }) => commands::versions::snapshot(&message),
        Command::Versions(VersionsCommand::List) => commands::versions::list(),
        Command::Versions(VersionsCommand::Changes { id }) => commands::versions::changes(id.as_deref()),
        Command::Versions(VersionsCommand::Diff { a, b }) => commands::versions::diff(&a, &b),
        Command::Versions(VersionsCommand::Restore { id }) => commands::versions::restore(&id),
        Command::Evolve(EvolveCommand::Init { origin }) => commands::evolve::init(origin.as_deref()),
        Command::Evolve(EvolveCommand::Run { goal, push, strict }) => {
            commands::evolve::run(&goal, push, strict).await
        }
        Command::Evolve(EvolveCommand::Attempts) => commands::evolve::attempts(),
        Command::Evolve(EvolveCommand::Fitness) => commands::evolve::fitness_report(),
        Command::Evolve(EvolveCommand::Push) => commands::evolve::push(),
        Command::ExitCodes => Outcome::from_report(Report::ok("exit code contract").with_data(
            serde_json::json!({
                "0": "ok",
                "1": "negative verdict (the command ran; the answer was no)",
                "2": "usage error (bad flag, missing argument)",
                "3": "environment error (no credential, no binary, no network)",
                "4": "internal error (unexpected; carries a class code)"
            }),
        )),
    }
}
