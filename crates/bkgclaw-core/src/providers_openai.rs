//! The OpenAI-compatible Chat Completions API.
//!
//! One implementation, six vendors. OpenAI, DeepSeek, xAI, OpenRouter, Groq,
//! Together and every self-hosted endpoint that speaks this wire format differ
//! in URL, credential variable and price — not in shape. That is what makes
//! "any OpenAI-compatible endpoint" one struct rather than six.
//!
//! Also the base for Google: Gemini's REST API accepts an OpenAI-compatibility
//! endpoint, which is the least surprising way to support it.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use crate::cost::{CostTable, ModelPrice, Usage};
use crate::models::{
    CompletionRequest, CompletionResponse, Message, ModelError, ModelId, ModelProvider, ToolCall,
    Vendor,
};

/// How long to wait for the first SSE chunk before deciding the gateway
/// does not stream this model.
const STREAM_FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stream may idle between chunks before the answer is taken
/// as-is and marked truncated.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Ceiling for a non-streaming request. A model that takes longer than this
/// is down, not thinking.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How one SSE line was classified. Pure data, so the parser is testable
/// without a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLine {
    /// A JSON chunk.
    Data,
    /// The terminal marker.
    Done,
    /// An error object — the stream endpoint answered, but with a failure.
    Error,
    /// A comment, keep-alive or blank line.
    Skip,
}

/// Accumulated state of one streamed response.
#[derive(Debug, Default)]
struct StreamState {
    content: String,
    tools: Vec<AccumToolCall>,
    usage: Option<WireUsage>,
    finish_reason: Option<String>,
    saw_data: bool,
}

#[derive(Debug, Default, Clone)]
struct AccumToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct WireStreamChunk {
    #[serde(default)]
    choices: Vec<WireStreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamChoice {
    #[serde(default)]
    delta: WireStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireStreamToolCall>,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: WireStreamFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Classify one SSE line. `data:`-prefixed JSON chunks are parsed and their
/// deltas applied; everything else (comments like `: connected`, blank
/// lines) is skipped without error.
fn process_sse_line(
    line: &str,
    state: &mut StreamState,
    sink: &dyn crate::observer::DeltaSink,
) -> SseLine {
    let payload = match line.strip_prefix("data:") {
        Some(rest) => rest.trim_start(),
        None => return SseLine::Skip,
    };
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    let chunk: WireStreamChunk = match serde_json::from_str(payload) {
        Ok(chunk) => chunk,
        // A malformed chunk is skipped rather than fatal: one bad line in a
        // long stream must not lose the answer.
        Err(_) => return SseLine::Skip,
    };
    if chunk.error.is_some() {
        return SseLine::Error;
    }
    state.saw_data = true;
    if let Some(usage) = chunk.usage {
        state.usage = Some(usage);
    }
    let Some(choice) = chunk.choices.into_iter().next() else {
        return SseLine::Data;
    };
    if let Some(reason) = choice.finish_reason {
        state.finish_reason = Some(reason);
    }
    if let Some(text) = choice.delta.content {
        if !text.is_empty() {
            sink.text(&text);
            state.content.push_str(&text);
        }
    }
    if let Some(text) = choice.delta.reasoning {
        if !text.is_empty() {
            // Reasoning is delivered to the sink (a UI shows it as thinking)
            // but never mixed into the answer text.
            sink.reasoning(&text);
        }
    }
    for call in choice.delta.tool_calls {
        let index = call.index;
        if state.tools.len() <= index {
            state.tools.resize(index + 1, AccumToolCall::default());
        }
        let slot = &mut state.tools[index];
        if !call.id.is_empty() && slot.id.is_empty() {
            slot.id = call.id;
        }
        if let Some(name) = call.function.name {
            slot.name.push_str(&name);
        }
        if let Some(args) = call.function.arguments {
            slot.arguments.push_str(&args);
        }
    }
    SseLine::Data
}

/// Whether the stream carried anything worth calling an answer: visible
/// text, or a tool call with a name. A stream that delivered only reasoning
/// or nothing at all is not an answer.
fn stream_produced(state: &StreamState) -> bool {
    state.saw_data && (!state.content.is_empty() || state.tools.iter().any(|t| !t.name.is_empty()))
}

/// Turn the accumulated state into the response the loop expects.
fn finish_stream(state: StreamState, request_model: &ModelId) -> CompletionResponse {
    let tool_calls: Vec<ToolCall> = state
        .tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| ToolCall {
            id: if tool.id.is_empty() {
                format!("call_{index}")
            } else {
                tool.id
            },
            name: tool.name,
            arguments: parse_arguments(Some(&tool.arguments)),
        })
        .collect();
    CompletionResponse {
        content: state.content,
        tool_calls,
        usage: state
            .usage
            .map(|u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cache_read_tokens: u.prompt_cache_hit_tokens,
                cache_write_tokens: 0,
            })
            .unwrap_or_default(),
        model: request_model.clone(),
        truncated: state.finish_reason.as_deref() == Some("length"),
    }
}

