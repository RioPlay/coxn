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
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

/// What a [`Menu`] selects, so the event loop knows how to act on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// Switch the active model to the selected id.
    Model,
    /// Resume the selected session slug.
    Session,
}

/// One selectable row: the `value` acted on, and the `label` shown.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub value: String,
    pub label: String,
}

/// An arrow-navigable picker overlaid on the pane. Up/Down move `selected`,
/// Enter acts on the selected item's `value`, Esc cancels.
#[derive(Debug, Clone)]
pub struct Menu {
    pub kind: MenuKind,
    pub title: String,
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

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
    /// Byte cursor position within `input`. Invariant: always on a char boundary.
    pub cursor: usize,
    /// A pending confirmation prompt. When set, the modal renders over the pane
    /// and the modal key mapping applies. The pump sets this (e.g. on a blocked
    /// gate) and clears it once the user answers.
    pub modal: Option<String>,
    /// Scroll position as distance-from-bottom in visual lines: `0` pins to the
    /// bottom (auto-scroll, the default); a larger value scrolls back that many
    /// lines. Render clamps it to the available scrollback, so it never needs a
    /// separate "is the user pinned" flag and always re-pins when it returns to 0.
    pub scroll_offset: u16,
    /// Lines submitted this session, oldest first.
    pub history: Vec<String>,
    /// Current position in history while browsing. `None` means the user is
    /// composing a new line (not navigating history).
    pub hist_pos: Option<usize>,
    /// Saved draft while browsing history -- restored when the user returns past
    /// the oldest entry.
    pub hist_draft: String,
    /// Set to the start time while a model turn is in progress; drives the
    /// live spinner + elapsed display. `None` when idle.
    pub pending_since: Option<Instant>,
    /// Kill ring: text cut by Ctrl-K / Ctrl-U, newest last; Ctrl-Y yanks the
    /// most recent. A simple stack (no yank-pop) -- enough to reuse cut text.
    pub kill_ring: Vec<String>,
    /// An active picker overlay (e.g. `/model`, `/session`). When set, keys
    /// navigate it instead of editing the input line.
    pub menu: Option<Menu>,
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

    /// Open a picker overlay (non-empty menus only).
    pub fn open_menu(&mut self, menu: Menu) {
        if !menu.items.is_empty() {
            self.menu = Some(menu);
        }
    }

    /// Close the picker.
    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Move the picker selection by `delta` (wraps at the ends).
    pub fn menu_move(&mut self, delta: i32) {
        if let Some(menu) = &mut self.menu {
            let len = menu.items.len() as i32;
            if len > 0 {
                menu.selected = (((menu.selected as i32 + delta) % len + len) % len) as usize;
            }
        }
    }

    /// The selected menu item, if a picker is open.
    pub fn menu_selected(&self) -> Option<&MenuItem> {
        self.menu.as_ref().and_then(|m| m.items.get(m.selected))
    }

    /// Append a typed character at the cursor position.
    pub fn input_push(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character immediately before the cursor (Backspace semantics).
    pub fn input_backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Step back to the previous char boundary.
        let mut pos = self.cursor - 1;
        while pos > 0 && !self.input.is_char_boundary(pos) {
            pos -= 1;
        }
        self.input.remove(pos);
        self.cursor = pos;
    }

