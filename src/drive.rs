//! Interactive TUI session: boot wiring and the event loop.
//!
//! Extracted from `main.rs` (Phase K) so the binary entrypoint stays CLI-only.
//! Behaviour unchanged — pure structural move.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use crate::app::{
    AGENT_PREAMBLE_ADEN, AGENT_PREAMBLE_BASE, ModelSel, openai_model, rebuild_codex_model,
    resolve_instance_from_config, resolve_role, task_config,
};
use crate::codex_model::{self, CODEX_ENDPOINT_SCHEME};
use crate::commands::{Command, complete_input, parse_command};
use crate::layout;
use crate::model::{AnyModel, Message, Role, StubModel, ThinkingLevel, ToolCall, Usage};
use crate::mouse::{MouseEffect, frame_layout, handle_mouse, osc52_copy};
use crate::pump::{Approval, BatchIo, Pump, TurnIo};
use crate::tools::{build_registry, register_aden_tools};
use crate::trust::Trust;
use crate::tui::{
    Action, Menu, MenuItem, MenuKind, ModalKind, ToolApprovalChoice, Tui, View, map_input_key,
    map_modal_key, map_tool_approval_key, menu_max_rows,
};
use crate::vim::{Outcome, Scroll};
use crate::{
    aden, agents, auth, doctor, execute, gate, openai, provider, run_ledger, sandbox, session,
    tools, trust, vim,
};

const TICK: Duration = Duration::from_millis(100);

/// Non-blocking Ctrl-C / quit check while a long local operation runs.
fn poll_user_cancel() -> bool {
    if let Ok(true) = event::poll(Duration::ZERO)
        && let Ok(Event::Key(key)) = event::read()
        && key.kind == KeyEventKind::Press
    {
        return matches!(map_input_key(key), Some(Action::Quit));
    }
    false
}

/// Lines the transcript scrolls per Up/Down (a wheel notch in most terminals).
const SCROLL_STEP: u16 = 3;

/// How many times a transient model error is retried before giving up.
const MAX_RETRIES: u32 = 3;
/// Backoff before each retry, in seconds (exponential).
const RETRY_BACKOFF_SECS: [u64; MAX_RETRIES as usize] = [2, 4, 8];

/// Re-discover capabilities that may have come online since boot, with no
/// reboot: a model backend (LM Studio/Ollama started, an endpoint exported) and
/// aden's context tools (aden installed / a graph generated). Returns a short
/// note naming what was hot-loaded, or `None` when nothing changed.
///
/// Cost discipline: shelling `aden --version` and probing endpoints is not free,
/// so unless `force` is set (an explicit `/model` / `/tools` / `/agents`), it
/// only probes while a model is still missing. Once a provider is resolved the
/// per-turn path returns immediately, so steady-state turns pay nothing.
fn refresh_discovery(
    dir: &Path,
    pump: &mut Pump<AnyModel>,
    sel: &mut ModelSel,
    force: bool,
) -> Option<String> {
    let need_model = sel.endpoint.is_none();
    if !force && !need_model {
        return None;
    }
    let caps = aden::probe(dir);
    let mut notes: Vec<String> = Vec::new();
    if need_model {
        let (model, new_sel) = resolve_model(dir, &caps);
        if new_sel.endpoint.is_some() {
            pump.set_model(model);
            *sel = new_sel;
            notes.push(format!("model {}", sel.name));
        }
    }
    if register_aden_tools(pump.registry_mut(), dir, caps.available) {
        notes.push("aden tools".to_string());
    }
    (!notes.is_empty()).then(|| format!("hot-loaded: {}", notes.join(" + ")))
}

/// The startup greeting, shown until the first turn and after `/clear`. A single
/// `sys:` line so it renders in the transcript's own voice -- no ASCII art.
fn welcome(aden_active: bool) -> String {
    let mut lines = vec!["sys: coxn — gated coding harness".to_string()];
    if aden_active {
        lines.push("sys: aden on PATH — scope gate + graph tools available".to_string());
    }
    if vim::enabled() {
        lines.push("sys: vim modes on (COXN_VIM=1) — Esc Normal, / search, g? help".to_string());
    } else {
        lines.push("sys: chat-first — type a task, Enter to send, g? for keys".to_string());
        lines.push(
            "sys: Ctrl-Space palette · @ attach files · !cmd shell (y/n gate) · /help".to_string(),
        );
    }
    lines.join("\n")
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
                    .map(tool_call_card)
                    .collect::<Vec<_>>()
                    .join("\n");
                if m.content.is_empty() {
                    format!("coxn:\n{calls}")
                } else {
                    format!("coxn: {}\n{calls}", m.content)
                }
            }
        })
        .collect::<Vec<_>>()
        // A blank line between turns gives the transcript vertical rhythm instead
        // of a wall of text; styled_output renders the empty line with the rule.
        .join("\n\n")
}

