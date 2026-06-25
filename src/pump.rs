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

use crate::gate::{Gate, GateOutcome};
use crate::model::{
    DEFAULT_SYSTEM_PROMPT, Message, Model, ModelError, ModelRequest, Role, ToolCall, call_model,
};
use crate::tools::ToolRegistry;

/// Dispatch a tool call, flattening the result/error into the text fed back to
/// the model (an unknown tool or a tool error is information, not a failure).
fn dispatch_result(tools: &ToolRegistry, call: &ToolCall) -> String {
    match tools.dispatch(call) {
        Ok(out) => out,
        Err(err) => err,
    }
}

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
    /// The blast-radius gate consulted before a mutating tool runs. None = no
    /// gate (Phase 1 behavior); Some = aden directs edits.
    gate: Option<Box<dyn Gate>>,
    /// The most recent gate block, for the TUI to surface as a modal.
    last_block: Option<GateOutcome>,
}

impl<M: Model> Pump<M> {
    /// A pump over `model` and `tools`, starting from the bare system prompt
    /// (the zero-default-context floor) and an empty conversation, no gate.
    pub fn new(model: M, tools: ToolRegistry) -> Self {
        Self {
            model,
            tools,
            system: DEFAULT_SYSTEM_PROMPT.to_string(),
            messages: Vec::new(),
            gate: None,
            last_block: None,
        }
    }

    /// Install the blast-radius gate; mutating tools are then checked before
    /// they run.
    pub fn set_gate(&mut self, gate: Box<dyn Gate>) {
        self.gate = Some(gate);
    }

    /// Swap the model backend, keeping the conversation, tools, gate, and
    /// context. Lets `/model` switch models mid-session (selection is data, not
    /// a type).
    pub fn set_model(&mut self, model: M) {
        self.model = model;
    }

    /// Load the scope-defined context for a named task into the system prompt.
    /// This is the one sanctioned departure from the zero-default-context floor:
    /// no task means a bare prompt; a named task means aden's scope defines the
    /// exact context, and coxn loads exactly that and nothing more.
    pub fn set_context(&mut self, context: impl Into<String>) {
        self.system = context.into();
    }

    /// Take the most recent gate block (clears it), for the TUI to surface.
    pub fn take_block(&mut self) -> Option<GateOutcome> {
        self.last_block.take()
    }

    /// Clear the conversation, keeping the loaded scope context and the gate.
    pub fn clear_conversation(&mut self) {
        self.messages.clear();
    }

