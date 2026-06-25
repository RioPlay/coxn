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
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    tools.register(Box::new(AsmTool::new(dir.clone())));
    tools.register(Box::new(UnderstandTool::new(dir)));
    let mut pump = Pump::new(StubModel, tools);

    let mut view = View::new();
    view.set_status("coxn  (stub backend)  Ctrl-C to quit");

    let mut tui = Tui::new()?;
    let result = drive(&mut tui, &mut view, &mut pump).await;
    drop(tui); // restore the terminal before surfacing any error
    result
}

/// The event loop: draw, read a key, route it by mode (modal vs input), and run
/// a turn on submit. Carries no intelligence; it only paces and shuttles.
async fn drive(tui: &mut Tui, view: &mut View, pump: &mut Pump<StubModel>) -> io::Result<()> {
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
