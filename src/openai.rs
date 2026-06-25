//! OpenAI-compatible chat-completions backend.
//!
//! One backend covers LM Studio, Ollama, OpenRouter (-> Claude / GPT / Gemini /
//! Llama / ...), vLLM, and OpenAI: they all speak
//! `POST {base_url}/chat/completions`. The provider is selected by data (a
//! `{base_url, model, key}` spec), not a type. See DESIGN.adoc Phase 3.
//!
//! Text and tool turns, streaming or buffered. The advertised tool defs are sent
//! as OpenAI `function` tools; assistant `tool_calls` and `tool` results thread
//! through the conversation, and tool calls are parsed back out of the response.
//! `call` buffers the whole reply; `stream` reads the SSE response and emits text
//! fragments live (assembling tool-call deltas by index). Ollama's native
//! `/api/chat` profile (its OpenAI-compat layer drops tool calls under streaming)
//! is a later addition; the OpenAI-compat path here covers LM Studio, OpenAI,
//! OpenRouter, and vLLM.

use std::io::BufRead;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{
    Message, Model, ModelError, ModelRequest, ModelResponse, Role, ToolCall, Usage,
};

/// Well-known local providers, probed in order for zero-config startup.
const LOCAL_CANDIDATES: [&str; 2] = ["http://localhost:11434/v1", "http://localhost:1234/v1"];

/// How long to wait to establish a connection before giving up (an unreachable
/// or down server fails here instead of hanging the TUI).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the server to start responding (time to first byte).
/// Generous, because a cold local model may need to load before the first token;
/// it bounds a server that accepts the connection but then stalls.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
/// Total receive budget for a non-streaming reply. Not applied to streaming,
/// where a long reply legitimately takes a while and only a stall should fail.
const BODY_TIMEOUT: Duration = Duration::from_secs(300);

/// An HTTP agent for a model call: never treat a non-2xx as a transport error
/// (so the server's own message reaches the status line), and bound connect and
/// time-to-first-byte so a dead or stalled server cannot hang the loop. A total
/// body timeout is applied only to non-streaming calls.
fn model_agent(streaming: bool) -> ureq::Agent {
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT));
    if !streaming {
        config = config.timeout_recv_body(Some(BODY_TIMEOUT));
    }
    config.build().into()
}

/// Probe the well-known local providers (Ollama, LM Studio) and return the
/// `(base_url, model)` of the first that responds. Prefers a model already
/// loaded in memory ("hot") when the server reports load state, so `cargo run`
/// does not pick a cold model that then stalls or fails to load; falls back to
/// the first advertised model otherwise. `None` when nothing is running.
pub fn detect() -> Option<(String, String)> {
    LOCAL_CANDIDATES.iter().find_map(|base| {
        let model = loaded_models(base, None)
            .and_then(|hot| hot.into_iter().next())
            .or_else(|| first_model(base))?;
        Some((base.to_string(), model))
    })
}

/// Best-effort set of currently-loaded ("hot") model ids, via a server's native
/// load-state endpoint. The chat-completions `/v1/models` does not carry load
/// state; some local servers expose it at `/api/v0/models` with a `state` field.
/// `None` for any server that does not expose it -- callers then simply omit the
/// hot/cold distinction. The `/v1` suffix is dropped to reach the native root.
pub fn loaded_models(base_url: &str, key: Option<&str>) -> Option<Vec<String>> {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let url = format!("{root}/api/v0/models");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(1500)))
        .build()
        .into();
    let mut request = agent.get(url);
    if let Some(k) = key {
        request = request.header("Authorization", &format!("Bearer {k}"));
    }
    let mut response = request.call().ok()?;
    let parsed: NativeModels = response.body_mut().read_json().ok()?;
    Some(
        parsed
            .data
            .into_iter()
            .filter(|m| m.state.as_deref() == Some("loaded"))
            .map(|m| m.id)
            .collect(),
    )
}

#[derive(Deserialize)]
struct NativeModels {
    #[serde(default)]
    data: Vec<NativeModel>,
}

#[derive(Deserialize)]
struct NativeModel {
    id: String,
    #[serde(default)]
    state: Option<String>,
}

