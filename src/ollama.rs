//! Native Ollama `/api/chat` backend.
//!
//! Ollama speaks its own chat protocol (NDJSON streaming, tools as
//! `{"type":"function","function":{...}}`). Its OpenAI-compat `/v1/chat/completions`
//! layer is served by `openai.rs`; this backend targets the native endpoint so
//! streaming plus tool calls hold for local users (Ollama's OpenAI-compat layer
//! historically drops tool-call deltas under streaming).
//!
//! The pump stays provider-neutral: this is just another `AnyModel` variant, and
//! the system prompt / message tool-linkage / Usage are mapped to the seam types
//! in `model.rs`. Ollama tool calls carry no id; coxn synthesizes one for the
//! pump's tool-call/result pairing (`ollama-<seq>`).

use std::io::BufRead;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    Message, Model, ModelError, ModelRequest, ModelResponse, Role, ToolCall, Usage,
};

/// Native Ollama base URL (no `/v1`). Overridable via the provider instance's
/// `base_url`, which typically points at `http://localhost:11434`.
const DEFAULT_BASE_URL: &str = "http://localhost:11434";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const BODY_TIMEOUT: Duration = Duration::from_secs(300);

fn ollama_agent(streaming: bool) -> ureq::Agent {
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT));
    if !streaming {
        config = config.timeout_recv_body(Some(BODY_TIMEOUT));
    }
    config.build().into()
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// A wire `tool_calls[i].function` shape sent back to Ollama when the assistant
/// message we recorded (with its tool calls) is replayed into a later request.
#[derive(Serialize)]
struct WireFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct WireToolCall {
    function: WireFunction,
}

/// The outbound `tools[i].function` shape.
#[derive(Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}

/// The outbound `/api/chat` request.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
}

/// Parse the tool-call argument payload (coxn's `ToolCall.arguments`, a raw
/// string from the pump) into the JSON object Ollama expects; a non-JSON
/// argument degrades to a JSON string so the call still round-trips.
fn args_as_object(arguments: &str) -> serde_json::Value {
    if arguments.trim().is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::Value::String(arguments.to_string()),
    }
}

/// Stringify a JSON object for the pump's raw-`arguments` field.
fn object_as_args(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn build_messages(request: &ModelRequest) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        out.push(json!({"role":"system","content": &request.system}));
    }
    for m in &request.messages {
        let mut obj = json!({"role": role_str(m.role), "content": &m.content});
        let calls: Vec<WireToolCall> = m
            .tool_calls
            .iter()
            .map(|c| WireToolCall {
                function: WireFunction {
                    name: c.name.clone(),
                    arguments: args_as_object(&c.arguments),
                },
            })
            .collect();
        if !calls.is_empty() {
            obj["tool_calls"] = json!(calls);
        }
        if let Some(id) = &m.tool_call_id {
            obj["tool_call_id"] = json!(id);
        }
        out.push(obj);
    }
    out
}

fn build_tools(request: &ModelRequest) -> Vec<WireTool> {
    request
        .tools
        .iter()
        .map(|t| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

fn to_wire<'a>(model: &'a str, request: &ModelRequest, stream: bool) -> ChatRequest<'a> {
    ChatRequest {
        model,
        messages: build_messages(request),
        stream,
        tools: build_tools(request),
    }
}

/// A streamed (or buffered) `message.tool_calls[i].function`.
#[derive(Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    function: Option<StreamFunction>,
    // Ollama newer versions may attach an `id` to tool calls; capture it so the
    // pump's pairing stays faithful when present.
    id: Option<String>,
}

