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
use tools::{AsmTool, ToolRegistry, UnderstandTool};
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
    // The MVP wiring: the stub backend, the built-in echo tool, and aden-backed
    // pull-context tools rooted at the working directory. The model pulls
    // context (asm/understand) on demand; aden directs, coxn relays.
    let dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Deferred disclosure: only the `aden_tools` discovery seam is advertised;
    // the aden tools are latent, found by intent. No tool bloat by default, and
    // no useless always-on tool for a model to call gratuitously.
    let mut tools = ToolRegistry::new();
    tools.register_latent(Box::new(AsmTool::new(dir.clone())));
    tools.register_latent(Box::new(UnderstandTool::new(dir.clone())));
    let (model, mut sel) = resolve_model(&dir);
    let mut pump = Pump::new(model, tools);

    // A named task (COXN_TASK_NAME) makes aden define the scope: it sets the
    // gate mandate and loads exactly the seeds' context. No task = bare prompt.
    let task = load_task(&dir, &mut pump);

    let mut view = View::new();
    view.set_status(status_line(&dir, &sel.label(), &task));

    let mut tui = Tui::new()?;
    let result = drive(&mut tui, &mut view, &mut pump, &dir, &mut sel, &task).await;
    drop(tui); // restore the terminal before surfacing any error
    result
}

/// The status line: the active model, then aden's savings estimate when there
/// is one, else the task and a `/help` hint.
fn status_line(dir: &Path, model_label: &str, task: &str) -> String {
    let detail = aden::savings(dir).unwrap_or_else(|| {
        if task.is_empty() {
            "/help".to_string()
        } else {
            format!("{task}  /help")
        }
    });
    format!("{model_label}  |  {detail}")
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

/// If `COXN_TASK_NAME` is set, let aden define the scope: run `aden scope` for
/// the task's seeds, persist the manifest for the gate, and load exactly the
/// seeds' assembled context into the pump. Returns the status-line text. No
/// task means the bare, ungated Phase 1 pump. coxn parses nothing — it gates on
/// the manifest file and loads context from `aden asm` on its own seed inputs.
fn load_task(dir: &Path, pump: &mut Pump<AnyModel>) -> String {
    let Some(name) = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return String::new();
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
    let mut gated = false;
    if !seeds.is_empty()
        && let Ok(manifest_json) = aden::scope(dir, &name, &seeds, budget)
    {
        let manifest = std::env::temp_dir().join(format!("coxn-scope-{}.json", std::process::id()));
        if std::fs::write(&manifest, manifest_json).is_ok() {
            pump.set_gate(Box::new(aden::AdenGate::new(dir.to_path_buf(), manifest)));
            gated = true;
        }
    }

    // Load exactly the scope's context: the seeds' assembled neighborhoods.
    let mut context = String::new();
    for s in &seeds {
        if let Ok(text) = aden::pull(dir, aden::Pull::Asm(s)) {
            context.push_str(&text);
            context.push('\n');
        }
    }
    if !context.is_empty() {
        pump.set_context(context);
    }

    format!(
        "task '{name}' ({} seed(s){})",
        seeds.len(),
        if gated { ", gated" } else { "" }
    )
}

/// Help shown by `/help`.
const HELP: &str = "commands:\n  \
/help            show this help\n  \
/model           list available models (* = active)\n  \
/model <name|#>  switch the active model\n  \
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
            Some(Action::Submit) => {
                let text = view.take_input();
                if text.trim().is_empty() {
                    continue;
                }
                // A leading slash is a local command, not a model turn.
                if text.trim_start().starts_with('/') {
                    match parse_command(text.trim()) {
                        Command::Quit => return Ok(()),
                        Command::Help => view.output = HELP.to_string(),
                        Command::Model(None) => view.output = model_listing(sel),
                        Command::Model(Some(target)) => {
                            view.output = switch_model(pump, sel, &target);
                            view.set_status(status_line(dir, &sel.label(), task));
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
                pump.push_user(text);
                match pump.run_turn().await {
                    Ok(_) => {
                        view.output = transcript(pump.messages());
                        // Refresh the model + savings status after the turn.
                        view.set_status(status_line(dir, &sel.label(), task));
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
        assert_eq!(
            parse_command("/bogus"),
            Command::Unknown("bogus".to_string())
        );
    }
}