/// The first model id advertised by an OpenAI-compatible `/models` endpoint, or
/// `None` if the endpoint is unreachable or empty. A short timeout keeps a
/// stalled server from blocking startup; a refused connection fails instantly.
fn first_model(base_url: &str) -> Option<String> {
    fetch_models(base_url, None, 800)?.into_iter().next()
}

/// Every model id advertised by an OpenAI-compatible `/models` endpoint (LM
/// Studio and Ollama list all installed models here, loaded or not), or `None`
/// when the endpoint is unreachable. A longer timeout than [`first_model`]'s
/// since this is on-demand (`/model`), not the startup path. The key, when set,
/// authorizes a cloud provider's listing (OpenRouter, OpenAI, ...).
pub fn list_models(base_url: &str, key: Option<&str>) -> Option<Vec<String>> {
    fetch_models(base_url, key, 2500)
}

/// GET `{base_url}/models` and return the advertised ids. Shared by startup
/// detection and `/model`; `timeout_ms` bounds a stalled server.
fn fetch_models(base_url: &str, key: Option<&str>, timeout_ms: u64) -> Option<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .build()
        .into();
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = agent.get(url);
    if let Some(k) = key {
        request = request.header("Authorization", &format!("Bearer {k}"));
    }
    let mut response = request.call().ok()?;
    let models: ModelsResponse = response.body_mut().read_json().ok()?;
    Some(models.data.into_iter().map(|m| m.id).collect())
}

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// An OpenAI-compatible chat backend bound to one model spec.
pub struct OpenAiCompatModel {
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl OpenAiCompatModel {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key,
        }
    }
}

/// The OpenAI chat role string for a coxn role.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    stream: bool,
    /// Ask for a final usage chunk when streaming; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// Reasoning effort for models that support it; omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Token usage as reported on the wire (`usage` object). Shared by the buffered
/// and streamed paths.
#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

impl WireUsage {
    fn into_usage(self) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
        }
    }
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// A tool/function call, in OpenAI's `{id, type:"function", function:{name,
/// arguments}}` shape. `arguments` is a JSON string. Used both directions.
#[derive(Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Serialize, Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

/// A tool definition offered to the model (request side).
#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}

#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

/// Map a coxn tool call to the wire shape.
fn wire_call(tc: &ToolCall) -> WireToolCall {
    WireToolCall {
        id: tc.id.clone(),
        kind: "function".to_string(),
        function: WireFunction {
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        },
    }
}

/// Build the chat-completions request body from coxn's request. The bare system
/// prompt (when non-empty) leads as a `system` message; the rest map by role,
/// carrying any tool calls (assistant) and answered id (tool). Advertised tool
/// defs become OpenAI `function` tools.
fn to_wire<'a>(model: &'a str, request: &ModelRequest, stream: bool) -> ChatRequest<'a> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(WireMessage {
            role: "system",
            content: Some(request.system.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for m in &request.messages {
        // A tool-call-only assistant turn has no text; send null content there.
        let content = if m.content.is_empty() && !m.tool_calls.is_empty() {
            None
        } else {
            Some(m.content.clone())
        };
        messages.push(WireMessage {
            role: role_str(m.role),
            content,
            tool_calls: m.tool_calls.iter().map(wire_call).collect(),
            tool_call_id: m.tool_call_id.clone(),
        });
    }
    let tools = request
        .tools
        .iter()
        .map(|d| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: d.name.clone(),
                description: d.description.clone(),
                parameters: d.parameters.clone(),
            },
        })
        .collect();
    ChatRequest {
        model,
        messages,
        tools,
        stream,
        // Ask for the final usage chunk only when streaming (it is in the body
        // for a buffered call).
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort: request.thinking.and_then(|t| t.effort()),
    }
}

/// Extract the assistant text and any tool calls from a chat-completions
/// response. A tool-call-only response (null content) is valid.
fn from_wire(response: ChatResponse) -> Result<ModelResponse, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::Backend("model returned no choices".to_string()))?;
    let content = choice.message.content.unwrap_or_default();
    let tool_calls: Vec<ToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        })
        .collect();
    if content.is_empty() && tool_calls.is_empty() {
        return Err(ModelError::Backend(
            "model returned an empty message".to_string(),
        ));
    }
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, content),
        tool_calls,
        usage: response.usage.map(WireUsage::into_usage),
    })
}

