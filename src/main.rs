//! coxn: a lean directional harness for aden.
//!
//! coxn is a "dumb pump": it steers and sets pace, and carries no intelligence.
//! aden directs and gates; the LLM acts; coxn steers. See DESIGN.adoc.

mod aden;
mod gate;
mod model;
mod openai;
mod pump;
mod tools;
mod tui;

use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use model::{AnyModel, Message, Role, StubModel};
use pump::Pump;
use tools::{AdenTool, EditTool, ReadFileTool, ToolRegistry, WriteTool};
use tui::{Action, Tui, View, map_input_key, map_modal_key};

/// How long the event loop waits for a key before redrawing.
const TICK: Duration = Duration::from_millis(100);

/// Format the conversation into the output pane text. An assistant turn that
/// only requested tools (no text) renders its calls so the line is not blank.
fn transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| match m.role {
            Role::User => format!("you: {}", m.content),
            Role::Tool => format!("tool: {}", m.content),
            Role::System => format!("sys: {}", m.content),
            Role::Assistant if m.tool_calls.is_empty() => format!("coxn: {}", m.content),
            Role::Assistant => {
                let calls = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, c.arguments))
                    .collect::<Vec<_>>()
                    .join(", ");
                if m.content.is_empty() {
                    format!("coxn: → {calls}")
                } else {
                    format!("coxn: {}  → {calls}", m.content)
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    // The wiring: a runtime-selected backend and aden-backed pull-context tools
    // rooted at the working directory. The model pulls context (asm/understand)
    // on demand; aden directs, coxn relays.
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Deferred disclosure: only the `aden_tools` discovery seam is advertised;
    // aden's read/search tools (the context layer) are latent, found by intent.
    // No tool bloat by default.
    let mut tools = ToolRegistry::new();
    tools.register_latent(Box::new(AdenTool::asm(dir.clone())));
    tools.register_latent(Box::new(AdenTool::understand(dir.clone())));
    tools.register_latent(Box::new(AdenTool::grep(dir.clone())));
    tools.register_latent(Box::new(AdenTool::ask(dir.clone())));
    tools.register_latent(Box::new(AdenTool::locate(dir.clone())));

    // Take over the terminal and paint a frame first, so the user sees coxn
    // start instead of a frozen blank while the aden subprocess calls below
    // (model resolution, scope, asm context) run -- which can take several
    // seconds on a large repo.
    let mut view = View::new();
    view.set_status("starting coxn...".to_string());
    let mut tui = Tui::new()?;
    tui.draw(&view)?;

    let (model, mut sel) = resolve_model(&dir);
    view.set_status(format!("{}  |  loading...", sel.label()));
    tui.draw(&view)?;

    // A named task (COXN_TASK_NAME) makes aden define the scope: the gate
    // mandate and exactly the seeds' context. No task = bare, ungated prompt.
    let task = load_task(&dir);
    // The action set (read_file / edit / write_file) is advertised up front only
    // when the scope actually gated: aden gates every edit, and there is no gate
    // without a scope, so editing is off by default (no ungated edits). read_file
    // is advertised with the editors so the model can fetch the exact text to
    // replace -- editing without reading would be guesswork.
    if task.gate.is_some() {
        tools.register(Box::new(ReadFileTool::new(dir.clone())));
        tools.register(Box::new(EditTool::new(dir.clone())));
        tools.register(Box::new(WriteTool::new(dir.clone())));
    }
    let mut pump = Pump::new(model, tools);
    if let Some(gate) = task.gate {
        pump.set_gate(gate);
    }
    if let Some(context) = task.context {
        pump.set_context(context);
    }
    // A savings-free status at boot: the `aden status` call it would make is the
    // slowest aden spawn and is purely cosmetic, so defer it to the first
    // post-turn refresh and let the user reach the prompt sooner.
    view.set_status(boot_status(&sel.label(), &task.status));

    let result = drive(&mut tui, &mut view, &mut pump, &dir, &mut sel, &task.status).await;
    drop(tui); // restore the terminal before surfacing any error
    result
}

/// The boot status line: model and task only, no aden `status` call. Used before
/// the event loop so the slow savings probe does not delay the first prompt; the
/// savings appear on the first post-turn [`status_line`] refresh.
fn boot_status(model_label: &str, task: &str) -> String {
    let detail = if task.is_empty() {
        "/help".to_string()
    } else {
        format!("{task}  /help")
    };
    format!("{model_label}  |  {detail}")
}