    /// Move the cursor one character to the left.
    pub fn cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor - 1;
        while pos > 0 && !self.input.is_char_boundary(pos) {
            pos -= 1;
        }
        self.cursor = pos;
    }

    /// Move the cursor one character to the right.
    pub fn cursor_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        // Step forward past the current char.
        self.cursor += self.input[self.cursor..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
    }

    /// Move the cursor to the beginning of the input.
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the input.
    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Delete the word immediately before the cursor (Ctrl-W semantics).
    /// Trims trailing spaces, then deletes back to the preceding space or start.
    pub fn word_delete(&mut self) {
        // Strip trailing spaces before the cursor.
        while self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .chars()
                .next_back()
                .unwrap_or('\0');
            if prev == ' ' {
                let len = prev.len_utf8();
                self.input.drain(self.cursor - len..self.cursor);
                self.cursor -= len;
            } else {
                break;
            }
        }
        // Delete back through the word.
        while self.cursor > 0 {
            let prev = self.input[..self.cursor]
                .chars()
                .next_back()
                .unwrap_or('\0');
            if prev == ' ' {
                break;
            }
            let len = prev.len_utf8();
            self.input.drain(self.cursor - len..self.cursor);
            self.cursor -= len;
        }
    }

    /// Cut from the cursor to the end of the line onto the kill ring (Ctrl-K).
    pub fn kill_to_end(&mut self) {
        if self.cursor < self.input.len() {
            let cut = self.input.split_off(self.cursor);
            self.kill_ring.push(cut);
        }
    }

    /// Cut from the start of the line to the cursor onto the kill ring (Ctrl-U).
    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            let cut: String = self.input[..self.cursor].to_string();
            self.input.replace_range(..self.cursor, "");
            self.cursor = 0;
            self.kill_ring.push(cut);
        }
    }

    /// Yank (insert) the most recently killed text at the cursor (Ctrl-Y).
    pub fn yank(&mut self) {
        if let Some(text) = self.kill_ring.last() {
            let text = text.clone();
            self.input.insert_str(self.cursor, &text);
            self.cursor += text.len();
        }
    }

    /// Take the input line, leaving it empty (on submit). Resets cursor and
    /// history navigation.
    pub fn take_input(&mut self) -> String {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        self.hist_draft.clear();
        line
    }

    /// Push a line into history after a successful submit.
    pub fn push_history(&mut self, line: String) {
        if !line.trim().is_empty() {
            self.history.push(line);
        }
    }

    /// Recall the previous history entry (Up arrow). Saves the current draft
    /// the first time.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_pos = match self.hist_pos {
            None => {
                // First Up: save draft.
                self.hist_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0, // already at oldest
            Some(n) => n - 1,
        };
        self.hist_pos = Some(new_pos);
        self.input = self.history[new_pos].clone();
        self.cursor = self.input.len();
    }

    /// Navigate forward in history (Down arrow). Returns to the draft at the
    /// bottom.
    pub fn history_next(&mut self) {
        let Some(pos) = self.hist_pos else { return };
        if pos + 1 >= self.history.len() {
            // Past the newest: restore draft.
            self.hist_pos = None;
            self.input = std::mem::take(&mut self.hist_draft);
            self.cursor = self.input.len();
        } else {
            let new_pos = pos + 1;
            self.hist_pos = Some(new_pos);
            self.input = self.history[new_pos].clone();
            self.cursor = self.input.len();
        }
    }

    /// Snap the output pane back to the bottom (called on submit / new turn).
    pub fn snap_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Scroll the output pane up (back) by `amount` lines, clamped to `max` (the
    /// available scrollback for the current pane, from [`View::max_scroll`]).
    pub fn scroll_up(&mut self, amount: u16, max: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount).min(max);
    }

    /// Scroll the output pane down (toward the bottom) by `amount` lines. Reaching
    /// 0 re-pins to the bottom (auto-scroll resumes).
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// The maximum scrollback (distance-from-bottom in visual lines) for an
    /// output pane `width` columns wide and `pane_height` rows tall: the number
    /// of wrapped lines that do not fit. Lets the caller clamp [`View::scroll_up`].
    pub fn max_scroll(&self, width: u16, pane_height: u16) -> u16 {
        let total = wrapped_line_count(&self.output, width as usize);
        total.saturating_sub(pane_height as usize) as u16
    }
}

// -- Wrapped-line counting (no unstable feature needed) -------------------