    /// A human-readable listing of the aden tools the model can discover.
    pub fn tool_catalog(&self) -> String {
        self.tools.discover("")
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
            tools: self.tools.advertised_defs(),
        }
    }

    /// Drive one user turn to completion: call the model, dispatch any tool
    /// calls, feed their results back as tool messages, and repeat until the
    /// model returns no tool calls or the hop cap is reached. Every message is
    /// appended to the conversation. Returns the final assistant text.
    pub async fn run_turn(&mut self) -> Result<String, ModelError> {
        for _ in 0..MAX_TOOL_HOPS {
            let response = call_model(&self.model, self.request()).await?;
            // The assistant message carries the tool calls it requested, so the
            // history threads correctly back to a function-calling provider.
            let content = response.message.content.clone();
            self.messages.push(Message::assistant(
                content.clone(),
                response.tool_calls.clone(),
            ));

            if response.tool_calls.is_empty() {
                return Ok(content);
            }

            // Dispatch each tool call and feed the result back as a tool
            // message. A mutating tool is gated first: aden directs edits, so a
            // non-in-scope verdict blocks the tool and feeds the verdict back to
            // the model (which must adapt) instead of running it.
            for call in &response.tool_calls {
                // Gate a mutating tool first (owned outcome releases the borrow).
                let outcome = if self.tools.mutates(&call.name) {
                    self.gate.as_ref().map(|g| g.check())
                } else {
                    None
                };
                let content = match outcome {
                    Some(o) if !o.proceed() => {
                        let msg = format!(
                            "EDIT BLOCKED by aden gate: {}. Revise to stay in scope.",
                            o.message
                        );
                        self.last_block = Some(o);
                        msg
                    }
                    _ => dispatch_result(&self.tools, call),
                };
                // The tool result records which call it answers.
                self.messages
                    .push(Message::tool_result(call.id.clone(), content));
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
    async fn tool_calls_and_results_carry_linkage() {
        let model = ScriptedModel::new(vec![calls_echo("calling", "ping"), assistant("done")]);
        let mut pump = Pump::new(model, echo_registry());
        pump.push_user("go");
        pump.run_turn().await.expect("turn completes");

        let msgs = pump.messages();
        // The assistant message records the tool call it requested.
        let assistant = msgs
            .iter()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
            .expect("assistant message with tool calls");
        assert_eq!(assistant.tool_calls[0].name, "echo");
        // The tool result records which call it answers (calls_echo uses id t1).
        let tool = msgs
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool result");
        assert_eq!(tool.tool_call_id.as_deref(), Some("t1"));
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

    use crate::gate::{Gate, GateOutcome, GateVerdict};
    use crate::tools::{Tool, ToolResult};

    /// A gate that always returns a fixed outcome.
    struct FakeGate(GateOutcome);
    impl Gate for FakeGate {
        fn check(&self) -> GateOutcome {
            self.0.clone()
        }
    }

    /// A mutating tool whose run is observable only when it is actually allowed.
    struct EditTool;
    impl Tool for EditTool {
        fn name(&self) -> &str {
            "edit"
        }
        fn run(&self, _arguments: &str) -> ToolResult {
            Ok("edited".to_string())
        }
        fn mutates(&self) -> bool {
            true
        }
    }

    fn edit_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EditTool));
        r
    }

    fn calls_edit(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, text),
            tool_calls: vec![ToolCall {
                id: "e1".to_string(),
                name: "edit".to_string(),
                arguments: "lib.rs".to_string(),
            }],
        }
    }

    fn outcome(verdict: GateVerdict, message: &str) -> GateOutcome {
        GateOutcome {
            verdict,
            message: message.to_string(),
        }
    }

    #[tokio::test]
    async fn gate_blocks_a_mutating_tool_and_records_the_block() {
        let model = ScriptedModel::new(vec![calls_edit("editing"), assistant("revised")]);
        let mut pump = Pump::new(model, edit_registry());
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::BlastLeak,
            "gate: BLAST-LEAK",
        ))));
        pump.push_user("change lib.rs");
        let out = pump.run_turn().await.expect("turn completes");
        assert_eq!(out, "revised");

        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool message was fed back");
        // The edit did NOT run ("edited" never appears); the block is fed back.
        assert!(tool_msg.content.contains("EDIT BLOCKED"), "{tool_msg:?}");
        assert!(!tool_msg.content.contains("edited"));
        // The block is recorded for the TUI to surface, then taken once.
        assert!(pump.take_block().is_some());
        assert!(pump.take_block().is_none());
    }

    #[tokio::test]
    async fn gate_in_scope_lets_the_edit_run() {
        let model = ScriptedModel::new(vec![calls_edit("editing"), assistant("done")]);
        let mut pump = Pump::new(model, edit_registry());
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::InScope,
            "in-scope",
        ))));
        pump.push_user("change lib.rs");
        pump.run_turn().await.expect("turn completes");

        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool message was fed back");
        assert_eq!(tool_msg.content, "edited");
        assert!(pump.take_block().is_none());
    }

    /// Records the system prompt it was called with.
    struct CapturingModel {
        seen: Mutex<Option<String>>,
    }
    impl Model for CapturingModel {
        async fn call(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
            *self.seen.lock().expect("lock") = Some(request.system.clone());
            Ok(assistant("ok"))
        }
    }

    #[tokio::test]
    async fn set_context_replaces_the_bare_system_prompt() {
        let model = CapturingModel {
            seen: Mutex::new(None),
        };
        let mut pump = Pump::new(model, ToolRegistry::new());
        pump.set_context("SCOPE CONTEXT");
        pump.push_user("go");
        pump.run_turn().await.expect("turn completes");
        let seen = pump.model.seen.lock().expect("lock").clone();
        assert_eq!(seen.as_deref(), Some("SCOPE CONTEXT"));
    }

    #[tokio::test]
    async fn non_mutating_tool_is_not_gated() {
        // Echo is read-only, so even a blocking gate never fires for it.
        let model = ScriptedModel::new(vec![calls_echo("calling", "ping"), assistant("done")]);
        let mut pump = Pump::new(model, echo_registry());
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::ScopeEscape,
            "would block",
        ))));
        pump.push_user("go");
        pump.run_turn().await.expect("turn completes");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool ran");
        assert_eq!(tool_msg.content, "ping");
        assert!(pump.take_block().is_none());
    }
}