/// The status line: the active model, then aden's savings estimate when there
/// is one, else the task and a `/help` hint (the [`boot_status`] form).
fn status_line(dir: &Path, model_label: &str, task: &str) -> String {
    match aden::savings(dir) {
        Some(savings) => format!("{model_label}  |  {savings}"),
        None => boot_status(model_label, task),
    }
}

/// The live provider connection, kept so `/model` can enumerate and switch
/// models at runtime without re-resolving. The stub has no endpoint.
struct Endpoint {
    base_url: String,
    key: Option<String>,
    source: &'static str,
}

/// The active model selection: which model, and (for a real provider) where it
/// lives. Selection is data, so switching is just rebuilding the backend.
struct ModelSel {
    name: String,
    endpoint: Option<Endpoint>,
}

impl ModelSel {
    /// The status-line label tagging the model and how it was resolved.
    fn label(&self) -> String {
        match &self.endpoint {
            Some(e) => format!("{} @ {} ({})", self.name, e.base_url, e.source),
            None => {
                "stub (no model; start Ollama/LM Studio or set COXN_MODEL_BASE_URL)".to_string()
            }
        }
    }
}

/// Build an OpenAI-compatible model paired with its selection state.
fn openai_model(
    base_url: String,
    model: String,
    key: Option<String>,
    source: &'static str,
) -> (AnyModel, ModelSel) {
    let backend = AnyModel::OpenAiCompat(openai::OpenAiCompatModel::new(
        base_url.clone(),
        model.clone(),
        key.clone(),
    ));
    (
        backend,
        ModelSel {
            name: model,
            endpoint: Some(Endpoint {
                base_url,
                key,
                source,
            }),
        },
    )
}

/// Pick the model backend at runtime, returning it with a short label for the
/// status line. Resolution order: an explicit `COXN_MODEL_BASE_URL`, then
/// `.aden/config.toml` (`model.base_url` / `model.name`), then a local provider
/// auto-detected on its well-known port (Ollama / LM Studio), then the offline
/// stub. Selection is data, not a type, so per-role routing and sub-agents drop
/// in without reworking the seam. The key always comes from the environment,
/// never the committed config.
fn resolve_model(dir: &Path) -> (AnyModel, ModelSel) {
    let env_key = || {
        std::env::var("COXN_MODEL_KEY")
            .ok()
            .filter(|k| !k.is_empty())
    };

    // 1. Explicit environment override.
    if let Ok(base_url) = std::env::var("COXN_MODEL_BASE_URL")
        && !base_url.trim().is_empty()
    {
        let model = std::env::var("COXN_MODEL_NAME").unwrap_or_else(|_| "local".to_string());
        return openai_model(base_url, model, env_key(), "env");
    }
    // 2. aden config (.aden/config.toml). Secrets stay in the env, not the file.
    if let Some(base_url) = aden::config_get(dir, "model.base_url") {
        let model = aden::config_get(dir, "model.name").unwrap_or_else(|| "local".to_string());
        return openai_model(base_url, model, env_key(), "config");
    }
    // 3. Local auto-detection.
    if let Some((base_url, model)) = openai::detect() {
        return openai_model(base_url, model, None, "auto");
    }
    // 4. Offline stub.
    (
        AnyModel::Stub(StubModel),
        ModelSel {
            name: "stub".to_string(),
            endpoint: None,
        },
    )
}

/// Render the `/model` listing: every model the provider advertises (loaded or
/// not), the active one marked. Falls back to the label when there is no
/// provider or the listing cannot be fetched.
fn model_listing(sel: &ModelSel) -> String {
    let Some(e) = &sel.endpoint else {
        return format!("model: {}", sel.label());
    };
    match openai::list_models(&e.base_url, e.key.as_deref()) {
        Some(models) if !models.is_empty() => {
            let mut out = format!("models on {} (/model <name|#> to switch):\n", e.base_url);
            for (i, m) in models.iter().enumerate() {
                let mark = if *m == sel.name { '*' } else { ' ' };
                out.push_str(&format!("  {mark} {:>2}. {m}\n", i + 1));
            }
            out.push_str("(* = active)");
            out
        }
        _ => format!(
            "model: {}  (could not list models from {})",
            sel.label(),
            e.base_url
        ),
    }
}

