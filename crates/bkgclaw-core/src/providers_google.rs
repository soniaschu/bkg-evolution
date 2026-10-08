//! Google Gemini and GitHub Copilot.
//!
//! Both are thin adapters rather than new wire formats:
//!
//! - Gemini is reachable through its OpenAI-compatibility endpoint, so it is a
//!   configured `OpenAiCompatible` rather than a second implementation of the
//!   same parsing code.
//! - Copilot exposes an Anthropic-compatible Messages endpoint for subscribers,
//!   so it is the Anthropic adapter pointed at a different base URL and token
//!   header.

use crate::cost::{CostTable, ModelPrice};
use crate::models::{ModelId, Vendor};

use super::providers_anthropic::Anthropic;
use super::providers_openai::OpenAiCompatible;

fn prices_gemini() -> CostTable {
    CostTable::new()
        .with(
            "gemini-2.5-flash",
            ModelPrice {
                input: 0.30,
                output: 2.50,
                ..ModelPrice::default()
            },
        )
        .with(
            "gemini-2.5-pro",
            ModelPrice {
                input: 1.25,
                output: 10.0,
                ..ModelPrice::default()
            },
        )
        .with_fallback(ModelPrice {
            input: 0.30,
            output: 2.50,
            ..ModelPrice::default()
        })
}

/// Gemini via its OpenAI-compatible endpoint.
///
/// One code path rather than a second parser: Gemini accepts the Chat
/// Completions shape at `/v1beta/openai/`, so the compatibility adapter is
/// exactly the right tool.
pub fn gemini(api_key: impl Into<String>) -> OpenAiCompatible {
    let mut provider = OpenAiCompatible::compatible(
        "https://generativelanguage.googleapis.com/v1beta/openai",
        api_key,
        "gemini-2.5-flash",
    );
    // Re-tag as Google so the registry and cost accounting see the right vendor.
    provider.set_vendor(Vendor::Google);
    provider.set_models(&["gemini-2.5-flash", "gemini-2.5-pro"]);
    provider.set_costs(prices_gemini());
    provider
}

/// GitHub Copilot for subscribers.
///
/// Copilot proxies Claude through an Anthropic-shaped Messages endpoint. The
/// token comes from the Copilot subscription, not from an Anthropic account.
pub fn copilot(token: impl Into<String>) -> Anthropic {
    let mut provider =
        Anthropic::new(token).with_endpoint("https://api.githubcopilot.com/v1/messages");
    provider.set_vendor(Vendor::Copilot);
    provider.set_models(&["claude-sonnet-5-5", "claude-haiku-4-5"]);
    provider
}

/// The model ids each provider should list, for `bkgclaw models`.
pub fn google_models() -> Vec<ModelId> {
    vec![
        ModelId::new(Vendor::Google, "gemini-2.5-flash"),
        ModelId::new(Vendor::Google, "gemini-2.5-pro"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelProvider;

    #[test]
    fn gemini_reports_the_google_vendor_not_compatible() {
        // Otherwise it collides with a local gateway in the registry and in
        // `bkgclaw models`.
        assert_eq!(gemini("k").vendor(), Vendor::Google);
    }

    #[test]
    fn gemini_uses_the_openai_compatibility_endpoint() {
        let provider = gemini("k");
        assert!(provider.base_url().ends_with("/v1beta/openai"));
    }

    #[test]
    fn gemini_declares_the_gemini_credential_variable() {
        assert_eq!(gemini("k").credential_env(), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn gemini_lists_its_own_models() {
        let names: Vec<String> = gemini("k")
            .models()
            .iter()
            .map(|m| m.model.clone())
            .collect();
        assert!(
            names.contains(&"gemini-2.5-flash".to_string()),
            "got {names:?}"
        );
    }

    #[test]
    fn copilot_uses_the_github_endpoint_and_vendor() {
        let provider = copilot("token");
        assert_eq!(provider.vendor(), Vendor::Copilot);
        assert!(provider.endpoint_url().contains("githubcopilot.com"));
    }

    #[test]
    fn copilot_declares_the_github_credential_variable() {
        assert_eq!(copilot("t").credential_env(), Some("GITHUB_TOKEN"));
    }

    #[test]
    fn copilot_is_priced_like_the_models_it_serves() {
        // Copilot bills through a subscription, but the token cost still drives
        // the routing decision, so a price must exist.
        let provider = copilot("t");
        let usage = crate::cost::Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert!(
            provider
                .cost_table()
                .cost_for("claude-sonnet-5-5", &usage)
                .unwrap()
                > 0.0
        );
    }

    #[test]
    fn every_google_model_has_a_price() {
        let usage = crate::cost::Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        let provider = gemini("k");
        for model in google_models() {
            assert!(
                provider
                    .cost_table()
                    .cost_for(&model.model, &usage)
                    .unwrap()
                    > 0.0,
                "{model} unpriced"
            );
        }
    }
}
