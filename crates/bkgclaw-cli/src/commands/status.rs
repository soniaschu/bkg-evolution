//! `bkgclaw status` — list instances.

use crate::commands::from_provider_error;
use crate::outcome::Outcome;
use bkgclaw_core::{Registry, Verdict};
use bkgclaw_ui::Report;

pub async fn run(registry: &Registry, provider: String) -> Outcome {
    let provider_impl = match registry.resolve(&provider) {
        Ok(p) => p,
        Err(message) => return Outcome::fail(Verdict::usage(message)),
    };

    match provider_impl.list().await {
        Ok(instances) => {
            let rows: Vec<_> = instances
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "name": i.name,
                        "state": i.state.as_str(),
                        "address": i.address,
                        "region": i.region,
                        "external_id": i.external_id,
                    })
                })
                .collect();
            Outcome::from_report(
                Report::ok(format!("{} instance(s) on {provider}", instances.len()))
                    .with_data(rows),
            )
        }
        Err(error) => from_provider_error(&provider, error),
    }
}