// --- streaming (Server-Sent Events) ---

/// One streamed chunk: `{"choices":[{"delta":{...}}]}` (and a trailing
/// `{"usage":{...}}` chunk when `stream_options.include_usage` was sent).
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

/// The incremental piece in a chunk: a text fragment and/or tool-call fragments.
#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

/// A tool-call fragment: identified by `index`, its fields arrive across chunks.
#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulates streamed chunks into a single response: text appended in order,
/// tool calls assembled by index (id/name set once, arguments concatenated),
/// and the usage from the trailing chunk.
#[derive(Default)]
struct StreamState {
    content: String,
    calls: Vec<ToolCall>,
    usage: Option<Usage>,
}

impl StreamState {
    /// Fold one chunk in, emitting any text fragment through `on_delta`. Returns
    /// `false` if `on_delta` requested cancellation.
    fn apply(&mut self, chunk: StreamChunk, on_delta: &mut dyn FnMut(&str) -> bool) -> bool {
        let mut keep_going = true;
        if let Some(u) = chunk.usage {
            self.usage = Some(u.into_usage());
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                if !on_delta(&text) {
                    keep_going = false;
                }
                self.content.push_str(&text);
            }
            for tc in choice.delta.tool_calls {
                while self.calls.len() <= tc.index {
                    self.calls.push(ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let slot = &mut self.calls[tc.index];
                if let Some(id) = tc.id {
                    slot.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        slot.name = name;
                    }
                    if let Some(args) = f.arguments {
                        slot.arguments.push_str(&args);
                    }
                }
            }
        }
        keep_going
    }

    fn finish(self) -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, self.content),
            tool_calls: self
                .calls
                .into_iter()
                .filter(|c| !c.name.is_empty())
                .collect(),
            usage: self.usage,
        }
    }
}

/// Fold one SSE line, returning `true` to stop the read loop -- at the `[DONE]`
/// sentinel or when `on_delta` requested cancellation. A blank, comment, or
/// unparseable line is skipped (`false`). Lenient by design: providers vary.
fn fold_sse_line(
    line: &str,
    state: &mut StreamState,
    on_delta: &mut dyn FnMut(&str) -> bool,
) -> bool {
    let Some(data) = line.strip_prefix("data:") else {
        return false;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return true;
    }
    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
        // apply returns false when cancellation was requested -> stop.
        return !state.apply(chunk, on_delta);
    }
    false
}

