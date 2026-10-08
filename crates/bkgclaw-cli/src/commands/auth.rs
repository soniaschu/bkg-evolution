//! `bkgclaw auth` — credential bookkeeping.
//!
//! Records the *pointer* to a credential (an env var name), never the value.
//! Reading a credential stays an explicit act at provider-call time.

use crate::outcome::Outcome;
use bkgclaw_config::Config;
use bkgclaw_core::Verdict;
use bkgclaw_ui::{Check, Report};

use crate::AuthCommand;

pub fn run(command: AuthCommand) -> Outcome {
    match command {
        AuthCommand::Login { provider, env_var } => login(provider, env_var),
        AuthCommand::Status => status(),
    }
}

fn login(provider: String, env_var: String) -> Outcome {
    // Validate before storing: a typo here means the credential silently
    // never resolves, and the failure surfaces much later as a 401.
    let valid = !env_var.is_empty()
        && env_var
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Outcome::fail(Verdict::usage(format!(
            "`{env_var}` is not a valid environment variable name (expected UPPER_SNAKE_CASE)"
        )));
    }

    // A malformed config must not look like an empty one. Propagate the error
    // rather than silently starting from defaults — otherwise a typo in the
    // file is indistinguishable from a fresh install, and the user's settings
    // appear to have been ignored.
    let mut config = match Config::load() {
        Ok(config) => config,
        Err(error) => return Outcome::fail(Verdict::usage(error.to_string())),
    };
    config
        .credential_env
        .insert(provider.clone(), env_var.clone());
    if !config.settings.configured.contains(&provider) {
        config.settings.configured.push(provider.clone());
        config.settings.configured.sort();
    }

    if let Err(error) = config.save() {
        return Outcome::fail(Verdict::environment(error.to_string()));
    }

    let present = std::env::var(&env_var).is_ok_and(|v| !v.trim().is_empty());
    let check = if present {
        Check::ok(&env_var, "[ENV]", "credential present")
    } else {
        // Recording the pointer succeeded; the value is simply not set yet.
        // That is a real finding, not a success line.
        Check::fail(
            &env_var,
            "[ENV]",
            format!("recorded but not set — export {env_var}=… before the first run"),
        )
    };

    Outcome::from_report(
        Report::with_checks(format!("{provider} now reads from {env_var}"), vec![check]).with_data(
            serde_json::json!({
                "provider": provider,
                "env_var": env_var,
                "present": present,
            }),
        ),
    )
}

fn status() -> Outcome {
    let config = match Config::load() {
        Ok(config) => config,
        // Distinguish "no config yet" (fine) from "config is broken" (exit 2).
        Err(error) => return Outcome::fail(Verdict::usage(error.to_string())),
    };
    if config.credential_env.is_empty() {
        return Outcome::from_report(Report::negative(
            "no credentials configured",
            "run `bkgclaw auth login <provider> --env-var <VAR>`",
        ));
    }

    let mut checks = Vec::new();
    let mut rows = Vec::new();
    for (provider, env_var) in &config.credential_env {
        let present = std::env::var(env_var).is_ok_and(|v| !v.trim().is_empty());
        checks.push(if present {
            Check::ok(&format!("{provider}.{env_var}"), "[AUTH]", "configured")
        } else {
            Check::fail(
                &format!("{provider}.{env_var}"),
                "[AUTH]",
                "env var not set",
            )
        });
        // Only the pointer is printed. Not the value, not its length.
        rows.push(serde_json::json!({
            "provider": provider,
            "env_var": env_var,
            "present": present,
        }));
    }

    Outcome::from_report(Report::with_checks("credential configuration", checks).with_data(rows))
}
