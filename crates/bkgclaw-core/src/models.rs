//! Model providers: the LLM side of the platform.
//!
//! Named `ModelProvider`, not `Provider` — `core::Provider` is already the
//! cloud-infra trait (droplets, snapshots). Two different things with two
//! different failure modes.
//!
//! The point of the trait is that adding a provider is one impl. The agent loop
//! never learns a provider's name; it resolves through `Registry` and gets back
//! something it can `complete()`.

use serde::{Deserialize, Serialize};

use crate::cost::{CostTable, Usage};

/// Which provider serves a request. Kept as a data type rather than a string
/// so routing decisions are type-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Vendor {
    Anthropic,
    OpenAi,
    Google,
    DeepSeek,
    Xai,
    Ollama,
    OpenRouter,
    /// Any endpoint that speaks the OpenAI wire format.
    Compatible,
    Copilot,
    /// An NVIDIA-NIM gateway: OpenAI wire format, NVIDIA model catalogue
    /// behind an operator-provided endpoint.
    Nim,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::OpenAi => "openai",
            Vendor::Google => "google",
            Vendor::DeepSeek => "deepseek",
            Vendor::Xai => "xai",
            Vendor::Ollama => "ollama",
            Vendor::OpenRouter => "openrouter",
            Vendor::Compatible => "compatible",
            Vendor::Copilot => "copilot",
            Vendor::Nim => "nim",
        }
    }
}

/// A fully-qualified model identifier: vendor plus model name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId {
    pub vendor: Vendor,
    pub model: String,
}

impl ModelId {
    pub fn new(vendor: Vendor, model: &str) -> Self {
        ModelId {
            vendor,
            model: model.to_string(),
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let (vendor, model) = raw.split_once('/')?;
        let vendor = match vendor {
            "anthropic" => Vendor::Anthropic,
            "openai" => Vendor::OpenAi,
            "google" | "gemini" => Vendor::Google,
            "deepseek" => Vendor::DeepSeek,
            "xai" | "grok" => Vendor::Xai,
            "ollama" => Vendor::Ollama,
            "openrouter" => Vendor::OpenRouter,
            "copilot" => Vendor::Copilot,
            // NIM model names already start with `nvidia/`, so the vendor
            // prefix of a full id is `nim/…`: `nim/nvidia/nemotron-…`.
            "nim" | "nvidia" | "nvidia-nim" => Vendor::Nim,
            _ => Vendor::Compatible,
        };
        Some(ModelId {
            vendor,
            model: model.to_string(),
        })
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.vendor.as_str(), self.model)
    }
}

/// Roles as the OpenAI-style wire format uses them. The Anthropic API calls the
/// last one `assistant` and the system block separate; the adapter translates.
///
/// Internally tagged (`{"role":"user","content":"…"}`): the wire shape every
/// chat client already speaks, and one less nesting level than the adjacent
/// form serde would produce for single-field variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "role")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
    },
    ToolResult {
        content: String,
        tool_call_id: String,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message::System {
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Message::User {
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Message::Assistant {
            content: content.into(),
        }
    }
    pub fn tool_result(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Message::ToolResult {
            content: content.into(),
            tool_call_id: tool_call_id.into(),
        }
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// What the agent sends.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionRequest {
    pub model: ModelId,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Provider-specific extended-thinking budget, where supported.
    pub thinking_budget: Option<u32>,
    /// Reasoning effort. Anthropic and o1-style models accept this; others
    /// ignore it. Preferred over a thinking budget on current models.
    pub effort: Option<String>,
}

/// A tool invocation the model requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub model: ModelId,
    /// Set when the provider stopped because it hit a token or time budget.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The caller cannot fix this by retrying the same call.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The credential is absent or rejected. Retry will not help.
    #[error("authentication failed for {vendor}: {reason}")]
    Auth { vendor: Vendor, reason: String },
    /// The vendor is down or throttling. Retry may help, later.
    #[error("{vendor} unavailable: {reason}")]
    Unavailable { vendor: Vendor, reason: String },
    /// The request exceeded the budget guard. Must not be retried.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),
    /// Rate limited; carries the retry hint when the vendor gave one.
    #[error("rate limited by {vendor}: {reason}")]
    RateLimited { vendor: Vendor, reason: String },
}

/// Whether a failure is worth retrying against the *same* vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    /// Same vendor, later.
    SameVendor,
    /// Fall through to the next vendor in the chain.
    NextVendor,
    /// Give up.
    Fatal,
}

impl ModelError {
    pub fn retryability(&self) -> Retryability {
        match self {
            // Auth will never succeed on retry; move to a vendor that has a
            // working credential rather than hammering the broken one.
            ModelError::Auth { .. } => Retryability::NextVendor,
            ModelError::Unavailable { .. } | ModelError::RateLimited { .. } => {
                Retryability::SameVendor
            }
            ModelError::BudgetExceeded(_) => Retryability::Fatal,
            ModelError::InvalidRequest(_) => Retryability::NextVendor,
        }
    }

    pub fn vendor(&self) -> Option<Vendor> {
        match self {
            ModelError::Auth { vendor, .. }
            | ModelError::Unavailable { vendor, .. }
            | ModelError::RateLimited { vendor, .. } => Some(*vendor),
            _ => None,
        }
    }
}

