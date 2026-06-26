//! coxn: a lean directional harness for aden.
//!
//! coxn is a "dumb pump": it steers and sets pace, and carries no intelligence.
//! aden directs and gates; the LLM acts; coxn steers. See DESIGN.adoc.

mod aden;
mod agents;
mod gate;
mod model;
mod openai;
mod pump;
mod sandbox;
mod session;
mod tools;
mod tui;

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use model::{AnyModel, Message, Role, StubModel, ThinkingLevel, ToolCall, Usage};
use pump::{Approval, Pump, TurnIo};
use tools::{AdenTool, EditTool, ReadFileTool, RunTool, ToolRegistry, WriteTool};
use tui::{
    Action, Menu, MenuItem, MenuKind, Tui, View, map_input_key, map_menu_key, map_modal_key,
};

/// How long the event loop waits for a key before redrawing.
const TICK: Duration = Duration::from_millis(100);

/// Lines the transcript scrolls per Up/Down (a wheel notch in most terminals).
const SCROLL_STEP: u16 = 3;

/// How many times a transient model error is retried before giving up.
const MAX_RETRIES: u32 = 3;
/// Backoff before each retry, in seconds (exponential).
const RETRY_BACKOFF_SECS: [u64; MAX_RETRIES as usize] = [2, 4, 8];

/// Register the five aden read tools.
///
/// When `available` is true, each tool is registered as active so the model can
/// use dense aden retrieval from turn one. When false, nothing is registered;
/// the `aden_tools` discovery seam will report an empty catalog, which is honest.
fn register_aden_tools(tools: &mut ToolRegistry, dir: &std::path::Path, available: bool) {
    if !available {
        return;
    }
    tools.register(Box::new(AdenTool::asm(dir.to_path_buf())));
    tools.register(Box::new(AdenTool::understand(dir.to_path_buf())));
    tools.register(Box::new(AdenTool::grep(dir.to_path_buf())));
    tools.register(Box::new(AdenTool::ask(dir.to_path_buf())));
    tools.register(Box::new(AdenTool::locate(dir.to_path_buf())));
}

/// A ship's wheel (the coxswain's helm): coxn steers, aden sets the heading.
/// Shown in the output pane at startup and after `/clear`, until the first turn.
const LOGO: &str = r#"
              .    |    .
          '.   \   |   /   .'
        '-._  \  \ | /  /  _.-'
      (==========( (o) )==========)
        '-._  /  / | \  \  _.-'
          .'   /   |   \   '.
              '    |    '
                   coxn
"#;

/// The startup splash: the logo plus a one-line hint.
fn welcome() -> String {
    format!("{LOGO}\n  you steer; aden sets the heading.  type a message, or /help")
}

/// Format the conversation into the output pane text. An assistant turn that
/// only requested tools (no text) renders its calls so the line is not blank.
fn transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| match m.role {
            Role::User => format!("you: {}", m.content),
            Role::Tool => {
                // run_command results start with "cmd:" and carry their own
                // structured lines (cmd:/ok:/err:); pass them through verbatim
                // so the TUI can color each line independently. Older sessions
                // and aden query results that lack the prefix get "tool: ".
                if m.content.starts_with("cmd:") {
                    m.content.clone()
                } else {
                    format!("tool: {}", m.content)
                }
            }
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

    // Probe aden availability once at boot. All downstream decisions read from
    // `caps`; nothing shells out to aden a second time for the same question.
    let caps = aden::probe(&dir);

    // When aden is present, register its five read tools as active so the model
    // uses dense retrieval immediately. When absent, register none; the discovery
    // seam reports an empty catalog, which is honest.
    let mut tools = ToolRegistry::new();
    register_aden_tools(&mut tools, &dir, caps.available);

    // The action set is always advertised; each mutating call is gated by user
    // approval at the prompt, and (when a task scope is active) by aden's
    // blast-radius gate on top. read_file is advertised with the editors so the
    // model can fetch the exact text to replace.
    tools.register(Box::new(ReadFileTool::new(dir.clone())));
    tools.register(Box::new(EditTool::new(dir.clone())));
    tools.register(Box::new(WriteTool::new(dir.clone())));
    // run_command lets the model close the edit->build->test loop. It is the
    // riskiest tool, so it is always approval-gated and confined by bwrap when
    // present (probed once here; the answer is shown at the approval prompt).
    let bwrap = sandbox::bwrap_available();
    tools.register(Box::new(RunTool::new(dir.clone(), bwrap)));

    // Take over the terminal and paint a frame first, so the user sees coxn
    // start instead of a frozen blank while the aden subprocess calls below
    // (model resolution, scope, asm context) run -- which can take several
    // seconds on a large repo.
    let mut view = View::new();
    view.output = welcome();
    view.set_status("starting coxn...".to_string());
    let mut tui = Tui::new()?;
    tui.draw(&view)?;

    let (model, mut sel) = resolve_model(&caps);
    view.set_status(format!("{}  |  loading...", sel.label()));
    tui.draw(&view)?;

    // A named task (COXN_TASK_NAME) makes aden define the scope: the gate
    // mandate and exactly the seeds' context. No task = bare prompt, edits gated
    // by approval alone.
    let task = load_task(&dir, &caps);
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
    // When no model was detected, nudge the user with a one-line hint so the
    // stub is not silently confusing.
    if sel.name == "stub" {
        view.output
            .push_str("\n\nno model detected -- set COXN_MODEL_BASE_URL or start LM Studio/Ollama");
    }

    let result = drive(
        &mut tui,
        &mut view,
        &mut pump,
        &dir,
        &caps,
        &mut sel,
        &task.status,
        bwrap,
    )
    .await;
    drop(tui); // restore the terminal before surfacing any error
    result
}