/// Count the number of visual lines the `text` occupies when rendered into a
/// pane of `width` columns with word-wrap enabled (same semantics as
/// `Paragraph::wrap(Wrap { trim: false })`).
///
/// This mirrors the Paragraph layout pass so that the render function can pin
/// the scroll offset to the bottom without enabling the `unstable-rendered-line-info`
/// ratatui feature.
fn wrapped_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let mut total = 0usize;
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            total += 1;
            continue;
        }
        // Count display columns per char (1 for ASCII, wider for CJK via a
        // simple heuristic: chars <= U+FF use width 1; others use width 2).
        // This mirrors the ratatui grapheme-width approach closely enough for
        // the scroll-offset math.
        let mut col = 0usize;
        let mut lines_for_this = 1usize;
        for ch in raw_line.chars() {
            let cw = if (ch as u32) > 0xFF { 2 } else { 1 };
            if col + cw > width {
                lines_for_this += 1;
                col = cw;
            } else {
                col += cw;
            }
        }
        total += lines_for_this;
    }
    // Matches `Text::from(output.lines())` in `styled_output`: a trailing newline
    // does not add a visual line, so the scroll math and the rendered Text agree.
    total
}

// -- Role color helpers ---------------------------------------------------

/// Style for the prefix label of a role.
fn role_style(prefix: &str) -> Style {
    match prefix {
        "you:" => Style::default().fg(Color::LightGreen),
        "coxn:" => Style::default().fg(Color::White),
        "tool:" => Style::default().fg(Color::Yellow),
        "cmd:" => Style::default().fg(Color::LightBlue),
        "ok:" => Style::default().fg(Color::Green),
        "err:" => Style::default().fg(Color::LightRed),
        "sys:" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::LightRed), // error / unknown
    }
}

/// Known role prefixes, longest-match order (longest first to avoid prefix collisions).
const ROLE_PREFIXES: &[&str] = &["coxn:", "tool:", "you:", "sys:", "cmd:", "ok:", "err:"];

/// Convert a plain-text transcript (with `you:` / `coxn:` / `tool:` / `sys:`
/// prefixes) into a ratatui [`Text`] with per-role colors. Lines that do not
/// start with a known prefix get the error/unknown style.
fn styled_output(output: &str) -> Text<'static> {
    let lines: Vec<Line<'static>> = output
        .lines()
        .map(|raw| {
            if let Some(prefix) = ROLE_PREFIXES.iter().find(|&&p| raw.starts_with(p)) {
                let rest = &raw[prefix.len()..];
                Line::from(vec![
                    Span::styled(prefix.to_string(), role_style(prefix)),
                    Span::raw(rest.to_string()),
                ])
            } else {
                // Not a role line (continuation from wrap, error message, etc.)
                Line::from(vec![Span::raw(raw.to_string())])
            }
        })
        .collect();
    Text::from(lines)
}

// -- Centered-rect helper -------------------------------------------------

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

