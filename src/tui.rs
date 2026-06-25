//! The TUI chrome: ratatui + crossterm, minimal.
//!
//! A streaming output pane and a status line (the confirm modal lands in P1.5).
//! Immediate-mode render loop: append to a buffer, redraw next frame. No graph
//! rendering; the inspector stays browser-native (`aden view`).
//!
//! Alt-screen tradeoff: coxn runs full-screen for layout (the status line needs
//! it), which loses native terminal scrollback. This is the one real TUI
//! tradeoff called out in DESIGN.adoc; raw-append is the alternative if
//! scrollback ever matters more than layout.
//!
//! Render logic is pure in [`View`] so it is testable headless via ratatui's
//! `TestBackend`; terminal lifecycle and event polling are the thin, untested
//! edges.

// Wired into the pump in P1.6 / P1.7; defined ahead of use until then.
#![allow(dead_code)]

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;

/// The view state coxn renders: the streaming output buffer and the status
/// line. Pure data; [`render`] is a function of it.
#[derive(Debug, Default)]
pub struct View {
    /// The streaming output, appended to as tokens arrive.
    pub output: String,
    /// The status line content (savings land here in Phase 2).
    pub status: String,
}

impl View {
    /// An empty view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append streamed text to the output pane.
    pub fn push(&mut self, chunk: &str) {
        self.output.push_str(chunk);
    }

    /// Set the status line.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }
}

/// The visible tail of the output: the last `height` lines, so a streaming pane
/// shows the latest output rather than the top. Wrapping is not accounted for
/// (long lines count as one); good enough for the MVP, revisit if it matters.
fn visible_tail(output: &str, height: usize) -> String {
    if height == 0 {
        return String::new();
    }
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(height);
    lines[start..].join("\n")
}

/// Render one frame: an output pane filling the screen above a one-row status
/// line. Pure in `view`; testable with `TestBackend`.
pub fn render(frame: &mut Frame, view: &View) {
    let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(frame.area());
    let pane = areas[0];
    let tail = visible_tail(&view.output, pane.height as usize);
    frame.render_widget(Paragraph::new(tail), pane);
    frame.render_widget(Paragraph::new(view.status.as_str()), areas[1]);
}

/// A user intent decoded from a key event. The pump decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the pump.
    Quit,
}

/// Map a key event to an action, if any. Pure and testable. `q` or `Ctrl-C`
/// quit; everything else is ignored at this layer.
pub fn map_key(key: KeyEvent) -> Option<Action> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => Some(Action::Quit),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
        _ => None,
    }
}

/// Poll for a key event for up to `timeout` and map it to an action. Returns
/// `None` on timeout or an unmapped key.
pub fn poll_action(timeout: Duration) -> io::Result<Option<Action>> {
    if event::poll(timeout)?
        && let Event::Key(key) = event::read()?
    {
        return Ok(map_key(key));
    }
    Ok(None)
}

/// The terminal lifecycle owner: enters the alt screen and raw mode on
/// construction (via `ratatui::init`) and restores on drop. The render loop
/// draws through [`Tui::draw`].
pub struct Tui {
    terminal: ratatui::DefaultTerminal,
}

impl Tui {
    /// Take over the terminal (alt screen, raw mode, panic-restore hook).
    pub fn new() -> Self {
        Self {
            terminal: ratatui::init(),
        }
    }

    /// Draw one frame of the view.
    pub fn draw(&mut self, view: &View) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, view))?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn tail_is_empty_when_no_room() {
        assert_eq!(visible_tail("a\nb", 0), "");
    }

    #[test]
    fn tail_returns_whole_output_when_it_fits() {
        assert_eq!(visible_tail("a\nb", 5), "a\nb");
    }

    #[test]
    fn tail_keeps_the_last_lines_when_it_overflows() {
        assert_eq!(visible_tail("a\nb\nc\nd", 2), "c\nd");
    }

    #[test]
    fn keys_map_to_actions() {
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(map_key(q), Some(Action::Quit));
        assert_eq!(map_key(ctrl_c), Some(Action::Quit));
        assert_eq!(map_key(other), None);
    }

    /// Stringify the test buffer so we can assert what was drawn.
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_draws_output_and_status() {
        let mut view = View::new();
        view.push("hello");
        view.set_status("ready");
        let mut terminal = Terminal::new(TestBackend::new(12, 4)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("hello"), "output pane: {text:?}");
        assert!(text.contains("ready"), "status line: {text:?}");
    }
}