/// True when the user opts into the zero-default-context floor (`COXN_BARE=1`).
fn bare_mode() -> bool {
    std::env::var("COXN_BARE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

pub(crate) async fn run_tui(dir: &Path) -> io::Result<()> {
    // The wiring: a runtime-selected backend and aden-backed pull-context tools
    // rooted at the working directory. The model pulls context (asm/understand)
    // on demand; aden directs, coxn relays.

    // Probe aden availability once at boot. All downstream decisions read from
    // `caps`; nothing shells out to aden a second time for the same question.
    let caps = aden::probe(dir);

    // Coxn owns the write path: one quiet gen at boot, then every read sets
    // ADEN_SKIP_AUTO_GEN so aden grep/asm/scope never fight the store lock.
    if caps.available {
        let _ = aden::ensure_indexed(dir);
    }

    // When aden is present, register its five read tools as active so the model
    // uses dense retrieval immediately. When absent, register none; the discovery
    // seam reports an empty catalog, which is honest.
    let bwrap = sandbox::bwrap_available();
    let tools = build_registry(dir, &caps, bwrap);

    // Take over the terminal and paint a frame first, so the user sees coxn
    // start instead of a frozen blank while the aden subprocess calls below
    // (model resolution, scope, asm context) run -- which can take several
    // seconds on a large repo.
    let mut view = View::new();
    view.output = welcome(caps.available);
    view.set_status("starting coxn...".to_string());
    view.refresh_suggestion();
    let mut tui = Tui::new()?;
    tui.draw(&view)?;

    let (model, mut sel) = resolve_model(dir, &caps);
    view.set_status(format!("{}  |  loading...", sel.label()));
    tui.draw(&view)?;

    // A named task (COXN_TASK_NAME) makes aden define the scope: the gate
    // mandate and exactly the seeds' context. No task = bare prompt, edits gated
    // by approval alone.
    let task = load_task(dir, &caps);
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
    view.set_status(boot_status(&sel.label(), &task.status, caps.available));
    if trust::auto_approve_enabled() {
        view.output
            .push_str("\nsys: WARNING — COXN_AUTO_APPROVE=1 bypasses the human approval gate\n");
    }
    // Offline stub: block chat until a model is reachable (not a silent echo toy).
    if !vim::enabled() {
        view.show_mode_tip();
    }
    if sel.is_offline_stub() {
        view.output.push_str(
            "\n\nsys: ⚠ offline — no model reachable yet\n\
             sys: fix: /auth setup · start Ollama/LM Studio · set COXN_MODEL_BASE_URL\n\
             sys: [r] retry  [q] quit  (slash commands still work)",
        );
        view.set_status("OFFLINE STUB  |  /auth setup  /model  [r] retry  [q] quit".to_string());
    }
    if std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some()
        && doctor::git_dirty(dir)
    {
        view.output.push_str(
            "\nsys: warn — dirty git tree may block scoped edits (commit or stash first)",
        );
    }

    let result = drive(
        &mut tui,
        &mut view,
        &mut pump,
        dir,
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
/// When no task is active the detail reads `"ungated (human approval only)  /help"`
/// to make explicit that only the human gate (via approval prompt) is active.
/// A non-empty `task` string carries its own mode annotation.
/// When aden active, appends a compact vim ADEN hint for discoverability.
fn boot_status(model_label: &str, task: &str, aden_active: bool) -> String {
    let mut detail = if task.is_empty() {
        "ungated (human approval only)  /help".to_string()
    } else {
        format!("{task}  /help")
    };
    if aden_active {
        detail.push_str("  K/gd ?");
    }
    format!("model: {model_label}  |  scope: {detail}")
}

/// The status line: the active model, then aden's savings estimate when there
/// is one (else the task + `/help` hint), then the context meter once a turn has
/// reported token usage.
fn status_line(
    dir: &Path,
    model_label: &str,
    task: &str,
    usage: Option<Usage>,
    aden_active: bool,
    trust: &Trust,
) -> String {
    let base = match aden::savings(dir) {
        Some(savings) => format!("model: {model_label}  |  scope: {savings}"),
        None => boot_status(model_label, task, aden_active),
    };
    let line = match usage {
        Some(u) if u.prompt_tokens > 0 => format!("{base}  |  ctx: {}", ctx_meter(u.prompt_tokens)),
        _ => base,
    };
    let task_gated = !task.is_empty() && task.contains("gated");
    format!("{line}  |  {}", trust.ladder_tag(task_gated))
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

/// Append ADEN result using "aden: " prefix so the transcript renderer gives it
/// a dedicated ⊙ sigil and distinct color. Makes the graph actions visually
/// stand out for better scannability and user feedback.
fn append_aden(view: &mut View, label: &str, content: &str) {
    view.snap_to_bottom();
    view.output.push('\n');
    let c = content.trim();
    if c.is_empty() {
        view.output.push_str(&format!("aden: {}", label));
    } else {
        view.output.push_str(&format!("aden: {}\n{}", label, c));
    }
}

/// Pick the model backend at runtime. Resolution order: explicit legacy
/// `COXN_MODEL_*`, then named `[provider.*]` + `[route]`, then legacy aden
/// `model.*`, then local auto-detect, then the offline stub.
fn resolve_model(dir: &Path, caps: &aden::AdenCaps) -> (AnyModel, ModelSel) {
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
        return openai_model("env".to_string(), base_url, model, env_key(), "env");
    }
    // 2. Provider instances and route.active in .aden/config.toml.
    let provider_cfg = provider::load_config(dir);
    if let Some(selection) = provider_cfg.route("active")
        && let Some(resolved) = resolve_instance_from_config(&provider_cfg, selection, "config")
    {
        return resolved;
    }
    // 3. aden config (.aden/config.toml) read from pre-probed caps.
    if let Some(base_url) = caps.model_base_url.clone() {
        let model = caps
            .model_name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        return openai_model("config".to_string(), base_url, model, env_key(), "config");
    }
    // 4. Local auto-detection.
    if let Some((base_url, model)) = openai::detect() {
        return openai_model("local".to_string(), base_url, model, None, "auto");
    }
    // 5. Offline stub.
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
fn model_listing(dir: &Path, sel: &ModelSel) -> String {
    let Some(e) = &sel.endpoint else {
        return format!("model: {}", sel.label());
    };
    if e.base_url.starts_with(CODEX_ENDPOINT_SCHEME) {
        let Some(binary) = codex_model::codex_binary_from_endpoint(&e.base_url) else {
            return format!("model: {}", sel.label());
        };
        let cfg = provider::load_config(dir);
        let instance = cfg.instance(&e.instance_id);
        let codex_home = instance.and_then(|i| i.shadow_home.as_deref().or(i.home_path.as_deref()));
        let env = instance.map(|i| i.env.as_slice()).unwrap_or(&[]);
        return match codex_model::list_models(binary, codex_home, env) {
            Some(models) if !models.is_empty() => {
                let mut out = format!("models on {binary} (/model <name|#> to switch):\n");
                for (i, m) in models.iter().enumerate() {
                    let mark = if *m == sel.name { '*' } else { ' ' };
                    out.push_str(&format!("  {mark} {:>2}. {m}\n", i + 1));
                }
                out.push_str("(* = active; codex CLI piggyback — text-only turns)");
                out
            }
            _ => format!(
                "model: {}  (could not list models from {binary})",
                sel.label()
            ),
        };
    }
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
fn switch_model(dir: &Path, pump: &mut Pump<AnyModel>, sel: &mut ModelSel, target: &str) -> String {
    if let Some((instance_id, model_name)) = target.split_once('/') {
        let cfg = provider::load_config(dir);
        let selection = provider::ModelSelection {
            instance_id: instance_id.trim().to_string(),
            model: model_name.trim().to_string(),
        };
        if selection.instance_id.is_empty() || selection.model.is_empty() {
            return "usage: /model <name|#|instance/name>".to_string();
        }
        let Some((model, new_sel)) = resolve_instance_from_config(&cfg, selection, "manual") else {
            return format!("provider instance '{instance_id}' is unavailable");
        };
        pump.set_model(model);
        *sel = new_sel;
        return format!("switched to {}", sel.label());
    }
    let Some(e) = &sel.endpoint else {
        return "no provider to switch on (offline stub)".to_string();
    };
    if e.base_url.starts_with(CODEX_ENDPOINT_SCHEME) {
        let cfg = provider::load_config(dir);
        let Some(binary) = codex_model::codex_binary_from_endpoint(&e.base_url) else {
            return "invalid codex endpoint".to_string();
        };
        let instance = cfg.instance(&e.instance_id);
        let codex_home = instance.and_then(|i| i.shadow_home.as_deref().or(i.home_path.as_deref()));
        let env = instance.map(|i| i.env.as_slice()).unwrap_or(&[]);
        let listed = codex_model::list_models(binary, codex_home, env).unwrap_or_default();
        let chosen = match target.parse::<usize>() {
            Ok(n) => match listed.get(n.wrapping_sub(1)) {
                Some(m) => m.clone(),
                None => return format!("no model #{n} (there are {})", listed.len()),
            },
            Err(_) => target.to_string(),
        };
        let Some(model) = rebuild_codex_model(dir, sel, chosen.clone()) else {
            return "failed to rebuild codex model".to_string();
        };
        pump.set_model(model);
        sel.name = chosen;
        return format!("switched to {}", sel.label());
    }
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
    format!("switched to {}", sel.label())
}
/// Build the `/model` picker (every advertised model, hot ones marked, the
/// active one starred). `None` for the offline stub or an unreachable endpoint.
fn model_menu(dir: &Path, sel: &ModelSel) -> Option<Menu> {
    let e = sel.endpoint.as_ref()?;
    let models = if e.base_url.starts_with(CODEX_ENDPOINT_SCHEME) {
        let binary = codex_model::codex_binary_from_endpoint(&e.base_url)?;
        let cfg = provider::load_config(dir);
        let instance = cfg.instance(&e.instance_id);
        let codex_home = instance.and_then(|i| i.shadow_home.as_deref().or(i.home_path.as_deref()));
        let env = instance.map(|i| i.env.as_slice()).unwrap_or(&[]);
        codex_model::list_models(binary, codex_home, env)?
    } else {
        openai::list_models(&e.base_url, e.key.as_deref())?
    };
    if models.is_empty() {
        return None;
    }
    let loaded = if e.base_url.starts_with(CODEX_ENDPOINT_SCHEME) {
        Vec::new()
    } else {
        openai::loaded_models(&e.base_url, e.key.as_deref()).unwrap_or_default()
    };
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
        scroll: 0,
        count: None,
        pending_g: false,
        filter: String::new(),
        catalog: Vec::new(),
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
        scroll: 0,
        count: None,
        pending_g: false,
        filter: String::new(),
        catalog: Vec::new(),
    })
}

/// Fuzzy unified palette (M4): slash verbs, models, sessions, recent commands.
fn palette_menu(dir: &Path, sel: &ModelSel, history: &[String]) -> Menu {
    let mut catalog: Vec<MenuItem> = Vec::new();

    for cmd in crate::commands::COMMANDS {
        catalog.push(MenuItem {
            label: format!("/{cmd}"),
            value: format!("input:/{cmd} "),
        });
    }

    if let Some(model_picker) = model_menu(dir, sel) {
        for item in model_picker.items {
            catalog.push(MenuItem {
                label: format!("model  {}", item.label),
                value: format!("model:{}", item.value),
            });
        }
    }

    for s in session::list() {
        catalog.push(MenuItem {
            label: format!(
                "session  {:>4}  {}  {}",
                session::relative_age(s.age_secs),
                s.slug,
                s.preview
            ),
            value: format!("session:{}", s.slug),
        });
    }

    let mut seen = std::collections::HashSet::new();
    for line in history.iter().rev() {
        let t = line.trim();
        if t.is_empty() || !seen.insert(t.to_string()) {
            continue;
        }
        let preview: String = t.chars().take(48).collect();
        let ell = if t.chars().count() > 48 { "…" } else { "" };
        catalog.push(MenuItem {
            label: format!("recent  {preview}{ell}"),
            value: format!("input:{t}"),
        });
        if seen.len() >= 8 {
            break;
        }
    }

    Menu {
        kind: MenuKind::Palette,
        title: "palette  Ctrl-Space".to_string(),
        items: catalog.clone(),
        catalog,
        selected: 0,
        scroll: 0,
        count: None,
        pending_g: false,
        filter: String::new(),
    }
}

/// Simple command palette for discoverability (average users). Tab on empty input or / opens this.
/// Selecting sets the input line (user presses Enter to run).
fn commands_menu() -> Option<Menu> {
    let mut items: Vec<MenuItem> = vec![];

    // ADEN cockpit items first for quick access to graph power.
    items.push(MenuItem {
        label: "ADEN: communities".to_string(),
        value: "ADEN_COMMUNITIES".to_string(),
    });
    items.push(MenuItem {
        label: "ADEN: doctor".to_string(),
        value: "ADEN_DOCTOR".to_string(),
    });
    items.push(MenuItem {
        label: "ADEN: audit".to_string(),
        value: "ADEN_AUDIT".to_string(),
    });

    // Perfect ADEN harness: pull live graph symbols into the palette.
    // Selecting executes directly (e.g. understand, view, impact) for instant
    // graph steering. This turns aden symbols into first-class cockpit controls.
    if let Ok(syms) = aden::list_symbols(std::path::Path::new("."), Some("fn-*|mod-*")) {
        for sym in syms.lines().take(3).filter(|l| !l.trim().is_empty()) {
            let s = sym.trim().to_string();
            // Short label for UI, full for value
            let short = s.split('/').next_back().unwrap_or(&s).to_string();
            items.push(MenuItem {
                label: format!("ADEN: {} understand", short),
                value: format!("ADEN_UNDERSTAND:{}", s),
            });
            items.push(MenuItem {
                label: format!("ADEN: {} view", short),
                value: format!("ADEN_VIEW:{}", s),
            });
            items.push(MenuItem {
                label: format!("ADEN: {} impact", short),
                value: format!("ADEN_IMPACT:{}", s),
            });
        }
    }

    // Regular commands and tips.
    items.push(MenuItem {
        label: "/help - this help".to_string(),
        value: "/help ".to_string(),
    });
    items.push(MenuItem {
        label: "/model - list/switch models".to_string(),
        value: "/model ".to_string(),
    });
    items.push(MenuItem {
        label: "/tools - list active (aden) tools".to_string(),
        value: "/tools ".to_string(),
    });
    items.push(MenuItem {
        label: "/agents - task partition (if scoped)".to_string(),
        value: "/agents ".to_string(),
    });
    items.push(MenuItem {
        label: "/session - list saved".to_string(),
        value: "/session ".to_string(),
    });
    items.push(MenuItem {
        label: "/runs - list execution ledgers".to_string(),
        value: "/runs ".to_string(),
    });
    items.push(MenuItem {
        label: "/clear - new chat".to_string(),
        value: "/clear ".to_string(),
    });
    items.push(MenuItem {
        label: ":view [sym] - launch aden graph view".to_string(),
        value: ":view ".to_string(),
    });
    items.push(MenuItem {
        label: ":gm [sym] - insert mermaid from aden".to_string(),
        value: ":gm ".to_string(),
    });
    items.push(MenuItem {
        label: ":doctor - env health".to_string(),
        value: ":doctor ".to_string(),
    });
    items.push(MenuItem {
        label: "TIP: Ctrl+L on word pulls context (works in Insert!)".to_string(),
        value: "".to_string(),
    });
    items.push(MenuItem {
        label: "TIP: Ctrl-Space palette, @ files, mouse wheel scroll".to_string(),
        value: "".to_string(),
    });

    Some(Menu {
        kind: MenuKind::Commands,
        title: "commands (Tab or type /) — for everyone".to_string(),
        items,
        selected: 0,
        scroll: 0,
        count: None,
        pending_g: false,
        filter: String::new(),
        catalog: Vec::new(),
    })
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
        let model = resolve_role(dir, caps, &s.role)
            .map(|s| format!("{}:{}", s.instance_id, s.model))
            .unwrap_or_else(|| "(default model)".to_string());
        let after = if s.depends_on.is_empty() {
            String::new()
        } else {
            format!("  after {}", s.depends_on.join(", "))
        };
        let policy = execute::ToolPolicy::for_role(&s.role);
        out.push_str(&format!(
            "  {} [{}; tools: {}] -> {model}{after}\n",
            s.id,
            s.role,
            policy.label()
        ));
    }
    out.push_str(
        "(partition ready; BatchIo + per-Pump execution substrate added; runner wiring next)",
    );
    out
}

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
        // Default agent preamble so local models call tools; COXN_BARE=1 opts out.
        context: if bare_mode() {
            None
        } else {
            Some(AGENT_PREAMBLE_BASE.to_string())
        },
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
        let b = if budget == 8192 {
            String::new()
        } else {
            format!(", budget {}", budget)
        };
        format!("task '{name}' ({} seed(s){}, gated)", seeds.len(), b)
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

const AT_PATH_MAX_FILES: usize = 3;
const AT_PATH_MAX_CHARS: usize = 8000;

/// Expand `@path` tokens into inline file blocks before sending to the model.
fn expand_at_paths(dir: &Path, input: &str) -> String {
    let mut out = Vec::new();
    let mut files = 0usize;
    for word in input.split_whitespace() {
        if let Some(path) = word.strip_prefix('@')
            && !path.is_empty()
            && files < AT_PATH_MAX_FILES
        {
            match tools::confine_path(dir, path) {
                Ok(full) => match std::fs::read_to_string(&full) {
                    Ok(text) => {
                        files += 1;
                        let body: String = text.chars().take(AT_PATH_MAX_CHARS).collect();
                        let trunc = if text.len() > AT_PATH_MAX_CHARS {
                            format!("\n[truncated: {path} is {} bytes]", text.len())
                        } else {
                            String::new()
                        };
                        out.push(format!("<file path=\"{path}\">\n{body}{trunc}\n</file>"));
                        continue;
                    }
                    Err(e) => {
                        out.push(format!("@{path} (read error: {e})"));
                        continue;
                    }
                },
                Err(e) => {
                    out.push(format!("@{path} ({e})"));
                    continue;
                }
            }
        }
        out.push(word.to_string());
    }
    out.join(" ")
}

/// One-line collapsed tool card for the transcript.
fn tool_call_card(call: &ToolCall) -> String {
    match call.name.as_str() {
        "read_file" | "edit" | "write_file" => {
            let path = tools::arg_preview(&call.arguments, "path");
            format!("▸ {} {}", call.name, path)
        }
        "run_command" => {
            let cmd = tools::arg_preview(&call.arguments, "command");
            let preview: String = cmd.chars().take(60).collect();
            let ell = if cmd.chars().count() > 60 { "…" } else { "" };
            format!("▸ run_command $ {preview}{ell}")
        }
        _ => {
            let preview: String = call.arguments.chars().take(40).collect();
            format!("▸ {} {preview}", call.name)
        }
    }
}

fn at_files_menu(dir: &Path) -> Option<Menu> {
    let catalog: Vec<MenuItem> = tools::project_file_picker(dir)
        .into_iter()
        .map(|p| MenuItem {
            label: p.clone(),
            value: format!("@{p}"),
        })
        .collect();
    if catalog.is_empty() {
        return None;
    }
    let mut menu = Menu {
        kind: MenuKind::AtFiles,
        title: "@ attach file — type to filter".to_string(),
        items: catalog.clone(),
        catalog,
        selected: 0,
        scroll: 0,
        count: None,
        pending_g: false,
        filter: String::new(),
    };
    menu.apply_palette_filter();
    Some(menu)
}

fn insert_at_path(input: &str, cursor: usize, token: &str) -> (String, usize) {
    let cursor = cursor.min(input.len());
    let before = &input[..cursor];
    let after = &input[cursor..];
    if let Some(at) = before.rfind('@') {
        let prefix = &input[..at];
        let new = format!("{prefix}{token}{after}");
        let new_cursor = (prefix.len() + token.len()).min(new.len());
        (new, new_cursor)
    } else {
        let new = format!("{before}{token}{after}");
        let new_cursor = (before.len() + token.len()).min(new.len());
        (new, new_cursor)
    }
}

/// Block until the user answers a gate confirm modal (`y`/`n`).
fn confirm_gate_blocking(tui: &mut Tui, view: &mut View) -> bool {
    loop {
        let _ = tui.draw(view);
        if !event::poll(TICK).unwrap_or(false) {
            continue;
        }
        let Ok(ev) = event::read() else {
            continue;
        };
        if let Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
        {
            if matches!(map_input_key(key), Some(Action::Quit)) {
                view.dismiss();
                return false;
            }
            match map_modal_key(key) {
                Some(Action::Confirm) => {
                    view.dismiss();
                    return true;
                }
                Some(Action::Cancel) => {
                    view.dismiss();
                    return false;
                }
                Some(Action::ModalExpand) if view.modal_diff.is_some() => {
                    view.modal_diff_expanded = true;
                }
                Some(Action::ModalCollapse) if view.modal_diff.is_some() => {
                    view.modal_diff_expanded = false;
                }
                _ => {}
            }
        }
    }
}

fn execute_preview_text(dir: &Path, caps: &aden::AdenCaps, resume: bool) -> String {
    let listing = agents_listing(dir, caps);
    if resume {
        if let Some(slug) =
            run_ledger::latest_for_task(&std::env::var("COXN_TASK_NAME").unwrap_or_default())
        {
            let statuses = run_ledger::scope_statuses(&slug);
            let mut lines = vec![format!("resume run: {slug}")];
            for (scope, st) in statuses {
                lines.push(format!("  {scope}: {} {}", st.status, st.result));
            }
            return lines.join("\n");
        }
        return "resume: no prior run found for this task\n\n".to_string() + &listing;
    }
    listing
}

fn undo_last_edit(dir: &Path, pump: &Pump<AnyModel>) -> String {
    let Some(path) = pump.last_edited() else {
        return "nothing to undo — no accepted file edit this session".to_string();
    };
    let rel = path
        .strip_prefix(dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    let out = std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "checkout", "--", &rel])
        .output();
    match out {
        Ok(o) if o.status.success() => format!("reverted {rel} via git checkout"),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            format!("undo failed for {rel}: {err}")
        }
        Err(e) => format!("undo failed: {e}"),
    }
}

fn export_transcript(pump: &Pump<AnyModel>) -> String {
    let text = transcript(pump.messages());
    let base = doctor::session_dir();
    let dir = base.parent().unwrap_or(&base).join("exports");
    if std::fs::create_dir_all(&dir).is_err() {
        return "export failed: cannot create exports directory".to_string();
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("coxn-{stamp}.md"));
    let body = format!("# coxn transcript\n\n{text}\n");
    match std::fs::write(&path, body) {
        Ok(()) => format!("exported to {}", path.display()),
        Err(e) => format!("export failed: {e}"),
    }
}

fn scope_listing() -> String {
    let name = std::env::var("COXN_TASK_NAME").unwrap_or_default();
    if name.trim().is_empty() {
        return "no active task scope (ungated — human approval only)\nset COXN_TASK_NAME + COXN_TASK_SEEDS for aden blast-radius gate".to_string();
    }
    let seeds = std::env::var("COXN_TASK_SEEDS").unwrap_or_default();
    let budget = std::env::var("COXN_TASK_BUDGET").unwrap_or_else(|_| "8192".into());
    format!(
        "task: {name}\nseeds: {seeds}\nbudget: {budget}\n(gate active when aden produced a manifest at boot)"
    )
}

fn copy_transcript(view: &View) -> String {
    let path = doctor::session_dir().with_file_name("last-transcript.txt");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, &view.output) {
        Ok(()) => {
            let clip = if std::env::var("COXN_CLIPBOARD")
                .ok()
                .is_some_and(|v| matches!(v.as_str(), "1" | "on" | "true" | "yes"))
            {
                " (drag-select in transcript copies via OSC52)"
            } else {
                " (set COXN_CLIPBOARD=on for OSC52 copy on selection)"
            };
            format!("transcript copied to {}{clip}", path.display())
        }
        Err(e) => format!("copy failed: {e}"),
    }
}