// -- Render ---------------------------------------------------------------

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

    // -- Output pane: wrapped, role-colored, scrollable ---
    let output_text = styled_output(&view.output);
    let total_lines = wrapped_line_count(&view.output, pane.width as usize);
    let pane_height = pane.height as usize;

    // scroll_offset is distance-from-bottom: 0 pins to the bottom (show the last
    // `pane_height` lines); a larger value backs up, clamped to the scrollback.
    let max_scrollback = total_lines.saturating_sub(pane_height) as u16;
    let from_bottom = view.scroll_offset.min(max_scrollback);
    let scroll_row = max_scrollback - from_bottom;

    let output_widget = Paragraph::new(output_text)
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0));

    // Activity indicator: live spinner + elapsed seconds when a turn is in
    // progress. The TICK (100ms) event loop redraws fast enough to animate it.
    const SPIN: &[&str] = &["-", "\\", "|", "/"];
    let status_text = if let Some(since) = view.pending_since {
        let e = since.elapsed();
        let frame = SPIN[(e.as_millis() / 250) as usize % SPIN.len()];
        format!(
            "{}  {} {}s  (Ctrl-C cancel)",
            view.status,
            frame,
            e.as_secs()
        )
    } else {
        view.status.clone()
    };

    // Input prompt with the cursor drawn as a reverse-video cell over the
    // character it sits on (or a trailing space at end-of-line), so it never
    // collides with literal text the user typed.
    let before = &view.input[..view.cursor];
    let (cursor_cell, after) = match view.input[view.cursor..].chars().next() {
        Some(c) => (c.to_string(), &view.input[view.cursor + c.len_utf8()..]),
        None => (" ".to_string(), ""),
    };
    let prompt_line = Line::from(vec![
        Span::raw("> "),
        Span::raw(before.to_string()),
        Span::styled(
            cursor_cell,
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(after.to_string()),
    ]);

    frame.render_widget(output_widget, pane);
    frame.render_widget(Paragraph::new(status_text.as_str()), areas[1]);
    frame.render_widget(Paragraph::new(prompt_line), areas[2]);

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
    } else if let Some(menu) = &view.menu {
        // The picker overlay: one row per item, the selected one reverse-video.
        let hint = "Up/Down select - Enter choose - Esc cancel";
        let width = menu
            .items
            .iter()
            .map(|i| i.label.chars().count())
            .chain([menu.title.chars().count(), hint.len()])
            .max()
            .unwrap_or(0) as u16;
        let lines: Vec<Line<'static>> = menu
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let style = if i == menu.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(format!(" {} ", item.label), style))
            })
            .chain([Line::from(Span::styled(
                format!(" {hint} "),
                Style::default().fg(Color::DarkGray),
            ))])
            .collect();
        let height = lines.len() as u16 + 2;
        let area = centered_rect(width + 4, height, frame.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(menu.title.clone());
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

// -- Actions --------------------------------------------------------------

/// A user intent decoded from a key event. The pump decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the pump.
    Quit,
    /// Submit the current input line as a user turn.
    Submit,
    /// Append a typed character to the input line.
    Append(char),
    /// Delete the character before the cursor.
    Backspace,
    /// Move cursor left one character.
    CursorLeft,
    /// Move cursor right one character.
    CursorRight,
    /// Move cursor to start of input.
    CursorHome,
    /// Move cursor to end of input.
    CursorEnd,
    /// Delete the word before the cursor (Ctrl-W).
    WordDelete,
    /// Cut from the cursor to end of line onto the kill ring (Ctrl-K).
    KillToEnd,
    /// Cut from start of line to the cursor onto the kill ring (Ctrl-U).
    KillToStart,
    /// Yank the most recently killed text at the cursor (Ctrl-Y).
    Yank,
    /// Recall the previous input-history entry (Ctrl-P).
    HistoryPrev,
    /// Navigate forward in input history (Ctrl-N).
    HistoryNext,
    /// Scroll the output pane up a few lines (Up / wheel).
    ScrollUp,
    /// Scroll the output pane down a few lines (Down / wheel).
    ScrollDown,
    /// Scroll the output pane up a full page (PageUp).
    PageUp,
    /// Scroll the output pane down a full page (PageDown).
    PageDown,
    /// Complete the current input token (Tab).
    Complete,
    /// Move the picker selection up.
    MenuUp,
    /// Move the picker selection down.
    MenuDown,
    /// Act on the selected picker item (Enter).
    MenuSelect,
    /// Close the picker without acting (Esc).
    MenuCancel,
    /// Answer a confirm modal: proceed.
    Confirm,
    /// Answer a confirm modal: block.
    Cancel,
}