#[derive(Deserialize)]
struct StreamMessage {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

/// A single NDJSON line from `/api/chat` (stream) or the whole body (buffered).
#[derive(Deserialize)]
struct ChatChunk {
    message: Option<StreamMessage>,
    done: Option<bool>,
    // Usage fields (final chunk).
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
}

/// Folder for assembling streamed content + tool calls into a `ModelResponse`.
#[derive(Default)]
struct StreamState {
    content: String,
    calls: Vec<ToolCall>,
    prompt_tokens: u64,
    completion_tokens: u64,
}

impl StreamState {
    fn apply(&mut self, chunk: ChatChunk, on_delta: &mut dyn FnMut(&str) -> bool) -> bool {
        if let Some(msg) = chunk.message {
            if let Some(text) = msg.content
                && !text.is_empty()
            {
                if !on_delta(&text) {
                    return false;
                }
                self.content.push_str(&text);
            }
            if let Some(tcs) = msg.tool_calls {
                for tc in tcs {
                    let Some(func) = tc.function else { continue };
                    let name = func.name.unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    // Skip a tool call we have already accumulated (Ollama may
                    // echo completed tool calls in the final chunk).
                    let args = func
                        .arguments
                        .as_ref()
                        .map(object_as_args)
                        .unwrap_or_default();
                    let id = tc
                        .id
                        .unwrap_or_else(|| format!("ollama-{}", self.calls.len()));
                    let dup = self
                        .calls
                        .iter()
                        .any(|existing| existing.name == name && existing.arguments == args);
                    if !dup {
                        self.calls.push(ToolCall {
                            id,
                            name,
                            arguments: args,
                        });
                    }
                }
            }
        }
        if let Some(p) = chunk.prompt_eval_count {
            self.prompt_tokens = p;
        }
        if let Some(c) = chunk.eval_count {
            self.completion_tokens = c;
        }
        true
    }

    fn finish(self) -> ModelResponse {
        let total = self.prompt_tokens + self.completion_tokens;
        let usage = (self.prompt_tokens > 0 || self.completion_tokens > 0).then_some(Usage {
            prompt_tokens: self.prompt_tokens as u32,
            completion_tokens: self.completion_tokens as u32,
            total_tokens: total as u32,
        });
        ModelResponse {
            message: Message::new(Role::Assistant, self.content),
            tool_calls: self.calls,
            usage,
        }
    }
}

fn snippet(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

/// The native Ollama chat backend. Constructed by `ollama_model` from a provider
/// instance with `driver = "ollama"`.
pub struct OllamaModel {
    pub base_url: String,
    pub model: String,
}

impl OllamaModel {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url: if base_url.is_empty() {
                DEFAULT_BASE_URL.to_string()
            } else {
                base_url
            },
            model,
        }
    }

    fn url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }
}