/// What a model provider must do. Four methods, so a real HTTP implementation
/// and a deterministic test double are both small.
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    fn vendor(&self) -> Vendor;
    fn models(&self) -> &[ModelId];
    /// Env var holding this provider's credential. `None` means no credential
    /// is needed (a local Ollama, for instance).
    fn credential_env(&self) -> Option<&'static str>;

    /// Whether reaching this provider requires network I/O. `doctor` reports
    /// a local provider as ready without probing anything.
    fn needs_network(&self) -> bool {
        true
    }
    fn cost_table(&self) -> &CostTable;

    async fn complete(&self, request: &CompletionRequest)
    -> Result<CompletionResponse, ModelError>;

    /// Streaming variant. Receives text and reasoning deltas as they arrive
    /// and returns the same response `complete` would.
    ///
    /// The default implementation is deliberately *not* fake streaming: a
    /// provider that cannot stream calls `complete` and delivers the whole
    /// text as one delta, so callers never need to know the difference.
    /// Real deltas arrive only from providers that really stream.
    async fn complete_streamed(
        &self,
        request: &CompletionRequest,
        sink: &dyn crate::observer::DeltaSink,
    ) -> Result<CompletionResponse, ModelError> {
        let response = self.complete(request).await?;
        if !response.content.is_empty() {
            sink.text(&response.content);
        }
        Ok(response)
    }
}

/// Registry of model providers, resolved by vendor.
#[derive(Default)]
pub struct ModelRegistry {
    providers: Vec<Box<dyn ModelProvider>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Box<dyn ModelProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    pub fn for_vendor(&self, vendor: Vendor) -> Option<&dyn ModelProvider> {
        self.providers
            .iter()
            .find(|p| p.vendor() == vendor)
            .map(|p| p.as_ref())
    }

    pub fn vendors(&self) -> Vec<Vendor> {
        self.providers.iter().map(|p| p.vendor()).collect()
    }

    /// Every registered (vendor, model) pair. This is what the CLI prints.
    ///
    /// `override` replaces the provider's declared list, which lets a local
    /// provider report what the daemon actually has installed rather than a
    /// list hard-coded at build time. Without it the declared list is used.
    pub fn catalog_with(&self, discovered: Option<&[ModelId]>) -> Vec<(ModelId, bool)> {
        let mut out = Vec::new();
        for provider in &self.providers {
            let has_credential = match provider.credential_env() {
                Some(var) => std::env::var(var).is_ok_and(|v| !v.trim().is_empty()),
                None => true,
            };
            // The override applies to exactly one vendor: the one the caller
            // discovered at runtime.
            let models: Vec<ModelId> = match discovered {
                Some(found) if found.first().map(|m| m.vendor) == Some(provider.vendor()) => {
                    found.to_vec()
                }
                _ => provider.models().to_vec(),
            };
            for model in models {
                out.push((model, has_credential));
            }
        }
        out.sort_by_key(|(model, _)| model.to_string());
        out
    }

    pub fn catalog(&self) -> Vec<(ModelId, bool)> {
        self.catalog_with(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_round_trip_through_strings() {
        let id = ModelId::new(Vendor::Anthropic, "claude-sonnet-5");
        assert_eq!(id.to_string(), "anthropic/claude-sonnet-5");
        assert_eq!(ModelId::parse("anthropic/claude-sonnet-5"), Some(id));
    }

    #[test]
    fn an_unknown_vendor_prefix_becomes_compatible() {
        // Any OpenAI-compatible endpoint is a first-class case, not an error.
        let id = ModelId::parse("my-local-host/custom-model").unwrap();
        assert_eq!(id.vendor, Vendor::Compatible);
        assert_eq!(id.model, "custom-model");
    }

    #[test]
    fn a_bare_string_without_a_slash_is_not_a_model_id() {
        assert_eq!(ModelId::parse("claude-sonnet-5"), None);
    }

    #[test]
    fn auth_failures_move_to_the_next_vendor_rather_than_retrying() {
        // Retrying a bad key forever is how a loop burns a budget for nothing.
        let error = ModelError::Auth {
            vendor: Vendor::OpenAi,
            reason: "401".into(),
        };
        assert_eq!(error.retryability(), Retryability::NextVendor);
    }

    #[test]
    fn a_budget_stop_is_fatal() {
        let error = ModelError::BudgetExceeded("daily cap reached".into());
        assert_eq!(
            error.retryability(),
            Retryability::Fatal,
            "a budget guard must not be worked around by a retry"
        );
    }

    #[test]
    fn rate_limits_are_retryable_against_the_same_vendor() {
        let error = ModelError::RateLimited {
            vendor: Vendor::Anthropic,
            reason: "429".into(),
        };
        assert_eq!(error.retryability(), Retryability::SameVendor);
        assert_eq!(error.vendor(), Some(Vendor::Anthropic));
    }

    #[test]
    fn the_registry_finds_a_provider_by_vendor() {
        let mut registry = ModelRegistry::new();
        registry.register(Box::new(crate::testing::StubProvider::new(Vendor::Ollama)));
        assert!(registry.for_vendor(Vendor::Ollama).is_some());
        assert!(registry.for_vendor(Vendor::Xai).is_none());
        assert_eq!(registry.vendors(), vec![Vendor::Ollama]);
    }

    #[test]
    fn the_catalog_reports_credential_availability() {
        let mut registry = ModelRegistry::new();
        registry.register(Box::new(crate::testing::StubProvider::new(Vendor::Ollama)));
        registry.register(Box::new(crate::testing::StubProvider::new(
            Vendor::Anthropic,
        )));
        let catalog = registry.catalog();
        assert_eq!(catalog.len(), 2);
        // Ollama needs no credential; Anthropic does and none is set here.
        let ollama = catalog
            .iter()
            .find(|(m, _)| m.vendor == Vendor::Ollama)
            .unwrap();
        assert!(
            ollama.1,
            "a credential-free provider must be marked available"
        );
    }
}