/// How a vendor plugs into the shared OpenAI shape.
///
/// Not `Clone`: the stream-failure memo is per process instance, and a
/// cloned provider silently forgetting which model does not stream would
/// re-pay the discovery cost on every copy.
#[derive(Debug)]
pub struct OpenAiCompatible {
    vendor: Vendor,
    key: String,
    base: String,
    /// Extra header some gateways demand (e.g. `HTTP-Referer` on OpenRouter).
    extra_headers: Vec<(String, String)>,
    costs: CostTable,
    http: reqwest::Client,
    models: Vec<ModelId>,
    /// Models whose stream endpoint answered with an error or nothing, so
    /// later calls skip the attempt. Process-lifetime memoisation: the
    /// gateway, not the model, is usually the reason.
    stream_blocked: Mutex<HashSet<String>>,
}

impl OpenAiCompatible {
    /// OpenAI itself.
    pub fn openai(key: impl Into<String>) -> Self {
        Self::new(
            Vendor::OpenAi,
            key,
            "https://api.openai.com/v1",
            &["gpt-4o", "gpt-4o-mini", "o3-mini"],
            prices_openai(),
        )
    }

    /// DeepSeek: cheap, OpenAI-shaped.
    pub fn deepseek(key: impl Into<String>) -> Self {
        Self::new(
            Vendor::DeepSeek,
            key,
            "https://api.deepseek.com/v1",
            &["deepseek-chat", "deepseek-reasoner"],
            prices_deepseek(),
        )
    }

    /// xAI Grok.
    pub fn xai(key: impl Into<String>) -> Self {
        Self::new(
            Vendor::Xai,
            key,
            "https://api.x.ai/v1",
            &["grok-2", "grok-2-mini"],
            prices_xai(),
        )
    }

    /// OpenRouter: 200+ models behind one key and one shape.
    pub fn openrouter(key: impl Into<String>) -> Self {
        let mut provider = Self::new(
            Vendor::OpenRouter,
            key,
            "https://openrouter.ai/api/v1",
            &["openrouter/auto"],
            CostTable::new()
                .with(
                    "openrouter/auto",
                    ModelPrice {
                        input: 2.0,
                        output: 8.0,
                        ..ModelPrice::default()
                    },
                )
                .with_fallback(ModelPrice {
                    input: 2.0,
                    output: 8.0,
                    ..ModelPrice::default()
                }),
        );
        // OpenRouter wants attribution headers; without them requests can 403.
        provider
            .extra_headers
            .push(("HTTP-Referer".into(), "https://bkgclaw.local".into()));
        provider
            .extra_headers
            .push(("X-Title".into(), "bkgclaw".into()));
        provider
    }

