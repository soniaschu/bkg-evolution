//! The model registry the gateway and its clients actually use.
//!
//! Lives in core — not in the gateway — because the CLI, the TUI and the
//! evolve engine all need exactly this wiring, and none of them should
//! depend on the daemon for it.

//! The model registry the gateway and its clients actually use.
//!
//! Providers are registered by credential presence, so `bkgclaw models`
//! describes the machine it runs on. A provider with no credential is not
//! registered at all, which means the router can never pick a candidate that
//! would fail on its first call.

use std::time::Duration;

use crate::models::{ModelId, ModelRegistry, Vendor};
use crate::router::{Candidate, CircuitBreakers};
use crate::{Anthropic, Ollama, OpenAiCompatible, copilot, gemini};

/// Every provider this build can talk to.
///
/// Local first: it needs no credential, so a fresh machine has a working model.
/// Each hosted provider registers only when its credential is present.
pub fn model_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Box::new(Ollama::new()));

    if let Some(key) = credential("ANTHROPIC_API_KEY") {
        registry.register(Box::new(Anthropic::new(key)));
    }
    if let Some(key) = credential("OPENAI_API_KEY") {
        registry.register(Box::new(OpenAiCompatible::openai(key)));
    }
    if let Some(key) = credential("DEEPSEEK_API_KEY") {
        registry.register(Box::new(OpenAiCompatible::deepseek(key)));
    }
    if let Some(key) = credential("XAI_API_KEY") {
        registry.register(Box::new(OpenAiCompatible::xai(key)));
    }
    if let Some(key) = credential("OPENROUTER_API_KEY") {
        registry.register(Box::new(OpenAiCompatible::openrouter(key)));
    }
    if let Some(key) = credential("GEMINI_API_KEY") {
        registry.register(Box::new(gemini(key)));
    }
    // The operator's NVIDIA-NIM gateway: designated primary, so the chain
    // below must put it first when the key is present.
    if let Some(key) = credential("NIM_API_KEY") {
        let mut nim = OpenAiCompatible::nvidia_nim(key);
        if let Some(base) = credential("NIM_BASE_URL") {
            nim = nim.with_endpoint(base);
        }
        registry.register(Box::new(nim));
    }
    if let Some(token) = credential("GITHUB_TOKEN") {
        registry.register(Box::new(copilot(token)));
    }
    // An OpenAI-compatible endpoint for a gateway or a self-hosted vLLM.
    if let (Some(base), Some(key)) = (
        std::env::var("BKGCLAW_COMPATIBLE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        credential("BKGCLAW_COMPATIBLE_KEY"),
    ) {
        let model =
            std::env::var("BKGCLAW_COMPATIBLE_MODEL").unwrap_or_else(|_| "local-model".to_string());
        registry.register(Box::new(OpenAiCompatible::compatible(base, key, &model)));
    }

    registry
}

