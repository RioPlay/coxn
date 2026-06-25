//! The pump: steers and sets pace, carries no intelligence.
//!
//! The manual agentic loop lives here: call the model, dispatch tools, feed
//! results back, repeat. It paces turns (a tool-hop cap) and is where the gate
//! is enforced, but it never reasons about code. aden directs and gates; the
//! LLM acts; the pump steers.
//!
//! The loop is TUI-agnostic and synchronous in shape so it is unit-testable
//! against the stub model; P1.7 wires it to the TUI, and Phase 2 wires the gate
//! at the edit point (a write tool consults `impact-diff --scope` before its
//! result is accepted). No aden calls happen here yet.

use crate::model::{
    DEFAULT_SYSTEM_PROMPT, Message, Model, ModelError, ModelRequest, Role, call_model,
};
use crate::tools::ToolRegistry;

/// The pace cap: the most tool hops the pump runs inside a single user turn
/// before giving up. Bounds a model that loops; the stub never reaches it.
const MAX_TOOL_HOPS: usize = 32;

/// The conversation and wiring the pump shuttles between the model and the
/// tools. Carries no intelligence: it appends messages, dispatches tool calls,
/// and feeds results back until the model stops calling tools.
pub struct Pump<M: Model> {
    model: M,
    tools: ToolRegistry,
    system: String,
    messages: Vec<Message>,
}

impl<M: Model> Pump<M> {
    /// A pump over `model` and `tools`, starting from the bare system prompt
    /// (the zero-default-context floor) and an empty conversation.
    pub fn new(model: M, tools: ToolRegistry) -> Self {
        Self {
            model,
            tools,
            system: DEFAULT_SYSTEM_PROMPT.to_string(),
            messages: Vec::new(),
        }
    }

    /// The conversation so far, for rendering the transcript.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Append a user message to start (or continue) a turn.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(Message::new(Role::User, text));
    }

    /// Build the request for the current conversation state.
    fn request(&self) -> ModelRequest {
        ModelRequest {
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.names(),
        }
    }

    /// Drive one user turn to completion: call the model, dispatch any tool
    /// calls, feed their results back as tool messages, and repeat until the
    /// model returns no tool calls or the hop cap is reached. Every message is
    /// appended to the conversation. Returns the final assistant text.
    pub async fn run_turn(&mut self) -> Result<String, ModelError> {
        for _ in 0..MAX_TOOL_HOPS {
            let response = call_model(&self.model, self.request()).await?;
            self.messages.push(response.message.clone());

            if response.tool_calls.is_empty() {
                return Ok(response.message.content);
            }

            // Dispatch each tool call and feed the result (or error) back as a
            // tool message. The gate hook for write tools lands here in Phase 2.
            for call in &response.tool_calls {
                let content = match self.tools.dispatch(call) {
                    Ok(out) => out,
                    Err(err) => err,
                };
                self.messages.push(Message::new(Role::Tool, content));
            }
        }
        Err(ModelError::Backend("tool-hop cap reached".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelResponse, ToolCall};
    use crate::tools::EchoTool;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A test model that replays a queued script of responses, so we can drive
    /// the loop's tool-dispatch-and-feed-back path deterministically.
    struct ScriptedModel {
        responses: Mutex<VecDeque<ModelResponse>>,
    }

    impl ScriptedModel {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    impl Model for ScriptedModel {
        async fn call(&self, _request: ModelRequest) -> Result<ModelResponse, ModelError> {
            let next = self.responses.lock().expect("lock").pop_front();
            next.ok_or_else(|| ModelError::Backend("script exhausted".to_string()))
        }
    }

    fn assistant(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, text),
            tool_calls: Vec::new(),
        }
    }

    fn calls_echo(text: &str, arguments: &str) -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, text),
            tool_calls: vec![ToolCall {
                id: "t1".to_string(),
                name: "echo".to_string(),
                arguments: arguments.to_string(),
            }],
        }
    }

    fn echo_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EchoTool));
        r
    }

    #[tokio::test]
    async fn stub_turn_returns_final_text_and_records_transcript() {
        use crate::model::StubModel;
        let mut pump = Pump::new(StubModel, ToolRegistry::new());
        pump.push_user("hi");
        let out = pump.run_turn().await.expect("turn completes");
        assert_eq!(out, "stub: hi");
        let roles: Vec<Role> = pump.messages().iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
    }

    #[tokio::test]
    async fn tool_call_is_dispatched_and_result_fed_back() {
        let model = ScriptedModel::new(vec![calls_echo("calling", "ping"), assistant("done")]);
        let mut pump = Pump::new(model, echo_registry());
        pump.push_user("go");
        let out = pump.run_turn().await.expect("turn completes");
        assert_eq!(out, "done");
        // user, assistant(tool call), tool(result), assistant(final)
        let transcript: Vec<(Role, &str)> = pump
            .messages()
            .iter()
            .map(|m| (m.role, m.content.as_str()))
            .collect();
        assert_eq!(
            transcript,
            vec![
                (Role::User, "go"),
                (Role::Assistant, "calling"),
                (Role::Tool, "ping"),
                (Role::Assistant, "done"),
            ]
        );
    }

    #[tokio::test]
    async fn unknown_tool_error_is_fed_back_not_fatal() {
        let bad_call = ModelResponse {
            message: Message::new(Role::Assistant, "try"),
            tool_calls: vec![ToolCall {
                id: "t1".to_string(),
                name: "missing".to_string(),
                arguments: String::new(),
            }],
        };
        let model = ScriptedModel::new(vec![bad_call, assistant("recovered")]);
        let mut pump = Pump::new(model, echo_registry());
        pump.push_user("go");
        let out = pump.run_turn().await.expect("turn completes");
        assert_eq!(out, "recovered");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool message was fed back");
        assert_eq!(tool_msg.content, "unknown tool: missing");
    }

    #[tokio::test]
    async fn hop_cap_stops_a_looping_model() {
        // A model that always asks for a tool exhausts the hop cap.
        let script: Vec<ModelResponse> = (0..MAX_TOOL_HOPS + 1)
            .map(|_| calls_echo("again", "x"))
            .collect();
        let model = ScriptedModel::new(script);
        let mut pump = Pump::new(model, echo_registry());
        pump.push_user("loop");
        let err = pump.run_turn().await.expect_err("cap reached");
        assert!(matches!(err, ModelError::Backend(_)));
    }
}
