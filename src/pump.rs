//! The pump: steers and sets pace, carries no intelligence.
//!
//! The manual agentic loop lives here: call the model, dispatch tools, feed
//! results back, repeat. It paces turns (a tool-hop cap) and is where the gate
//! is enforced, but it never reasons about code. aden directs and gates; the
//! LLM acts; the pump steers.
//!
//! The loop is TUI-agnostic and synchronous in shape so it is unit-testable
//! against the stub model. The gate is enforced at the edit point: a mutating
//! tool is applied, then `impact-diff --scope` judges the working-tree diff, and
//! a non-in-scope verdict reverts the file before its result is accepted.

use std::path::Path;

use crate::gate::{Gate, GateOutcome};
use crate::model::{
    DEFAULT_SYSTEM_PROMPT, Message, Model, ModelError, ModelRequest, Role, ThinkingLevel, ToolCall,
    Usage,
};
use crate::tools::ToolRegistry;

/// A file's contents before a tentative edit, kept so a gate-blocked edit can be
/// undone -- touching only the edited file, never other uncommitted work.
enum Snapshot {
    /// The file existed; revert restores these bytes.
    Existed(Vec<u8>),
    /// The file did not exist (a newly created file); revert removes it.
    Absent,
}

impl Snapshot {
    fn capture(path: &Path) -> Snapshot {
        match std::fs::read(path) {
            Ok(bytes) => Snapshot::Existed(bytes),
            Err(_) => Snapshot::Absent,
        }
    }

    fn restore(&self, path: &Path) {
        let _ = match self {
            Snapshot::Existed(bytes) => std::fs::write(path, bytes),
            Snapshot::Absent => std::fs::remove_file(path),
        };
    }
}

/// The user's decision on a mutating tool call (the four-decision approval).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Run the call.
    Allow,
    /// Skip this call; feed a declined note back to the model.
    Decline,
    /// Decline this call and end the turn.
    CancelTurn,
}

/// Per-turn I/O the pump drives: stream assistant text out, and ask the user to
/// approve a mutating tool before it runs. The TUI implements it (drive); a
/// silent default ([`SilentIo`]) serves tests and non-interactive callers. One
/// owner of the terminal avoids the two-borrow problem of separate closures.
pub trait TurnIo {
    /// A streamed assistant text fragment; return `false` to cancel the turn.
    fn on_delta(&mut self, delta: &str) -> bool;
    /// Approve a mutating tool before it runs. Default: allow (tests / silent).
    fn approve(&mut self, _call: &ToolCall) -> Approval {
        Approval::Allow
    }
    /// A line of live output from a streaming `run_command` call. Return `false`
    /// to cancel (kill the child). Default: accept all lines silently.
    fn on_run_output(&mut self, _line: &str) -> bool {
        true
    }
}

/// A silent [`TurnIo`]: stream nothing, allow every mutation. Test-only -- it
/// bypasses the approval prompt, so it must never reach production (the live
/// loop always drives a real [`TurnIo`]).
#[cfg(test)]
struct SilentIo;
#[cfg(test)]
impl TurnIo for SilentIo {
    fn on_delta(&mut self, _delta: &str) -> bool {
        true
    }
}

/// Batch collector [`TurnIo`] for autonomous sub-agent execution (Phase 5).
/// Always approves mutations (the per-sub AdenGate + user-initiated task scope
/// are the safety boundary). Accumulates streamed deltas and run_command output
/// into `transcript`. A sub-agent "dense result" is the final transcript content
/// (not the full chat history). Usable from tests or a /run-agents path.
#[allow(dead_code)] // prepared for sub-agent runner; constructed in batch tests + future /execute
pub struct BatchIo {
    pub transcript: String,
}

#[allow(dead_code)]
impl BatchIo {
    pub fn new() -> Self {
        Self {
            transcript: String::new(),
        }
    }
    pub fn result(&self) -> String {
        self.transcript.clone()
    }
}