/// Map a key event while typing. Up/Down scroll the transcript (so a mouse
/// wheel, which many terminals translate to arrow keys in the alt screen,
/// scrolls the chat); input history is on Ctrl-P / Ctrl-N. Pure and testable.
pub fn map_input_key(key: KeyEvent) -> Option<Action> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
        (KeyCode::Enter, _) => Some(Action::Submit),
        (KeyCode::Backspace, _) => Some(Action::Backspace),
        (KeyCode::Left, _) => Some(Action::CursorLeft),
        (KeyCode::Right, _) => Some(Action::CursorRight),
        (KeyCode::Home, _) => Some(Action::CursorHome),
        (KeyCode::End, _) => Some(Action::CursorEnd),
        (KeyCode::Tab, _) => Some(Action::Complete),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => Some(Action::WordDelete),
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => Some(Action::KillToEnd),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Action::KillToStart),
        (KeyCode::Char('y'), KeyModifiers::CONTROL) => Some(Action::Yank),
        // Input history lives on Ctrl-P / Ctrl-N so the arrows can scroll.
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(Action::HistoryPrev),
        (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(Action::HistoryNext),
        (KeyCode::Up, _) => Some(Action::ScrollUp),
        (KeyCode::Down, _) => Some(Action::ScrollDown),
        (KeyCode::PageUp, _) => Some(Action::PageUp),
        (KeyCode::PageDown, _) => Some(Action::PageDown),
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

/// Map a key event while a picker is open: Up/Down (or Ctrl-P/Ctrl-N) move the
/// selection, Enter acts, Esc / Ctrl-C cancels. Selected by [`View::menu`].
pub fn map_menu_key(key: KeyEvent) -> Option<Action> {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) | (KeyCode::Char('k' | 'p'), KeyModifiers::CONTROL) => {
            Some(Action::MenuUp)
        }
        (KeyCode::Down, _) | (KeyCode::Char('j' | 'n'), KeyModifiers::CONTROL) => {
            Some(Action::MenuDown)
        }
        (KeyCode::Enter, _) => Some(Action::MenuSelect),
        (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::MenuCancel),
        _ => None,
    }
}

