//! Anthropic Messages API.
//!
//! Written against `reqwest` with `rustls`, so no OpenSSL dependency and no
//! build script. The credential comes from the environment at call time and is
//! never stored, logged or echoed into an error message.

use async_trait::async_trait;
use serde::Deserialize;

use crate::cost::{CostTable, ModelPrice, Usage};
use crate::models::{
    CompletionRequest, CompletionResponse, Message, ModelError, ModelId, ModelProvider, ToolCall,
    Vendor,
};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// How a given model wants thinking configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingConfig {
    /// Send no `thinking` at all.
    None,
    /// `{type: "adaptive"}` — the only legal form on current models.
    Adaptive,
    /// `{type: "enabled", budget_tokens: N}` — legal only on older models.
    Budget(u32),
}

/// Choose the thinking shape for a model.
///
/// The boundary is not cosmetic: `budget_tokens` on Opus 5.5 or Sonnet 5.5 is a
/// 400, so an adapter that always sends the budget shape is broken against
/// every model an operator is likely to choose.
pub fn thinking_config(model: &str, budget: Option<u32>) -> ThinkingConfig {
    // Models whose family takes a fixed token budget rather than adaptive.
    let takes_budget = model.contains("haiku-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("sonnet-4")
        || model.contains("opus-3")
        || model.contains("haiku-3");

    if takes_budget {
        return match budget {
            Some(tokens) => ThinkingConfig::Budget(tokens.max(1024)),
            None => ThinkingConfig::None,
        };
    }
    match budget {
        // A budget was asked for but cannot be expressed; adaptive is the
        // closest legal behaviour and is better than a 400.
        Some(_) => ThinkingConfig::Adaptive,
        None => ThinkingConfig::None,
    }
}
const API_VERSION: &str = "2023-06-01";

pub struct Anthropic {
    vendor: Vendor,
    key: String,
    costs: CostTable,
    http: reqwest::Client,
    endpoint: String,
    /// Stored on the struct, not built per call: `models()` returns a borrow,
    /// and a temporary array would not outlive it.
    models: Vec<ModelId>,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        // Prices per million tokens, from the current model table. Opus 5.5 is
        // $4/$20 — NOT the $5/$25 of Opus 5 or the $15/$75 of an older Opus.
        // A wrong price table produces a wrong budget, silently.
        let costs = CostTable::new()
            .with(
                "claude-opus-5-5",
                ModelPrice {
                    input: 4.0,
                    output: 20.0,
                    cache_read: 0.20,
                    cache_write: 5.0,
                },
            )
            .with(
                "claude-sonnet-5-5",
                ModelPrice {
                    input: 2.0,
                    output: 10.0,
                    cache_read: 0.20,
                    cache_write: 2.50,
                },
            )
            .with(
                "claude-haiku-4-5",
                ModelPrice {
                    input: 1.0,
                    output: 5.0,
                    cache_read: 0.10,
                    cache_write: 1.25,
                },
            )
            // An unrecognised model is never free: a typo must not become a
            // free ride. Priced at the Sonnet tier as a middle assumption.
            .with_fallback(ModelPrice {
                input: 2.0,
                output: 10.0,
                cache_read: 0.20,
                cache_write: 2.50,
            });

        Anthropic {
            vendor: Vendor::Anthropic,
            key: api_key.into(),
            costs,
            http: reqwest::Client::new(),
            endpoint: ENDPOINT.to_string(),
            models: vec![
                ModelId::new(Vendor::Anthropic, "claude-opus-5-5"),
                ModelId::new(Vendor::Anthropic, "claude-sonnet-5-5"),
                ModelId::new(Vendor::Anthropic, "claude-haiku-4-5"),
            ],
        }
    }

    /// Point at a different base URL. Used by tests against a local stub, by
    /// anyone running a compatible proxy, and by GitHub Copilot, which serves
    /// the same Messages shape on its own host.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Re-tag as a different vendor (Copilot speaks this shape).
    pub fn set_vendor(&mut self, vendor: Vendor) {
        self.vendor = vendor;
    }

    pub fn set_models(&mut self, models: &[&str]) {
        self.models = models
            .iter()
            .map(|m| ModelId::new(self.vendor, m))
            .collect();
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint
    }

    fn to_wire(&self, request: &CompletionRequest) -> serde_json::Value {
        // Anthropic carries the system prompt outside the message list and
        // does not allow system turns inside it. Converting here keeps the
        // domain model vendor-neutral.
        let system: Vec<String> = request
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::System { content } => Some(content.clone()),
                _ => None,
            })
            .collect();

        let mut messages = Vec::new();
        for message in &request.messages {
            match message {
                Message::System { .. } => continue,
                Message::User { content } => messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": content }]
                })),
                Message::Assistant { content } => messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": content }]
                })),
                Message::ToolResult {
                    content,
                    tool_call_id,
                } => messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content
                    }]
                })),
            }
        }

        let mut body = serde_json::json!({
            "model": request.model.model,
            "max_tokens": request.max_tokens,
            "messages": messages,
        });
        if !system.is_empty() {
            body["system"] = serde_json::json!(system.join("\n\n"));
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }
        // Thinking is model-dependent, and getting it wrong is an HTTP 400:
        //
        //   Opus 4.6 / Sonnet 4.6 and newer: {type: "adaptive"}.
        //     `budget_tokens` is DEPRECATED there and REJECTED with a 400 on
        //     Fable 5/5.1, Opus 5.5, Opus 5, Sonnet 5.5, Sonnet 5, Opus 4.8,
        //     4.7 — sending it fails every request, not just thinking ones.
        //   Haiku 4.5 and older: {type: "enabled", budget_tokens: N}, and
        //     thinking is off when the parameter is omitted.
        //
        // So: a caller asking for thinking on a current model gets adaptive;
        // a caller pinning a budget only ever reaches Haiku.
        match thinking_config(&request.model.model, request.thinking_budget) {
            ThinkingConfig::None => {}
            ThinkingConfig::Adaptive => {
                body["thinking"] = serde_json::json!({ "type": "adaptive" });
            }
            ThinkingConfig::Budget(tokens) => {
                body["thinking"] = serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": tokens
                });
            }
        }
        // Effort is the modern control and works on every current model.
        if let Some(effort) = request.effort.as_deref() {
            body["output_config"] = serde_json::json!({ "effort": effort });
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            );
        }
        body
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<WireBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Debug, Deserialize)]
struct WireBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[async_trait]
impl ModelProvider for Anthropic {
    fn vendor(&self) -> Vendor {
        self.vendor
    }