fn credential(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// What a local Ollama actually has installed.
///
/// A hard-coded list of Ollama models is a guess that is wrong on every machine
/// but the author's. Asking the daemon is one HTTP call and makes
/// `bkgclaw models` tell the truth about *this* machine.
pub async fn installed_ollama_models() -> Vec<String> {
    let base = std::env::var("OLLAMA_HOST")
        .map(|h| h.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return Vec::new();
    };
    let Ok(response) = client.get(format!("{base}/api/tags")).send().await else {
        return Vec::new();
    };
    let Ok(payload) = response.json::<serde_json::Value>().await else {
        return Vec::new();
    };

    payload["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["name"].as_str())
                // Embedding and rerank models are not chat models; offering
                // one as a completion target produces a confusing failure.
                .filter(|name| {
                    !name.contains("embed") && !name.contains("nomic") && !name.contains("rerank")
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn costs(input: f64, output: f64) -> crate::ModelPrice {
    crate::ModelPrice {
        input,
        output,
        cache_read: input * 0.1,
        cache_write: input * 1.25,
    }
}

/// The failover chain, cheapest usable model first.
///
/// Built from what is actually registered, so every entry can serve a request.
/// Ascending price: an expensive model ahead of a cheap one wastes money on
/// every turn that could have used the cheap one.
pub async fn default_chain() -> Vec<Candidate> {
    let registry = model_registry();
    let mut chain: Vec<Candidate> = Vec::new();

    // The NIM gateway is operator-designated as the primary: capable models
    // first, ahead of everything else, when the key is configured. Only the
    // tool-capable, live-verified models (see `OpenAiCompatible::NIM_CHAIN`)
    // enter the chain; the text-only responders stay switchable via `/model`.
    if registry.for_vendor(Vendor::Nim).is_some() {
        for model in crate::OpenAiCompatible::NIM_CHAIN {
            push_unique(
                &mut chain,
                Candidate::new(ModelId::new(Vendor::Nim, model), costs(0.0, 0.0)),
            );
        }
    }

    // Local models: free.
    for model in installed_ollama_models().await {
        push_unique(
            &mut chain,
            Candidate::new(ModelId::new(Vendor::Ollama, &model), costs(0.0, 0.0)),
        );
    }
    if chain.is_empty() {
        // The daemon did not answer. Keep a nominal entry so the chain is not
        // empty; the router will fail over honestly rather than refuse outright.
        push_unique(
            &mut chain,
            Candidate::new(ModelId::new(Vendor::Ollama, "llama3.2"), costs(0.0, 0.0)),
        );
    }

    // A configured gateway is usually free too.
    if credential("BKGCLAW_COMPATIBLE_KEY").is_some() {
        if let Ok(model) = std::env::var("BKGCLAW_COMPATIBLE_MODEL") {
            push_unique(
                &mut chain,
                Candidate::new(ModelId::new(Vendor::Compatible, &model), costs(0.0, 0.0)),
            );
        }
    }

    // Ascending input price across the hosted providers.
    for (vendor, model, price) in [
        (Vendor::DeepSeek, "deepseek-chat", costs(0.27, 1.10)),
        (Vendor::OpenRouter, "openrouter/auto", costs(1.50, 6.00)),
        (Vendor::OpenAi, "gpt-4o-mini", costs(0.15, 0.60)),
        (Vendor::Google, "gemini-2.5-flash", costs(0.30, 2.50)),
        (Vendor::Copilot, "claude-haiku-4-5", costs(1.00, 5.00)),
        (Vendor::Anthropic, "claude-haiku-4-5", costs(1.00, 5.00)),
        (Vendor::Xai, "grok-2-mini", costs(0.30, 0.50)),
        (Vendor::Anthropic, "claude-sonnet-5-5", costs(2.00, 10.00)),
    ] {
        if registry.for_vendor(vendor).is_some() {
            push_unique(
                &mut chain,
                Candidate::new(ModelId::new(vendor, model), price),
            );
        }
    }

    // Keep the cheap ones first; two entries share a price and order is stable
    // because the array above is walked in order.
    chain.sort_by(|a, b| {
        a.price
            .input
            .partial_cmp(&b.price.input)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    chain
}

fn push_unique(chain: &mut Vec<Candidate>, candidate: Candidate) {
    if !chain.iter().any(|c| c.model == candidate.model) {
        chain.push(candidate);
    }
}

/// Breakers tuned for an agent loop: fail fast, recover soon. A short task
/// should not spend three of its turns waiting on one dead provider.
pub fn default_breakers() -> CircuitBreakers {
    CircuitBreakers::new(2, Duration::from_secs(30))
}

/// Whether the given vendor has a usable credential right now.
#[cfg_attr(not(test), allow(dead_code))]
pub fn vendor_ready(registry: &ModelRegistry, vendor: Vendor) -> bool {
    match registry.for_vendor(vendor) {
        None => false,
        Some(provider) => match provider.credential_env() {
            Some(var) => credential(var).is_some(),
            None => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_is_always_registered_because_it_needs_no_credential() {
        let registry = model_registry();
        assert!(registry.for_vendor(Vendor::Ollama).is_some());
        assert!(vendor_ready(&registry, Vendor::Ollama));
    }

    #[test]
    fn a_provider_is_registered_exactly_when_its_credential_is_present() {
        // Whatever the machine has, registry and vendor_ready must agree.
        let registry = model_registry();
        for (vendor, var) in [
            (Vendor::Anthropic, "ANTHROPIC_API_KEY"),
            (Vendor::OpenAi, "OPENAI_API_KEY"),
            (Vendor::DeepSeek, "DEEPSEEK_API_KEY"),
            (Vendor::Xai, "XAI_API_KEY"),
            (Vendor::OpenRouter, "OPENROUTER_API_KEY"),
            (Vendor::Google, "GEMINI_API_KEY"),
            (Vendor::Copilot, "GITHUB_TOKEN"),
            (Vendor::Nim, "NIM_API_KEY"),
        ] {
            let present = credential(var).is_some();
            assert_eq!(
                registry.for_vendor(vendor).is_some(),
                present,
                "{vendor} registration disagrees with {var}"
            );
        }
    }

    #[tokio::test]
    async fn the_chain_starts_with_a_free_model() {
        let chain = default_chain().await;
        assert_eq!(chain[0].price.input, 0.0, "the first hop must be free");
    }

    #[tokio::test]
    async fn the_chain_is_ordered_cheapest_first() {
        let prices: Vec<f64> = default_chain()
            .await
            .iter()
            .map(|c| c.price.input)
            .collect();
        let mut sorted = prices.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            prices, sorted,
            "an expensive model before a cheap one wastes money"
        );
    }

    #[tokio::test]
    async fn the_chain_never_holds_an_unregistered_model() {
        // A candidate the registry cannot serve fails on the first call and
        // again on the next turn, forever.
        let registry = model_registry();
        for candidate in default_chain().await {
            assert!(
                registry.for_vendor(candidate.model.vendor).is_some(),
                "{} is in the chain but not registered",
                candidate.model
            );
        }
    }

    #[tokio::test]
    async fn the_chain_has_no_duplicates() {
        let chain = default_chain().await;
        let mut ids: Vec<String> = chain.iter().map(|c| c.model.to_string()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "the chain holds the same model twice");
    }

    #[tokio::test]
    async fn without_anthropic_the_chain_names_no_claude_model() {
        if credential("ANTHROPIC_API_KEY").is_none() {
            for candidate in default_chain().await {
                assert_ne!(candidate.model.vendor, Vendor::Anthropic);
            }
        }
    }

    #[tokio::test]
    async fn the_chain_puts_nim_first_when_configured() {
        // The gateway is operator-designated as primary; its models must
        // never sort behind the local fallback when the key is present.
        let registry = model_registry();
        if registry.for_vendor(Vendor::Nim).is_none() {
            return; // not configured on this machine; nothing to assert
        }
        let chain = default_chain().await;
        assert!(!chain.is_empty());
        assert_eq!(
            chain[0].model.vendor,
            Vendor::Nim,
            "NIM must lead the chain"
        );
        for model in crate::OpenAiCompatible::NIM_CHAIN {
            assert!(
                chain.iter().any(|c| c.model.model == *model),
                "{model} missing from the chain"
            );
        }
    }

    #[tokio::test]
    async fn the_chain_holds_only_verified_nim_models() {
        // Every NIM chain entry must be one of the live-verified models; a
        // listed-but-dead model in the chain would fail on its first call.
        let chain = default_chain().await;
        for candidate in chain.iter().filter(|c| c.model.vendor == Vendor::Nim) {
            assert!(
                crate::OpenAiCompatible::NIM_MODELS
                    .contains(&candidate.model.model.as_str()),
                "{} is in the chain but was never verified to respond",
                candidate.model
            );
        }
    }

    #[tokio::test]
    async fn the_chain_is_never_empty() {
        // Even with no daemon and no credentials, the chain holds a nominal
        // local entry; failing over from nothing is worse than trying.
        assert!(!default_chain().await.is_empty());
    }

    #[test]
    fn the_breakers_fail_fast() {
        assert_eq!(default_breakers().failure_threshold, 2);
        assert_eq!(default_breakers().cooldown, Duration::from_secs(30));
    }
}
