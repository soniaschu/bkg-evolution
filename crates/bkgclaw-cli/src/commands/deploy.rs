//! `bkgclaw deploy` — create an instance.
//!
//! Seven flags, regardless of how many providers exist. The templates needed
//! 306 because each provider re-declared every credential; here the provider is
//! a `--provider` value and credentials live under `auth`.

use crate::commands::from_provider_error;
use crate::outcome::Outcome;
use bkgclaw_core::{Registry, Spec, Verdict};
use bkgclaw_ui::Report;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    registry: &Registry,
    provider: String,
    size: String,
    disk: u32,
    image: String,
    region: String,
    name: Option<String>,
) -> Outcome {
    let provider_impl = match registry.resolve(&provider) {
        Ok(p) => p,
        Err(message) => return Outcome::fail(Verdict::usage(message)),
    };

    if disk == 0 {
        return Outcome::fail(Verdict::usage("--disk must be greater than zero"));
    }

    let spec = Spec {
        name: name.unwrap_or_else(|| format!("bkgclaw-{region}")),
        region,
        image,
        size,
        disk_gb: disk,
        tags: vec!["bkgclaw".to_string()],
    };

    match provider_impl.create(&spec).await {
        Ok(instance) => Outcome::from_report(
            Report::ok(format!("created {}", instance.name)).with_data(serde_json::json!({
                "id": instance.id,
                "name": instance.name,
                "external_id": instance.external_id,
                "address": instance.address,
                "state": instance.state.as_str(),
                "region": instance.region,
            })),
        ),
        Err(error) => from_provider_error(&provider, error),
    }
}