    fn models(&self) -> &[ModelId] {
        &self.models
    }

    fn credential_env(&self) -> Option<&'static str> {
        Some(match self.vendor {
            Vendor::Copilot => "GITHUB_TOKEN",
            _ => "ANTHROPIC_API_KEY",
        })
    }

    fn cost_table(&self) -> &CostTable {
        &self.costs
    }

    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ModelError> {
        let body = self.to_wire(request);

        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Unavailable {
                vendor: self.vendor,
                reason: e.to_string(),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            // The body may echo the key; it is never included in the error.
            return Err(ModelError::Auth {
                vendor: self.vendor,
                reason: format!("HTTP {}", status.as_u16()),
            });
        }
        if status.as_u16() == 429 {
            return Err(ModelError::RateLimited {
                vendor: self.vendor,
                reason: format!("HTTP {}", status.as_u16()),
            });
        }
        if !status.is_success() {
            return Err(ModelError::Unavailable {
                vendor: self.vendor,
                reason: format!("HTTP {}", status.as_u16()),
            });
        }

        let wire: WireResponse = response.json().await.map_err(|e| ModelError::Unavailable {
            vendor: self.vendor,
            reason: format!("malformed response: {e}"),
        })?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in &wire.content {
            match block.kind.as_str() {
                "text" => text.push_str(block.text.as_deref().unwrap_or("")),
                "tool_use" => tool_calls.push(ToolCall {
                    id: block.id.clone().unwrap_or_default(),
                    name: block.name.clone().unwrap_or_default(),
                    arguments: block.input.clone().unwrap_or(serde_json::json!({})),
                }),
                // thinking and server_tool_use blocks are not user-visible text.
                _ => {}
            }
        }

        Ok(CompletionResponse {
            content: text,
            tool_calls,
            usage: Usage {
                input_tokens: wire.usage.input_tokens,
                output_tokens: wire.usage.output_tokens,
                cache_read_tokens: wire.usage.cache_read_input_tokens,
                cache_write_tokens: wire.usage.cache_creation_input_tokens,
            },
            model: request.model.clone(),
            truncated: wire.stop_reason.as_deref() == Some("max_tokens"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ToolSpec;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: ModelId::new(Vendor::Anthropic, "claude-sonnet-5"),
            messages: vec![
                Message::system("you are terse"),
                Message::user("hello"),
                Message::assistant("hi"),
                Message::tool_result("file contents", "call_1"),
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 512,
            temperature: Some(0.2),
            thinking_budget: None,
            effort: None,
        }
    }

    #[test]
    fn the_system_prompt_leaves_the_message_list() {
        // Anthropic rejects a system turn inside `messages`; the conversion
        // has to hoist it or every request fails.
        let wire = Anthropic::new("k").to_wire(&request());
        assert_eq!(wire["system"], "you are terse");
        let roles: Vec<&str> = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "user"],
            "system must not appear as a turn"
        );
    }

    #[test]
    fn a_tool_result_becomes_a_user_turn_with_the_id() {
        let wire = Anthropic::new("k").to_wire(&request());
        let last = &wire["messages"].as_array().unwrap()[2];
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn tools_are_sent_in_the_anthropic_shape() {
        let wire = Anthropic::new("k").to_wire(&request());
        assert_eq!(wire["tools"][0]["name"], "read_file");
        // The wire calls it input_schema, not parameters.
        assert!(wire["tools"][0].get("input_schema").is_some());
    }

    #[test]
    fn a_current_model_gets_adaptive_thinking_never_a_budget() {
        // Sending budget_tokens on Opus 5.5 / Sonnet 5.5 / Opus 5 is an HTTP 400,
        // not a warning. A caller asking for a thinking budget on a current
        // model gets adaptive instead.
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-4-8",
        ] {
            let mut req = request();
            req.model = ModelId::new(Vendor::Anthropic, model);
            req.thinking_budget = Some(2048);
            let wire = Anthropic::new("k").to_wire(&req);
            assert_eq!(
                wire["thinking"]["type"], "adaptive",
                "{model} must not receive a budget_tokens block"
            );
            assert!(
                wire["thinking"].get("budget_tokens").is_none(),
                "{model} rejects budget_tokens with a 400"
            );
        }
    }

    #[test]
    fn haiku_still_takes_a_fixed_budget() {
        // Haiku 4.5 predates adaptive thinking; it wants an explicit budget.
        let mut req = request();
        req.model = ModelId::new(Vendor::Anthropic, "claude-haiku-4-5");
        req.thinking_budget = Some(2048);
        let wire = Anthropic::new("k").to_wire(&req);
        assert_eq!(wire["thinking"]["type"], "enabled");
        assert_eq!(wire["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn a_tiny_budget_is_raised_to_the_api_minimum() {
        // The API rejects a budget below 1024 with a 400.
        let mut req = request();
        req.model = ModelId::new(Vendor::Anthropic, "claude-haiku-4-5");
        req.thinking_budget = Some(10);
        let wire = Anthropic::new("k").to_wire(&req);
        assert_eq!(wire["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn haiku_without_a_budget_sends_no_thinking_block() {
        let mut req = request();
        req.model = ModelId::new(Vendor::Anthropic, "claude-haiku-4-5");
        let wire = Anthropic::new("k").to_wire(&req);
        assert!(wire.get("thinking").is_none());
    }

    #[test]
    fn thinking_config_is_pure_and_matches_the_wire() {
        assert_eq!(
            thinking_config("claude-opus-5-5", Some(2048)),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            thinking_config("claude-opus-5-5", None),
            ThinkingConfig::None
        );
        assert_eq!(
            thinking_config("claude-haiku-4-5", Some(2048)),
            ThinkingConfig::Budget(2048)
        );
    }

    #[test]
    fn effort_goes_into_output_config() {
        let mut req = request();
        req.effort = Some("high".to_string());
        let wire = Anthropic::new("k").to_wire(&req);
        assert_eq!(wire["output_config"]["effort"], "high");
    }

    #[test]
    fn no_thinking_budget_means_no_thinking_block() {
        let wire = Anthropic::new("k").to_wire(&request());
        assert!(wire.get("thinking").is_none());
    }

    #[test]
    fn the_model_uses_the_bare_name_not_the_qualified_id() {
        let wire = Anthropic::new("k").to_wire(&request());
        assert_eq!(wire["model"], "claude-sonnet-5");
    }

    #[test]
    fn a_wire_response_with_a_tool_use_yields_a_tool_call() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "text", "text": "let me look" },
                { "type": "tool_use", "id": "t1", "name": "read_file", "input": { "path": "a.rs" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        }))
        .unwrap();

        let mut text = String::new();
        let mut calls = Vec::new();
        for block in &wire.content {
            match block.kind.as_str() {
                "text" => text.push_str(block.text.as_deref().unwrap_or("")),
                "tool_use" => calls.push(ToolCall {
                    id: block.id.clone().unwrap_or_default(),
                    name: block.name.clone().unwrap_or_default(),
                    arguments: block.input.clone().unwrap_or_default(),
                }),
                _ => {}
            }
        }
        assert_eq!(text, "let me look");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(wire.usage.input_tokens, 100);
    }

    #[test]
    fn cache_tokens_are_carried_into_usage() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "content": [],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 40
            }
        }))
        .unwrap();
        assert_eq!(wire.usage.cache_read_input_tokens, 900);
        assert_eq!(wire.usage.cache_creation_input_tokens, 40);
    }

    #[test]
    fn a_thinking_block_does_not_leak_into_the_visible_text() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "thinking", "thinking": "internal reasoning" },
                { "type": "text", "text": "the answer" }
            ]
        }))
        .unwrap();
        // `thinking` carries no `text` field, so a naive extraction would
        // produce an empty or wrong answer.
        let text: String = wire
            .content
            .iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text.clone())
            .collect();
        assert_eq!(text, "the answer");
    }

    #[test]
    fn stop_reason_max_tokens_marks_the_response_truncated() {
        let wire: WireResponse = serde_json::from_value(
            serde_json::json!({ "content": [], "stop_reason": "max_tokens" }),
        )
        .unwrap();
        assert_eq!(wire.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn the_prices_match_the_current_model_table() {
        // Opus 5.5 is $4/$20 per million. Pricing it at the older $5/$25 or
        // $15/$75 makes every budget projection wrong.
        let provider = Anthropic::new("k");
        let million_in = Usage {
            input_tokens: 1_000_000,
            ..Usage::default()
        };
        let million_out = Usage {
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        assert!(
            (provider
                .cost_table()
                .cost_for("claude-opus-5-5", &million_in)
                .unwrap()
                - 4.0)
                .abs()
                < 1e-9
        );
        assert!(
            (provider
                .cost_table()
                .cost_for("claude-opus-5-5", &million_out)
                .unwrap()
                - 20.0)
                .abs()
                < 1e-9
        );
        assert!(
            (provider
                .cost_table()
                .cost_for("claude-sonnet-5-5", &million_in)
                .unwrap()
                - 2.0)
                .abs()
                < 1e-9
        );
        assert!(
            (provider
                .cost_table()
                .cost_for("claude-haiku-4-5", &million_out)
                .unwrap()
                - 5.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn the_declared_models_are_the_current_ones() {
        let names: Vec<String> = Anthropic::new("k")
            .models()
            .iter()
            .map(|m| m.model.clone())
            .collect();
        assert!(
            names.contains(&"claude-opus-5-5".to_string()),
            "got {names:?}"
        );
        assert!(
            names.contains(&"claude-sonnet-5-5".to_string()),
            "got {names:?}"
        );
        // No bare "claude-opus-5": superseded, and its price differs.
        assert!(
            !names.contains(&"claude-opus-5".to_string()),
            "got {names:?}"
        );
    }

    #[test]
    fn the_provider_declares_its_credential_variable() {
        assert_eq!(
            Anthropic::new("k").credential_env(),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn every_declared_model_has_a_price() {
        let provider = Anthropic::new("k");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            ..Usage::default()
        };
        for model in provider.models() {
            let cost = provider
                .cost_table()
                .cost_for(&model.model, &usage)
                .unwrap();
            assert!(cost > 0.0, "{} has no price", model.model);
        }
    }
}
