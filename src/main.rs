//! coxn: a lean directional harness for aden.
//!
//! coxn is a "dumb pump": it steers and sets pace, and carries no intelligence.
//! aden directs and gates; the LLM acts; coxn steers. See DESIGN.adoc.

mod aden;
mod gate;
mod model;
mod pump;
mod tools;
mod tui;

use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use model::{Message, Role, StubModel};
use pump::Pump;
use tools::{AsmTool, EchoTool, ToolRegistry, UnderstandTool};
use tui::{Action, Tui, View, map_input_key, map_modal_key};

/// How long the event loop waits for a key before redrawing.
const TICK: Duration = Duration::from_millis(100);

/// Format the conversation into the output pane text.
fn transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let who = match m.role {
                Role::User => "you",
                Role::Assistant => "coxn",
                Role::Tool => "tool",
                Role::System => "sys",
            };
            format!("{who}: {}", m.content)
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
    // Deferred disclosure: only the discovery seam (+ echo) is advertised; the
    // aden tools are latent, found by intent via `aden_tools`. No tool bloat by
    // default.
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    tools.register_latent(Box::new(AsmTool::new(dir.clone())));
    tools.register_latent(Box::new(UnderstandTool::new(dir.clone())));
    let mut pump = Pump::new(StubModel, tools);

    // A named task (COXN_TASK_NAME) makes aden define the scope: it sets the
    // gate mandate and loads exactly the seeds' context. No task = bare prompt.
    let base_status = load_task(&dir, &mut pump);

    let mut view = View::new();
    view.set_status(status_line(&dir, &base_status));

    let mut tui = Tui::new()?;
    let result = drive(&mut tui, &mut view, &mut pump, &dir, &base_status).await;
    drop(tui); // restore the terminal before surfacing any error
    result
}

/// The status line: aden's savings estimate when available, else the base text.
fn status_line(dir: &Path, base: &str) -> String {
    aden::savings(dir).unwrap_or_else(|| base.to_string())
}

/// If `COXN_TASK_NAME` is set, let aden define the scope: run `aden scope` for
/// the task's seeds, persist the manifest for the gate, and load exactly the
/// seeds' assembled context into the pump. Returns the status-line text. No
/// task means the bare, ungated Phase 1 pump. coxn parses nothing — it gates on
/// the manifest file and loads context from `aden asm` on its own seed inputs.
fn load_task(dir: &Path, pump: &mut Pump<StubModel>) -> String {
    let Some(name) = std::env::var("COXN_TASK_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return "coxn  (stub backend)  Ctrl-C to quit".to_string();
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
        "coxn  task '{name}' ({} seed(s){})  Ctrl-C to quit",
        seeds.len(),
        if gated { ", gated" } else { "" }
    )
}

/// The event loop: draw, read a key, route it by mode (modal vs input), and run
/// a turn on submit. Carries no intelligence; it only paces and shuttles.
async fn drive(
    tui: &mut Tui,
    view: &mut View,
    pump: &mut Pump<StubModel>,
    dir: &Path,
    base_status: &str,
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
                pump.push_user(text);
                if let Err(err) = pump.run_turn().await {
                    view.set_status(format!("error: {err}"));
                }
                view.output = transcript(pump.messages());
                // Refresh aden's savings estimate after the turn.
                view.set_status(status_line(dir, base_status));
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
}
