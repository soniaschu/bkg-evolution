//! Domain layer: what a caller can depend on without a terminal, a network or
//! a config file. Everything here is testable in isolation and forbidden from
//! doing I/O of its own.

#![forbid(unsafe_code)]

pub mod cost;
pub mod exit;
pub mod leak;
pub mod loop_engine;
pub mod models;
pub mod observer;
pub mod prompt;
pub mod provider;
pub mod providers_anthropic;
pub mod providers_google;
pub mod providers_ollama;
pub mod providers_openai;
pub mod router;
pub mod tools;
pub mod wiring;

pub use cost::{Budget, BudgetState, Complexity, CostTable, ModelPrice, Route, Usage};
pub use exit::Verdict;
pub use leak::{Detector, Finding};
pub use loop_engine::{
    LoopConfig, StopReason, ToolOutcome, Turn, run_agent_turns, run_agent_turns_with,
};
pub use models::{
    CompletionRequest, CompletionResponse, Message, ModelError, ModelId, ModelProvider,
    ModelRegistry, Retryability, ToolCall, ToolSpec, Vendor,
};
pub use observer::{DeltaSink, LoopObserver, NoObserver, NoSink, ObserverSink, StreamDelta};
pub use provider::{Instance, InstanceState, Provider, ProviderError, Registry, Spec};
pub use providers_anthropic::Anthropic;
pub use providers_google::{copilot, gemini};
pub use providers_ollama::Ollama;
pub use providers_openai::OpenAiCompatible;
pub use router::{Candidate, CircuitBreakers, CircuitState, RoutedCompletion, Router};
pub use tools::{Decision, Overrides, Policy, Risk, Tool, ToolRegistry};
pub use wiring::{default_breakers, default_chain, model_registry};

#[cfg(test)]
pub(crate) mod testing {
    use super::cost::{CostTable, Usage};
    use super::models::*;

    /// A model provider that returns a fixed response and never touches a
    /// network. Every routing test uses this.
    pub struct StubProvider {
        vendor: Vendor,
        models: Vec<ModelId>,
        costs: CostTable,
    }

    impl StubProvider {
        pub fn new(vendor: Vendor) -> Self {
            let models = vec![ModelId::new(vendor, "test-model")];
            StubProvider {
                vendor,
                models,
                costs: free_cost_table(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for StubProvider {
        fn vendor(&self) -> Vendor {
            self.vendor
        }
        fn models(&self) -> &[ModelId] {
            &self.models
        }
        fn credential_env(&self) -> Option<&'static str> {
            None
        }
        fn cost_table(&self) -> &CostTable {
            &self.costs
        }
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> Result<CompletionResponse, ModelError> {
            Ok(CompletionResponse {
                content: "stub".to_string(),
                tool_calls: vec![],
                usage: Usage::default(),
                model: self.models[0].clone(),
                truncated: false,
            })
        }
    }

    /// A zero-cost table so a stub never reports a spend.
    pub fn free_cost_table() -> CostTable {
        CostTable::new()
    }
}