/// Switch the active model to `target` (a 1-based index into the listing or a
/// model name). A name not in the listing is still allowed so an unloaded model
/// can be selected (the backend JIT-loads it on first call). Returns the status
/// message to show.
fn switch_model(pump: &mut Pump<AnyModel>, sel: &mut ModelSel, target: &str) -> String {
    let Some(e) = &sel.endpoint else {
        return "no provider to switch on (offline stub)".to_string();
    };
    let listed = openai::list_models(&e.base_url, e.key.as_deref()).unwrap_or_default();
    let chosen = match target.parse::<usize>() {
        Ok(n) => match listed.get(n.wrapping_sub(1)) {
            Some(m) => m.clone(),
            None => return format!("no model #{n} (there are {})", listed.len()),
        },
        Err(_) => target.to_string(),
    };
    pump.set_model(AnyModel::OpenAiCompat(openai::OpenAiCompatModel::new(
        e.base_url.clone(),
        chosen.clone(),
        e.key.clone(),
    )));
    sel.name = chosen;
    format!("switched to {}", sel.name)
}

/// Resolve a role to a model id via the `[route]` table in `.aden/config.toml`
/// (`route.<role>`), e.g. `route.scout = "qwen2.5-coder"`. Selection is data: the
/// role is an opaque key, the map is config, coxn only looks it up. The same
/// lookup the sub-agent runner (B4) uses to pick a model per scope. `None` when
/// the role is unmapped. (A future `instance:model` value selects the instance
/// too; today the value is a model id on the active endpoint.)
fn resolve_role(dir: &Path, role: &str) -> Option<String> {
    aden::config_get(dir, &format!("route.{role}"))
}

/// The minimal operating instruction prepended to the scope context in task
/// mode. coxn's default prompt is empty (zero-default-context), which leaves a
/// weak local model passive -- it will not reach for the action tools unprompted
/// and answers "I can't edit files." DESIGN sanctions this single nudge: it is
/// operating instruction, not repo context, and appears only when the scope has
/// actually gated, so editing is governed.
const AGENT_PREAMBLE: &str = "\
You are coxn, a coding agent working within a task scope that aden defined. To \
change code, call `read_file` to get the exact current text, then `edit` (replace \
an exact unique string) or `write_file` (whole file) -- do not just print a patch \
for the user to apply. Every edit is gated by aden against the scope and reverted \
if it escapes, so keep changes minimal and in scope. To understand or search code, \
use the aden tools (discover them via `aden_tools`): aden_grep, aden_locate, \
aden_asm, aden_understand, aden_ask.\n\n\
=== task scope context ===\n\n";

/// The result of resolving a task scope: the status-line text, the gate (when
/// `aden scope` produced a mandate), and the context to load into the pump.
struct Task {
    status: String,
    gate: Option<Box<dyn gate::Gate>>,
    context: Option<String>,
}

/// If `COXN_TASK_NAME` is set, let aden define the scope: run `aden scope` for
/// the task's seeds, persist the manifest for the gate, and assemble exactly the
/// seeds' context. No task means a bare, ungated prompt. coxn parses nothing --
/// it gates on the manifest file and loads context from `aden asm` on its own
/// seed inputs.
fn load_task(dir: &Path) -> Task {
    let bare = || Task {
        status: String::new(),
        gate: None,
        context: None,
    };
    let Some(name) = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return bare();
    };
    let seeds: Vec<String> = std::env::var("COXN_TASK_SEEDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let budget: u64 = std::env::var("COXN_TASK_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);

    // Define the mandate: aden scope -> manifest file -> gate.
    let mut gate: Option<Box<dyn gate::Gate>> = None;
    if !seeds.is_empty()
        && let Ok(manifest_json) = aden::scope(dir, &name, &seeds, budget)
    {
        let manifest = std::env::temp_dir().join(format!("coxn-scope-{}.json", std::process::id()));
        if std::fs::write(&manifest, manifest_json).is_ok() {
            gate = Some(Box::new(aden::AdenGate::new(dir.to_path_buf(), manifest)));
        }
    }
    let gated = gate.is_some();

    // Assemble exactly the scope's context: the operating preamble (only when
    // gated, so the model is told to act and edits are governed) plus the seeds'
    // assembled neighborhoods.
    let mut context = String::new();
    if gated {
        context.push_str(AGENT_PREAMBLE);
    }
    for s in &seeds {
        if let Ok(text) = aden::pull(dir, aden::Pull::Asm(s)) {
            context.push_str(&text);
            context.push('\n');
        }
    }

    Task {
        status: format!(
            "task '{name}' ({} seed(s){})",
            seeds.len(),
            if gated { ", gated" } else { "" }
        ),
        gate,
        context: (!context.is_empty()).then_some(context),
    }
}

