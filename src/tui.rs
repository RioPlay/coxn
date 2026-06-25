//! The TUI chrome: ratatui + crossterm, minimal.
//!
//! A streaming output pane, a status line, a one-line input prompt, and a
//! confirm modal overlaid on a blocked gate. Immediate-mode render loop: append
//! to a buffer, redraw next frame. No graph rendering; the inspector stays
//! browser-native (`aden view`).
//!
//! Alt-screen tradeoff: coxn runs full-screen for layout (the status line needs
//! it), which loses native terminal scrollback. This is the one real TUI
//! tradeoff called out in DESIGN.adoc; raw-append is the alternative if
//! scrollback ever matters more than layout.
//!
//! Render logic is pure in [`View`] so it is testable headless via ratatui's
//! `TestBackend`; terminal lifecycle and event polling are the thin, untested
//! edges.

// View::push is the streaming-append API exercised by tests and used once a
// provider streams (Phase 3); allow it ahead of that consumer.
#![allow(dead_code)]

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// The view state coxn renders: the streaming output buffer and the status
/// line. Pure data; [`render`] is a function of it.
#[derive(Debug, Default)]
pub struct View {
    /// The streaming output, appended to as tokens arrive.
    pub output: String,
    /// The status line content (savings land here in Phase 2).
    pub status: String,
    /// The current input line the user is typing.
    pub input: String,
    /// A pending confirmation prompt. When set, the modal renders over the pane
    /// and the modal key mapping applies. The pump sets this (e.g. on a blocked
    /// gate) and clears it once the user answers.
    pub modal: Option<String>,
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

    /// Raise a confirmation modal with `prompt`. Block on the user's answer
    /// (proceed / block) is the pump's job; this only sets the view state.
    pub fn confirm(&mut self, prompt: impl Into<String>) {
        self.modal = Some(prompt.into());
    }

    /// Dismiss the modal once answered.
    pub fn dismiss(&mut self) {
        self.modal = None;
    }

    /// Append a typed character to the input line.
    pub fn input_push(&mut self, c: char) {
        self.input.push(c);
    }

    /// Delete the last character of the input line.
    pub fn input_backspace(&mut self) {
        self.input.pop();
    }

    /// Take the input line, leaving it empty (on submit).
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
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

/// A rectangle of `width` x `height` centered in `area`, clamped to fit.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Render one frame: an output pane above a one-row status line and a one-row
/// input prompt, with the confirm modal overlaid when active. Pure in `view`;
/// testable with `TestBackend`.
pub fn render(frame: &mut Frame, view: &View) {
    let areas = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let pane = areas[0];
    let tail = visible_tail(&view.output, pane.height as usize);
    frame.render_widget(Paragraph::new(tail), pane);
    frame.render_widget(Paragraph::new(view.status.as_str()), areas[1]);
    frame.render_widget(Paragraph::new(format!("> {}", view.input)), areas[2]);

    if let Some(prompt) = &view.modal {
        let hint = "[y] proceed   [n] block";
        let inner_width = prompt.chars().count().max(hint.len()) as u16;
        let area = centered_rect(inner_width + 4, 4, frame.area());
        let block = Block::default().borders(Borders::ALL).title("confirm");
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!("{prompt}\n{hint}")).block(block),
            area,
        );
    }
}

/// A user intent decoded from a key event. The pump decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the pump.
    Quit,
    /// Submit the current input line as a user turn.
    Submit,
    /// Append a typed character to the input line.
    Append(char),
    /// Delete the last input character.
    Backspace,
    /// Answer a confirm modal: proceed.
    Confirm,
    /// Answer a confirm modal: block.
    Cancel,
}

/// Map a key event while typing. `Ctrl-C` quits, Enter submits, Backspace
/// deletes, and any other printable character is appended. Pure and testable.
pub fn map_input_key(key: KeyEvent) -> Option<Action> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
        (KeyCode::Enter, _) => Some(Action::Submit),
        (KeyCode::Backspace, _) => Some(Action::Backspace),
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => Some(Action::Append(c)),
        _ => None,
    }
}

/// Map a key event while a confirm modal is up. `y`/Enter proceed; `n`/Esc
/// block. The pump selects this mapping when [`View::modal`] is set.
pub fn map_modal_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Action::Cancel),
        _ => None,
    }
}

/// The terminal lifecycle owner: enters the alt screen and raw mode on
/// construction and restores on drop. The render loop draws through
/// [`Tui::draw`].
pub struct Tui {
    terminal: ratatui::DefaultTerminal,
}

impl Tui {
    /// Take over the terminal (alt screen, raw mode, panic-restore hook).
    /// Fails gracefully without panicking when there is no terminal (CI,
    /// containers, pipes), which coxn is meant to run in.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            terminal: ratatui::try_init()?,
        })
    }

    /// Draw one frame of the view.
    pub fn draw(&mut self, view: &View) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, view))?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        ratatui::try_restore().ok();
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
    fn input_keys_map_to_actions() {
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map_input_key(a), Some(Action::Append('a')));
        assert_eq!(map_input_key(enter), Some(Action::Submit));
        assert_eq!(map_input_key(backspace), Some(Action::Backspace));
        assert_eq!(map_input_key(ctrl_c), Some(Action::Quit));
    }

    #[test]
    fn input_edits_and_submit_clears() {
        let mut view = View::new();
        view.input_push('h');
        view.input_push('i');
        view.input_push('x');
        view.input_backspace();
        assert_eq!(view.input, "hi");
        assert_eq!(view.take_input(), "hi");
        assert!(view.input.is_empty());
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

    #[test]
    fn modal_keys_map_to_confirm_and_cancel() {
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let other = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(map_modal_key(y), Some(Action::Confirm));
        assert_eq!(map_modal_key(enter), Some(Action::Confirm));
        assert_eq!(map_modal_key(n), Some(Action::Cancel));
        assert_eq!(map_modal_key(esc), Some(Action::Cancel));
        assert_eq!(map_modal_key(other), None);
    }

    #[test]
    fn confirm_and_dismiss_toggle_the_modal() {
        let mut view = View::new();
        assert!(view.modal.is_none());
        view.confirm("scope-escape: src/other.rs");
        assert_eq!(view.modal.as_deref(), Some("scope-escape: src/other.rs"));
        view.dismiss();
        assert!(view.modal.is_none());
    }

    #[test]
    fn render_overlays_the_modal_when_active() {
        let mut view = View::new();
        view.push("background");
        view.confirm("blocked");
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("blocked"), "modal prompt: {text:?}");
        assert!(text.contains("[y] proceed"), "modal hint: {text:?}");
    }
}