/// The boot status line: model and task only, no aden `status` call. Used before
/// the event loop so the slow savings probe does not delay the first prompt; the
/// savings appear on the first post-turn [`status_line`] refresh.
///
/// When no task is active the detail reads `"ungated  /help"` to make explicit
/// that edits are not scope-governed. A non-empty `task` string carries its own
/// mode annotation (e.g. "gated" or "ungated, no aden").
fn boot_status(model_label: &str, task: &str) -> String {
    let detail = if task.is_empty() {
        "ungated  /help".to_string()
    } else {
        format!("{task}  /help")
    };
    format!("{model_label}  |  {detail}")
}

/// The status line: the active model, then aden's savings estimate when there
/// is one (else the task + `/help` hint), then the context meter once a turn has
/// reported token usage.
fn status_line(dir: &Path, model_label: &str, task: &str, usage: Option<Usage>) -> String {
    let base = match aden::savings(dir) {
        Some(savings) => format!("{model_label}  |  {savings}"),
        None => boot_status(model_label, task),
    };
    match usage {
        Some(u) if u.prompt_tokens > 0 => format!("{base}  |  {}", ctx_meter(u.prompt_tokens)),
        _ => base,
    }
}

/// Format a context-size meter: the prompt tokens sent on the last turn, so the
/// user can see the conversation growing and `/clear` before it gets unwieldy.
fn ctx_meter(prompt_tokens: u32) -> String {
    if prompt_tokens >= 1000 {
        format!("~{:.1}k ctx", prompt_tokens as f64 / 1000.0)
    } else {
        format!("~{prompt_tokens} ctx")
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
/// status line. Resolution order: an explicit `COXN_MODEL_BASE_URL`, then the
/// pre-probed aden caps (`model.base_url` / `model.name`), then a local provider
/// auto-detected on its well-known port (Ollama / LM Studio), then the offline
/// stub. Selection is data, not a type, so per-role routing and sub-agents drop
/// in without reworking the seam. The key always comes from the environment,
/// never the committed config.
///
/// `caps` is passed in rather than re-shelling to aden here; startup already
/// called `aden::probe` once.
fn resolve_model(caps: &aden::AdenCaps) -> (AnyModel, ModelSel) {
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
    // 2. aden config (.aden/config.toml) read from pre-probed caps.
    if let Some(base_url) = caps.model_base_url.clone() {
        let model = caps
            .model_name
            .clone()
            .unwrap_or_else(|| "local".to_string());
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
            // Best-effort load state, so the user can pick a hot model and skip a
            // slow cold load. Empty (no annotations) on servers that do not report it.
            let loaded = openai::loaded_models(&e.base_url, e.key.as_deref()).unwrap_or_default();
            let mut out = format!("models on {} (/model <name|#> to switch):\n", e.base_url);
            for (i, m) in models.iter().enumerate() {
                let mark = if *m == sel.name { '*' } else { ' ' };
                let hot = if loaded.contains(m) { "  [loaded]" } else { "" };
                out.push_str(&format!("  {mark} {:>2}. {m}{hot}\n", i + 1));
            }
            out.push_str("(* = active, [loaded] = hot in memory)");
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
/// the role is unmapped or aden is unavailable. (A future `instance:model` value
/// selects the instance too; today the value is a model id on the active endpoint.)
fn resolve_role(dir: &Path, caps: &aden::AdenCaps, role: &str) -> Option<String> {
    if !caps.available {
        return None;
    }
    aden::config_get(dir, &format!("route.{role}"))
}

/// Slash command verbs, for Tab completion.
const COMMANDS: &[&str] = &[
    "help", "model", "think", "tools", "agents", "session", "resume", "edit", "clear", "quit",
];

/// The longest common prefix of `items` (empty if they share none).
fn longest_common_prefix(items: &[&str]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for s in &items[1..] {
        end = end.min(s.len());
        while !s.is_char_boundary(end) || first[..end] != s[..end] {
            end -= 1;
        }
    }
    first[..end].to_string()
}

/// Tab-complete a slash-command input: the command verb, or a `/resume` slug.
/// Returns the completed line, or `None` when there is nothing to add. Model
/// names are completed via the `/model` picker, not here.
fn complete_input(input: &str) -> Option<String> {
    let rest = input.strip_prefix('/')?;
    match rest.split_once(' ') {
        // Completing the command verb.
        None => {
            let cands: Vec<&str> = COMMANDS
                .iter()
                .copied()
                .filter(|c| c.starts_with(rest))
                .collect();
            match cands.as_slice() {
                [] => None,
                [only] => Some(format!("/{only} ")),
                many => {
                    let lcp = longest_common_prefix(many);
                    (lcp.len() > rest.len()).then(|| format!("/{lcp}"))
                }
            }
        }
        // Completing a `/resume <slug>` argument from saved sessions.
        Some(("resume", arg)) => {
            let slugs: Vec<String> = session::list()
                .into_iter()
                .map(|s| s.slug)
                .filter(|s| s.starts_with(arg))
                .collect();
            let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();
            match refs.as_slice() {
                [] => None,
                [only] => Some(format!("/resume {only}")),
                many => {
                    let lcp = longest_common_prefix(many);
                    (lcp.len() > arg.len()).then(|| format!("/resume {lcp}"))
                }
            }
        }
        _ => None,
    }
}

/// Build the `/model` picker (every advertised model, hot ones marked, the
/// active one starred). `None` for the offline stub or an unreachable endpoint.
fn model_menu(sel: &ModelSel) -> Option<Menu> {
    let e = sel.endpoint.as_ref()?;
    let models = openai::list_models(&e.base_url, e.key.as_deref())?;
    if models.is_empty() {
        return None;
    }
    let loaded = openai::loaded_models(&e.base_url, e.key.as_deref()).unwrap_or_default();
    let selected = models.iter().position(|m| *m == sel.name).unwrap_or(0);
    let items = models
        .into_iter()
        .map(|m| {
            let hot = if loaded.contains(&m) {
                "  [loaded]"
            } else {
                ""
            };
            let active = if m == sel.name { "  *" } else { "" };
            MenuItem {
                label: format!("{m}{hot}{active}"),
                value: m,
            }
        })
        .collect();
    Some(Menu {
        kind: MenuKind::Model,
        title: "models".to_string(),
        items,
        selected,
    })
}

/// Build the `/session` picker (saved sessions, newest first). `None` if none.
fn session_menu() -> Option<Menu> {
    let sessions = session::list();
    if sessions.is_empty() {
        return None;
    }
    let items = sessions
        .into_iter()
        .map(|s| MenuItem {
            label: format!(
                "{:>4}  {}  {}",
                session::relative_age(s.age_secs),
                s.slug,
                s.preview
            ),
            value: s.slug,
        })
        .collect();
    Some(Menu {
        kind: MenuKind::Session,
        title: "sessions".to_string(),
        items,
        selected: 0,
    })
}

/// Read the task config from the environment: `(name, seeds, budget)`. `None`
/// when no task is set. Shared by the boot path and `/agents`.
fn task_config() -> Option<(String, Vec<String>, u64)> {
    let name = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let seeds = std::env::var("COXN_TASK_SEEDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let budget = std::env::var("COXN_TASK_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8192);
    Some((name, seeds, budget))
}

/// Render `/agents`: run aden's partition for the current task and show each
/// sub-scope, the model its role routes to, and its dependencies. Inspection
/// only -- the autonomous runner (one pump per sub-scope) is a later step.
fn agents_listing(dir: &Path, caps: &aden::AdenCaps) -> String {
    if !caps.available {
        return "aden is not available; /agents requires aden on PATH".to_string();
    }
    let Some((name, seeds, budget)) = task_config() else {
        return "set COXN_TASK_NAME + COXN_TASK_SEEDS to partition a task".to_string();
    };
    if seeds.is_empty() {
        return "the task has no seeds to partition".to_string();
    }
    let index = match aden::scope_agents(dir, &name, &seeds, budget) {
        Ok(index) => index,
        Err(e) => return format!("aden scope --agents failed: {e}"),
    };
    let scopes = agents::parse_index(&index);
    if scopes.is_empty() {
        return "aden returned no sub-scopes".to_string();
    }
    let mut out = format!("partition of '{name}' (dependency order):\n");
    for s in agents::dependency_order(&scopes) {
        let model =
            resolve_role(dir, caps, &s.role).unwrap_or_else(|| "(default model)".to_string());
        let after = if s.depends_on.is_empty() {
            String::new()
        } else {
            format!("  after {}", s.depends_on.join(", "))
        };
        out.push_str(&format!("  {} [{}] -> {model}{after}\n", s.id, s.role));
    }
    out.push_str("(plan only; the sub-agent runner is not yet wired)");
    out
}

/// The aden-agnostic action instructions prepended to scope context in task mode.
///
/// coxn's default prompt is empty (zero-default-context), which leaves a weak
/// local model passive. This nudge makes it act even without aden present.
/// DESIGN sanctions this single preamble: it is operating instruction, not repo
/// context, and appears only when a task name is active.
const AGENT_PREAMBLE_BASE: &str = "\
You are coxn, a coding agent. To change code, call `read_file` to get the exact \
current text, then `edit` (replace an exact unique string) or `write_file` (whole \
file) -- do not print a patch for the user to apply. To build, test, run, or use \
git, call `run_command`: it runs in a sandbox confined to the project, with no \
network unless you set network:true. Verify your changes by running the tests.\n\n";

/// The aden-specific suffix appended when aden is present and the scope gated.
///
/// Appended after [`AGENT_PREAMBLE_BASE`] when aden produced a scope manifest,
/// so every edit is governed. Followed immediately by the per-seed asm context.
const AGENT_PREAMBLE_ADEN: &str = "\
Edits are gated by aden against the task scope and reverted if they escape, so \
keep changes minimal and in scope. To search or understand code, use the aden \
tools: aden_grep, aden_locate, aden_asm, aden_understand, aden_ask.\n\n\
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
///
/// When aden is absent, the base preamble is still loaded so the model can act;
/// edits are approval-gated only, and the status line records that honestly.
fn load_task(dir: &Path, caps: &aden::AdenCaps) -> Task {
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

    // Define the mandate when aden is available: aden scope -> manifest -> gate.
    let mut gate: Option<Box<dyn gate::Gate>> = None;
    if caps.available
        && !seeds.is_empty()
        && let Ok(manifest_json) = aden::scope(dir, &name, &seeds, budget)
    {
        let manifest = std::env::temp_dir().join(format!("coxn-scope-{}.json", std::process::id()));
        if std::fs::write(&manifest, manifest_json).is_ok() {
            gate = Some(Box::new(aden::AdenGate::new(dir.to_path_buf(), manifest)));
        }
    }
    let gated = gate.is_some();

    // Assemble context: always start with the base preamble so the model acts.
    // When gated, append the aden suffix and the seeds' assembled neighborhoods.
    // When aden is absent, append a one-line note instead.
    let mut context = AGENT_PREAMBLE_BASE.to_string();
    if gated {
        context.push_str(AGENT_PREAMBLE_ADEN);
        for s in &seeds {
            if let Ok(text) = aden::pull(dir, aden::Pull::Asm(s)) {
                context.push_str(&text);
                context.push('\n');
            }
        }
    } else if !caps.available {
        context.push_str(
            "aden is not available, so edits are gated by your approval only; \
browse code with read_file and run_command (e.g. grep).\n\n",
        );
    }

    let status = if gated {
        format!("task '{name}' ({} seed(s), gated)", seeds.len())
    } else if caps.available {
        // aden is present but produced no gate (no seeds, or scope failed).
        format!("task '{name}' (ungated)")
    } else {
        format!("task '{name}' (ungated, no aden)")
    };

    Task {
        status,
        gate,
        context: Some(context),
    }
}

/// Help shown by `/help`.
const HELP: &str = "commands:\n  \
/help            show this help\n  \
/model           list available models (* = active, [loaded] = hot)\n  \
/model <name|#>  switch the active model\n  \
/model @<role>   switch to the model mapped for a role (route.<role> config)\n  \
/tools           list the aden tools the model can discover\n  \
/think <level>   reasoning effort: off | low | med | high\n  \
/agents          show the task partition (sub-scopes + routed models)\n  \
/session         list saved sessions\n  \
/resume <slug>   load a saved session\n  \
/edit [path]     open the last-edited file (or path) in $EDITOR\n  \
/clear           clear the conversation (keeps the task scope)\n  \
/quit            leave coxn\n\
/model and /session open an arrow-navigable picker (Up/Down, Enter, Esc).\n\
keys:\n  \
Enter            send         Ctrl-C   cancel a turn / quit when idle\n  \
Tab              complete a command or /resume slug\n  \
Up/Down          scroll chat  PgUp/Dn  scroll a page\n  \
Ctrl-P/Ctrl-N    input history             Ctrl-W   delete word\n  \
Ctrl-K/Ctrl-U    cut to end/start          Ctrl-Y   yank (paste)\n  \
Left/Right Home/End  move cursor\n\
anything else is sent to the model.\n\
the model can run shell commands (sandboxed; network off by default); you \
approve each one at the prompt.";

/// A slash command typed into the input line.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Quit,
    Clear,
    /// `/model` lists; `/model <name|#>` switches.
    Model(Option<String>),
    Tools,
    /// `/session` lists saved sessions.
    Session,
    /// `/resume <slug>` loads a saved session.
    Resume(Option<String>),
    /// `/edit [path]` opens the last-edited file (or `path`) in `$EDITOR`.
    OpenEditor(Option<String>),
    /// `/think [off|low|med|high]` sets the reasoning-effort level.
    Think(Option<String>),
    /// `/agents` shows the task partition (sub-scopes + routed models).
    Agents,
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
        "session" | "sessions" => Command::Session,
        "resume" => Command::Resume(arg),
        "edit" => Command::OpenEditor(arg),
        "think" => Command::Think(arg),
        "agents" => Command::Agents,
        other => Command::Unknown(other.to_string()),
    }
}

/// Wait before retrying a transient model error, showing a per-second countdown
/// in the status line. Returns `true` if the user pressed Ctrl-C to give up.
fn retry_wait(tui: &mut Tui, view: &mut View, attempt: u32, secs: u64) -> io::Result<bool> {
    for remaining in (1..=secs).rev() {
        view.set_status(format!(
            "model error -- retrying {attempt}/{MAX_RETRIES} in {remaining}s (Ctrl-C to cancel)"
        ));
        tui.draw(view)?;
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < until {
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && matches!(map_input_key(key), Some(Action::Quit))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The output pane's (width, height), for wrapping and PageUp/PageDown scroll
/// amounts. Height excludes the status and input rows. Falls back to (80, 1) if
/// the terminal size cannot be determined.
fn pane_dims(tui: &Tui) -> (u16, u16) {
    tui.size()
        .map(|s| (s.width.max(1), s.height.saturating_sub(2).max(1)))
        .unwrap_or((80, 1))
}

/// Summarize a tool call for the approval prompt. For `run_command`, show the
/// command, whether network was requested, and the confinement level (so the
/// user judges the real risk before approving). For the file tools, the name
/// plus its target path. `bwrap` is whether sandbox confinement is available.
fn approval_summary(call: &ToolCall, bwrap: bool) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    if call.name == "run_command" {
        let command = parsed
            .as_ref()
            .and_then(|v| v.get("command").and_then(|c| c.as_str()))
            .unwrap_or("")
            .trim();
        let network = parsed
            .as_ref()
            .and_then(|v| v.get("network").and_then(|n| n.as_bool()))
            .unwrap_or(false);
        let box_tag = if bwrap { "sandbox" } else { "NO SANDBOX" };
        let net_tag = if network { ", NET ON" } else { "" };
        return format!("run [{box_tag}{net_tag}]: {command}");
    }
    let path = parsed.and_then(|v| v.get("path").and_then(|p| p.as_str()).map(str::to_string));
    match path {
        Some(p) => format!("{} {p}", call.name),
        None => call.name.clone(),
    }
}

/// Extract the trimmed command string from a `run_command` tool call so that
/// session approval can be scoped to the exact command rather than the tool
/// name. Returns `Some(trimmed_command)` when the call is `run_command` and its
/// arguments parse to a JSON object with a string `"command"` field; `None` for
/// any other tool or malformed arguments. A `None` result means session approval
/// must never auto-allow via the command path.
fn command_key(call: &ToolCall) -> Option<String> {
    if call.name != "run_command" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let cmd = v.get("command")?.as_str()?;
    Some(cmd.trim().to_string())
}

/// The per-turn terminal I/O the pump drives: stream the reply live (with a
/// Ctrl-C cancel check between fragments) and prompt for approval before a
/// mutating tool runs. Owns the terminal borrows so the pump needs one handle.
struct DriveIo<'a> {
    tui: &'a mut Tui,
    view: &'a mut View,
    prior: &'a str,
    buf: String,
    /// Accumulated live output from a streaming run_command call.
    run_buf: String,
    /// Set when the stream was cancelled (Ctrl-C mid-reply).
    cancelled: bool,
    /// Set when an approval prompt returned cancel-turn.
    cancel_turn: bool,
    approvals: &'a mut HashSet<String>,
    /// Session-approved command strings for `run_command`. Keyed on the exact
    /// trimmed command so [s] on one command does not auto-allow a different one.
    approved_commands: &'a mut HashSet<String>,
    /// Whether bwrap confinement is available, surfaced in the run_command
    /// approval prompt so the user knows the isolation level before approving.
    bwrap: bool,
}

impl DriveIo<'_> {
    /// Repaint the output pane with the current assistant text and any live
    /// run_command output appended below it.
    fn repaint(&mut self) {
        self.view.output = if self.run_buf.is_empty() {
            format!("{}\ncoxn: {}", self.prior, self.buf)
        } else {
            format!("{}\ncoxn: {}\n{}", self.prior, self.buf, self.run_buf)
        };
        let _ = self.tui.draw(self.view);
    }
}

impl TurnIo for DriveIo<'_> {
    fn on_delta(&mut self, delta: &str) -> bool {
        self.buf.push_str(delta);
        self.repaint();
        // Non-blocking cancel check: Ctrl-C aborts the turn.
        if let Ok(true) = event::poll(Duration::ZERO)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
            && matches!(map_input_key(key), Some(Action::Quit))
        {
            self.cancelled = true;
            return false;
        }
        true
    }

    fn on_run_output(&mut self, line: &str) -> bool {
        self.run_buf.push_str(line);
        self.run_buf.push('\n');
        self.repaint();
        // Non-blocking cancel check: Ctrl-C kills the child.
        if let Ok(true) = event::poll(Duration::ZERO)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
            && matches!(map_input_key(key), Some(Action::Quit))
        {
            self.cancelled = true;
            return false;
        }
        true
    }

    fn approve(&mut self, call: &ToolCall) -> Approval {
        // run_command session approval is keyed on the exact command string, not
        // the tool name, so [s] on one command does not auto-allow a different one.
        if let Some(cmd) = command_key(call) {
            if self.approved_commands.contains(&cmd) {
                return Approval::Allow;
            }
        } else if self.approvals.contains(&call.name) {
            // For all other tools, session approval is by tool name.
            return Approval::Allow;
        }
        let summary = approval_summary(call, self.bwrap);
        let session_label = if command_key(call).is_some() {
            "[s]ession (this exact command)".to_string()
        } else {
            format!("[s]ession (all {} calls)", call.name)
        };
        loop {
            self.view.set_status(format!(
                "approve {summary}?  [o]nce  {session_label}  [d]ecline  [x] cancel turn",
            ));
            let _ = self.tui.draw(self.view);
            if event::poll(TICK).unwrap_or(false)
                && let Ok(Event::Key(key)) = event::read()
                && key.kind == KeyEventKind::Press
            {
                // Ctrl-C cancels the turn here too, not just x/Esc.
                if matches!(map_input_key(key), Some(Action::Quit)) {
                    self.cancel_turn = true;
                    return Approval::CancelTurn;
                }
                match key.code {
                    KeyCode::Char('o' | 'O') => return Approval::Allow,
                    KeyCode::Char('s' | 'S') => {
                        if let Some(cmd) = command_key(call) {
                            self.approved_commands.insert(cmd);
                        } else {
                            self.approvals.insert(call.name.clone());
                        }
                        return Approval::Allow;
                    }
                    KeyCode::Char('d' | 'D') => return Approval::Decline,
                    KeyCode::Char('x' | 'X') | KeyCode::Esc => {
                        self.cancel_turn = true;
                        return Approval::CancelTurn;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The event loop: draw, read a key, route it by mode (modal vs input), and run
/// a turn on submit. Carries no intelligence; it only paces and shuttles.
#[allow(clippy::too_many_arguments)]
async fn drive(
    tui: &mut Tui,
    view: &mut View,
    pump: &mut Pump<AnyModel>,
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &mut ModelSel,
    task: &str,
    bwrap: bool,
) -> io::Result<()> {
    // Append-only session persistence: every message not yet written is flushed
    // after each turn. `persisted` tracks how many of pump.messages() are on disk.
    let mut session = session::Session::create();
    let mut persisted = 0usize;
    // Tool names approved "for the session" -- they skip the approval prompt.
    let mut approvals: HashSet<String> = HashSet::new();
    // Exact command strings approved for the session via run_command [s].
    let mut approved_commands: HashSet<String> = HashSet::new();
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

        // A picker grabs input until selected or cancelled.
        if view.menu.is_some() {
            match map_menu_key(key) {
                Some(Action::MenuUp) => view.menu_move(-1),
                Some(Action::MenuDown) => view.menu_move(1),
                Some(Action::MenuCancel) => view.close_menu(),
                Some(Action::MenuSelect) => {
                    let pick = view
                        .menu
                        .as_ref()
                        .and_then(|m| m.items.get(m.selected).map(|it| (m.kind, it.value.clone())));
                    view.close_menu();
                    if let Some((kind, value)) = pick {
                        match kind {
                            MenuKind::Model => {
                                view.output = switch_model(pump, sel, &value);
                                view.set_status(status_line(
                                    dir,
                                    &sel.label(),
                                    task,
                                    pump.last_usage(),
                                ));
                            }
                            MenuKind::Session => {
                                let messages = session::load(&value);
                                if messages.is_empty() {
                                    view.output = format!("no session '{value}'");
                                } else {
                                    persisted = messages.len();
                                    pump.load_conversation(messages);
                                    // Switching sessions resets session-scoped approvals.
                                    approvals.clear();
                                    approved_commands.clear();
                                    session = session::Session::open(&value);
                                    view.output = transcript(pump.messages());
                                    view.set_status(status_line(
                                        dir,
                                        &sel.label(),
                                        task,
                                        pump.last_usage(),
                                    ));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        match map_input_key(key) {
            Some(Action::Quit) => return Ok(()),
            Some(Action::Complete) => {
                if let Some(completed) = complete_input(&view.input) {
                    view.input = completed;
                    view.cursor_end();
                }
            }
            Some(Action::Append(c)) => view.input_push(c),
            Some(Action::Backspace) => view.input_backspace(),
            Some(Action::CursorLeft) => view.cursor_left(),
            Some(Action::CursorRight) => view.cursor_right(),
            Some(Action::CursorHome) => view.cursor_home(),
            Some(Action::CursorEnd) => view.cursor_end(),
            Some(Action::WordDelete) => view.word_delete(),
            Some(Action::KillToEnd) => view.kill_to_end(),
            Some(Action::KillToStart) => view.kill_to_start(),
            Some(Action::Yank) => view.yank(),
            Some(Action::HistoryPrev) => view.history_prev(),
            Some(Action::HistoryNext) => view.history_next(),
            Some(Action::ScrollUp) => {
                let (w, h) = pane_dims(tui);
                view.scroll_up(SCROLL_STEP, view.max_scroll(w, h));
            }
            Some(Action::ScrollDown) => view.scroll_down(SCROLL_STEP),
            Some(Action::PageUp) => {
                let (w, h) = pane_dims(tui);
                view.scroll_up(h, view.max_scroll(w, h));
            }
            Some(Action::PageDown) => {
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
                        Command::Model(None) => match model_menu(sel) {
                            Some(menu) => view.open_menu(menu),
                            None => view.output = model_listing(sel),
                        },
                        Command::Model(Some(target)) => {
                            // `@role` resolves through the [route] table; anything
                            // else is a model name or index.
                            let resolved = if let Some(role) = target.strip_prefix('@') {
                                resolve_role(dir, caps, role).ok_or_else(|| {
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
                                    view.set_status(status_line(
                                        dir,
                                        &sel.label(),
                                        task,
                                        pump.last_usage(),
                                    ));
                                }
                                Err(msg) => view.output = msg,
                            }
                        }
                        Command::Tools => view.output = pump.tool_catalog(),
                        Command::Agents => view.output = agents_listing(dir, caps),
                        Command::Think(arg) => {
                            view.output = match arg.as_deref().map(ThinkingLevel::parse) {
                                Some(Some(level)) => {
                                    pump.set_thinking(level);
                                    format!("reasoning effort: {}", level.label())
                                }
                                _ => "usage: /think [off|low|med|high]".to_string(),
                            };
                        }
                        Command::OpenEditor(arg) => {
                            let full = match arg {
                                Some(a) => Some(dir.join(a)),
                                None => pump.last_edited(),
                            };
                            view.output = match full {
                                None => "no file to open (usage: /edit <path>)".to_string(),
                                Some(path) => {
                                    let editor = std::env::var("VISUAL")
                                        .or_else(|_| std::env::var("EDITOR"))
                                        .unwrap_or_default();
                                    if editor.trim().is_empty() {
                                        "set $VISUAL or $EDITOR to open files".to_string()
                                    } else {
                                        tui.run_external(|| {
                                            let _ = std::process::Command::new(&editor)
                                                .arg(&path)
                                                .status();
                                        })?;
                                        format!("opened {} in {editor}", path.display())
                                    }
                                }
                            };
                        }
                        Command::Session => match session_menu() {
                            Some(menu) => view.open_menu(menu),
                            None => view.output = session_listing(&session.slug()),
                        },
                        Command::Resume(slug) => {
                            view.output = match slug {
                                Some(slug) => {
                                    let messages = session::load(&slug);
                                    if messages.is_empty() {
                                        format!("no session '{slug}' (try /session)")
                                    } else {
                                        let n = messages.len();
                                        pump.load_conversation(messages);
                                        // Switching sessions resets session-scoped approvals.
                                        approvals.clear();
                                        approved_commands.clear();
                                        session = session::Session::open(&slug);
                                        persisted = n;
                                        let out = transcript(pump.messages());
                                        view.set_status(status_line(
                                            dir,
                                            &sel.label(),
                                            task,
                                            pump.last_usage(),
                                        ));
                                        out
                                    }
                                }
                                None => "usage: /resume <slug>  (see /session)".to_string(),
                            };
                        }
                        Command::Clear => {
                            pump.clear_conversation();
                            view.output = welcome();
                            // A cleared conversation starts a fresh session file
                            // and forgets session-level tool approvals.
                            session = session::Session::create();
                            persisted = 0;
                            approvals.clear();
                            approved_commands.clear();
                            view.set_status(status_line(
                                dir,
                                &sel.label(),
                                task,
                                pump.last_usage(),
                            ));
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
                view.pending_since = Some(Instant::now());
                // Run the turn (streaming + per-tool approval via DriveIo),
                // retrying transient backend errors with a cancellable countdown.
                let mut attempt = 0u32;
                let result;
                let mut cancelled;
                loop {
                    let mut io = DriveIo {
                        tui,
                        view,
                        prior: &prior,
                        buf: String::new(),
                        run_buf: String::new(),
                        cancelled: false,
                        cancel_turn: false,
                        approvals: &mut approvals,
                        approved_commands: &mut approved_commands,
                        bwrap,
                    };
                    let r = pump.run_turn_streaming(&mut io).await;
                    cancelled = io.cancelled || io.cancel_turn;
                    if let Err(e) = &r
                        && e.is_transient()
                        && attempt < MAX_RETRIES
                        && !cancelled
                    {
                        attempt += 1;
                        let secs = RETRY_BACKOFF_SECS[(attempt - 1) as usize];
                        if retry_wait(tui, view, attempt, secs)? {
                            result = r; // user gave up; surface the error
                            break;
                        }
                        continue;
                    }
                    result = r;
                    break;
                }
                view.pending_since = None;
                match result {
                    Ok(_) => {
                        view.output = transcript(pump.messages());
                        // Refresh the model + savings + context status after the turn.
                        let status = status_line(dir, &sel.label(), task, pump.last_usage());
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
                // Persist any messages this turn added (user + assistant + tools).
                let messages = pump.messages();
                for message in &messages[persisted.min(messages.len())..] {
                    session.append(message);
                }
                persisted = messages.len();
            }
            _ => {}
        }
    }
}

/// Render the `/session` listing: saved sessions, newest first, with a relative
/// age and a preview of the first user line. The active session is marked `*`.
fn session_listing(active: &str) -> String {
    let sessions = session::list();
    if sessions.is_empty() {
        return "no saved sessions yet".to_string();
    }
    let mut out = String::from("saved sessions (/resume <slug>):\n");
    for s in &sessions {
        let mark = if s.slug == active { '*' } else { ' ' };
        out.push_str(&format!(
            "  {mark} {:>4}  {}  {}\n",
            session::relative_age(s.age_secs),
            s.slug,
            s.preview
        ));
    }
    out.push_str("(* = active, newest first)");
    out
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
    fn transcript_passes_cmd_tool_messages_through_verbatim() {
        // A tool message whose content starts with "cmd:" (a run_command result)
        // must not be prefixed with "tool: " so the TUI can color each line.
        let cmd_content = "cmd: sandboxed\nok: exit 0\nhello";
        let messages = vec![Message::tool_result("r1", cmd_content)];
        let out = transcript(&messages);
        assert_eq!(out, cmd_content, "cmd: message must be verbatim: {out}");

        // A regular tool message (aden query result) still gets "tool: ".
        let aden_content = "asm result here";
        let messages2 = vec![Message::tool_result("r2", aden_content)];
        let out2 = transcript(&messages2);
        assert_eq!(
            out2,
            format!("tool: {aden_content}"),
            "non-cmd tool must be prefixed: {out2}"
        );
    }

    #[test]
    fn longest_common_prefix_of_candidates() {
        assert_eq!(longest_common_prefix(&["think", "tools"]), "t");
        assert_eq!(longest_common_prefix(&["model"]), "model");
        assert_eq!(longest_common_prefix(&["abc", "abd", "abe"]), "ab");
        assert_eq!(longest_common_prefix(&["x", "y"]), "");
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn tab_completes_command_verbs() {
        // A unique prefix completes with a trailing space.
        assert_eq!(complete_input("/mod").as_deref(), Some("/model "));
        assert_eq!(complete_input("/he").as_deref(), Some("/help "));
        // An ambiguous prefix that cannot be extended yields nothing
        // (think/tools share only the typed "t").
        assert_eq!(complete_input("/t"), None);
        // No match, and non-command input, complete to nothing.
        assert_eq!(complete_input("/zzz"), None);
        assert_eq!(complete_input("hello"), None);
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

    #[test]
    fn command_key_extracts_trimmed_command_for_run_command() {
        // Valid run_command call yields the trimmed command string.
        let valid = ToolCall {
            id: "t1".into(),
            name: "run_command".into(),
            arguments: r#"{"command":"  cargo test  "}"#.into(),
        };
        assert_eq!(command_key(&valid), Some("cargo test".to_string()));

        // A different tool name always returns None.
        let other = ToolCall {
            id: "t2".into(),
            name: "edit".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        };
        assert_eq!(command_key(&other), None);

        // Malformed JSON returns None.
        let bad_json = ToolCall {
            id: "t3".into(),
            name: "run_command".into(),
            arguments: "not json".into(),
        };
        assert_eq!(command_key(&bad_json), None);

        // Missing "command" field returns None.
        let missing_field = ToolCall {
            id: "t4".into(),
            name: "run_command".into(),
            arguments: r#"{"network":true}"#.into(),
        };
        assert_eq!(command_key(&missing_field), None);
    }

    #[test]
    fn boot_status_ungated_when_no_task() {
        // No task -> detail must contain "ungated".
        let s = boot_status("stub-model", "");
        assert!(s.contains("ungated"), "expected 'ungated' in: {s}");
        assert!(!s.contains("/help") || s.contains("ungated"), "{s}");
    }

    #[test]
    fn boot_status_task_text_when_task_set() {
        // A non-empty task string appears in the status line.
        let s = boot_status("stub-model", "task 'foo' (1 seed(s), gated)");
        assert!(s.contains("task 'foo'"), "expected task text in: {s}");
        // Should not inject "ungated" when the task string is already set.
        assert!(!s.starts_with("ungated"), "{s}");
    }

    #[test]
    fn register_aden_tools_active_when_available() {
        let mut tools = ToolRegistry::new();
        register_aden_tools(&mut tools, std::path::Path::new("."), true);
        let names: Vec<String> = tools
            .advertised_defs()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for expected in [
            "aden_asm",
            "aden_understand",
            "aden_grep",
            "aden_ask",
            "aden_locate",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn register_aden_tools_empty_when_unavailable() {
        let mut tools = ToolRegistry::new();
        register_aden_tools(&mut tools, std::path::Path::new("."), false);
        let names: Vec<String> = tools
            .advertised_defs()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for absent in [
            "aden_asm",
            "aden_understand",
            "aden_grep",
            "aden_ask",
            "aden_locate",
        ] {
            assert!(
                !names.contains(&absent.to_string()),
                "unexpected {absent} in {names:?}"
            );
        }
    }

    #[test]
    fn preamble_base_has_no_aden_references() {
        assert!(
            !AGENT_PREAMBLE_BASE.contains("aden"),
            "AGENT_PREAMBLE_BASE must not mention aden: {AGENT_PREAMBLE_BASE}"
        );
    }

    #[test]
    fn preamble_aden_mentions_aden() {
        assert!(
            AGENT_PREAMBLE_ADEN.contains("aden"),
            "AGENT_PREAMBLE_ADEN must mention aden"
        );
    }
}