/// Help shown by `/help`.
const HELP: &str = "commands:\n  \
/help            show this help\n  \
/model           list available models (* = active)\n  \
/model <name|#>  switch the active model\n  \
/model @<role>   switch to the model mapped for a role (route.<role> config)\n  \
/tools           list the aden tools the model can discover\n  \
/clear           clear the conversation (keeps the task scope)\n  \
/quit            leave coxn\n\
anything else is sent to the model.";

/// A slash command typed into the input line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Quit,
    Clear,
    /// `/model` lists; `/model <name|#>` switches.
    Model(Option<String>),
    Tools,
    Unknown(String),
}

/// Parse a leading-slash input into a command. Pure and testable.
fn parse_command(input: &str) -> Command {
    let mut words = input.trim_start_matches('/').split_whitespace();
    let word = words.next().unwrap_or("");
    let arg = words.next().map(|s| s.to_string());
    match word {
        "help" | "h" | "?" => Command::Help,
        "quit" | "q" | "exit" => Command::Quit,
        "clear" => Command::Clear,
        "model" => Command::Model(arg),
        "tools" => Command::Tools,
        other => Command::Unknown(other.to_string()),
    }
}

/// The output pane's (width, height), for wrapping and PageUp/PageDown scroll
/// amounts. Height excludes the status and input rows. Falls back to (80, 1) if
/// the terminal size cannot be determined.
fn pane_dims(tui: &Tui) -> (u16, u16) {
    tui.size()
        .map(|s| (s.width.max(1), s.height.saturating_sub(2).max(1)))
        .unwrap_or((80, 1))
}

