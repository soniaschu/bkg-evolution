//! Ollama: local models, no credential, no cost.
//!
//! This is what makes the CLI usable out of the box. `bkgclaw chat` works on a
//! machine with Ollama running and nothing else configured, which is the only
//! honest way to ship an agent CLI people will actually try.

use async_trait::async_trait;
use serde::Deserialize;

use crate::cost::{CostTable, ModelPrice, Usage};
use crate::models::{
    CompletionRequest, CompletionResponse, Message, ModelError, ModelId, ModelProvider, ToolCall,
    Vendor,
};

pub struct Ollama {
    base: String,
    costs: CostTable,
    http: reqwest::Client,
    models: Vec<ModelId>,
}

impl Ollama {
    pub fn new() -> Self {
        Ollama {
            base: std::env::var("OLLAMA_HOST")
                .map(|h| h.trim_end_matches('/').to_string())
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            // Local inference is genuinely free. That is the honest price, and
            // it is different from "unknown", which falls back to a real number.
            costs: CostTable::new()
                .with("llama3.2", ModelPrice::default())
                .with("qwen2.5-coder", ModelPrice::default())
                .with("mistral", ModelPrice::default())
                .with_fallback(ModelPrice::default()),
            http: reqwest::Client::new(),
            models: vec![
                ModelId::new(Vendor::Ollama, "llama3.2"),
                ModelId::new(Vendor::Ollama, "qwen2.5-coder"),
                ModelId::new(Vendor::Ollama, "mistral"),
            ],
        }
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_string();
        self
    }

    fn to_wire(&self, request: &CompletionRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| match m {
                Message::System { content } => {
                    serde_json::json!({ "role": "system", "content": content })
                }
                Message::User { content } => {
                    serde_json::json!({ "role": "user", "content": content })
                }
                Message::Assistant { content } => {
                    serde_json::json!({ "role": "assistant", "content": content })
                }
                Message::ToolResult { content, .. } => {
                    serde_json::json!({ "role": "tool", "content": content })
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": request.model.model,
            "messages": messages,
            "stream": false,
            "options": { "num_predict": request.max_tokens },
        });
        if let Some(temperature) = request.temperature {
            body["options"]["temperature"] = serde_json::json!(temperature);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            );
        }
        body
    }
}

impl Default for Ollama {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    message: WireMessage,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Default, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    function: WireFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

/// Ollama sends tool arguments as a JSON object on some versions and as a
/// JSON *string* on others. Normalising here means the rest of the platform
/// only ever sees an object — passing the raw string through would hand the
/// model `"{\\"a\\":1}"` where it expects `{"a":1}`.
fn normalise_arguments(raw: Option<serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(text)) => {
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }))
        }
        Some(value) => value,
        None => serde_json::json!({}),
    }
}

#[async_trait]
impl ModelProvider for Ollama {
    fn vendor(&self) -> Vendor {
        Vendor::Ollama
    }

    fn models(&self) -> &[ModelId] {
        &self.models
    }

    fn credential_env(&self) -> Option<&'static str> {
        // None means "no credential needed", which doctor renders differently
        // from a provider that is merely unconfigured.
        None
    }

    fn needs_network(&self) -> bool {
        false
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
            .post(format!("{}/api/chat", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Unavailable {
                vendor: Vendor::Ollama,
                // The most common cause by far, so it is named explicitly.
                reason: format!("{e} (is `ollama serve` running at {}?)", self.base),
            })?;

        if !response.status().is_success() {
            return Err(ModelError::Unavailable {
                vendor: Vendor::Ollama,
                reason: format!("HTTP {}", response.status().as_u16()),
            });
        }

        let wire: WireResponse = response.json().await.map_err(|e| ModelError::Unavailable {
            vendor: Vendor::Ollama,
            reason: format!("malformed response: {e}"),
        })?;

        let tool_calls: Vec<ToolCall> = wire
            .message
            .tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| ToolCall {
                // Ollama does not always supply a call id, and the loop needs a
                // stable one to correlate the result.
                id: format!("call_{index}"),
                name: call.function.name.clone(),
                arguments: normalise_arguments(call.function.arguments.clone()),
            })
            .collect();

        Ok(CompletionResponse {
            content: wire.message.content,
            tool_calls,
            // Ollama reports no token counts on /api/chat without
            // stream=true. Zero here means "not reported", and the cost table
            // prices local inference at zero regardless.
            usage: Usage::default(),
            model: request.model.clone(),
            truncated: wire.done_reason.as_deref() == Some("length"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: ModelId::new(Vendor::Ollama, "llama3.2"),
            messages: vec![
                Message::system("be terse"),
                Message::user("hello"),
                Message::assistant("hi"),
                Message::tool_result("contents", "c1"),
            ],
            tools: vec![crate::models::ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 256,
            temperature: Some(0.1),
            thinking_budget: None,
            effort: None,
        }
    }

    #[test]
    fn messages_keep_their_roles() {
        let wire = Ollama::new().to_wire(&request());
        let roles: Vec<&str> = wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    }

    #[test]
    fn tools_use_the_openai_function_shape() {
        let wire = Ollama::new().to_wire(&request());
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn streaming_is_off_so_the_response_arrives_at_once() {
        // A non-streaming call keeps the loop simple; streaming would need a
        // second code path through the NDJSON reader.
        assert_eq!(Ollama::new().to_wire(&request())["stream"], false);
    }

    #[test]
    fn a_tool_call_without_an_id_gets_a_stable_one() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "message": {
                "content": "",
                "tool_calls": [
                    { "function": { "name": "read_file", "arguments": { "path": "a.rs" } } },
                    { "function": { "name": "list_directory", "arguments": "{}" } }
                ]
            }
        }))
        .unwrap();
        let calls: Vec<ToolCall> = wire
            .message
            .tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| ToolCall {
                id: format!("call_{index}"),
                name: call.function.name.clone(),
                arguments: normalise_arguments(call.function.arguments.clone()),
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[1].name, "list_directory");
    }

    #[test]
    fn arguments_sent_as_a_json_string_are_parsed() {
        // Ollama returns `arguments` as a string on some versions. Passing it
        // through unparsed hands the model a string where it expects an object.
        assert_eq!(
            normalise_arguments(Some(serde_json::json!("{\"path\":\"a.rs\"}"))),
            serde_json::json!({ "path": "a.rs" })
        );
    }

    #[test]
    fn an_object_passes_through_unchanged() {
        assert_eq!(
            normalise_arguments(Some(serde_json::json!({ "path": "a.rs" }))),
            serde_json::json!({ "path": "a.rs" })
        );
    }

    #[test]
    fn a_missing_arguments_field_becomes_an_empty_object() {
        assert_eq!(normalise_arguments(None), serde_json::json!({}));
    }

    #[test]
    fn an_unparsable_string_is_preserved_rather_than_dropped() {
        // Losing the value silently would make the model guess. Keeping it
        // under `raw` makes the malformation visible to the operator.
        let parsed = normalise_arguments(Some(serde_json::json!("not json")));
        assert_eq!(parsed, serde_json::json!({ "raw": "not json" }));
    }

    #[test]
    fn ollama_needs_no_credential() {
        assert_eq!(Ollama::new().credential_env(), None);
    }

    #[test]
    fn local_inference_is_free() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        assert_eq!(
            Ollama::new()
                .cost_table()
                .cost_for("llama3.2", &usage)
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn the_base_url_drops_a_trailing_slash() {
        assert_eq!(
            Ollama::new().with_base("http://host:1234/").base,
            "http://host:1234"
        );
    }
}