/// Append exit / cancel footer after a streaming `!command` run.
fn append_bang_footer(view: &mut View, outcome: &sandbox::RunOutcome, cancelled: bool) {
    if cancelled {
        view.output.push_str("err: cancelled\n");
    } else if outcome.timed_out {
        view.output.push_str("err: timed out\n");
    } else if let Some(code) = outcome.exit_code {
        if code == 0 {
            view.output.push_str("ok: exit 0\n");
        } else {
            view.output.push_str(&format!("err: exit {code}\n"));
        }
    }
}

/// Run `!command` with streaming output and Ctrl-C cancel (after y/n confirm).
async fn run_bang_shell_streaming(
    dir: &Path,
    bwrap: bool,
    command: &str,
    view: &mut View,
    tui: &mut Tui,
) {
    view.output.push('\n');
    view.output.push_str(&format!("you: !{command}\n"));
    if bwrap {
        view.output.push_str("cmd: sandboxed\n");
    } else {
        view.output.push_str("cmd: NO SANDBOX\n");
    }
    view.snap_to_bottom();
    let _ = tui.draw(view);

    let mut cancelled = false;
    let outcome = sandbox::run_streaming(dir, command, false, bwrap, &mut |line| {
        view.output.push_str(line);
        view.output.push('\n');
        view.snap_to_bottom();
        let _ = tui.draw(view);
        if poll_user_cancel() {
            cancelled = true;
            return false;
        }
        true
    })
    .await;

    append_bang_footer(view, &outcome, cancelled);
    view.snap_to_bottom();
    let _ = tui.draw(view);
}

/// An ex-style command (`:cmd`) parsed from the vim command line.
///
/// Keeps model/aden dispatch separate from the slash-command path so the two
/// can evolve independently. Every variant corresponds to a single user intent;
/// the `drive` loop calls existing functions to satisfy each one.
#[derive(Debug, PartialEq)]
enum ExCmd {
    /// `:q` / `:quit` — exit coxn.
    Quit,
    /// `:h` / `:help` — show the help text.
    Help,
    /// `:model [name]` — list (no arg) or switch model.
    Model(Option<String>),
    /// `:tools` — list active tools.
    Tools,
    /// `:clear` / `:new` — clear the conversation and start fresh.
    Clear,
    /// `:understand <sym>` — run `aden understand` and append the result.
    Understand(String),
    /// `:grep <pattern>` — run `aden grep` and append the result.
    Grep(String),
    /// `:ask <text>` — run `aden ask` and append the result.
    Ask(String),
    /// `:view [anchor]` — launch aden browser view (centered on anchor if given).
    View(Option<String>),
    /// `:viz [anchor]` or `:mermaid [anchor]` — export Mermaid diagram text.
    Viz(Option<String>),
    /// `:doctor` — run aden doctor for env + repo diagnostics.
    Doctor,
    /// `:impact <sym>` — blast radius / downstream via aden query (gi style).
    Impact(String),
    /// `:communities` — list functional communities from aden.
    Communities,
    /// `:audit` — aden security audit.
    Audit,
    /// Unknown command — append a notice.
    Unknown(String),
}