/// Best-effort reachability probe: GET `{base_url}/api/tags` (Ollama's
/// model-list endpoint). Returns `true` on a 2xx. Used by `coxn auth status` /
/// `coxn doctor` to confirm an Ollama instance is up without a key.
pub fn reachable(base_url: &str) -> bool {
    let base = if base_url.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url
    };
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let agent = ollama_agent(false);
    match agent.get(&url).call() {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

impl Model for OllamaModel {
    async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let url = self.url();
        let body = to_wire(&self.model, &request, false);
        let agent = ollama_agent(false);
        let mut response = agent
            .post(&url)
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
        let chunk: ChatChunk = serde_json::from_str(&text).map_err(|e| {
            ModelError::Backend(format!(
                "decoding ollama response failed: {e}; body: {}",
                snippet(&text)
            ))
        })?;
        let mut state = StreamState::default();
        let mut delta = |_d: &str| true;
        state.apply(chunk, &mut delta);
        let resp = state.finish();
        if resp.message.content.is_empty() && resp.tool_calls.is_empty() {
            return Err(ModelError::Backend(
                "ollama returned an empty response".into(),
            ));
        }
        Ok(resp)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        on_delta: &mut dyn FnMut(&str) -> bool,
    ) -> Result<ModelResponse, ModelError> {
        let url = self.url();
        let body = to_wire(&self.model, &request, true);
        let agent = ollama_agent(true);
        let mut response = agent
            .post(&url)
            .send_json(&body)
            .map_err(|e| ModelError::Backend(format!("request to {url} failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(ModelError::Backend(format!(
                "{url} returned {status}: {}",
                snippet(&text)
            )));
        }
        // Ollama streams NDJSON: one JSON object per line, terminated by a
        // `{"done":true,...}` line.
        let mut state = StreamState::default();
        let reader = std::io::BufReader::new(response.body_mut().as_reader());
        for line in reader.lines() {
            let line = line
                .map_err(|e| ModelError::Backend(format!("reading ollama stream failed: {e}")))?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(chunk) = serde_json::from_str::<ChatChunk>(&line) else {
                continue;
            };
            let is_done = chunk.done.unwrap_or(false);
            if !state.apply(chunk, on_delta) {
                break;
            }
            if is_done {
                break;
            }
        }
        let response = state.finish();
        if response.message.content.is_empty() && response.tool_calls.is_empty() {
            return Err(ModelError::Backend(
                "ollama returned an empty stream".into(),
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip_object_and_string() {
        // A JSON object argument is embedded as an object on the wire.
        assert_eq!(args_as_object(r#"{"a":1}"#), json!({"a":1}));
        // Non-JSON degrades to a JSON string so the call still round-trips.
        assert_eq!(
            args_as_object("plain"),
            serde_json::Value::String("plain".into())
        );
        assert_eq!(args_as_object(""), json!({}));
        assert_eq!(object_as_args(&json!({"a":1})), r#"{"a":1}"#);
        assert_eq!(object_as_args(&serde_json::Value::String("s".into())), "s");
    }

    #[test]
    fn stream_state_folds_content_tools_usage() {
        let mut state = StreamState::default();
        let mut keep = true;
        let mut emit = |d: &str| {
            keep = keep && !d.is_empty();
            true
        };
        state.apply(
            ChatChunk {
                message: Some(StreamMessage {
                    content: Some("Hello ".into()),
                    tool_calls: None,
                }),
                done: Some(false),
                prompt_eval_count: None,
                eval_count: None,
            },
            &mut emit,
        );
        let _ = keep;
        state.apply(
            ChatChunk {
                message: Some(StreamMessage {
                    content: Some("world".into()),
                    tool_calls: Some(vec![StreamToolCall {
                        function: Some(StreamFunction {
                            name: Some("edit".into()),
                            arguments: Some(json!({"path":"a.rs"})),
                        }),
                        id: None,
                    }]),
                }),
                done: Some(false),
                prompt_eval_count: None,
                eval_count: None,
            },
            &mut emit,
        );
        // Dedup: the same tool call echoed in the final chunk is not repeated.
        state.apply(
            ChatChunk {
                message: Some(StreamMessage {
                    content: None,
                    tool_calls: Some(vec![StreamToolCall {
                        function: Some(StreamFunction {
                            name: Some("edit".into()),
                            arguments: Some(json!({"path":"a.rs"})),
                        }),
                        id: None,
                    }]),
                }),
                done: Some(true),
                prompt_eval_count: Some(12),
                eval_count: Some(7),
            },
            &mut emit,
        );
        let resp = state.finish();
        assert_eq!(resp.message.content, "Hello world");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "edit");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"path":"a.rs"}"#);
        let usage = resp.usage.expect("usage present on the final chunk");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 19);
    }

    #[test]
    fn to_wire_prepends_system_and_maps_roles() {
        let req = ModelRequest {
            system: "be terse".into(),
            messages: vec![Message::new(Role::User, "hi")],
            tools: Vec::new(),
            thinking: None,
        };
        let wire = to_wire("qwen", &req, true);
        assert_eq!(wire.model, "qwen");
        assert!(wire.stream);
        assert_eq!(wire.messages.len(), 2);
        assert_eq!(wire.messages[0]["role"], "system");
        assert_eq!(wire.messages[0]["content"], "be terse");
        assert_eq!(wire.messages[1]["role"], "user");
    }
}