/// Trim a body to a short, single-line snippet for an error message.
fn snippet(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

impl Model for OpenAiCompatModel {
    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        // ureq is blocking; the pump already blocks on a turn, so this is fine
        // for the single-threaded loop. (Revisit with an async client if the TUI
        // needs to stay live during a call.)
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = to_wire(&self.model, &request, false);

        let agent = model_agent(false);
        let mut builder = agent.post(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", &format!("Bearer {key}"));
        }
        let mut response = builder
            .send_json(&body)
            .map_err(|e| ModelError::Backend(format!("request to {url} failed: {e}")))?;

        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ModelError::Backend(format!("reading response from {url} failed: {e}")))?;
        if !status.is_success() {
            return Err(ModelError::Backend(format!(
                "{url} returned {status}: {}",
                snippet(&text)
            )));
        }
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
            ModelError::Backend(format!(
                "decoding response failed: {e}; body: {}",
                snippet(&text)
            ))
        })?;
        from_wire(parsed)
    }

    // Streaming turn: same request with `stream: true`, then read the SSE body
    // line by line, emitting text fragments through `on_delta` as they arrive and
    // assembling the full response (text + tool calls) to return. ureq is
    // blocking, and coxn's loop is single-threaded, so the read loop drives the
    // sink directly; the TUI repaints per fragment from within it.
    async fn stream(
        &self,
        request: ModelRequest,
        on_delta: &mut dyn FnMut(&str) -> bool,
    ) -> Result<ModelResponse, ModelError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = to_wire(&self.model, &request, true);

        let agent = model_agent(true);
        let mut builder = agent.post(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", &format!("Bearer {key}"));
        }
        let mut response = builder
            .send_json(&body)
            .map_err(|e| ModelError::Backend(format!("request to {url} failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            // On an error the body is JSON, not a stream: read it for the message.
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(ModelError::Backend(format!(
                "{url} returned {status}: {}",
                snippet(&text)
            )));
        }

        let mut state = StreamState::default();
        let reader = std::io::BufReader::new(response.body_mut().as_reader());
        for line in reader.lines() {
            let line =
                line.map_err(|e| ModelError::Backend(format!("reading stream failed: {e}")))?;
            if fold_sse_line(&line, &mut state, on_delta) {
                break;
            }
        }

        let response = state.finish();
        if response.message.content.is_empty() && response.tool_calls.is_empty() {
            return Err(ModelError::Backend(
                "model returned an empty stream".to_string(),
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolDef;

    fn request(system: &str, turns: &[(Role, &str)]) -> ModelRequest {
        ModelRequest {
            system: system.to_string(),
            messages: turns
                .iter()
                .map(|(role, text)| Message::new(*role, *text))
                .collect(),
            tools: Vec::new(),
            thinking: None,
        }
    }

    #[test]
    fn to_wire_leads_with_system_then_maps_roles() {
        let req = request("be terse", &[(Role::User, "hi"), (Role::Assistant, "yo")]);
        let wire = to_wire("local", &req, false);
        assert_eq!(wire.model, "local");
        assert!(!wire.stream);
        let roles: Vec<&str> = wire.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec!["system", "user", "assistant"]);
        assert_eq!(wire.messages[0].content.as_deref(), Some("be terse"));
    }

    #[test]
    fn to_wire_omits_empty_system() {
        let req = request("", &[(Role::User, "hi")]);
        let wire = to_wire("local", &req, false);
        assert_eq!(
            wire.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec!["user"]
        );
    }

    #[test]
    fn request_serializes_to_openai_shape() {
        let req = request("sys", &[(Role::User, "q")]);
        let json = serde_json::to_value(to_wire("m", &req, false)).unwrap();
        assert_eq!(json["model"], "m");
        assert_eq!(json["stream"], false);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "q");
    }

    #[test]
    fn from_wire_pulls_the_first_choice_text() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hello there"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let out = from_wire(resp).unwrap();
        assert_eq!(out.message.role, Role::Assistant);
        assert_eq!(out.message.content, "hello there");
    }

    #[test]
    fn from_wire_errors_on_empty_choices() {
        let resp: ChatResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(matches!(from_wire(resp), Err(ModelError::Backend(_))));
    }

    #[test]
    fn request_carries_tools_and_tool_call_linkage() {
        let mut req = request("", &[(Role::User, "go")]);
        req.messages.push(Message::assistant(
            "",
            vec![ToolCall {
                id: "c1".into(),
                name: "aden_asm".into(),
                arguments: r#"{"anchor":"foo"}"#.into(),
            }],
        ));
        req.messages.push(Message::tool_result("c1", "result text"));
        req.tools = vec![ToolDef {
            name: "aden_asm".into(),
            description: "assemble".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let json = serde_json::to_value(to_wire("m", &req, false)).unwrap();
        // Tool defs become OpenAI function tools.
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "aden_asm");
        // The assistant turn carries its tool call (null content omitted).
        let assistant = &json["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant.get("content").is_none());
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "aden_asm");
        // The tool result carries the answered id and role.
        let tool = &json["messages"][2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "c1");
        assert_eq!(tool["content"], "result text");
    }

    #[test]
    fn models_list_parses_all_ids() {
        let json = r#"{"object":"list","data":[{"id":"llama3.1","object":"model"},{"id":"qwen"}]}"#;
        let models: ModelsResponse = serde_json::from_str(json).unwrap();
        let ids: Vec<String> = models.data.into_iter().map(|m| m.id).collect();
        // All advertised models surface (not just the first), and the first is
        // still the auto-detect default.
        assert_eq!(ids, vec!["llama3.1".to_string(), "qwen".to_string()]);
        assert_eq!(ids.first().map(String::as_str), Some("llama3.1"));
    }

    #[test]
    fn streaming_request_asks_for_usage() {
        let req = request("", &[(Role::User, "hi")]);
        // Streaming sets stream_options.include_usage; buffered omits it.
        let streamed = serde_json::to_value(to_wire("m", &req, true)).unwrap();
        assert_eq!(streamed["stream_options"]["include_usage"], true);
        let buffered = serde_json::to_value(to_wire("m", &req, false)).unwrap();
        assert!(buffered.get("stream_options").is_none());
    }

    #[test]
    fn from_wire_carries_usage() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],
            "usage":{"prompt_tokens":1234,"completion_tokens":56,"total_tokens":1290}}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let out = from_wire(resp).unwrap();
        let u = out.usage.expect("usage present");
        assert_eq!(u.prompt_tokens, 1234);
        assert_eq!(u.total_tokens, 1290);
    }

    #[test]
    fn streaming_captures_the_trailing_usage_chunk() {
        let mut state = StreamState::default();
        for line in [
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":99,"completion_tokens":3,"total_tokens":102}}"#,
            "data: [DONE]",
        ] {
            let mut sink = |_: &str| true;
            if fold_sse_line(line, &mut state, &mut sink) {
                break;
            }
        }
        let resp = state.finish();
        assert_eq!(resp.message.content, "hi");
        assert_eq!(resp.usage.map(|u| u.prompt_tokens), Some(99));
    }

    #[test]
    fn native_models_parses_only_loaded_ids() {
        let json = r#"{"data":[
            {"id":"hot-a","state":"loaded","type":"llm"},
            {"id":"cold-b","state":"not-loaded","type":"llm"},
            {"id":"hot-c","state":"loaded"}
        ]}"#;
        let parsed: NativeModels = serde_json::from_str(json).unwrap();
        let loaded: Vec<String> = parsed
            .data
            .into_iter()
            .filter(|m| m.state.as_deref() == Some("loaded"))
            .map(|m| m.id)
            .collect();
        assert_eq!(loaded, vec!["hot-a".to_string(), "hot-c".to_string()]);
    }

    #[test]
    fn from_wire_parses_tool_calls() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"aden_asm","arguments":"{\"anchor\":\"foo\"}"}}]}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let out = from_wire(resp).unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "c1");
        assert_eq!(out.tool_calls[0].name, "aden_asm");
        assert_eq!(out.tool_calls[0].arguments, r#"{"anchor":"foo"}"#);
    }

    /// Drive the SSE folder over a scripted stream and collect the deltas.
    fn fold_stream(lines: &[&str]) -> (ModelResponse, Vec<String>) {
        let mut state = StreamState::default();
        let mut deltas = Vec::new();
        for line in lines {
            let mut sink = |d: &str| {
                deltas.push(d.to_string());
                true
            };
            if fold_sse_line(line, &mut state, &mut sink) {
                break;
            }
        }
        (state.finish(), deltas)
    }

    #[test]
    fn streaming_assembles_text_deltas_in_order() {
        let (resp, deltas) = fold_stream(&[
            r#"data: {"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}"#,
            ": keep-alive comment is ignored",
            "",
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
            "data: [DONE]",
            r#"data: {"choices":[{"delta":{"content":"AFTER-DONE"}}]}"#,
        ]);
        assert_eq!(deltas, vec!["Hel", "lo", " world"]);
        assert_eq!(resp.message.content, "Hello world");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn streaming_stops_when_the_sink_cancels() {
        let mut state = StreamState::default();
        let mut seen = 0;
        let lines = [
            r#"data: {"choices":[{"delta":{"content":"one"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"two"}}]}"#,
        ];
        for line in lines {
            let mut sink = |_d: &str| {
                seen += 1;
                false // request cancellation on the first fragment
            };
            if fold_sse_line(line, &mut state, &mut sink) {
                break;
            }
        }
        assert_eq!(seen, 1, "sink fires once, then the loop stops");
        assert_eq!(state.finish().message.content, "one");
    }

    #[test]
    fn streaming_assembles_tool_calls_across_chunks() {
        let (resp, deltas) = fold_stream(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"edit","arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            "data: [DONE]",
        ]);
        // No text fragments emitted for a tool-only stream.
        assert!(deltas.is_empty());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "c1");
        assert_eq!(resp.tool_calls[0].name, "edit");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
    }
}
