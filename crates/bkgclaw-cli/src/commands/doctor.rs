//! `bkgclaw doctor` — the environment check.
//!
//! Reads the provider registry for what each provider needs instead of
//! hard-coding a flag list. A provider registered tomorrow appears here
//! without this file changing.

use crate::outcome::Outcome;
use bkgclaw_core::Registry;
use bkgclaw_ui::{Check, Report};

pub async fn run(registry: &Registry) -> Outcome {
    let mut checks = Vec::new();

    for name in registry.names() {
        let provider = registry.get(name).expect("name came from the registry");
        for env_var in provider.required_env() {
            let label = format!("{name}.{env_var}");
            match std::env::var(env_var) {
                Ok(value) if !value.trim().is_empty() => {
                    // Presence is the only question. The value is never printed.
                    checks.push(Check::ok(&label, "[ENV]", "set"));
                }
                Ok(_) => checks.push(Check::fail(
                    &label,
                    "[ENV]",
                    "set but empty — an empty credential is not a credential",
                )),
                Err(_) => checks.push(Check::fail(
                    &label,
                    "[ENV]",
                    format!("unset — run `bkgclaw auth login {name} --env-var {env_var}`"),
                )),
            }
        }
    }

    checks.push(Check::ok("cli", "[BIN]", env!("CARGO_PKG_VERSION")));
    checks.push(Check::ok(
        "providers",
        "[REG]",
        format!(
            "{} registered: {}",
            registry.names().len(),
            if registry.names().is_empty() {
                "none".into()
            } else {
                registry.names().join(", ")
            }
        ),
    ));

    let report = Report::with_checks("environment", checks);
    // A failing doctor is a correct answer, not a crash: exit 1.
    Outcome::from_report(report)
}