// -- Tui ------------------------------------------------------------------

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

    /// Leave the alt screen and raw mode, run `f` (which gets the real terminal,
    /// e.g. to launch `$EDITOR`), then re-enter. Lets coxn shell out to a
    /// full-screen program and come back cleanly.
    pub fn run_external<F: FnOnce()>(&mut self, f: F) -> io::Result<()> {
        ratatui::try_restore().ok();
        f();
        self.terminal = ratatui::try_init()?;
        Ok(())
    }

    /// The current terminal size. Used to compute PageUp/PageDown scroll amounts.
    pub fn size(&self) -> Option<ratatui::layout::Size> {
        self.terminal.size().ok()
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

    // -- wrapped_line_count tests -----------------------------------------

    #[test]
    fn wrapped_line_count_empty() {
        assert_eq!(wrapped_line_count("", 80), 0);
    }

    #[test]
    fn wrapped_line_count_single_short_line() {
        assert_eq!(wrapped_line_count("hello", 80), 1);
    }

    #[test]
    fn wrapped_line_count_wraps_long_line() {
        // "aaaaa" is 5 chars; width 3 -> ceil(5/3) = 2 lines.
        assert_eq!(wrapped_line_count("aaaaa", 3), 2);
    }

    #[test]
    fn wrapped_line_count_two_short_lines() {
        assert_eq!(wrapped_line_count("a\nb", 80), 2);
    }

    #[test]
    fn wrapped_line_count_empty_line_in_middle() {
        assert_eq!(wrapped_line_count("a\n\nb", 80), 3);
    }

    #[test]
    fn wrapped_line_count_trailing_newline() {
        // A trailing newline adds no visual line (matches Text::from(.lines())).
        assert_eq!(wrapped_line_count("a\n", 80), 1);
    }

    // -- cursor movement tests --------------------------------------------

    #[test]
    fn cursor_left_does_not_underflow() {
        let mut v = View::new();
        v.cursor_left();
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn cursor_right_does_not_overflow() {
        let mut v = View::new();
        v.input_push('a');
        v.cursor_right();
        assert_eq!(v.cursor, 1);
        v.cursor_right(); // already at end
        assert_eq!(v.cursor, 1);
    }

    #[test]
    fn cursor_home_and_end() {
        let mut v = View::new();
        v.input_push('a');
        v.input_push('b');
        v.cursor_home();
        assert_eq!(v.cursor, 0);
        v.cursor_end();
        assert_eq!(v.cursor, 2);
    }

    #[test]
    fn input_push_at_cursor_inserts_mid() {
        let mut v = View::new();
        v.input_push('a');
        v.input_push('c');
        v.cursor_left();
        v.input_push('b');
        assert_eq!(v.input, "abc");
        assert_eq!(v.cursor, 2);
    }

    #[test]
    fn kill_ring_cut_and_yank() {
        let mut v = View::new();
        for c in "hello world".chars() {
            v.input_push(c);
        }
        // Kill to start cuts "hello world" (cursor at end -> nothing after).
        v.cursor_home();
        v.kill_to_end(); // cut "hello world"
        assert_eq!(v.input, "");
        assert_eq!(v.kill_ring.last().map(String::as_str), Some("hello world"));
        // Yank it back.
        v.yank();
        assert_eq!(v.input, "hello world");
        assert_eq!(v.cursor, "hello world".len());
        // Kill from cursor (end) does nothing; kill to start cuts everything.
        v.kill_to_start();
        assert_eq!(v.input, "");
        assert_eq!(v.kill_ring.len(), 2);
    }

    #[test]
    fn kill_keys_map_to_actions() {
        let ck = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(map_input_key(ck('k')), Some(Action::KillToEnd));
        assert_eq!(map_input_key(ck('u')), Some(Action::KillToStart));
        assert_eq!(map_input_key(ck('y')), Some(Action::Yank));
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(map_input_key(tab), Some(Action::Complete));
    }

    fn menu_item(s: &str) -> MenuItem {
        MenuItem {
            value: s.to_string(),
            label: s.to_string(),
        }
    }

    #[test]
    fn menu_navigation_wraps_and_selects() {
        let mut v = View::new();
        v.open_menu(Menu {
            kind: MenuKind::Model,
            title: "m".to_string(),
            items: vec![menu_item("a"), menu_item("b"), menu_item("c")],
            selected: 0,
        });
        v.menu_move(-1); // wrap up to the last
        assert_eq!(v.menu_selected().unwrap().value, "c");
        v.menu_move(1); // wrap back to the first
        assert_eq!(v.menu_selected().unwrap().value, "a");
        v.menu_move(1);
        assert_eq!(v.menu_selected().unwrap().value, "b");
        v.close_menu();
        assert!(v.menu.is_none());
        // An empty menu does not open.
        v.open_menu(Menu {
            kind: MenuKind::Session,
            title: "s".to_string(),
            items: Vec::new(),
            selected: 0,
        });
        assert!(v.menu.is_none());
    }

    #[test]
    fn menu_keys_map_to_actions() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(map_menu_key(k(KeyCode::Up)), Some(Action::MenuUp));
        assert_eq!(map_menu_key(k(KeyCode::Down)), Some(Action::MenuDown));
        assert_eq!(map_menu_key(k(KeyCode::Enter)), Some(Action::MenuSelect));
        assert_eq!(map_menu_key(k(KeyCode::Esc)), Some(Action::MenuCancel));
    }

    #[test]
    fn word_delete_removes_preceding_word() {
        let mut v = View::new();
        for c in "hello world".chars() {
            v.input_push(c);
        }
        v.word_delete();
        assert_eq!(v.input, "hello ");
    }

    #[test]
    fn word_delete_trims_trailing_spaces_first() {
        let mut v = View::new();
        for c in "hello   ".chars() {
            v.input_push(c);
        }
        v.word_delete();
        assert_eq!(v.input, "");
    }

    // -- history tests ----------------------------------------------------

    #[test]
    fn history_prev_recalls_last_submitted() {
        let mut v = View::new();
        v.push_history("first".to_string());
        v.push_history("second".to_string());
        v.history_prev();
        assert_eq!(v.input, "second");
    }

    #[test]
    fn history_next_restores_draft() {
        let mut v = View::new();
        v.push_history("first".to_string());
        for c in "draft".chars() {
            v.input_push(c);
        }
        v.history_prev();
        assert_eq!(v.input, "first");
        v.history_next();
        assert_eq!(v.input, "draft");
        assert!(v.hist_pos.is_none());
    }

    #[test]
    fn history_prev_at_oldest_stays() {
        let mut v = View::new();
        v.push_history("only".to_string());
        v.history_prev();
        v.history_prev(); // should not go below 0
        assert_eq!(v.hist_pos, Some(0));
        assert_eq!(v.input, "only");
    }

    // -- scroll tests -----------------------------------------------------

    #[test]
    fn scroll_up_from_bottom_moves_and_clamps() {
        let mut v = View::new();
        // From the pinned bottom (0), one page up backs off by that many lines,
        // clamped to the available scrollback.
        v.scroll_up(5, 20);
        assert_eq!(v.scroll_offset, 5);
        v.scroll_up(100, 20); // clamp to max
        assert_eq!(v.scroll_offset, 20);
    }

    #[test]
    fn scroll_down_repins_to_bottom() {
        let mut v = View::new();
        v.scroll_offset = 10;
        v.scroll_down(4);
        assert_eq!(v.scroll_offset, 6);
        v.scroll_down(100); // past the bottom -> pinned (0 = auto-scroll)
        assert_eq!(v.scroll_offset, 0);
    }

    #[test]
    fn snap_to_bottom_pins() {
        let mut v = View::new();
        v.scroll_offset = 10;
        v.snap_to_bottom();
        assert_eq!(v.scroll_offset, 0);
    }

    #[test]
    fn max_scroll_counts_overflow_lines() {
        let mut v = View::new();
        v.output = "a\nb\nc\nd\ne".to_string(); // 5 lines
        assert_eq!(v.max_scroll(80, 2), 3); // 5 - 2 = 3 lines of scrollback
        assert_eq!(v.max_scroll(80, 10), 0); // all fit
    }

    // -- input keys -------------------------------------------------------

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
    fn cursor_keys_map_to_actions() {
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(map_input_key(left), Some(Action::CursorLeft));
        assert_eq!(map_input_key(right), Some(Action::CursorRight));
        assert_eq!(map_input_key(home), Some(Action::CursorHome));
        assert_eq!(map_input_key(end), Some(Action::CursorEnd));
        assert_eq!(map_input_key(ctrl_w), Some(Action::WordDelete));
        // Arrows scroll the transcript (so a wheel scrolls chat); history is Ctrl-P/N.
        assert_eq!(map_input_key(up), Some(Action::ScrollUp));
        assert_eq!(map_input_key(down), Some(Action::ScrollDown));
        assert_eq!(map_input_key(pgup), Some(Action::PageUp));
        assert_eq!(map_input_key(pgdn), Some(Action::PageDown));
        assert_eq!(map_input_key(ctrl_p), Some(Action::HistoryPrev));
        assert_eq!(map_input_key(ctrl_n), Some(Action::HistoryNext));
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
        assert_eq!(view.cursor, 0);
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
    fn render_shows_cursor_in_input() {
        let mut view = View::new();
        view.input_push('a');
        view.input_push('b');
        view.cursor_home();
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let backend = terminal.backend();
        let buffer = backend.buffer();
        // Input row is the last row: "> ab" with a reverse-video cursor on 'a'
        // (cursor at home), at column 2 (after the "> " prompt).
        let row = buffer.area.height - 1;
        let cursor_cell = &buffer[(2, row)];
        assert_eq!(cursor_cell.symbol(), "a");
        assert!(
            cursor_cell.modifier.contains(Modifier::REVERSED),
            "cursor cell should be reverse-video: {:?}",
            cursor_cell
        );
    }

    #[test]
    fn render_shows_activity_indicator_when_pending() {
        let mut view = View::new();
        view.set_status("ready");
        view.pending_since = Some(Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        // The elapsed marker "s" (e.g. "0s") must appear in the status row.
        assert!(text.contains("0s"), "activity indicator elapsed: {text:?}");
        // "Ctrl-C cancel" hint must also appear.
        assert!(text.contains("Ctrl-C"), "cancel hint: {text:?}");
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

    #[test]
    fn styled_output_labels_prefixes() {
        let text = styled_output("you: hello\ncoxn: world\ntool: ok\nsys: info\nunknown");
        assert_eq!(text.lines.len(), 5);
        // Each role line has a styled span followed by the rest.
        assert_eq!(text.lines[0].spans.len(), 2);
        assert_eq!(text.lines[0].spans[0].content, "you:");
        assert_eq!(text.lines[4].spans[0].content, "unknown");
    }
}
