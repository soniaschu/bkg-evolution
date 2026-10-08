//! `bkgclaw providers` — what is registered and whether it is configured.

use crate::outcome::Outcome;
use bkgclaw_core::Registry;
use bkgclaw_ui::{Check, Report};

pub fn run(registry: &Registry) -> Outcome {
    if registry.names().is_empty() {
        return Outcome::from_report(Report::negative(
            "no providers registered",
            "no providers registered — this build ships none",
        ));
    }

    let mut rows = Vec::new();
    let mut checks = Vec::new();
    for name in registry.names() {
        let provider = registry.get(name).expect("name came from the registry");
        let needs = provider.required_env();
        let configured = needs
            .iter()
            .all(|var| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()));

        checks.push(Check::ok(
            name,
            "[REG]",
            if configured {
                format!("configured ({} env var(s))", needs.len())
            } else if needs.is_empty() {
                "no credential needed".to_string()
            } else {
                format!("not configured — needs {}", needs.join(", "))
            },
        ));

        rows.push(serde_json::json!({
            "name": name,
            "configured": configured,
            "required_env": needs,
            "needs_network": provider.needs_network(),
        }));
    }

    Outcome::from_report(Report::with_checks("registered providers", checks).with_data(rows))
}
