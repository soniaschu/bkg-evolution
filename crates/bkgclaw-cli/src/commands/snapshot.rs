//! `bkgclaw snapshot` — create a labelled snapshot.

use crate::commands::from_provider_error;
use crate::outcome::Outcome;
use bkgclaw_core::{Registry, Verdict};
use bkgclaw_ui::Report;

pub async fn run(registry: &Registry, provider: String, id: String, label: String) -> Outcome {
    let provider_impl = match registry.resolve(&provider) {
        Ok(p) => p,
        Err(message) => return Outcome::fail(Verdict::usage(message)),
    };

    match provider_impl.snapshot(&id, &label).await {
        Ok(snapshot_id) => Outcome::from_report(
            Report::ok(format!("snapshot {snapshot_id} created")).with_data(serde_json::json!({
                "snapshot_id": snapshot_id,
                "source": id,
                "label": label,
            })),
        ),
        Err(error) => from_provider_error(&provider, error),
    }
}