/// Parse a `:command` string (already without the leading colon) into an
/// [`ExCmd`]. Pure and unit-testable; contains no side effects.
fn parse_ex_command(input: &str) -> ExCmd {
    let trimmed = input.trim();
    let mut words = trimmed.splitn(2, char::is_whitespace);
    let verb = words.next().unwrap_or("");
    // The rest after the verb, trimmed of leading whitespace.
    let rest = words.next().map(|s| s.trim()).unwrap_or("");
    let arg = if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    };
    match verb {
        "q" | "quit" => ExCmd::Quit,
        "h" | "help" => ExCmd::Help,
        "model" => ExCmd::Model(arg),
        "tools" => ExCmd::Tools,
        "clear" | "new" => ExCmd::Clear,
        "understand" => match arg {
            Some(sym) => ExCmd::Understand(sym),
            None => ExCmd::Unknown("understand requires a symbol name".to_string()),
        },
        "grep" => match arg {
            Some(pat) => ExCmd::Grep(pat),
            None => ExCmd::Unknown("grep requires a pattern".to_string()),
        },
        "ask" => match arg {
            Some(text) => ExCmd::Ask(text),
            None => ExCmd::Unknown("ask requires a question".to_string()),
        },
        "view" => ExCmd::View(arg),
        "viz" | "mermaid" | "gm" => ExCmd::Viz(arg),
        "doctor" => ExCmd::Doctor,
        "impact" => match arg {
            Some(sym) => ExCmd::Impact(sym),
            None => ExCmd::Unknown("impact requires a symbol".to_string()),
        },
        "communities" => ExCmd::Communities,
        "audit" => ExCmd::Audit,
        other => ExCmd::Unknown(other.to_string()),
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
/// amounts. Height excludes the separator, status, and input rows. Falls back to
/// (80, 1) if the terminal size cannot be determined.
fn pane_dims(tui: &Tui, view: &View) -> (u16, u16) {
    tui.size()
        .map(|s| layout::pane_dims((s.width, s.height), view))
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
    dir: &'a Path,
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
    fn apply_tool_approval(
        &mut self,
        choice: ToolApprovalChoice,
        call: &ToolCall,
    ) -> Option<Approval> {
        match choice {
            ToolApprovalChoice::Once => {
                self.view.dismiss();
                Some(Approval::Allow)
            }
            ToolApprovalChoice::Session => {
                if let Some(cmd) = command_key(call) {
                    self.approved_commands.insert(cmd);
                } else {
                    self.approvals.insert(call.name.clone());
                }
                self.view.dismiss();
                Some(Approval::Allow)
            }
            ToolApprovalChoice::Decline => {
                self.view.dismiss();
                Some(Approval::Decline)
            }
            ToolApprovalChoice::CancelTurn => {
                self.view.dismiss();
                self.cancel_turn = true;
                Some(Approval::CancelTurn)
            }
            ToolApprovalChoice::Expand if self.view.modal_diff.is_some() => {
                self.view.modal_diff_expanded = true;
                None
            }
            ToolApprovalChoice::Collapse if self.view.modal_diff.is_some() => {
                self.view.modal_diff_expanded = false;
                None
            }
            _ => None,
        }
    }

    /// Repaint the output pane with the current assistant text and any live
    /// run_command output appended below it.
    fn repaint(&mut self) {
        // A blank line before the live coxn turn matches the inter-turn rhythm
        // of transcript() (messages joined with a blank line).
        self.view.output = if self.run_buf.is_empty() {
            format!("{}\n\ncoxn: {}", self.prior, self.buf)
        } else {
            format!("{}\n\ncoxn: {}\n{}", self.prior, self.buf, self.run_buf)
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
            "session = this exact command".to_string()
        } else {
            format!("session = all {} calls", call.name)
        };
        let diff = tools::approval_preview_diff(self.dir, call, self.bwrap).unwrap_or_default();
        self.view
            .confirm_tool_approval(format!("Approve {summary}?\n({session_label})"), diff);
        loop {
            let _ = self.tui.draw(self.view);
            if !event::poll(TICK).unwrap_or(false) {
                continue;
            }
            let Ok(ev) = event::read() else {
                continue;
            };
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if matches!(map_input_key(key), Some(Action::Quit)) {
                        self.view.dismiss();
                        self.cancel_turn = true;
                        return Approval::CancelTurn;
                    }
                    if let Some(choice) = map_tool_approval_key(key)
                        && let Some(decision) = self.apply_tool_approval(choice, call)
                    {
                        return decision;
                    }
                }
                Event::Mouse(me) => {
                    if let Some(size) = self.tui.size() {
                        let frame = Rect::new(0, 0, size.width, size.height);
                        let layout = frame_layout(frame, self.view);
                        if let MouseEffect::ToolApproval(choice) =
                            handle_mouse(self.view, &layout, me, 0)
                            && let Some(decision) = self.apply_tool_approval(choice, call)
                        {
                            return decision;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub(crate) async fn run_once(dir: &Path, args: &[String]) -> io::Result<()> {
    let auto = std::env::var("COXN_AUTO_APPROVE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    if !auto {
        eprintln!("coxn once requires COXN_AUTO_APPROVE=1 (auto-approves all tool calls)");
        std::process::exit(1);
    }
    let prompt = args
        .iter()
        .position(|a| a == "-p" || a == "--prompt")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| args.get(1).cloned())
        .filter(|s| !s.starts_with('-'));
    let Some(prompt) = prompt else {
        eprintln!("usage: coxn once -p \"your prompt\"");
        std::process::exit(2);
    };
    let caps = aden::probe(dir);
    let bwrap = sandbox::bwrap_available();
    let (model, sel) = resolve_model(dir, &caps);
    if sel.is_offline_stub() {
        eprintln!("no model reachable");
        std::process::exit(1);
    }
    let task = load_task(dir, &caps);
    let mut pump = Pump::new(model, build_registry(dir, &caps, bwrap));
    if let Some(gate) = task.gate {
        pump.set_gate(gate);
    }
    if let Some(context) = task.context {
        pump.set_context(context);
    }
    pump.push_user(prompt);
    let mut io = BatchIo::new();
    match pump.run_turn_streaming(&mut io).await {
        Ok(_) => {
            print!("{}", io.result());
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
/// Apply a picker choice (keyboard Enter or mouse row click).
#[allow(clippy::too_many_arguments)]
fn dispatch_menu_pick(
    view: &mut View,
    pump: &mut Pump<AnyModel>,
    dir: &Path,
    sel: &mut ModelSel,
    task: &str,
    trust: &Trust,
    kind: MenuKind,
    value: String,
    persisted: &mut usize,
    approvals: &mut HashSet<String>,
    approved_commands: &mut HashSet<String>,
    session: &mut session::Session,
) {
    match kind {
        MenuKind::Model => {
            view.output = switch_model(dir, pump, sel, &value);
            view.set_status(status_line(
                dir,
                &sel.label(),
                task,
                pump.last_usage(),
                view.aden_active,
                trust,
            ));
        }
        MenuKind::Session => {
            let messages = session::load(&value);
            if messages.is_empty() {
                view.output = format!("no session '{value}'");
            } else {
                *persisted = messages.len();
                pump.load_conversation(messages);
                approvals.clear();
                approved_commands.clear();
                *session = session::Session::open(&value);
                view.output = transcript(pump.messages());
                view.set_status(status_line(
                    dir,
                    &sel.label(),
                    task,
                    pump.last_usage(),
                    view.aden_active,
                    trust,
                ));
            }
        }
        MenuKind::Commands => {
            if value.starts_with("ADEN_UNDERSTAND:") {
                let sym = value.strip_prefix("ADEN_UNDERSTAND:").unwrap_or("");
                if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Understand(sym)) {
                        Ok(out) => {
                            append_aden(view, &format!("understand '{}'", sym), &out);
                            view.last_aden = Some(format!("understand '{}'", sym));
                        }
                        Err(e) => view
                            .output
                            .push_str(&format!("\naden: understand failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str(&format!("\naden: (not present) would understand: {}", sym));
                }
                view.snap_to_bottom();
            } else if value.starts_with("ADEN_VIEW:") {
                let sym = value.strip_prefix("ADEN_VIEW:").unwrap_or("");
                if pump.registry_mut().has_aden() {
                    match aden::launch_view(dir, Some(sym)) {
                        Ok(()) => {
                            view.output
                                .push_str(&format!("\naden: launched view for '{}'", sym));
                            view.last_aden = Some(format!("view '{}'", sym));
                        }
                        Err(e) => view.output.push_str(&format!("\naden: view failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str(&format!("\naden: (not present) would view: {}", sym));
                }
                view.snap_to_bottom();
            } else if value.starts_with("ADEN_IMPACT:") {
                let sym = value.strip_prefix("ADEN_IMPACT:").unwrap_or("");
                if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Impact(sym)) {
                        Ok(out) => {
                            append_aden(view, &format!("impact '{}'", sym), &out);
                            view.last_aden = Some(format!("impact '{}'", sym));
                        }
                        Err(e) => view.output.push_str(&format!("\naden: impact failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str(&format!("\naden: (not present) would impact: {}", sym));
                }
                view.snap_to_bottom();
            } else if value == "ADEN_COMMUNITIES" {
                if pump.registry_mut().has_aden() {
                    match aden::communities(dir) {
                        Ok(out) => {
                            append_aden(view, "communities", &out);
                            view.last_aden = Some("communities".to_string());
                        }
                        Err(e) => view
                            .output
                            .push_str(&format!("\naden: communities failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str("\naden: (not present) would show communities");
                }
                view.snap_to_bottom();
            } else if value == "ADEN_DOCTOR" {
                if pump.registry_mut().has_aden() {
                    match aden::doctor(dir) {
                        Ok(out) => {
                            append_aden(view, "doctor", &out);
                            view.last_aden = Some("doctor".to_string());
                        }
                        Err(e) => view.output.push_str(&format!("\naden: doctor failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str("\naden: (not present) would run doctor");
                }
                view.snap_to_bottom();
            } else if value == "ADEN_AUDIT" {
                if pump.registry_mut().has_aden() {
                    match aden::audit(dir) {
                        Ok(out) => {
                            append_aden(view, "audit", &out);
                            view.last_aden = Some("audit".to_string());
                        }
                        Err(e) => view.output.push_str(&format!("\naden: audit failed: {e}")),
                    }
                } else {
                    view.output
                        .push_str("\naden: (not present) would run audit");
                }
                view.snap_to_bottom();
            } else {
                view.input = value;
                view.cursor_end();
                view.refresh_suggestion();
            }
        }
        MenuKind::Palette => {
            if let Some(name) = value.strip_prefix("model:") {
                view.output = switch_model(dir, pump, sel, name);
                view.set_status(status_line(
                    dir,
                    &sel.label(),
                    task,
                    pump.last_usage(),
                    view.aden_active,
                    trust,
                ));
            } else if let Some(slug) = value.strip_prefix("session:") {
                let messages = session::load(slug);
                if messages.is_empty() {
                    view.output = format!("no session '{slug}'");
                } else {
                    *persisted = messages.len();
                    pump.load_conversation(messages);
                    approvals.clear();
                    approved_commands.clear();
                    *session = session::Session::open(slug);
                    view.output = transcript(pump.messages());
                    view.set_status(status_line(
                        dir,
                        &sel.label(),
                        task,
                        pump.last_usage(),
                        view.aden_active,
                        trust,
                    ));
                }
            } else if let Some(text) = value.strip_prefix("input:") {
                view.input = text.to_string();
                view.cursor_end();
                view.refresh_suggestion();
            }
        }
        MenuKind::AtFiles => {
            let (input, cursor) = insert_at_path(&view.input, view.cursor, &value);
            view.input = input;
            view.cursor = cursor;
            view.refresh_suggestion();
        }
    }
}

/// The event loop: draw, read a key, route it by mode (modal vs input), and run
/// a turn on submit. Carries no intelligence; it only paces and shuttles.
#[allow(clippy::too_many_arguments, clippy::collapsible_if)]
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
    let mut trust = Trust::default();
    trust.seed_approvals(&mut approvals);
    // One-time tip for average users the first time they trigger an ADEN action.
    let mut aden_tip_shown = false;
    loop {
        // Keep the aden badge on the status line in sync with caps each frame.
        view.aden_active = caps.available;
        if caps.available && !aden_tip_shown {
            view.output.push('\n');
            view.output.push_str("sys: tip — aden active for smart context. Ctrl+L (or K in Normal) on symbol words. Tab for commands. ? help.");
            aden_tip_shown = true;
        }
        view.refresh_suggestion();
        view.refresh_mode_tip();

        // Keep the picker viewport pinned to the selection even on a resize
        // (handlers also re-pin on each key; this covers the no-key frame).
        if view.menu.is_some() {
            let (_, term_h) = pane_dims(tui, view);
            let count = view.menu.as_ref().map(|m| m.items.len()).unwrap_or(0);
            view.menu_refit(menu_max_rows(term_h, count));
        }

        tui.draw(view)?;

        if !event::poll(TICK)? {
            continue;
        }
        let ev = event::read()?;
        view.touch_mode_tip();
        // M5: mouse scroll, picker clicks, modal hints, input cursor, transcript
        // drag-select + OSC52 copy (gated on COXN_CLIPBOARD).
        if let Event::Mouse(me) = ev {
            if !view.show_help {
                let (w, h) = pane_dims(tui, view);
                let max_scroll = view.max_scroll(w, h);
                let frame = tui
                    .size()
                    .map(|s| Rect::new(0, 0, s.width, s.height))
                    .unwrap_or(Rect::new(0, 0, 80, 24));
                let layout = frame_layout(frame, view);
                let effect = handle_mouse(view, &layout, me, max_scroll);
                let mut copy_text = None;
                match effect {
                    MouseEffect::ScrollUp => view.scroll_up(SCROLL_STEP, max_scroll),
                    MouseEffect::ScrollDown => view.scroll_down(SCROLL_STEP),
                    MouseEffect::SetCursor(_) => {}
                    MouseEffect::MenuRow(idx) => {
                        if let Some(m) = view.menu.as_mut() {
                            m.selected = idx;
                        }
                        let pick = view.menu.as_ref().and_then(|m| {
                            m.items.get(m.selected).map(|it| (m.kind, it.value.clone()))
                        });
                        view.close_menu();
                        if let Some((kind, value)) = pick {
                            dispatch_menu_pick(
                                view,
                                pump,
                                dir,
                                sel,
                                task,
                                &trust,
                                kind,
                                value,
                                &mut persisted,
                                &mut approvals,
                                &mut approved_commands,
                                &mut session,
                            );
                        }
                    }
                    MouseEffect::Modal(action) => match action {
                        Action::Confirm | Action::Cancel => view.dismiss(),
                        Action::ModalExpand if view.modal_diff.is_some() => {
                            view.modal_diff_expanded = true;
                        }
                        Action::ModalCollapse if view.modal_diff.is_some() => {
                            view.modal_diff_expanded = false;
                        }
                        _ => {}
                    },
                    MouseEffect::ToolApproval(choice) => match choice {
                        ToolApprovalChoice::Expand if view.modal_diff.is_some() => {
                            view.modal_diff_expanded = true;
                        }
                        ToolApprovalChoice::Collapse if view.modal_diff.is_some() => {
                            view.modal_diff_expanded = false;
                        }
                        _ => view.dismiss(),
                    },
                    MouseEffect::CopySelection(text) => copy_text = Some(text),
                    MouseEffect::None => {}
                }
                if let Some(text) = copy_text {
                    tui.draw(view)?;
                    osc52_copy(&text)?;
                }
            }
            continue;
        }
        // Bracketed paste (M1): the terminal wraps the paste in
        // `CSI ? 2004 h` .. `CSI ? 2004 l`; crossterm surfaces it as one
        // `Event::Paste(String)` so the whole payload lands as one bulk insert
        // at the cursor (see `View::input_push_str`). This bypasses the vim
        // engine entirely -- a paste containing the literal `Escape\ni` does
        // NOT flip into Normal mode mid-paste (the adversarial paste case).
        if let Event::Paste(s) = ev {
            // Either the modal/menu paths above already ate the key, or we are
            // in normal Insert/Normal input composition: paste is always an
            // input edit, never a confirm/select gesture.
            if view.modal.is_none() && view.menu.is_none() {
                view.input_push_str(&s);
            }
            continue;
        }
        let Event::Key(key) = ev else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Offline stub: keep the quick retry/quit keys, but do not lock out
        // normal input. Local slash commands like /auth and /model must remain
        // reachable precisely when model resolution failed.
        if sel.is_offline_stub() && view.input.is_empty() {
            let mut handled_offline_key = false;
            match (key.code, key.modifiers) {
                (KeyCode::Char('r'), KeyModifiers::NONE) => {
                    handled_offline_key = true;
                    if let Some(note) = refresh_discovery(dir, pump, sel, true) {
                        view.output.push('\n');
                        view.output.push_str(&format!("sys: {note}"));
                    } else {
                        view.output.push('\n');
                        view.output
                            .push_str("sys: still offline — start a model server and retry");
                    }
                    if !sel.is_offline_stub() {
                        view.output.push_str("\nsys: model online — ready to chat");
                        view.set_status(boot_status(&sel.label(), task, caps.available));
                    }
                }
                (KeyCode::Char('q'), KeyModifiers::NONE)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                _ => {
                    // Fall through to normal input handling.
                }
            }
            if handled_offline_key {
                continue;
            }
        }

        // Chat-first: `?` with empty input opens help (vim uses `g?` in Normal).
        if !vim::enabled()
            && view.modal.is_none()
            && view.menu.is_none()
            && view.input.is_empty()
            && key.code == KeyCode::Char('?')
            && key.modifiers.is_empty()
        {
            view.toggle_help();
            view.show_mode_tip();
            continue;
        }

        // Help overlay: Esc, q, or ? are the close keys and are consumed.
        // Any other key (including Ctrl-C) closes the overlay and falls through
        // to normal routing, so Ctrl-C always quits and other keys are not lost.
        if view.show_help {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _)
                | (KeyCode::Char('q'), KeyModifiers::NONE)
                | (KeyCode::Char('?'), _) => {
                    view.close_help();
                    continue;
                }
                _ => {
                    view.close_help();
                    // fall through — key routes normally below
                }
            }
        }

        // M4: fuzzy palette — Ctrl-Space or Ctrl-P in any mode except modals.
        if view.modal.is_none()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(' ') | KeyCode::Char('p'))
        {
            if view
                .menu
                .as_ref()
                .is_none_or(|m| m.kind != MenuKind::Palette)
            {
                view.open_palette(palette_menu(dir, sel, &view.history));
            }
            continue;
        }

        // A modal grabs input until answered.
        if view.modal.is_some() {
            match view.modal_kind {
                ModalKind::Gate => match map_modal_key(key) {
                    Some(Action::Confirm) | Some(Action::Cancel) => view.dismiss(),
                    Some(Action::ModalExpand) if view.modal_diff.is_some() => {
                        view.modal_diff_expanded = true;
                    }
                    Some(Action::ModalCollapse) if view.modal_diff.is_some() => {
                        view.modal_diff_expanded = false;
                    }
                    _ => {}
                },
                ModalKind::ToolApproval => match map_tool_approval_key(key) {
                    Some(ToolApprovalChoice::Once)
                    | Some(ToolApprovalChoice::Session)
                    | Some(ToolApprovalChoice::Decline)
                    | Some(ToolApprovalChoice::CancelTurn) => view.dismiss(),
                    Some(ToolApprovalChoice::Expand) if view.modal_diff.is_some() => {
                        view.modal_diff_expanded = true;
                    }
                    Some(ToolApprovalChoice::Collapse) if view.modal_diff.is_some() => {
                        view.modal_diff_expanded = false;
                    }
                    _ => {}
                },
            }
            continue;
        }

        // A picker grabs input until selected or cancelled.
        if view.menu.is_some() {
            let (_, term_h) = pane_dims(tui, view);
            let count = view.menu.as_ref().map(|m| m.items.len()).unwrap_or(0);
            let rows = menu_max_rows(term_h, count);
            let fuzzy = view
                .menu
                .as_ref()
                .is_some_and(|m| matches!(m.kind, MenuKind::Palette | MenuKind::AtFiles));
            let action = if fuzzy {
                view.map_palette_key(key)
            } else {
                view.map_menu_key(key)
            };
            match action {
                Some(Action::MenuStep(d)) => view.menu_step(d, rows),
                Some(Action::MenuTop) => view.menu_top(rows),
                Some(Action::MenuBottom) => view.menu_bottom(rows),
                Some(Action::MenuPageUp) => view.menu_page(-1, rows),
                Some(Action::MenuPageDown) => view.menu_page(1, rows),
                Some(Action::MenuCancel) => view.close_menu(),
                Some(Action::MenuSelect) => {
                    let pick = view
                        .menu
                        .as_ref()
                        .and_then(|m| m.items.get(m.selected).map(|it| (m.kind, it.value.clone())));
                    view.close_menu();
                    if let Some((kind, value)) = pick {
                        dispatch_menu_pick(
                            view,
                            pump,
                            dir,
                            sel,
                            task,
                            &trust,
                            kind,
                            value,
                            &mut persisted,
                            &mut approvals,
                            &mut approved_commands,
                            &mut session,
                        );
                    }
                }
                None if fuzzy => {
                    // Filter edit consumed the key; redraw.
                }
                _ => {}
            }
            continue;
        }

        // Alt-Enter / Shift-Enter insert a literal `\n` so the input box can grow
        // into a multi-line prompt (M1). These must bypass the vim engine:
        // `Vim::handle_normal`/`handle_visual` return `Outcome::Submit` on
        // plain Enter with no modifier check, so routing Alt-Enter through
        // vim would submit instead of newline. `map_input_key` already maps
        // these modifiers to `Action::Newline`; we synthesize a Pass outcome
        // so the dispatch arm below treats it like Insert-mode typing.
        let newline_enter = matches!(key.code, KeyCode::Enter)
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT));

        // Route the key through the vim modal engine first. In Insert mode
        // (the default) nearly every key returns Pass, so existing emacs
        // bindings and plain typing are completely unaffected. Only Esc
        // (mode change), Normal-mode motions/operators, and scroll bindings
        // are ever consumed before the map_input_key path sees them.
        let vim_outcome = if newline_enter || !vim::enabled() {
            Outcome::Pass
        } else {
            view.vim.handle(&mut view.input, &mut view.cursor, key)
        };

        // Resolve a vim-level Scroll before potentially falling through to the
        // map_input_key path so the arms below stay symmetric.
        if let Outcome::Scroll(dir) = vim_outcome {
            let (w, h) = pane_dims(tui, view);
            match dir {
                // j/k are single-line motions; only PageUp/Down use SCROLL_STEP.
                Scroll::LineUp => view.scroll_up(1, view.max_scroll(w, h)),
                Scroll::LineDown => view.scroll_down(1),
                Scroll::HalfPageUp => view.scroll_up(h / 2, view.max_scroll(w, h)),
                Scroll::HalfPageDown => view.scroll_down(h / 2),
                Scroll::Top => view.scroll_up(view.max_scroll(w, h), view.max_scroll(w, h)),
                Scroll::Bottom => view.scroll_down(view.max_scroll(w, h)),
            }
            continue;
        }
        // A counted scroll (e.g. `3j`): the host applies the step `n` times so
        // `3j` scrolls exactly 3 lines (not 3 * SCROLL_STEP).
        if let Outcome::ScrollN(dir, n) = vim_outcome {
            let (w, h) = pane_dims(tui, view);
            for _ in 0..n {
                match dir {
                    Scroll::LineUp => view.scroll_up(1, view.max_scroll(w, h)),
                    Scroll::LineDown => view.scroll_down(1),
                    Scroll::HalfPageUp => view.scroll_up(h / 2, view.max_scroll(w, h)),
                    Scroll::HalfPageDown => view.scroll_down(h / 2),
                    Scroll::Top => view.scroll_up(view.max_scroll(w, h), view.max_scroll(w, h)),
                    Scroll::Bottom => view.scroll_down(view.max_scroll(w, h)),
                }
            }
            continue;
        }

        if vim_outcome == Outcome::Consumed {
            // Vim mutated the buffer; just redraw — do NOT also run map_input_key.
            continue;
        }

        // `g?`: toggle help overlay and flash the compact mode tip (M6).
        if vim_outcome == Outcome::ToggleHelp {
            view.show_mode_tip();
            view.toggle_help();
            continue;
        }

        // Transcript search (M2). `/` and `?` open the search prompt; `n`/
        // `N` cycle the *active* search. While the search prompt is open
        // (`view.search_editing()`), keys go into the live query below, not
        // through the map_input_key path.
        match vim_outcome {
            Outcome::SearchForward | Outcome::SearchBackward => {
                let backward = vim_outcome == Outcome::SearchBackward;
                view.search_open(backward);
                continue;
            }
            Outcome::SearchNext if view.search.is_some() => {
                view.search_step(1);
                continue;
            }
            Outcome::SearchPrev if view.search.is_some() => {
                view.search_step(-1);
                continue;
            }
            _ => {}
        }

        // Active search-prompt edit: keys route into the live query, not the
        // map_input_key path. Esc cancels, Backspace edits, Enter commits,
        // chars push, and Ctrl-C acts as Esc (it does NOT quit while the
        // search prompt is up).
        if view.search_editing() {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    view.search_cancel();
                }
                (KeyCode::Backspace, _) => view.search_backspace(),
                (KeyCode::Enter, _) => view.search_commit(),
                (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                    view.search_push(c);
                }
                _ => {}
            }
            continue;
        }

        // A committed search persists across typing so `n`/`N` keep cycling
        // while the user drafts a new message. `Esc` clears it; any new
        // prompt opens fresh. Intercept Esc-at-search-state before the
        // composite action dispatch.
        if view.search.is_some() && key.code == KeyCode::Esc {
            view.search_cancel();
            continue;
        }

        // Vim-native aden symbol navigation: K or gd on a word at input cursor.
        if let Outcome::AdenLookup(ref sym) = vim_outcome {
            if sym.is_empty() {
                view.output.push('\n');
                view.output.push_str(
                    "aden: place cursor on a symbol word in the input (or use :understand <sym>)",
                );
                continue;
            }
            if pump.registry_mut().has_aden() {
                match aden::pull(dir, aden::Pull::Understand(sym)) {
                    Ok(out) => {
                        append_aden(view, &format!("understand '{}'", sym), &out);
                        view.last_aden = Some(format!("understand '{}'", sym));
                    }
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: understand '{}' failed: {e}", sym));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str(&format!("aden: (not present) would understand: {}", sym));
            }
            continue;
        }
        // ga : assemble context for symbol at cursor.
        if let Outcome::AdenAsm(ref sym) = vim_outcome {
            if sym.is_empty() {
                view.output.push('\n');
                view.output.push_str(
                    "aden: place cursor on a symbol word in the input (or use :asm via understand)",
                );
                continue;
            }
            if pump.registry_mut().has_aden() {
                match aden::pull(dir, aden::Pull::Asm(sym)) {
                    Ok(out) => {
                        append_aden(view, &format!("asm '{}'", sym), &out);
                        view.last_aden = Some(format!("asm '{}'", sym));
                    }
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: asm '{}' failed: {e}", sym));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str(&format!("aden: (not present) would asm: {}", sym));
            }
            continue;
        }

        // gi : impact / blast radius.
        if let Outcome::AdenImpact(ref sym) = vim_outcome {
            if sym.is_empty() {
                view.output.push('\n');
                view.output.push_str(
                    "aden: place cursor on a symbol word in the input (or use :impact <sym>)",
                );
                continue;
            }
            if pump.registry_mut().has_aden() {
                match aden::pull(dir, aden::Pull::Impact(sym)) {
                    Ok(out) => append_aden(view, &format!("impact '{}'", sym), &out),
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: impact '{}' failed: {e}", sym));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str(&format!("aden: (not present) would impact: {}", sym));
            }
            continue;
        }

        // gv : launch aden view for symbol at cursor.
        if let Outcome::AdenView(ref sym) = vim_outcome {
            if sym.is_empty() {
                view.output.push('\n');
                view.output
                    .push_str("aden: place cursor on a symbol word (or use :view [sym])");
                continue;
            }
            if pump.registry_mut().has_aden() {
                match aden::launch_view(dir, Some(sym)) {
                    Ok(()) => {
                        view.snap_to_bottom();
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: launched view for '{}'", sym));
                        view.last_aden = Some(format!("view '{}'", sym));
                    }
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: view '{}' failed: {e}", sym));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str(&format!("aden: (not present) would view: {}", sym));
            }
            continue;
        }

        // / : aden grep on word at cursor (fuzzy search style).
        if let Outcome::AdenGrep(ref pat) = vim_outcome {
            if pat.is_empty() {
                view.output.push('\n');
                view.output
                    .push_str("grep: move cursor over a word or type pattern first");
                continue;
            }
            if pump.registry_mut().has_aden() {
                match aden::pull(dir, aden::Pull::Grep(pat)) {
                    Ok(out) => {
                        append_aden(view, &format!("grep '{}'", pat), &out);
                        view.last_aden = Some(format!("grep '{}'", pat));
                    }
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: grep '{}' failed: {e}", pat));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str(&format!("(aden not present) would grep: {pat}"));
            }
            continue;
        }

        // ] : communities / graph nav.
        if let Outcome::AdenCommunities = vim_outcome {
            if pump.registry_mut().has_aden() {
                match aden::communities(dir) {
                    Ok(out) => append_aden(view, "communities", &out),
                    Err(e) => {
                        view.output.push('\n');
                        view.output
                            .push_str(&format!("aden: communities failed: {e}"));
                    }
                }
            } else {
                view.output.push('\n');
                view.output
                    .push_str("(aden not present) would show communities");
            }
            continue;
        }

        // A completed ex command from Command mode: dispatch it, then redraw.
        if let Outcome::Command(ref cmd) = vim_outcome {
            view.snap_to_bottom();
            match parse_ex_command(cmd) {
                ExCmd::Quit => return Ok(()),
                ExCmd::Help => view.toggle_help(),
                ExCmd::Model(None) => {
                    refresh_discovery(dir, pump, sel, true);
                    match model_menu(dir, sel) {
                        Some(menu) => view.open_menu(menu),
                        None => view.output = model_listing(dir, sel),
                    }
                }
                ExCmd::Model(Some(target)) => {
                    view.output = switch_model(dir, pump, sel, &target);
                    view.set_status(status_line(
                        dir,
                        &sel.label(),
                        task,
                        pump.last_usage(),
                        view.aden_active,
                        &trust,
                    ));
                }
                ExCmd::Tools => {
                    refresh_discovery(dir, pump, sel, true);
                    view.output = pump.tool_catalog();
                }
                ExCmd::Clear => {
                    pump.clear_conversation();
                    view.output = welcome(view.aden_active);
                    view.last_aden = None;
                    session = session::Session::create();
                    persisted = 0;
                    approvals.clear();
                    approved_commands.clear();
                    view.set_status(status_line(
                        dir,
                        &sel.label(),
                        task,
                        pump.last_usage(),
                        view.aden_active,
                        &trust,
                    ));
                }
                ExCmd::Understand(sym) => {
                    if pump.registry_mut().has_aden() {
                        match aden::pull(dir, aden::Pull::Understand(&sym)) {
                            Ok(out) => {
                                append_aden(view, &format!("understand '{}'", sym), &out);
                                view.last_aden = Some(format!("understand '{}'", sym));
                            }
                            Err(e) => {
                                view.output.push('\n');
                                view.output
                                    .push_str(&format!("aden: understand '{}' error: {e}", sym));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output
                            .push_str("aden not available (install aden and generate a graph)");
                    }
                }
                ExCmd::Grep(pat) => {
                    if pump.registry_mut().has_aden() {
                        match aden::pull(dir, aden::Pull::Grep(&pat)) {
                            Ok(out) => append_aden(view, &format!("grep '{}'", pat), &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output
                                    .push_str(&format!("aden: grep '{}' error: {e}", pat));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output
                            .push_str("aden not available (install aden and generate a graph)");
                    }
                }
                ExCmd::Ask(question) => {
                    if pump.registry_mut().has_aden() {
                        match aden::pull(dir, aden::Pull::Ask(&question)) {
                            Ok(out) => append_aden(view, &format!("ask '{}'", question), &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output.push_str(&format!("aden: ask error: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output
                            .push_str("aden not available (install aden and generate a graph)");
                    }
                }
                ExCmd::View(anchor) => {
                    if pump.registry_mut().has_aden() {
                        match aden::launch_view(dir, anchor.as_deref()) {
                            Ok(()) => {
                                let label = anchor.as_deref().unwrap_or("<root>");
                                view.output.push('\n');
                                view.output.push_str(&format!(
                                    "aden: launched view for {} (see browser)",
                                    label
                                ));
                            }
                            Err(e) => {
                                view.output.push('\n');
                                view.output.push_str(&format!("aden: view failed: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output.push_str("aden not available for :view");
                    }
                }
                ExCmd::Viz(anchor) => {
                    if pump.registry_mut().has_aden() {
                        match aden::diagram(dir, anchor.as_deref()) {
                            Ok(out) => {
                                view.output.push('\n');
                                view.output.push_str("aden: mermaid\n```mermaid\n");
                                view.output.push_str(out.trim());
                                view.output.push_str("\n```");
                            }
                            Err(e) => {
                                view.output.push('\n');
                                view.output.push_str(&format!("aden: viz failed: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output
                            .push_str("aden not available for :viz/:mermaid/:gm");
                    }
                }
                ExCmd::Doctor => {
                    if pump.registry_mut().has_aden() {
                        match aden::doctor(dir) {
                            Ok(out) => append_aden(view, "doctor", &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output.push_str(&format!("aden: doctor failed: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output.push_str("aden not available for :doctor");
                    }
                }
                ExCmd::Impact(sym) => {
                    if pump.registry_mut().has_aden() {
                        // Use query impact or understand (understand already includes downstream).
                        // For explicit, fall back to understand for now (rich output).
                        match aden::pull(dir, aden::Pull::Impact(&sym)) {
                            Ok(out) => append_aden(view, &format!("impact '{}'", sym), &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output
                                    .push_str(&format!("aden: impact '{}' failed: {e}", sym));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output.push_str("aden not available for :impact");
                    }
                }
                ExCmd::Communities => {
                    if pump.registry_mut().has_aden() {
                        match aden::communities(dir) {
                            Ok(out) => append_aden(view, "communities", &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output
                                    .push_str(&format!("aden: communities failed: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output.push_str("aden not available for :communities");
                    }
                }
                ExCmd::Audit => {
                    if pump.registry_mut().has_aden() {
                        match aden::audit(dir) {
                            Ok(out) => append_aden(view, "audit", &out),
                            Err(e) => {
                                view.output.push('\n');
                                view.output.push_str(&format!("aden: audit failed: {e}"));
                            }
                        }
                    } else {
                        view.output.push('\n');
                        view.output.push_str("aden not available for :audit");
                    }
                }
                ExCmd::Unknown(s) => {
                    if s.is_empty() {
                        // Bare ':' + Enter with no text: silently ignore.
                    } else {
                        view.output.push('\n');
                        view.output.push_str(&format!("not a command: {s}"));
                    }
                }
            }
            continue;
        }

        // Outcome::Submit (Normal-mode Enter) or Outcome::Pass (Insert typing).
        // Map Pass through the existing input-key table; treat Submit the same
        // as Action::Submit so both paths share one submit block.
        let action = if vim_outcome == Outcome::Submit {
            Some(Action::Submit)
        } else {
            map_input_key(key)
        };

        if !vim::enabled() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('f')
                && key.modifiers.contains(KeyModifiers::SHIFT)
            {
                view.search_open(true);
                continue;
            }
            if matches!(action, Some(Action::SearchForward)) {
                view.search_open(false);
                continue;
            }
        }

        match action {
            Some(Action::Quit) => return Ok(()),
            Some(Action::Complete) => {
                // For average users: if no specific completion (empty or bare / or ambiguous), open friendly command palette.
                // Otherwise do inline complete (with ghost text).
                let trimmed = view.input.trim();
                if trimmed.is_empty() || (trimmed == "/" && complete_input(&view.input).is_none()) {
                    if let Some(menu) = commands_menu() {
                        view.open_menu(menu);
                    }
                } else if let Some(completed) = complete_input(&view.input) {
                    view.input = completed;
                    view.cursor_end();
                } else if trimmed.starts_with('/') {
                    // still open palette for / if no exact
                    if let Some(menu) = commands_menu() {
                        view.open_menu(menu);
                    }
                }
            }
            Some(Action::Append(c)) => {
                view.input_push(c);
                if c == '@'
                    && let Some(menu) = at_files_menu(dir)
                {
                    view.open_menu(menu);
                }
            }
            Some(Action::Newline) => view.input_push('\n'),
            Some(Action::Backspace) => view.input_backspace(),
            Some(Action::CursorLeft) => view.cursor_left(),
            Some(Action::CursorRight) => {
                if view.cursor == view.input.len() {
                    if let Some(sugg) = &view.suggestion {
                        view.input.push_str(sugg);
                        view.cursor_end();
                        // suggestion will be refreshed on next draw
                        continue;
                    }
                }
                view.cursor_right();
            }
            Some(Action::CursorHome) => view.cursor_home(),
            Some(Action::CursorEnd) => view.cursor_end(),
            Some(Action::WordDelete) => view.word_delete(),
            Some(Action::KillToEnd) => view.kill_to_end(),
            Some(Action::KillToStart) => view.kill_to_start(),
            Some(Action::Yank) => view.yank(),
            Some(Action::HistoryPrev) => view.history_prev(),
            Some(Action::HistoryNext) => view.history_next(),
            Some(Action::ScrollUp) => {
                let (w, h) = pane_dims(tui, view);
                view.scroll_up(SCROLL_STEP, view.max_scroll(w, h));
            }
            Some(Action::ScrollDown) => view.scroll_down(SCROLL_STEP),
            Some(Action::PageUp) => {
                let (w, h) = pane_dims(tui, view);
                view.scroll_up(h, view.max_scroll(w, h));
            }
            Some(Action::PageDown) => {
                let (_, h) = pane_dims(tui, view);
                view.scroll_down(h);
            }
            // ADEN actions via Ctrl shortcuts (available in Insert mode too).
            // Lets "average joe" use powerful context without learning Normal vim mode.
            Some(Action::AdenUnderstand) => {
                let sym = vim::word_at_cursor(&view.input, view.cursor).unwrap_or_default();
                if sym.is_empty() {
                    view.output.push('\n');
                    view.output.push_str(
                        "aden: no word at cursor for understand (Ctrl-L or :understand sym)",
                    );
                } else if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Understand(&sym)) {
                        Ok(out) => {
                            append_aden(view, &format!("understand '{}'", sym), &out);
                            view.last_aden = Some(format!("understand '{}'", sym));
                        }
                        Err(e) => view
                            .output
                            .push_str(&format!("\naden: understand failed: {e}")),
                    }
                } else {
                    view.output.push('\n');
                    view.output
                        .push_str(&format!("aden: (not present) would understand: {}", sym));
                }
            }
            Some(Action::AdenAsm) => {
                let sym = vim::word_at_cursor(&view.input, view.cursor).unwrap_or_default();
                if sym.is_empty() {
                    view.output.push('\n');
                    view.output
                        .push_str("aden: no word at cursor for asm (Ctrl-A)");
                } else if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Asm(&sym)) {
                        Ok(out) => append_aden(view, &format!("asm '{}'", sym), &out),
                        Err(e) => view.output.push_str(&format!("\naden: asm failed: {e}")),
                    }
                } else {
                    view.output.push('\n');
                    view.output
                        .push_str(&format!("aden: (not present) would asm: {}", sym));
                }
            }
            Some(Action::AdenImpact) => {
                let sym = vim::word_at_cursor(&view.input, view.cursor).unwrap_or_default();
                if sym.is_empty() {
                    view.output.push('\n');
                    view.output
                        .push_str("aden: no word at cursor for impact (Ctrl-I or :impact sym)");
                } else if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Impact(&sym)) {
                        Ok(out) => append_aden(view, &format!("impact '{}'", sym), &out),
                        Err(e) => view.output.push_str(&format!("\naden: impact failed: {e}")),
                    }
                } else {
                    view.output.push('\n');
                    view.output
                        .push_str(&format!("aden: (not present) would impact: {}", sym));
                }
            }
            Some(Action::AdenView) => {
                let sym = vim::word_at_cursor(&view.input, view.cursor).unwrap_or_default();
                if sym.is_empty() {
                    view.output.push('\n');
                    view.output
                        .push_str("aden: no word at cursor for view (Ctrl-V or :view sym)");
                } else if pump.registry_mut().has_aden() {
                    match aden::launch_view(dir, Some(&sym)) {
                        Ok(()) => view
                            .output
                            .push_str(&format!("\naden: launched view for '{}'", sym)),
                        Err(e) => view.output.push_str(&format!("\naden: view failed: {e}")),
                    }
                } else {
                    view.output.push('\n');
                    view.output
                        .push_str(&format!("aden: (not present) would view: {}", sym));
                }
            }
            Some(Action::AdenGrep) => {
                let sym = vim::word_at_cursor(&view.input, view.cursor).unwrap_or_default();
                if sym.is_empty() {
                    view.output.push('\n');
                    view.output
                        .push_str("aden: no word at cursor for grep (Ctrl-G or :grep pat)");
                } else if pump.registry_mut().has_aden() {
                    match aden::pull(dir, aden::Pull::Grep(&sym)) {
                        Ok(out) => append_aden(view, &format!("grep '{}'", sym), &out),
                        Err(e) => view.output.push_str(&format!("\naden: grep failed: {e}")),
                    }
                } else {
                    view.output.push('\n');
                    view.output
                        .push_str(&format!("aden: (not present) would grep: {}", sym));
                }
            }
            Some(Action::Submit) => {
                let text = view.take_input();
                if text.trim().is_empty() {
                    continue;
                }
                // Snap the output pane to the bottom on every submit.
                view.snap_to_bottom();
                // `!cmd` runs locally after y/n confirm (no model turn).
                if let Some(cmd) = text.trim().strip_prefix('!').map(str::trim)
                    && !cmd.is_empty()
                {
                    let isolation = if bwrap {
                        "Isolation: bwrap sandbox (project root, no network)"
                    } else {
                        "WARNING: NO SANDBOX — host shell with cleared env"
                    };
                    view.confirm(format!(
                        "Run shell locally (human gate, not model)?\n!{cmd}\n{isolation}"
                    ));
                    if !confirm_gate_blocking(tui, view) {
                        continue;
                    }
                    view.push_history(text.clone());
                    run_bang_shell_streaming(dir, bwrap, cmd, view, tui).await;
                    continue;
                }
                // A leading slash is a local command, not a model turn.
                if text.trim_start().starts_with('/') {
                    match parse_command(text.trim()) {
                        Command::Quit => return Ok(()),
                        Command::Help => {
                            view.toggle_help();
                            view.show_mode_tip();
                        }
                        Command::Model(None) => {
                            // Re-discover first: a backend started after boot is
                            // selectable now, no reboot.
                            refresh_discovery(dir, pump, sel, true);
                            match model_menu(dir, sel) {
                                Some(menu) => view.open_menu(menu),
                                None => view.output = model_listing(dir, sel),
                            }
                        }
                        Command::Model(Some(target)) => {
                            // `@role` resolves through the [route] table; anything
                            // else is a model name or index.
                            let resolved: Result<String, String> = if let Some(role) =
                                target.strip_prefix('@')
                            {
                                resolve_role(dir, caps, role).map(|s| format!("{}/{}", s.instance_id, s.model)).ok_or_else(|| {
                                    format!(
                                        "no model mapped for role '@{role}'; set route.{role} via aden config"
                                    )
                                })
                            } else {
                                Ok(target.clone())
                            };
                            match resolved {
                                Ok(target) => {
                                    view.output = switch_model(dir, pump, sel, &target);
                                    view.set_status(status_line(
                                        dir,
                                        &sel.label(),
                                        task,
                                        pump.last_usage(),
                                        view.aden_active,
                                        &trust,
                                    ));
                                }
                                Err(msg) => view.output = msg,
                            }
                        }
                        Command::Tools => {
                            // Pick up aden tools that came online since boot.
                            refresh_discovery(dir, pump, sel, true);
                            view.output = pump.tool_catalog();
                        }
                        Command::Agents => {
                            refresh_discovery(dir, pump, sel, true);
                            // Re-probe aden so the listing reflects any tools
                            // that came online after boot (stale `caps` has
                            // available=false until we re-check).
                            let live_caps = aden::probe(dir);
                            view.output = agents_listing(dir, &live_caps);
                        }
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
                        Command::Runs(slug) => {
                            view.output = match slug {
                                Some(slug) => run_ledger::summarize(&slug)
                                    .unwrap_or_else(|e| format!("run '{slug}' unavailable: {e}")),
                                None => runs_listing(),
                            };
                        }
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
                                        trust.seed_approvals(&mut approvals);
                                        session = session::Session::open(&slug);
                                        persisted = n;
                                        let out = transcript(pump.messages());
                                        view.set_status(status_line(
                                            dir,
                                            &sel.label(),
                                            task,
                                            pump.last_usage(),
                                            view.aden_active,
                                            &trust,
                                        ));
                                        out
                                    }
                                }
                                None => "usage: /resume <slug>  (see /session)".to_string(),
                            };
                        }
                        Command::Clear => {
                            pump.clear_conversation();
                            view.output = welcome(view.aden_active);
                            // A cleared conversation starts a fresh session file
                            // and forgets session-level tool approvals.
                            session = session::Session::create();
                            persisted = 0;
                            approvals.clear();
                            approved_commands.clear();
                            trust.seed_approvals(&mut approvals);
                            view.set_status(status_line(
                                dir,
                                &sel.label(),
                                task,
                                pump.last_usage(),
                                view.aden_active,
                                &trust,
                            ));
                        }
                        Command::Scope => view.output = scope_listing(),
                        Command::Trust => {
                            let note = trust.toggle_read();
                            if matches!(trust.read, trust::TrustLevel::Session) {
                                approvals.insert("read_file".to_string());
                            } else {
                                approvals.remove("read_file");
                            }
                            view.output = note;
                            view.set_status(status_line(
                                dir,
                                &sel.label(),
                                task,
                                pump.last_usage(),
                                view.aden_active,
                                &trust,
                            ));
                        }
                        Command::Copy => view.output = copy_transcript(view),
                        Command::Undo => view.output = undo_last_edit(dir, pump),
                        Command::Export => view.output = export_transcript(pump),
                        Command::Auth(args) => {
                            if args.first().is_some_and(|arg| arg == "set-key") {
                                view.output =
                                    "run key storage from your shell: coxn auth set-key <id> < key.txt\n"
                                        .to_string();
                            } else {
                                let result = auth::report(dir, &args);
                                view.output = result.output;
                                if result.code != 0 {
                                    view.output.push_str(&format!(
                                        "status: auth exited {}\n",
                                        result.code
                                    ));
                                }
                            }
                        }
                        Command::Execute { resume } => {
                            let preview = execute_preview_text(dir, caps, resume);
                            let prompt = if resume {
                                "Resume partition?"
                            } else {
                                "Start partition?"
                            };
                            view.confirm_with_diff(prompt, preview);
                            if confirm_gate_blocking(tui, view) {
                                view.set_status(if resume {
                                    "resuming partition…".to_string()
                                } else {
                                    "executing partition…".to_string()
                                });
                                let _ = tui.draw(view);
                                let prior_output = view.output.clone();
                                let format_execute_report = |report: &str| {
                                    if prior_output.is_empty() {
                                        format!("sys: /execute\n{report}")
                                    } else {
                                        format!("{prior_output}\n\nsys: /execute\n{report}")
                                    }
                                };
                                let mut on_execute_progress = |report: &str| {
                                    view.output = format_execute_report(report);
                                    view.snap_to_bottom();
                                    let _ = tui.draw(view);
                                };
                                let mut poll_cancel = || poll_user_cancel();
                                view.output = format_execute_report(
                                    &execute::execute_partition(
                                        dir,
                                        caps,
                                        sel,
                                        bwrap,
                                        resume,
                                        execute::ExecuteProgress::new(
                                            &mut on_execute_progress,
                                            Some(&mut poll_cancel),
                                        ),
                                    )
                                    .await,
                                );
                            } else {
                                view.output = "partition cancelled".to_string();
                            }
                            view.set_status(status_line(
                                dir,
                                &sel.label(),
                                task,
                                pump.last_usage(),
                                view.aden_active,
                                &trust,
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
                // Hot-plug: if no model was up at boot, a backend started since is
                // found now (and aden tools registered) so this very turn uses it.
                if let Some(note) = refresh_discovery(dir, pump, sel, false) {
                    view.set_status(note);
                }
                if sel.is_offline_stub() {
                    view.output =
                        "no model reachable — use /auth setup, /auth status, /model, or [r] retry"
                            .to_string();
                    view.set_status(
                        "OFFLINE STUB  |  /auth setup  /model  [r] retry  [q] quit".to_string(),
                    );
                    continue;
                }
                let expanded = expand_at_paths(dir, &text);
                pump.push_user(expanded);
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
                        dir,
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
                // Capture the turn's duration before clearing the pending mark,
                // so completed turns carry their timing in the transcript.
                let turn_elapsed = view.pending_since.take().map(|s| s.elapsed());
                match result {
                    Ok(_) => {
                        view.output = transcript(pump.messages());
                        // A quiet per-turn timing footer in the transcript -- the
                        // record keeps it after the live status bar moves on.
                        if let Some(e) = turn_elapsed.filter(|_| !cancelled) {
                            view.output
                                .push_str(&format!("\nsys: done in {:.1}s", e.as_secs_f64()));
                        }
                        // Refresh the model + savings + context status after the turn.
                        let status = status_line(
                            dir,
                            &sel.label(),
                            task,
                            pump.last_usage(),
                            view.aden_active,
                            &trust,
                        );
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
                // Surface a gate block from this turn as a confirm modal. The block's
                // message often carries diff-like text; confirm_with_diff
                // routes it through paint_diff_line so +/- hunks render.
                if let Some(block) = pump.take_block() {
                    view.output.push_str(&format!(
                        "\nsys: GATE BLOCKED — {} — change reverted\n{}",
                        block.verdict.label(),
                        block.message
                    ));
                    let prompt = format!("GATE BLOCKED: {}", block.verdict.label());
                    view.confirm_with_diff(prompt, block.message.clone());
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

fn runs_listing() -> String {
    let runs = run_ledger::list();
    if runs.is_empty() {
        return "no execution runs yet".to_string();
    }
    let mut out = String::from("execution runs (/runs <slug>):\n");
    for slug in runs {
        out.push_str(&format!("  {slug}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolRegistry, register_aden_tools};

    #[test]
    fn transcript_labels_each_role() {
        let messages = vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "stub: hi"),
        ];
        assert_eq!(transcript(&messages), "you: hi\n\ncoxn: stub: hi");
    }

    #[test]
    fn transcript_renders_a_tool_call_turn() {
        use crate::model::ToolCall;
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
            "you: go\n\ncoxn:\n▸ aden_asm {}\n\ntool: result"
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
        // No task -> must explicitly surface that only human approval gates edits.
        let s = boot_status("stub-model", "", false);
        assert!(s.contains("model: stub-model"), "{s}");
        assert!(
            s.contains("scope: ungated (human approval only)"),
            "expected explicit ungated text in: {s}"
        );
        assert!(s.contains("/help"), "{s}");
    }

    #[test]
    fn boot_status_task_text_when_task_set() {
        // A non-empty task string appears in the status line.
        let s = boot_status("stub-model", "task 'foo' (1 seed(s), gated)", false);
        assert!(
            s.contains("scope: task 'foo'"),
            "expected task text in: {s}"
        );
        // Should not inject "ungated" when the task string is already set.
        assert!(!s.contains("ungated"), "{s}");
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
    fn register_aden_tools_is_idempotent_and_reports_change() {
        // The hot-plug refresh may call this every time aden might have appeared;
        // it must add the tools exactly once and report whether it did.
        let mut tools = ToolRegistry::new();
        assert!(
            register_aden_tools(&mut tools, std::path::Path::new("."), true),
            "first registration should report a change"
        );
        let count = tools.advertised_defs().len();
        assert!(
            !register_aden_tools(&mut tools, std::path::Path::new("."), true),
            "second registration is a no-op"
        );
        assert_eq!(
            tools.advertised_defs().len(),
            count,
            "no duplicate aden tools on refresh"
        );
        // An unavailable probe never registers and never reports a change.
        let mut empty = ToolRegistry::new();
        assert!(!register_aden_tools(
            &mut empty,
            std::path::Path::new("."),
            false
        ));
        assert!(!empty.has_aden());
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

    // -- parse_ex_command tests -----------------------------------------------

    #[test]
    fn ex_quit_aliases() {
        assert_eq!(parse_ex_command("q"), ExCmd::Quit);
        assert_eq!(parse_ex_command("quit"), ExCmd::Quit);
    }

    #[test]
    fn ex_help_aliases() {
        assert_eq!(parse_ex_command("h"), ExCmd::Help);
        assert_eq!(parse_ex_command("help"), ExCmd::Help);
    }

    #[test]
    fn ex_model_no_arg_and_with_arg() {
        assert_eq!(parse_ex_command("model"), ExCmd::Model(None));
        assert_eq!(
            parse_ex_command("model gpt-4"),
            ExCmd::Model(Some("gpt-4".to_string()))
        );
        assert_eq!(
            parse_ex_command("model 2"),
            ExCmd::Model(Some("2".to_string()))
        );
    }

    #[test]
    fn ex_tools() {
        assert_eq!(parse_ex_command("tools"), ExCmd::Tools);
    }

    #[test]
    fn ex_clear_and_new() {
        assert_eq!(parse_ex_command("clear"), ExCmd::Clear);
        assert_eq!(parse_ex_command("new"), ExCmd::Clear);
    }

    #[test]
    fn ex_understand_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("understand Vim"),
            ExCmd::Understand("Vim".to_string())
        );
        // No arg: yields Unknown with a hint message.
        assert!(matches!(parse_ex_command("understand"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_grep_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("grep fn drive"),
            ExCmd::Grep("fn drive".to_string())
        );
        assert!(matches!(parse_ex_command("grep"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_ask_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("ask how does the pump work"),
            ExCmd::Ask("how does the pump work".to_string())
        );
        assert!(matches!(parse_ex_command("ask"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_view_viz_doctor() {
        assert_eq!(parse_ex_command("view"), ExCmd::View(None));
        assert_eq!(
            parse_ex_command("view Foo"),
            ExCmd::View(Some("Foo".to_string()))
        );
        assert_eq!(parse_ex_command("viz"), ExCmd::Viz(None));
        assert_eq!(
            parse_ex_command("mermaid Bar"),
            ExCmd::Viz(Some("Bar".to_string()))
        );
        assert_eq!(parse_ex_command("gm"), ExCmd::Viz(None));
        assert_eq!(parse_ex_command("doctor"), ExCmd::Doctor);
        assert_eq!(
            parse_ex_command("impact Foo"),
            ExCmd::Impact("Foo".to_string())
        );
        assert!(matches!(parse_ex_command("impact"), ExCmd::Unknown(_)));
        assert_eq!(parse_ex_command("communities"), ExCmd::Communities);
        assert_eq!(parse_ex_command("audit"), ExCmd::Audit);
    }

    #[test]
    fn ex_unknown_and_empty() {
        assert!(matches!(
            parse_ex_command("bogus"),
            ExCmd::Unknown(s) if s == "bogus"
        ));
        // Empty input (bare ':' + Enter) yields Unknown("").
        assert!(matches!(
            parse_ex_command(""),
            ExCmd::Unknown(s) if s.is_empty()
        ));
    }

    #[test]
    fn ex_leading_trailing_spaces_are_trimmed() {
        assert_eq!(parse_ex_command("  quit  "), ExCmd::Quit);
        assert_eq!(
            parse_ex_command("  model   some-model  "),
            ExCmd::Model(Some("some-model".to_string()))
        );
    }
}