impl TurnIo for BatchIo {
    fn on_delta(&mut self, delta: &str) -> bool {
        self.transcript.push_str(delta);
        true
    }
    fn approve(&mut self, _call: &ToolCall) -> Approval {
        Approval::Allow
    }
    fn on_run_output(&mut self, line: &str) -> bool {
        self.transcript.push_str(line);
        self.transcript.push('\n');
        true
    }
}

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
    /// The blast-radius gate (optional) that judges a mutating tool's edit.
    /// None = no scope active: the human approval from TurnIo is the *only* gate
    /// and approved mutations stand (the "ungated" path). Some = aden's
    /// `impact-diff --scope` additionally gates: the edit is applied first (so
    /// aden sees the working-tree diff) then reverted on block.
    gate: Option<Box<dyn Gate>>,
    /// The most recent gate block, for the TUI to surface as a modal.
    last_block: Option<GateOutcome>,
    /// Token usage from the last turn that reported it, for the context meter.
    last_usage: Option<Usage>,
    /// The file the last accepted edit touched, for `/edit` to open.
    last_edited: Option<std::path::PathBuf>,
    /// Reasoning-effort level sent with each request (None = unset / provider
    /// default), set via `/think`.
    thinking: Option<ThinkingLevel>,
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
            last_usage: None,
            last_edited: None,
            thinking: None,
        }
    }

    /// Set the reasoning-effort level sent with each request (`/think`).
    pub fn set_thinking(&mut self, level: ThinkingLevel) {
        self.thinking = Some(level);
    }

    /// Token usage from the most recent turn that reported it (the context
    /// meter). `None` until a backend reports usage.
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    /// The file the last accepted edit touched, for `/edit` to open. `None`
    /// until an edit lands.
    pub fn last_edited(&self) -> Option<std::path::PathBuf> {
        self.last_edited.clone()
    }

    /// Install the blast-radius gate; a mutating tool's edit is then judged
    /// against the scope (applied, gated, reverted on a block).
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

    /// Replace the conversation with a loaded one (for `/resume`). Keeps the
    /// scope context and gate; the caller is responsible for the session file.
    pub fn load_conversation(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// A human-readable listing of the aden tools the model can discover.
    pub fn tool_catalog(&self) -> String {
        self.tools.aden_catalog()
    }

    /// Mutable access to the tool registry, so the caller can hot-register tools
    /// (e.g. aden's context tools once aden becomes available) without a reboot.
    pub fn registry_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
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
            thinking: self.thinking,
        }
    }

    /// A non-streaming turn (the delta sink is a no-op). The live TUI uses
    /// [`Pump::run_turn_streaming`]; this is the simple form for tests and any
    /// non-interactive caller. Drives the model, dispatches tools, feeds results
    /// back, and repeats until the model stops calling tools or the hop cap hits.
    #[cfg(test)]
    pub async fn run_turn(&mut self) -> Result<String, ModelError> {
        self.run_turn_streaming(&mut SilentIo).await
    }

    /// Drive one turn through `io`: stream assistant text (io.on_delta, which
    /// returns false to cancel) and approve each mutating tool (io.approve)
    /// before it runs. A cancelled stream keeps the partial text and drops
    /// partial tool calls; a declined tool is skipped; a cancel-turn approval
    /// ends the turn after feeding results for the remaining calls.
    pub async fn run_turn_streaming(&mut self, io: &mut dyn TurnIo) -> Result<String, ModelError> {
        for _ in 0..MAX_TOOL_HOPS {
            let request = self.request();
            // Wrap the sink to notice a cancellation request mid-stream.
            let mut cancelled = false;
            let response = {
                let mut sink = |delta: &str| {
                    let keep_going = io.on_delta(delta);
                    cancelled |= !keep_going;
                    keep_going
                };
                self.model.stream(request, &mut sink).await?
            };
            // Track context size: the latest hop that reports usage wins.
            if response.usage.is_some() {
                self.last_usage = response.usage;
            }
            if cancelled {
                // User aborted: record the partial text, drop partial tool calls,
                // and end the turn.
                let content = response.message.content.clone();
                self.messages
                    .push(Message::assistant(content.clone(), Vec::new()));
                return Ok(content);
            }
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

            // Dispatch each tool call and feed the result back as a tool message.
            // A mutating tool is approved by the user, then applied and (when a
            // task scope is active) gated by aden. Every call gets a result so
            // tool-call/result pairing stays valid even on a mid-turn cancel.
            let mut end_turn = false;
            for call in &response.tool_calls.clone() {
                let result = if end_turn {
                    "turn cancelled by the user".to_string()
                } else if self.tools.mutates(&call.name) {
                    match io.approve(call) {
                        Approval::Allow => {
                            if !self.tools.revertible(&call.name) {
                                self.run_command_streaming(call, io).await
                            } else {
                                self.run_gated_mutation(call)
                            }
                        }
                        Approval::Decline => format!("the user declined the {} call", call.name),
                        Approval::CancelTurn => {
                            end_turn = true;
                            "turn cancelled by the user".to_string()
                        }
                    }
                } else {
                    dispatch_result(&self.tools, call)
                };
                self.messages
                    .push(Message::tool_result(call.id.clone(), result));
            }
            if end_turn {
                return Ok(content);
            }
        }
        Err(ModelError::Backend("tool-hop cap reached".to_string()))
    }

    /// Apply a mutating tool, then let aden's gate accept or reject the result.
    /// Per DESIGN, "before coxn accepts an edit it runs `impact-diff --scope` and
    /// obeys the exit code." aden reads the working-tree diff, so the edit must be
    /// on disk to be judged: the pump snapshots the target, applies the tool,
    /// runs the gate, and reverts the file on a block. Returns the text fed back
    /// to the model.
    ///
    /// For revertible (file-edit) tools, a blocked mutation reverts the file on
    /// disk. For non-revertible tools (commands), `gate_command_result` handles
    /// the gate tail so the behaviour matches the streaming path.
    fn run_gated_mutation(&mut self, call: &ToolCall) -> String {
        let revertible = self.tools.revertible(&call.name);
        // Snapshot the target before applying so a gate-blocked edit can be
        // reverted. Non-revertible tools skip the snapshot.
        let path = self.tools.target_path(call);
        let snapshot = if revertible {
            path.as_deref().map(Snapshot::capture)
        } else {
            None
        };
        // Apply. A tool error means nothing landed: surface it, skip the gate.
        let applied = match self.tools.dispatch(call) {
            Ok(text) => text,
            Err(err) => return err,
        };
        if !revertible {
            // Commands use the shared gate-tail helper so the behaviour cannot
            // diverge from the streaming path.
            return self.gate_command_result(applied);
        }
        // With no task scope, the user's approval is the only gate and the
        // effect stands. With a scope, aden also judges the working-tree diff.
        let Some(gate) = self.gate.as_ref() else {
            self.last_edited = path;
            return applied;
        };
        let outcome = gate.check();
        if outcome.proceed() {
            self.last_edited = path;
            return applied;
        }
        // Out of scope: revert the file and report the block.
        self.last_block = Some(outcome.clone());
        if let (Some(p), Some(snap)) = (&path, &snapshot) {
            snap.restore(p);
        }
        format!(
            "EDIT BLOCKED by aden gate: {}. The change was reverted; revise to stay in scope.",
            outcome.message
        )
    }

    /// Gate tail for a non-revertible command result: record a block when the
    /// gate fires, append the WARNING note, or return the applied output cleanly.
    /// Called by both `run_gated_mutation` (non-revertible path, now removed) and
    /// `run_command_streaming`, so the behaviour cannot diverge.
    fn gate_command_result(&mut self, applied: String) -> String {
        let Some(gate) = self.gate.as_ref() else {
            return applied;
        };
        let outcome = gate.check();
        if outcome.proceed() {
            return applied;
        }
        self.last_block = Some(outcome.clone());
        format!(
            "{applied}\n\nWARNING: the aden gate reports this command pushed the working tree out of scope: {}. coxn cannot auto-revert a command's effects -- inspect and revert manually if needed.",
            outcome.message
        )
    }

    /// Streaming `run_command` path: spawn the child asynchronously, feed each
    /// line to `io.on_run_output` for live TUI rendering, then apply the gate.
    /// Falls back to `run_gated_mutation` when `run_command_params` is unavailable
    /// so any future non-revertible-non-command tool still works.
    async fn run_command_streaming(&mut self, call: &ToolCall, io: &mut dyn TurnIo) -> String {
        // Copy out sandbox params before the await so no borrow of self.tools
        // crosses the await point.
        let Some((dir, bwrap)) = self.tools.run_command_params() else {
            return self.run_gated_mutation(call);
        };
        let dir = dir.to_path_buf();

        // Parse the command and network flag from the call arguments, mirroring
        // the arg/arg_bool logic in RunTool::run.
        let command = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Ok(v) if v.is_object() => v
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            _ => call.arguments.trim().to_string(),
        };
        if command.trim().is_empty() {
            return "run_command needs a command argument".to_string();
        }
        let network = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("network").and_then(|n| n.as_bool()))
            .unwrap_or(false);

        let outcome = crate::sandbox::run_streaming(&dir, &command, network, bwrap, &mut |line| {
            io.on_run_output(line)
        })
        .await;

        let applied = crate::tools::format_run(&outcome);
        self.gate_command_result(applied)
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
            usage: None,
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
            usage: None,
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
            usage: None,
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
            usage: None,
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
        // The block verdict is fed back, not the applied result ("edited").
        assert!(tool_msg.content.contains("EDIT BLOCKED"), "{tool_msg:?}");
        assert!(!tool_msg.content.contains("edited"));
        // The block is recorded for the TUI to surface, then taken once.
        assert!(pump.take_block().is_some());
        assert!(pump.take_block().is_none());
    }

    /// A TurnIo that cancels the stream on the first fragment.
    struct CancelIo;
    impl TurnIo for CancelIo {
        fn on_delta(&mut self, _delta: &str) -> bool {
            false
        }
    }

    /// A TurnIo that streams silently and returns a fixed approval decision.
    struct ApproveIo(Approval);
    impl TurnIo for ApproveIo {
        fn on_delta(&mut self, _delta: &str) -> bool {
            true
        }
        fn approve(&mut self, _call: &ToolCall) -> Approval {
            self.0
        }
    }

    #[tokio::test]
    async fn cancel_drops_partial_tool_calls_and_ends_the_turn() {
        // The model returns text + a tool call; the sink cancels on the text.
        let model = ScriptedModel::new(vec![calls_edit("editing"), assistant("unreached")]);
        let mut pump = Pump::new(model, edit_registry());
        pump.set_gate(Box::new(FakeGate(outcome(GateVerdict::InScope, "ok"))));
        pump.push_user("go");
        let out = pump.run_turn_streaming(&mut CancelIo).await.expect("turn");
        assert_eq!(out, "editing");
        // The edit was never dispatched (no tool message), and the turn ended
        // without consuming the second scripted response.
        assert!(pump.messages().iter().all(|m| m.role != Role::Tool));
        assert!(pump.take_block().is_none());
    }

    #[tokio::test]
    async fn declined_mutation_is_not_applied() {
        let dir = temp_dir("decline");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world").unwrap();
        let args = r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#;
        let model = ScriptedModel::new(vec![calls_tool("edit", args), assistant("ok")]);
        let mut pump = Pump::new(model, real_edit_registry(&dir));
        pump.set_gate(Box::new(FakeGate(outcome(GateVerdict::InScope, "ok"))));
        pump.push_user("edit a.txt");
        // The user declines the edit: it must not touch the file.
        pump.run_turn_streaming(&mut ApproveIo(Approval::Decline))
            .await
            .expect("turn");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool result");
        assert!(tool_msg.content.contains("declined"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn declined_mutation_without_a_gate_is_not_applied() {
        // The common post-F4 case: no task scope, approval is the only gate.
        let dir = temp_dir("decline-nogate");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world").unwrap();
        let args = r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#;
        let model = ScriptedModel::new(vec![calls_tool("edit", args), assistant("ok")]);
        let mut pump = Pump::new(model, real_edit_registry(&dir));
        pump.push_user("edit a.txt");
        pump.run_turn_streaming(&mut ApproveIo(Approval::Decline))
            .await
            .expect("turn");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
        assert!(
            pump.messages()
                .iter()
                .any(|m| m.role == Role::Tool && m.content.contains("declined"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_turn_feeds_results_for_every_remaining_call() {
        // Two tool calls in one response; CancelTurn must still feed a result for
        // each (valid tool-call/result pairing) and run neither.
        let two = ModelResponse {
            message: Message::new(Role::Assistant, "editing two"),
            tool_calls: vec![
                ToolCall {
                    id: "c1".to_string(),
                    name: "edit".to_string(),
                    arguments: "a".to_string(),
                },
                ToolCall {
                    id: "c2".to_string(),
                    name: "edit".to_string(),
                    arguments: "b".to_string(),
                },
            ],
            usage: None,
        };
        let model = ScriptedModel::new(vec![two, assistant("unreached")]);
        let mut pump = Pump::new(model, edit_registry());
        pump.push_user("go");
        let out = pump
            .run_turn_streaming(&mut ApproveIo(Approval::CancelTurn))
            .await
            .expect("turn");
        assert_eq!(out, "editing two");
        let tool_msgs: Vec<&str> = pump
            .messages()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(tool_msgs.len(), 2, "both calls get a result");
        assert!(tool_msgs.iter().all(|c| c.contains("cancelled")));
        assert!(tool_msgs.iter().all(|c| !c.contains("edited")));
    }

    /// A mutating-but-not-revertible tool, like a shell command.
    struct CommandTool;
    impl Tool for CommandTool {
        fn name(&self) -> &str {
            "run_command"
        }
        fn run(&self, _arguments: &str) -> ToolResult {
            Ok("exit 0\nbuilt".to_string())
        }
        fn mutates(&self) -> bool {
            true
        }
        fn revertible(&self) -> bool {
            false
        }
    }

    fn command_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(CommandTool));
        r
    }

    fn calls_command() -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, "running"),
            tool_calls: vec![ToolCall {
                id: "r1".to_string(),
                name: "run_command".to_string(),
                arguments: r#"{"command":"cargo build"}"#.to_string(),
            }],
            usage: None,
        }
    }

    #[tokio::test]
    async fn a_blocked_command_is_reported_not_reverted() {
        // A command that escapes scope keeps its output (nothing to revert) and
        // is reported honestly, unlike an edit which says "reverted".
        let model = ScriptedModel::new(vec![calls_command(), assistant("noted")]);
        let mut pump = Pump::new(model, command_registry());
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::ScopeEscape,
            "out of scope",
        ))));
        pump.push_user("build it");
        pump.run_turn().await.expect("turn completes");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool message");
        // The command output stands; the message says it was NOT reverted.
        assert!(tool_msg.content.contains("built"), "{}", tool_msg.content);
        assert!(tool_msg.content.contains("cannot auto-revert"));
        assert!(!tool_msg.content.contains("was reverted"));
        // The block is still recorded for the TUI.
        assert!(pump.take_block().is_some());
    }

    #[tokio::test]
    async fn an_in_scope_command_returns_its_output() {
        let model = ScriptedModel::new(vec![calls_command(), assistant("ok")]);
        let mut pump = Pump::new(model, command_registry());
        pump.set_gate(Box::new(FakeGate(outcome(GateVerdict::InScope, "ok"))));
        pump.push_user("build it");
        pump.run_turn().await.expect("turn completes");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool message");
        assert_eq!(tool_msg.content, "exit 0\nbuilt");
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

    /// A unique temp dir for a real-file gate test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("coxn-pump-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).expect("create temp dir");
        d
    }

    fn calls_tool(name: &str, arguments: &str) -> ModelResponse {
        ModelResponse {
            message: Message::new(Role::Assistant, "acting"),
            tool_calls: vec![ToolCall {
                id: "m1".to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
            }],
            usage: None,
        }
    }

    /// A registry holding the real `edit` action tool rooted at `dir`.
    fn real_edit_registry(dir: &Path) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(crate::tools::EditTool::new(dir.to_path_buf())));
        r
    }

    #[tokio::test]
    async fn a_blocked_edit_is_reverted_on_disk() {
        let dir = temp_dir("revert");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world").unwrap();
        let args = r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#;
        let model = ScriptedModel::new(vec![calls_tool("edit", args), assistant("ok")]);
        let mut pump = Pump::new(model, real_edit_registry(&dir));
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::ScopeEscape,
            "out of scope",
        ))));
        pump.push_user("edit a.txt");
        pump.run_turn().await.expect("turn completes");
        // The edit was applied to disk, then reverted by the gate block.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
        let tool_msg = pump
            .messages()
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool message");
        assert!(tool_msg.content.contains("EDIT BLOCKED"));
        assert!(pump.take_block().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_in_scope_edit_persists_on_disk() {
        let dir = temp_dir("persist");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world").unwrap();
        let args = r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#;
        let model = ScriptedModel::new(vec![calls_tool("edit", args), assistant("ok")]);
        let mut pump = Pump::new(model, real_edit_registry(&dir));
        pump.set_gate(Box::new(FakeGate(outcome(
            GateVerdict::InScope,
            "in-scope",
        ))));
        pump.push_user("edit a.txt");
        pump.run_turn().await.expect("turn completes");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
        assert!(pump.take_block().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_approved_edit_without_a_gate_applies() {
        let dir = temp_dir("nogate");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello world").unwrap();
        let args = r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#;
        let model = ScriptedModel::new(vec![calls_tool("edit", args), assistant("ok")]);
        let mut pump = Pump::new(model, real_edit_registry(&dir));
        // No task scope, so no aden gate: the user's approval is the only gate,
        // and the approved edit lands (run_turn's SilentIo allows by default).
        pump.push_user("edit a.txt");
        pump.run_turn().await.expect("turn completes");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn batch_io_collects_and_always_allows_for_subs() {
        // BatchIo is the collector for autonomous sub-agent pumps (Phase 5).
        // Always allows (gate is the enforcer); transcript is the dense result.
        let mut io = BatchIo::new();
        let _ = io.on_delta("sub result part A");
        let call = ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        };
        assert_eq!(io.approve(&call), Approval::Allow);
        let _ = io.on_run_output("ran ls");
        assert!(io.result().contains("sub result part A"));
        assert!(io.result().contains("ran ls"));
    }
}
