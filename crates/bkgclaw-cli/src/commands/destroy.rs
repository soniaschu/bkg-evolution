//! `bkgclaw destroy` — delete an instance.
//!
//! Destructive and irreversible, so `--yes` is required. Without it this is a
//! usage error (exit 2), not a confirmation prompt — a CLI that blocks on
//! input inside a pipeline is a CLI that hangs inside a pipeline.

use crate::commands::from_provider_error;
use crate::outcome::Outcome;
use bkgclaw_core::{Registry, Verdict};
use bkgclaw_ui::Report;

pub async fn run(registry: &Registry, provider: String, id: String, yes: bool) -> Outcome {
    if !yes {
        return Outcome::fail(Verdict::usage(
            "destroy is irreversible: re-run with --yes to confirm",
        ));
    }

    let provider_impl = match registry.resolve(&provider) {
        Ok(p) => p,
        Err(message) => return Outcome::fail(Verdict::usage(message)),
    };

    match provider_impl.destroy(&id).await {
        Ok(()) => Outcome::from_report(
            Report::ok(format!("destroyed {id} on {provider}")).with_data(serde_json::json!({
                "external_id": id,
                "provider": provider,
            })),
        ),
        Err(error) => from_provider_error(&provider, error),
    }
}