/// The event loop: draw, read a key, route it by mode (modal vs input), and run
/// a turn on submit. Carries no intelligence; it only paces and shuttles.
async fn drive(
    tui: &mut Tui,
    view: &mut View,
    pump: &mut Pump<AnyModel>,
    dir: &Path,
    sel: &mut ModelSel,
    task: &str,
) -> io::Result<()> {
    loop {
        tui.draw(view)?;

        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // A modal grabs input until answered.
        if view.modal.is_some() {
            if matches!(map_modal_key(key), Some(Action::Confirm | Action::Cancel)) {
                view.dismiss();
            }
            continue;
        }

        match map_input_key(key) {
            Some(Action::Quit) => return Ok(()),
            Some(Action::Append(c)) => view.input_push(c),
            Some(Action::Backspace) => view.input_backspace(),
            Some(Action::CursorLeft) => view.cursor_left(),
            Some(Action::CursorRight) => view.cursor_right(),
            Some(Action::CursorHome) => view.cursor_home(),
            Some(Action::CursorEnd) => view.cursor_end(),
            Some(Action::WordDelete) => view.word_delete(),
            Some(Action::HistoryPrev) => view.history_prev(),
            Some(Action::HistoryNext) => view.history_next(),
            Some(Action::ScrollUp) => {
                let (w, h) = pane_dims(tui);
                view.scroll_up(h, view.max_scroll(w, h));
            }
            Some(Action::ScrollDown) => {
                let (_, h) = pane_dims(tui);
                view.scroll_down(h);
            }
            Some(Action::Submit) => {
                let text = view.take_input();
                if text.trim().is_empty() {
                    continue;
                }
                // Snap the output pane to the bottom on every submit.
                view.snap_to_bottom();
                // A leading slash is a local command, not a model turn.
                if text.trim_start().starts_with('/') {
                    match parse_command(text.trim()) {
                        Command::Quit => return Ok(()),
                        Command::Help => view.output = HELP.to_string(),
                        Command::Model(None) => view.output = model_listing(sel),
                        Command::Model(Some(target)) => {
                            // `@role` resolves through the [route] table; anything
                            // else is a model name or index.
                            let resolved = if let Some(role) = target.strip_prefix('@') {
                                resolve_role(dir, role).ok_or_else(|| {
                                    format!(
                                        "no model mapped for role '@{role}'; set route.{role} via aden config"
                                    )
                                })
                            } else {
                                Ok(target.clone())
                            };
                            match resolved {
                                Ok(model) => {
                                    view.output = switch_model(pump, sel, &model);
                                    view.set_status(status_line(dir, &sel.label(), task));
                                }
                                Err(msg) => view.output = msg,
                            }
                        }
                        Command::Tools => view.output = pump.tool_catalog(),
                        Command::Clear => {
                            pump.clear_conversation();
                            view.output.clear();
                            view.set_status(status_line(dir, &sel.label(), task));
                        }
                        Command::Unknown(c) => {
                            view.output = format!("unknown command: /{c}  (try /help)");
                        }
                    }
                    continue;
                }
                // Record in history before submitting.
                view.push_history(text.clone());
                pump.push_user(text);
                // Stream the reply: render the transcript so far plus the
                // assistant text as it arrives, repainting per fragment. The
                // reply appears live instead of all at once, and a Ctrl-C between
                // fragments cancels the turn (kept partial) rather than quitting.
                let prior = transcript(pump.messages());
                let mut cancelled = false;
                view.pending = true;
                let result = {
                    let mut buf = String::new();
                    let mut sink = |delta: &str| -> bool {
                        buf.push_str(delta);
                        view.output = format!("{prior}\ncoxn: {buf}");
                        let _ = tui.draw(view);
                        // Non-blocking cancel check: Ctrl-C aborts the turn.
                        if let Ok(true) = event::poll(Duration::ZERO)
                            && let Ok(Event::Key(key)) = event::read()
                            && key.kind == KeyEventKind::Press
                            && matches!(map_input_key(key), Some(Action::Quit))
                        {
                            cancelled = true;
                            return false;
                        }
                        true
                    };
                    pump.run_turn_streaming(&mut sink).await
                };
                view.pending = false;
                match result {
                    Ok(_) => {
                        view.output = transcript(pump.messages());
                        // Refresh the model + savings status after the turn.
                        let status = status_line(dir, &sel.label(), task);
                        view.set_status(if cancelled {
                            format!("{status}  (cancelled)")
                        } else {
                            status
                        });
                    }
                    Err(err) => {
                        // Keep the error visible (don't let the savings refresh
                        // clobber it); the partial transcript still renders.
                        view.output = transcript(pump.messages());
                        view.set_status(format!("error: {err}"));
                    }
                }
                // Surface a gate block from this turn as a confirm modal.
                if let Some(block) = pump.take_block() {
                    view.confirm(block.message);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_labels_each_role() {
        let messages = vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "stub: hi"),
        ];
        assert_eq!(transcript(&messages), "you: hi\ncoxn: stub: hi");
    }

    #[test]
    fn transcript_renders_a_tool_call_turn() {
        use model::ToolCall;
        let messages = vec![
            Message::new(Role::User, "go"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "c1".into(),
                    name: "aden_asm".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("c1", "result"),
        ];
        assert_eq!(
            transcript(&messages),
            "you: go\ncoxn: → aden_asm({})\ntool: result"
        );
    }

    #[test]
    fn parse_command_maps_aliases_and_unknowns() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command("/?"), Command::Help);
        assert_eq!(parse_command("/q"), Command::Quit);
        assert_eq!(parse_command("/clear"), Command::Clear);
        assert_eq!(parse_command("/tools"), Command::Tools);
        // /model lists; /model <arg> carries the switch target.
        assert_eq!(parse_command("/model"), Command::Model(None));
        assert_eq!(
            parse_command("/model gpt"),
            Command::Model(Some("gpt".to_string()))
        );
        assert_eq!(
            parse_command("/model 3"),
            Command::Model(Some("3".to_string()))
        );
        // A role switch carries the @-prefixed role as the target.
        assert_eq!(
            parse_command("/model @scout"),
            Command::Model(Some("@scout".to_string()))
        );
        assert_eq!(
            parse_command("/bogus"),
            Command::Unknown("bogus".to_string())
        );
    }
}