    /// Any endpoint that speaks the format: a local proxy, a gateway, a
    /// self-hosted vLLM.
    pub fn compatible(base: impl Into<String>, key: impl Into<String>, model: &str) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        Self::new(
            Vendor::Compatible,
            key,
            &base,
            &[model],
            // An unknown endpoint's price is unknown, not zero.
            CostTable::new().with_fallback(ModelPrice {
                input: 1.0,
                output: 3.0,
                ..ModelPrice::default()
            }),
        )
    }

    /// The NVIDIA-NIM gateway. OpenAI wire format, so this is configuration,
    /// not a new protocol.
    ///
    /// The endpoint and credential are operator-provided (`NIM_BASE_URL`,
    /// `NIM_API_KEY`), which makes this provider *their* gateway rather than
    /// a metered public API: the per-token price to us is zero, and token
    /// usage is still tracked and reported.
    pub fn nvidia_nim(key: impl Into<String>) -> Self {
        Self::new(
            Vendor::Nim,
            key,
            Self::NIM_DEFAULT_BASE,
            Self::NIM_MODELS,
            CostTable::new(),
        )
    }

    /// Default NIM gateway endpoint. Overridable because the gateway is
    /// operator infrastructure, and operators move things.
    pub const NIM_DEFAULT_BASE: &'static str = "https://nim.eysho.info/v1";

    /// Models that answered a real chat request on the gateway, verified live
    /// on 2026-10-08 with a "say HALLO" completion against every model in
    /// `GET /models` (80 listed, 39 answered 404, 28 answered 403 for this
    /// key, the rest timed out twice). Only the seven that answered are
    /// here; a listed-but-dead model must not appear as usable.
    ///
    /// The first four also answered a tool-call request with a well-formed
    /// `read_file` call, which is what the agent chain requires. The other
    /// three respond with text only and stay available for plain chat, out of
    /// the failover chain.
    pub const NIM_MODELS: &[&str] = &[
        // Tool-capable, verified 2026-10-08.
        "nvidia/nemotron-3-super-120b-a12b",
        "nvidia/nemotron-3.5-lightning-30b-a3b",
        "meta/muse-glimmer-30b",
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
        // Text-only responders, verified 2026-10-08, no tool calling.
        "meta/llama-3.2-90b-vision-instruct",
        "meta/llama-3.2-11b-vision-instruct",
        "nvidia/riva-translate-4b-instruct-v2",
    ];

    /// The tool-capable subset, in failover order. Separate from `NIM_MODELS`
    /// because a model that cannot call tools is useless as the agent's
    /// primary even when it chats fine.
    pub const NIM_CHAIN: &[&str] = &[
        "nvidia/nemotron-3-super-120b-a12b",
        "nvidia/nemotron-3.5-lightning-30b-a3b",
        "meta/muse-glimmer-30b",
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
    ];

    fn new(
        vendor: Vendor,
        key: impl Into<String>,
        base: &str,
        models: &[&str],
        costs: CostTable,
    ) -> Self {
        OpenAiCompatible {
            vendor,
            key: key.into(),
            base: base.trim_end_matches('/').to_string(),
            extra_headers: Vec::new(),
            costs,
            http: reqwest::Client::new(),
            models: models.iter().map(|m| ModelId::new(vendor, m)).collect(),
            stream_blocked: Mutex::new(HashSet::new()),
        }
    }

    /// Whether streaming was already turned off for this model.
    fn streaming_blocked(&self, model: &str) -> bool {
        self.stream_blocked
            .lock()
            .expect("stream_blocked")
            .contains(model)
    }

    /// Turn streaming off for this model and remember it.
    fn block_streaming(&self, model: &str) {
        self.stream_blocked
            .lock()
            .expect("stream_blocked")
            .insert(model.to_string());
    }

    /// Non-streaming completion with the whole text pushed to the sink as
    /// one delta. Used as the fallback when a model's stream endpoint is
    /// broken, and by models that never streamed in the first place.
    async fn complete_then_emit(
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

    pub fn with_endpoint(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Re-tag a configured provider as a different vendor. Gemini reaches the
    /// same wire format through this adapter, and the registry must see it as
    /// Google rather than as a local gateway.
    pub fn set_vendor(&mut self, vendor: Vendor) {
        self.vendor = vendor;
        self.models = self
            .models
            .iter()
            .map(|m| ModelId::new(vendor, &m.model))
            .collect();
    }

    pub fn set_models(&mut self, models: &[&str]) {
        self.models = models
            .iter()
            .map(|m| ModelId::new(self.vendor, m))
            .collect();
    }

    pub fn set_costs(&mut self, costs: CostTable) {
        self.costs = costs;
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn to_wire(&self, request: &CompletionRequest) -> serde_json::Value {
        // This format carries the system prompt as a turn, unlike Anthropic.
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
                Message::ToolResult { content, tool_call_id } => {
                    serde_json::json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": request.model.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
        });
        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }
        // o1/o3-style models reject a temperature other than 1 and reject
        // sampling params entirely; sending one is a 400.
        if is_reasoning_model(&request.model.model) {
            if let Some(map) = body.as_object_mut() {
                map.remove("temperature");
            }
        }
        // There is no portable thinking field. `reasoning_effort` is the
        // closest thing o1/o3 accept, so it is the only mapping attempted.
        if let Some(effort) = &request.effort {
            body["reasoning_effort"] = serde_json::json!(effort);
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

/// Models that reject sampling parameters.
fn is_reasoning_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("o1") || lower.starts_with("o3") || lower.contains("reasoner")
}

fn prices_openai() -> CostTable {
    CostTable::new()
        .with(
            "gpt-4o",
            ModelPrice {
                input: 2.5,
                output: 10.0,
                ..ModelPrice::default()
            },
        )
        .with(
            "gpt-4o-mini",
            ModelPrice {
                input: 0.15,
                output: 0.60,
                ..ModelPrice::default()
            },
        )
        .with(
            "o3-mini",
            ModelPrice {
                input: 1.1,
                output: 4.4,
                ..ModelPrice::default()
            },
        )
        .with_fallback(ModelPrice {
            input: 2.5,
            output: 10.0,
            ..ModelPrice::default()
        })
}

fn prices_deepseek() -> CostTable {
    CostTable::new()
        .with(
            "deepseek-chat",
            ModelPrice {
                input: 0.27,
                output: 1.10,
                cache_read: 0.07,
                ..ModelPrice::default()
            },
        )
        .with(
            "deepseek-reasoner",
            ModelPrice {
                input: 0.55,
                output: 2.19,
                ..ModelPrice::default()
            },
        )
        .with_fallback(ModelPrice {
            input: 0.27,
            output: 1.10,
            ..ModelPrice::default()
        })
}

fn prices_xai() -> CostTable {
    CostTable::new()
        .with(
            "grok-2",
            ModelPrice {
                input: 2.0,
                output: 10.0,
                ..ModelPrice::default()
            },
        )
        .with(
            "grok-2-mini",
            ModelPrice {
                input: 0.30,
                output: 0.50,
                ..ModelPrice::default()
            },
        )
        .with_fallback(ModelPrice {
            input: 2.0,
            output: 10.0,
            ..ModelPrice::default()
        })
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    /// Some gateways return the error in a 200 body.
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(default)]
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
    /// Some providers return a reasoning trace alongside content.
    #[serde(default, rename = "reasoning_content")]
    reasoning: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: WireFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: String,
    /// OpenAI sends arguments as a JSON *string*, unlike Ollama's object form.
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
}

/// Arguments arrive as a JSON string; the loop needs an object.
fn parse_arguments(raw: Option<&str>) -> serde_json::Value {
    match raw {
        Some(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({ "raw": text }))
        }
        None => serde_json::json!({}),
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatible {
    fn vendor(&self) -> Vendor {
        self.vendor
    }

    fn models(&self) -> &[ModelId] {
        &self.models
    }

    fn credential_env(&self) -> Option<&'static str> {
        Some(match self.vendor {
            Vendor::OpenAi => "OPENAI_API_KEY",
            Vendor::DeepSeek => "DEEPSEEK_API_KEY",
            Vendor::Xai => "XAI_API_KEY",
            Vendor::OpenRouter => "OPENROUTER_API_KEY",
            Vendor::Google => "GEMINI_API_KEY",
            Vendor::Copilot => "GITHUB_TOKEN",
            // A local gateway may need nothing, but the var stays so the
            // operator can point at one.
            Vendor::Compatible => "BKGCLAW_COMPATIBLE_KEY",
            Vendor::Nim => "NIM_API_KEY",
            // This adapter does not serve Anthropic; the catch-all exists only
            // so an exhaustive match compiles.
            Vendor::Ollama | Vendor::Anthropic => "BKGCLAW_COMPATIBLE_KEY",
        })
    }

    fn cost_table(&self) -> &CostTable {
        &self.costs
    }

    async fn complete_streamed(
        &self,
        request: &CompletionRequest,
        sink: &dyn crate::observer::DeltaSink,
    ) -> Result<CompletionResponse, ModelError> {
        let model_name = request.model.model.clone();
        if self.streaming_blocked(&model_name) {
            return self.complete_then_emit(request, sink).await;
        }

        let mut body = self.to_wire(request);
        if let Some(map) = body.as_object_mut() {
            map.insert("stream".into(), serde_json::json!(true));
        }

        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(&body);
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }

        let response = match req.send().await {
            Ok(response) => response,
            // A transport error on the stream attempt says nothing about the
            // model; one non-streaming retry is the cheapest honest answer.
            Err(_) => return self.complete_then_emit(request, sink).await,
        };

        let status = response.status();
        let code = status.as_u16();
        // Auth and rate limits are model-level facts. Swallowing them into a
        // non-streaming retry would hide a broken credential from the
        // failover chain.
        if code == 401 || code == 403 {
            return Err(ModelError::Auth {
                vendor: self.vendor,
                reason: format!("HTTP {code}"),
            });
        }
        if code == 429 {
            return Err(ModelError::RateLimited {
                vendor: self.vendor,
                reason: format!("HTTP {code}"),
            });
        }
        if !status.is_success() {
            self.block_streaming(&model_name);
            return self.complete_then_emit(request, sink).await;
        }

        // SSE over a byte stream. Lines are split on `\n`; UTF-8 never
        // contains that byte inside a character, so the split is safe.
        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut state = StreamState::default();

        loop {
            let next = if state.saw_data {
                tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await
            } else {
                tokio::time::timeout(STREAM_FIRST_CHUNK_TIMEOUT, stream.next()).await
            };
            let chunk = match next {
                Ok(Some(item)) => item,
                // Deadline or EOF. Partial content is kept and marked
                // truncated; an empty stream is no answer at all and means
                // the model does not stream here — fall back.
                Ok(None) | Err(_) => {
                    if stream_produced(&state) {
                        let mut response = finish_stream(state, &request.model);
                        response.truncated = true;
                        return Ok(response);
                    }
                    self.block_streaming(&model_name);
                    return self.complete_then_emit(request, sink).await;
                }
            };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(_) => break,
            };
            buffer.extend_from_slice(&bytes);

            while let Some(position) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=position).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                match process_sse_line(&line, &mut state, sink) {
                    SseLine::Skip | SseLine::Data => {}
                    SseLine::Done => {
                        if stream_produced(&state) {
                            return Ok(finish_stream(state, &request.model));
                        }
                        // `[DONE]` with nothing before it is the silent-stream
                        // case: the endpoint works, the model does not.
                        self.block_streaming(&model_name);
                        return self.complete_then_emit(request, sink).await;
                    }
                    // The endpoint exists but refuses to stream this model.
                    // Answer the ordinary way and stop trying.
                    SseLine::Error => {
                        self.block_streaming(&model_name);
                        return self.complete_then_emit(request, sink).await;
                    }
                }
            }
        }

        // EOF without [DONE]: some proxies cut the marker. Accept what
        // arrived — but an empty stream is no answer at all, and means the
        // model does not stream here.
        if stream_produced(&state) {
            return Ok(finish_stream(state, &request.model));
        }
        self.block_streaming(&model_name);
        self.complete_then_emit(request, sink).await
    }

    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ModelError> {
        let body = self.to_wire(request);

        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            // A non-streaming call that takes minutes is a dead vendor, and
            // without a ceiling it holds the whole turn hostage.
            .timeout(REQUEST_TIMEOUT);
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }

        let response = req.send().await.map_err(|e| ModelError::Unavailable {
            vendor: self.vendor,
            reason: e.to_string(),
        })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
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

        // A gateway can return HTTP 200 with an error body. Treating that as a
        // success would report an empty answer as a real one.
        if let Some(error) = wire.error {
            return Err(ModelError::Unavailable {
                vendor: self.vendor,
                reason: error.message,
            });
        }

        let Some(choice) = wire.choices.first() else {
            return Err(ModelError::Unavailable {
                vendor: self.vendor,
                reason: "response contained no choices".to_string(),
            });
        };

        let tool_calls: Vec<ToolCall> = choice
            .message
            .tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| ToolCall {
                // A provider that omits the id still needs a stable one.
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id.clone()
                },
                name: call.function.name.clone(),
                arguments: parse_arguments(call.function.arguments.as_deref()),
            })
            .collect();

        // `reasoning_content` is a reasoning trace some providers return. It
        // is kept for callers that want to show reasoning, and dropped here
        // because the domain model's `content` is the answer.
        let _reasoning = choice.message.reasoning.as_deref();

        Ok(CompletionResponse {
            content: choice.message.content.clone().unwrap_or_default(),
            tool_calls,
            usage: wire
                .usage
                .map(|u| Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    cache_read_tokens: u.prompt_cache_hit_tokens,
                    cache_write_tokens: 0,
                })
                .unwrap_or_default(),
            model: request.model.clone(),
            truncated: choice.finish_reason.as_deref() == Some("length"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(model: &str) -> CompletionRequest {
        CompletionRequest {
            model: ModelId::new(Vendor::OpenAi, model),
            messages: vec![
                Message::system("be terse"),
                Message::user("hello"),
                Message::tool_result("contents", "c1"),
            ],
            tools: vec![crate::models::ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            max_tokens: 512,
            temperature: Some(0.3),
            thinking_budget: None,
            effort: None,
        }
    }

    #[test]
    fn the_system_prompt_is_a_turn_in_this_format() {
        // The opposite of Anthropic. Getting it backwards makes every request 400.
        let wire = OpenAiCompatible::openai("k").to_wire(&request("gpt-4o"));
        assert_eq!(wire["messages"][0]["role"], "system");
    }

    #[test]
    fn a_tool_result_carries_its_call_id() {
        let wire = OpenAiCompatible::openai("k").to_wire(&request("gpt-4o"));
        let last = &wire["messages"].as_array().unwrap()[2];
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "c1");
    }

    #[test]
    fn tools_use_the_function_wrapper() {
        let wire = OpenAiCompatible::openai("k").to_wire(&request("gpt-4o"));
        assert_eq!(wire["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn reasoning_models_do_not_receive_a_temperature() {
        // o1/o3 reject sampling parameters outright.
        for model in ["o1-preview", "o3-mini", "deepseek-reasoner"] {
            let mut req = request(model);
            req.temperature = Some(0.7);
            let wire = OpenAiCompatible::openai("k").to_wire(&req);
            assert!(
                wire.get("temperature").is_none(),
                "{model} must not get a temperature"
            );
        }
    }

    #[test]
    fn ordinary_models_do_receive_a_temperature() {
        let wire = OpenAiCompatible::openai("k").to_wire(&request("gpt-4o"));
        // f32 -> f64 in JSON widens the value; compare within float noise.
        let sent = wire["temperature"].as_f64().unwrap();
        assert!((sent - 0.3).abs() < 1e-6, "got {sent}");
    }

    #[test]
    fn reasoning_effort_is_sent_when_asked_for() {
        let mut req = request("o3-mini");
        req.effort = Some("high".into());
        let wire = OpenAiCompatible::openai("k").to_wire(&req);
        assert_eq!(wire["reasoning_effort"], "high");
    }

    #[test]
    fn arguments_are_parsed_from_the_json_string() {
        // OpenAI sends a string; handing it through gives the model a string.
        assert_eq!(
            parse_arguments(Some(r#"{"path":"a.rs"}"#)),
            serde_json::json!({ "path": "a.rs" })
        );
    }

    #[test]
    fn malformed_arguments_are_preserved_rather_than_dropped() {
        let parsed = parse_arguments(Some("not json"));
        assert_eq!(parsed, serde_json::json!({ "raw": "not json" }));
    }

    #[test]
    fn a_missing_arguments_field_becomes_an_empty_object() {
        assert_eq!(parse_arguments(None), serde_json::json!({}));
    }

    #[test]
    fn an_error_inside_a_200_response_is_not_treated_as_success() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "error": { "message": "model not found" }
        }))
        .unwrap();
        // The gateway answered 200 with a failure body.
        assert_eq!(wire.error.unwrap().message, "model not found");
        assert!(wire.choices.is_empty());
    }

    #[test]
    fn a_response_with_no_choices_is_an_error_not_an_empty_answer() {
        let wire: WireResponse =
            serde_json::from_value(serde_json::json!({ "choices": [] })).unwrap();
        assert!(wire.choices.is_empty());
    }

    #[test]
    fn cache_hits_map_onto_the_usage_cache_field() {
        let wire: WireResponse = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 100, "completion_tokens": 5, "prompt_cache_hit_tokens": 80 }
        }))
        .unwrap();
        assert_eq!(wire.usage.unwrap().prompt_cache_hit_tokens, 80);
    }

    #[test]
    fn openrouter_gets_its_attribution_headers() {
        // Without them OpenRouter can reject the request outright.
        let provider = OpenAiCompatible::openrouter("k");
        let names: Vec<&str> = provider
            .extra_headers
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(names.contains(&"HTTP-Referer"));
        assert!(names.contains(&"X-Title"));
    }

    #[test]
    fn each_vendor_declares_its_own_credential_variable() {
        assert_eq!(
            OpenAiCompatible::openai("k").credential_env(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            OpenAiCompatible::deepseek("k").credential_env(),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(
            OpenAiCompatible::xai("k").credential_env(),
            Some("XAI_API_KEY")
        );
        assert_eq!(
            OpenAiCompatible::openrouter("k").credential_env(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            OpenAiCompatible::compatible("http://localhost:8080/v1", "", "m").credential_env(),
            Some("BKGCLAW_COMPATIBLE_KEY")
        );
    }

    #[test]
    fn a_trailing_slash_in_the_base_is_normalised() {
        assert_eq!(
            OpenAiCompatible::compatible("http://localhost:8080/v1/", "k", "m").base,
            "http://localhost:8080/v1"
        );
    }

    #[test]
    fn an_unknown_compatible_model_is_priced_not_free() {
        let provider = OpenAiCompatible::compatible("http://x/v1", "k", "custom");
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Usage::default()
        };
        assert!(provider.cost_table().cost_for("custom", &usage).unwrap() > 0.0);
    }

    #[test]
    fn nvidia_nim_lists_exactly_the_verified_models() {
        let provider = OpenAiCompatible::nvidia_nim("k");
        let names: Vec<&str> = provider.models().iter().map(|m| m.model.as_str()).collect();
        assert_eq!(names, OpenAiCompatible::NIM_MODELS.to_vec());
    }

    #[test]
    fn the_nim_chain_is_a_subset_of_the_verified_models() {
        // A chain entry that was never verified would fail on its first call.
        for model in OpenAiCompatible::NIM_CHAIN {
            assert!(
                OpenAiCompatible::NIM_MODELS.contains(model),
                "{model} is in the chain but not in the verified set"
            );
        }
        assert!(!OpenAiCompatible::NIM_CHAIN.is_empty());
    }

    #[test]
    fn nvidia_nim_targets_the_operator_gateway() {
        let provider = OpenAiCompatible::nvidia_nim("k");
        assert_eq!(provider.base_url(), "https://nim.eysho.info/v1");
        assert_eq!(provider.vendor(), Vendor::Nim);
        assert_eq!(provider.credential_env(), Some("NIM_API_KEY"));
    }

    #[test]
    fn nvidia_nim_is_operator_metered_not_per_token() {
        // The gateway belongs to the operator; per-token price to us is zero,
        // matching the existing compatible-gateway precedent. Token usage is
        // still tracked, so the budget's token side keeps working.
        let provider = OpenAiCompatible::nvidia_nim("k");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        assert_eq!(
            provider
                .cost_table()
                .cost_for(OpenAiCompatible::NIM_CHAIN[0], &usage),
            Some(0.0)
        );
    }

    #[test]
    fn nim_models_round_trip_through_model_ids() {
        let id = crate::models::ModelId::parse("nim/nvidia/nemotron-3-super-120b-a12b").unwrap();
        assert_eq!(id.vendor, Vendor::Nim);
        assert_eq!(id.model, "nvidia/nemotron-3-super-120b-a12b");
        assert_eq!(id.to_string(), "nim/nvidia/nemotron-3-super-120b-a12b");
    }

    /// A sink that records every delta, in order.
    struct LogSink(std::sync::Mutex<Vec<(bool, String)>>);
    impl crate::observer::DeltaSink for LogSink {
        fn text(&self, delta: &str) {
            self.0.lock().unwrap().push((false, delta.to_string()));
        }
        fn reasoning(&self, delta: &str) {
            self.0.lock().unwrap().push((true, delta.to_string()));
        }
    }

    fn parse_chunk(line: &str, sink: &dyn crate::observer::DeltaSink) -> (SseLine, StreamState) {
        let mut state = StreamState::default();
        let result = process_sse_line(line, &mut state, sink);
        (result, state)
    }

    #[test]
    fn a_content_chunk_delivers_text_and_accumulates() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let (result, state) = parse_chunk(
            r#"data: {"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            &sink,
        );
        assert_eq!(result, SseLine::Data);
        assert_eq!(state.content, "Hel");
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![(false, "Hel".to_string())]
        );
    }

    #[test]
    fn reasoning_deltas_reach_the_sink_but_never_the_answer() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let (result, state) = parse_chunk(
            r#"data: {"choices":[{"delta":{"reasoning_content":"thinking aloud"},"content":null}]}"#,
            &sink,
        );
        assert_eq!(result, SseLine::Data);
        assert!(
            state.content.is_empty(),
            "reasoning must not leak into the answer"
        );
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![(true, "thinking aloud".to_string())]
        );
    }

    #[test]
    fn tool_call_fragments_assemble_across_chunks() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let mut state = StreamState::default();
        let first = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#;
        let second = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"x\"}"}}]}}]}"#;
        assert_eq!(process_sse_line(first, &mut state, &sink), SseLine::Data);
        assert_eq!(process_sse_line(second, &mut state, &sink), SseLine::Data);
        assert_eq!(state.tools.len(), 1);
        assert_eq!(state.tools[0].id, "call_1");
        assert_eq!(state.tools[0].name, "read_file");
        assert_eq!(state.tools[0].arguments, r#"{"path":"x"}"#);
    }

    #[test]
    fn done_markers_comments_and_errors_are_classified() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        assert_eq!(parse_chunk("data: [DONE]", &sink).0, SseLine::Done);
        assert_eq!(parse_chunk(": connected", &sink).0, SseLine::Skip);
        assert_eq!(parse_chunk("", &sink).0, SseLine::Skip);
        let (result, _) = parse_chunk(r#"data: {"error":{"message":"upstream 403"}}"#, &sink);
        assert_eq!(result, SseLine::Error);
    }

    #[test]
    fn usage_arrives_on_the_final_chunk() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let mut state = StreamState::default();
        process_sse_line(
            r#"data: {"choices":[{"delta":{"content":"!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
            &mut state,
            &sink,
        );
        let response = finish_stream(state, &ModelId::new(Vendor::Nim, "m"));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 2);
        assert!(!response.truncated, "stop is not truncation");
    }

    #[test]
    fn a_length_finish_marks_the_response_truncated() {
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let mut state = StreamState::default();
        process_sse_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            &mut state,
            &sink,
        );
        let response = finish_stream(state, &ModelId::new(Vendor::Nim, "m"));
        assert!(response.truncated);
    }

    /// A one-shot HTTP/1.1 server that answers `/chat/completions` from a
    /// script: streaming bodies get SSE, plain bodies get JSON. Counts how
    /// many requests asked to stream.
    async fn sse_server(
        stream_reply: &'static str,
        plain_reply: &'static str,
        stream_hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = stream_hits.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 16_384];
                    let mut read = 0usize;
                    // Read until the end of the header block.
                    loop {
                        let Ok(n) = socket.read(&mut buffer[read..]).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        read += n;
                        if buffer[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let length: usize = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let body_start = read - length.min(read);
                    let body =
                        String::from_utf8_lossy(&buffer[body_start.min(read)..read]).to_string();
                    if body.contains("\"stream\":true") {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            stream_reply.len(),
                            stream_reply
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    } else {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            plain_reply.len(),
                            plain_reply
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        port
    }

    fn streamed_request(model: &str) -> CompletionRequest {
        CompletionRequest {
            model: ModelId::new(Vendor::Nim, model),
            messages: vec![Message::user("hello")],
            tools: vec![],
            max_tokens: 64,
            temperature: None,
            thinking_budget: None,
            effort: None,
        }
    }

    #[tokio::test]
    async fn a_streaming_response_is_parsed_into_deltas_and_one_answer() {
        let sse = concat!(
            ": connected\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let plain = serde_json::json!({
            "choices": [{"message": {"content": "should not be used"}, "finish_reason": "stop"}]
        })
        .to_string();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let port = sse_server(sse, Box::leak(plain.into_boxed_str()), hits.clone()).await;

        let provider =
            OpenAiCompatible::nvidia_nim("k").with_endpoint(format!("http://127.0.0.1:{port}"));
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let response = provider
            .complete_streamed(
                &streamed_request("nvidia/nemotron-3-super-120b-a12b"),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(response.content, "Hello");
        assert_eq!(response.usage.input_tokens, 7);
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![
                (false, "Hel".to_string()),
                (true, "hmm".to_string()),
                (false, "lo".to_string()),
            ],
            "deltas must arrive in stream order"
        );
    }

    #[tokio::test]
    async fn a_broken_stream_falls_back_once_and_remembers_the_model() {
        // The stream endpoint errors for this model; the plain endpoint
        // answers. The second streamed call must not retry streaming.
        let sse = "data: {\"error\":{\"message\":\"upstream 403\"}}\n\ndata: [DONE]\n\n";
        let plain = serde_json::json!({
            "choices": [{"message": {"content": "plain answer"}, "finish_reason": "stop"}]
        })
        .to_string();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let port = sse_server(sse, Box::leak(plain.into_boxed_str()), hits.clone()).await;

        let provider =
            OpenAiCompatible::nvidia_nim("k").with_endpoint(format!("http://127.0.0.1:{port}"));
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let response = provider
            .complete_streamed(&streamed_request("meta/muse-glimmer-30b"), &sink)
            .await
            .unwrap();
        assert_eq!(response.content, "plain answer");
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![(false, "plain answer".to_string())],
            "the fallback delivers the whole text as one delta"
        );

        // Second call: no new stream attempt.
        let sink2 = LogSink(std::sync::Mutex::new(Vec::new()));
        let response = provider
            .complete_streamed(&streamed_request("meta/muse-glimmer-30b"), &sink2)
            .await
            .unwrap();
        assert_eq!(response.content, "plain answer");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "streaming is memoised as broken for this model"
        );
    }

    #[tokio::test]
    async fn an_empty_stream_falls_back_to_the_plain_endpoint() {
        // Some models connect but send nothing: that is not an answer.
        let sse = ": connected\n\ndata: [DONE]\n\n";
        let plain = serde_json::json!({
            "choices": [{"message": {"content": "eventually"}, "finish_reason": "stop"}]
        })
        .to_string();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let port = sse_server(sse, Box::leak(plain.into_boxed_str()), hits.clone()).await;

        let provider =
            OpenAiCompatible::nvidia_nim("k").with_endpoint(format!("http://127.0.0.1:{port}"));
        let sink = LogSink(std::sync::Mutex::new(Vec::new()));
        let response = provider
            .complete_streamed(
                &streamed_request("nvidia/nemotron-3.5-lightning-30b-a3b"),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(response.content, "eventually");
    }

    #[test]
    fn every_vendor_has_a_price_for_every_model_it_declares() {
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Usage::default()
        };
        for provider in [
            OpenAiCompatible::openai("k"),
            OpenAiCompatible::deepseek("k"),
            OpenAiCompatible::xai("k"),
            OpenAiCompatible::openrouter("k"),
        ] {
            for model in provider.models() {
                assert!(
                    provider
                        .cost_table()
                        .cost_for(&model.model, &usage)
                        .unwrap()
                        > 0.0,
                    "{}/{model} has no price",
                    provider.vendor.as_str(),
                );
            }
        }
    }
}
