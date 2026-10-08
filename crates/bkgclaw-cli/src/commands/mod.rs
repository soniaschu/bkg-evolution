//! One module per command. Each returns an `Outcome` and never prints.

pub mod agent;
pub mod auth;
pub mod chat;
pub mod deploy;
pub mod destroy;
pub mod evolve;
pub mod doctor;
pub mod gateway;
pub mod init;
pub mod plugins;
pub mod providers;
pub mod snapshot;
pub mod status;
pub mod versions;

use crate::outcome::Outcome;
use bkgclaw_core::Verdict;

/// Turn a provider error into a verdict. The mapping is the contract:
/// a missing credential is an environment problem (exit 3), a rejected
/// request is a negative verdict (exit 1), and neither is "internal".
pub fn from_provider_error(provider: &str, error: bkgclaw_core::ProviderError) -> Outcome {
    use bkgclaw_core::ProviderError as E;
    let verdict = match error {
        E::MissingCredential(_) | E::Unreachable { .. } => Verdict::environment(error.to_string()),
        E::Rejected { .. } => Verdict::negative(format!("{provider}: {error}")),
        E::NotFound(_) => Verdict::negative(error.to_string()),
    };
    Outcome::fail(verdict)
}
