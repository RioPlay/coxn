//! The TUI chrome: ratatui + crossterm, minimal.
//!
//! A streaming output pane, a status line, a one-line input prompt, and a
//! confirm modal overlaid on a blocked gate. Immediate-mode render loop: append
//! to a buffer, redraw next frame. No graph rendering; the inspector stays
//! browser-native (`aden view`).
//!
//! Visual language ("Ledger"): the transcript is set like a typeset column --
//! a continuous left rule (the pane's `Borders::LEFT`) with a per-turn role
//! sigil in the gutter, warm off-white text, and a single slate-blue accent.
//! All motion is a pure function of the elapsed-millis phase, redrawn each
//! 100ms tick (no threads): a braille spinner, a cosine "breath" on the
//! separator, a brightened live line, and a blinking scroll marker. Truecolor
//! (`Color::Rgb`) that degrades to nearest-ANSI; role identity is carried by
//! the sigil glyph, not hue alone, so it survives a 16-color terminal.
//!
//! Alt-screen tradeoff: coxn runs full-screen for layout (the status line needs
//! it), which loses native terminal scrollback. This is the one real TUI
//! tradeoff called out in DESIGN.adoc; raw-append is the alternative if
//! scrollback ever matters more than layout.
//!
//! Render logic is pure in [`View`] so it is testable headless via ratatui's
//! `TestBackend`; terminal lifecycle and event polling are the thin, untested
//! edges.

use std::io;
use std::time::Instant;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use crate::vim::{Mode, Vim};

/// Columns the output gutter consumes: the left rule (`Block` border) plus one
/// column of padding. Subtracted from the pane width when computing wrap/scroll
/// so the math matches what the [`Paragraph`] actually lays out inside the block.
pub const PANE_GUTTER: u16 = 2;

/// What a [`Menu`] selects, so the event loop knows how to act on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// Switch the active model to the selected id.
    Model,
    /// Resume the selected session slug.
    Session,
    /// Command palette / slash commands (sets the input line).
    Commands,
}

/// One selectable row: the `value` acted on, and the `label` shown.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub value: String,
    pub label: String,
}

/// An arrow-navigable picker overlaid on the pane. Up/Down move `selected`,
/// Enter acts on the selected item's `value`, Esc cancels.
///
/// Navigation is vim-native: `j`/`k`, `G`/`gg`, `Ctrl-D`/`Ctrl-U`, PageUp/PageDown,
/// and a `[count]` prefix (`5j`). `scroll` is the index of the topmost visible
/// row; render re-pins it to keep `selected` on screen. `count` and `pending_g`
/// are transient key-state (cleared when the menu closes).
#[derive(Debug, Clone)]
pub struct Menu {
    pub kind: MenuKind,
    pub title: String,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    /// Index of the first visible row in the viewport.
    pub scroll: usize,
    /// Pending count prefix consumed by the next motion (`3j` -> move 3).
    pub count: Option<u32>,
    /// A bare `g` was typed; the next key resolves `gg` (top) or cancels.
    pub pending_g: bool,
}

impl Menu {
    /// Re-pin `scroll` so `selected` stays within `[scroll, scroll+rows)`,
    /// scrolling only when the selection leaves the window.
    fn clamp_scroll(&mut self, rows: usize) {
        if self.items.is_empty() || rows == 0 {
            return;
        }
        let rows = rows.min(self.items.len());
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
    }
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
    /// Vim modal editor state. Insert is the default, so typing and the
    /// existing emacs-style keys keep working untouched when in Insert mode.
    pub vim: Vim,
    /// Whether the help overlay is currently shown. Toggled by `?` in Normal
    /// mode or `:help` / `/help`. Closed by `Esc`, `q`, or a second `?`.
    pub show_help: bool,
    /// Whether aden is active this session. Drives the status-line badge.
    /// Set by the event loop each time capabilities are (re-)probed.
    pub aden_active: bool,
    /// Last ADEN action performed (for cockpit status feel, e.g. "understand 'drive'").
    pub last_aden: Option<String>,
    /// Inline ghost-text suggestion (dim) for /commands as you type (Tab or
    /// Right to accept). Populated live for better discoverability of commands.
    pub suggestion: Option<String>,
}

impl View {
    /// An empty view.
    pub fn new() -> Self {
        Self {
            vim: Vim::new(),
            last_aden: None,
            suggestion: None,
            ..Self::default()
        }
    }

    /// Append streamed text to the output pane.
    #[allow(dead_code)]
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

    /// Toggle the help overlay on or off.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Close the help overlay.
    pub fn close_help(&mut self) {
        self.show_help = false;
    }

    /// Move the picker selection by `delta` (clamps at the ends, vim-style;
    /// no wrap) and re-pin the viewport so the selection stays visible. `rows`
    /// is the item-row capacity of the menu body, computed by the caller from
    /// the terminal height (see [`menu_max_rows`]).
    pub fn menu_step(&mut self, delta: i32, rows: usize) {
        if let Some(menu) = &mut self.menu {
            if menu.items.is_empty() {
                return;
            }
            let len = menu.items.len() as i32;
            let next = (menu.selected as i32 + delta).clamp(0, len - 1) as usize;
            menu.selected = next;
            menu.clamp_scroll(rows);
        }
    }

    /// Shift the selection by one full viewport in `dir` (+1 down, -1 up),
    /// keeping the selection inside the new window. Falls back to a single
    /// step when the viewport is tiny.
    pub fn menu_page(&mut self, dir: i32, rows: usize) {
        if let Some(menu) = &mut self.menu {
            if menu.items.is_empty() {
                return;
            }
            let step = rows.max(1) as i32 * dir.signum();
            let len = menu.items.len() as i32;
            let next = (menu.selected as i32 + step).clamp(0, len - 1) as usize;
            // Page keeps the previous page's edge row visible for context.
            menu.selected = next;
            menu.clamp_scroll(rows);
        }
    }

    /// Jump to the first row.
    pub fn menu_top(&mut self, rows: usize) {
        if let Some(menu) = &mut self.menu {
            menu.selected = 0;
            menu.scroll = 0;
            menu.clamp_scroll(rows);
        }
    }

    /// Jump to the last row.
    pub fn menu_bottom(&mut self, rows: usize) {
        if let Some(menu) = &mut self.menu {
            if !menu.items.is_empty() {
                menu.selected = menu.items.len() - 1;
                menu.clamp_scroll(rows);
            }
        }
    }

    /// Back-compat shim: a single step with a viewport large enough to show
    /// everything (no windowing). Used by tests that drive selection directly.
    #[cfg(test)]
    pub fn menu_move(&mut self, delta: i32) {
        let rows = self
            .menu
            .as_ref()
            .map(|m| m.items.len())
            .unwrap_or(0)
            .max(1);
        self.menu_step(delta, rows);
    }

    /// Re-pin the viewport to the current selection for the given row
    /// capacity. Called once per frame so a terminal resize keeps the
    /// selection on screen even when no key was pressed.
    pub fn menu_refit(&mut self, rows: usize) {
        if let Some(menu) = &mut self.menu {
            menu.clamp_scroll(rows);
        }
    }

    /// The selected menu item, if a picker is open.
    #[allow(dead_code)]
    pub fn menu_selected(&self) -> Option<&MenuItem> {
        self.menu.as_ref().and_then(|m| m.items.get(m.selected))
    }

    /// Decode a key while a picker is open. Vim-native: `j`/`k`, `G`/`gg`,
    /// `Ctrl-D`/`Ctrl-U`, PageUp/PageDown, Home/End, and a `[count]` prefix
    /// (`5j`). Arrows and Ctrl-P/N still work for users who have not adopted
    /// the vim motions. Stateful: the count prefix and a pending `g` (for
    /// `gg`) live on [`Menu`] so they are cleared when the picker closes.
    pub fn map_menu_key(&mut self, key: KeyEvent) -> Option<Action> {
        let menu = self.menu.as_mut()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-C cancels even mid-count / mid-g.
        if key.code == KeyCode::Char('c') && ctrl {
            menu.count = None;
            menu.pending_g = false;
            return Some(Action::MenuCancel);
        }

        // Resolve a pending `g`: `gg` jumps to top. Any other key cancels the
        // pending state and is then handled fresh (its count, if any, was
        // already consumed before the `g` and is gone).
        if menu.pending_g {
            menu.pending_g = false;
            menu.count = None;
            if let KeyCode::Char('g') = key.code {
                return Some(Action::MenuTop);
            }
            // Non-g cancels `gg`; fall through and handle the key normally.
        }

        // Count accumulation: `[1-9][0-9]*`. A bare `0` is End-of-line in vim
        // but in a one-D list it is a no-op digit, so treat it as End here only
        // when no count is in progress (matches the input-line `0` motion).
        if let KeyCode::Char(d) = key.code
            && d.is_ascii_digit()
            && !ctrl
        {
            let digit = d as u32 - '0' as u32;
            if digit != 0 || menu.count.is_some() {
                let acc = menu
                    .count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit);
                menu.count = Some(acc);
                return None;
            }
        }

        let n = menu.count.take().unwrap_or(1);
        let n_step = n as i32;
        match key.code {
            KeyCode::Enter => Some(Action::MenuSelect),
            KeyCode::Esc => Some(Action::MenuCancel),
            // Linear motion (count repeats).
            KeyCode::Char('j') | KeyCode::Down if !ctrl => Some(Action::MenuStep(n_step)),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => Some(Action::MenuStep(-n_step)),
            KeyCode::Char('n') if ctrl => Some(Action::MenuStep(n_step)),
            KeyCode::Char('p') if ctrl => Some(Action::MenuStep(-n_step)),
            // Jumps.
            KeyCode::Char('G') if !ctrl => Some(Action::MenuBottom),
            KeyCode::Char('g') if !ctrl => {
                // Wait for the second `g`. The count (if any) is dropped:
                // `gg` is a jump-to-top, not a counted motion.
                menu.pending_g = true;
                None
            }
            KeyCode::Home => Some(Action::MenuTop),
            KeyCode::End => Some(Action::MenuBottom),
            // Pages.
            KeyCode::Char('d') if ctrl => Some(Action::MenuPageDown),
            KeyCode::Char('u') if ctrl => Some(Action::MenuPageUp),
            KeyCode::PageDown => Some(Action::MenuPageDown),
            KeyCode::PageUp => Some(Action::MenuPageUp),
            // Unknown: drop the count, eat the key (picker owns input).
            _ => None,
        }
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
        self.suggestion = None;
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        self.hist_draft.clear();
        // Submitting clears modal state: the next line starts fresh in Insert,
        // and no stale Visual anchor can survive into a now-empty buffer.
        self.vim = Vim::new();
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

    /// Update inline suggestion (ghost text) for / slash commands based on
    /// current input. Called after any input edit so typing /hel immediately
    /// shows the dim "p " completion.
    pub fn refresh_suggestion(&mut self) {
        self.suggestion = if self.input.starts_with('/') {
            super::complete_input(&self.input).and_then(|full| {
                if full.len() > self.input.len() && full.starts_with(&self.input) {
                    Some(full[self.input.len()..].to_string())
                } else {
                    None
                }
            })
        } else {
            None
        };
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
        let content = width.saturating_sub(PANE_GUTTER);
        let total = wrapped_line_count(&self.output, content as usize);
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

// -- Ledger palette -------------------------------------------------------
//
// Truecolor (Color::Rgb). On a 16-color terminal these degrade to the nearest
// ANSI color; role distinction survives because the gutter *sigil* carries it,
// not the hue alone. No background is forced, so the theme sits on whatever
// field the user's terminal provides.

/// Warm off-white: primary body text (the first line of each turn).
const PRIMARY: Color = Color::Rgb(212, 201, 176);
/// A gentle step down: wrapped continuation and non-role lines.
const SECONDARY: Color = Color::Rgb(168, 158, 138);
/// Brighter than primary: the live line, only while a turn streams.
const SHIMMER: Color = Color::Rgb(238, 231, 212);
/// The gutter rule and separator at rest.
const RULE: Color = Color::Rgb(58, 53, 48);
/// The single accent (spinner, caret, key hints, overlay borders): slate blue.
const ACCENT: Color = Color::Rgb(91, 127, 166);
/// Receding text: status, hints, the elapsed counter, the scroll marker.
const DIM: Color = Color::Rgb(107, 100, 88);
/// Settled-success color (a `ok:` outcome): sage, reads as data not alarm.
const OK: Color = Color::Rgb(122, 158, 126);
/// Settled-failure color (an `err:` outcome): terracotta, not alarm-red.
const ERR: Color = Color::Rgb(196, 123, 90);
/// The user's voice in the gutter: warm sand.
const SAND: Color = Color::Rgb(139, 115, 85);
/// Tool/cmd voice in the gutter: dusty mauve.
const MAUVE: Color = Color::Rgb(155, 142, 160);
/// sys: voice: one step above the rule, very quiet.
const QUIET: Color = Color::Rgb(90, 82, 72);

/// Known role prefixes, longest-match order (longest first to avoid prefix collisions).
const ROLE_PREFIXES: &[&str] = &[
    "aden:", "coxn:", "tool:", "you:", "sys:", "cmd:", "ok:", "err:",
];

/// The single-cell gutter sigil that stands in for a role's text prefix.
fn role_sigil(prefix: &str) -> &'static str {
    match prefix {
        "aden:" => "⊙",
        "you:" => "▸",
        "coxn:" => "♦", // U+2666: a diamond that is always one cell wide (U+25C6 ◆ is ambiguous)
        "tool:" | "cmd:" => "▪",
        "ok:" => "✓",
        "err:" => "✗",
        "sys:" => "·",
        _ => "·", // unknown
    }
}

/// The accent color for a role's sigil. Color lives only here in the gutter.
fn role_color(prefix: &str) -> Color {
    match prefix {
        "aden:" => ACCENT,         // structural graph actions
        "you:" => SAND,            // the human voice
        "coxn:" => ACCENT,         // slate: the model voice
        "tool:" | "cmd:" => MAUVE, // dusty mauve
        "ok:" => OK,               // sage
        "err:" => ERR,             // terracotta
        _ => QUIET,                // sys: and unknown, very quiet
    }
}

/// Body-text color per role, so the sigil and the line it labels agree. The
/// model's voice is brightest (the live content); the user's prompt recedes one
/// step; tool outcomes settle into their semantic color (sage ok / terracotta
/// err); tool/sys lines stay quiet. This is the change that turns a monochrome
/// transcript into a scannable record without adding any new hue.
fn role_body_color(prefix: &str) -> Color {
    match prefix {
        "aden:" => SECONDARY,    // ADEN graph results are informative but secondary
        "coxn:" => PRIMARY,      // brightest: the model's voice
        "you:" => SECONDARY,     // the human voice, one step down
        "tool:" | "cmd:" => DIM, // subordinate machine steps, below the human
        "ok:" => OK,             // settled outcomes leave the brightness axis
        "err:" => ERR,           // for their semantic color
        "sys:" => QUIET,         // recede furthest, matching the · sigil's depth
        _ => SECONDARY,          // unknown
    }
}

/// A cosine "breath" between [`RULE`] and a brighter rule, on a 4s cycle, so the
/// separator gently pulses while a turn runs. Pure: phase is the elapsed millis
/// passed in. Returns the static [`RULE`] when idle (no motion at rest). The
/// bright target is wide enough (a ~50-step delta) to be perceptible on average
/// terminals -- a narrower range reads as no motion at all.
fn rule_breath(elapsed_ms: u128, pending: bool) -> Color {
    if !pending {
        return RULE;
    }
    let phase = (elapsed_ms % 4000) as f64;
    let t = (1.0 + (phase * std::f64::consts::TAU / 4000.0).cos()) / 2.0;
    let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    Color::Rgb(lerp(58, 110), lerp(53, 100), lerp(48, 88))
}

/// Convert a plain-text transcript (with `you:` / `coxn:` / `tool:` / `sys:`
/// prefixes) into a styled [`Text`]: a per-role sigil in the gutter, the role's
/// text in its body color. Continuation lines (a turn's later paragraphs, code,
/// raw output) inherit the *owning* turn's body color, so a multi-paragraph
/// `coxn:` answer stays PRIMARY throughout while raw `tool:` output recedes to
/// DIM -- the brightness follows the voice, not the line shape. While `pending`,
/// the final line is brightened to [`SHIMMER`] so streaming output reads as live.
fn styled_output(output: &str, pending: bool) -> Text<'static> {
    let last = output.lines().count().saturating_sub(1);
    // The body color of the turn currently being rendered; continuation lines
    // adopt it until the next role line changes it.
    let mut owner_body = SECONDARY;
    let lines: Vec<Line<'static>> = output
        .lines()
        .enumerate()
        .map(|(idx, raw)| {
            let live = pending && idx == last;
            if let Some(prefix) = ROLE_PREFIXES.iter().find(|&&p| raw.starts_with(p)) {
                owner_body = role_body_color(prefix);
                let after = &raw[prefix.len()..];
                let rest = after.strip_prefix(' ').unwrap_or(after);
                let body = if live { SHIMMER } else { owner_body };
                Line::from(vec![
                    Span::styled(
                        format!("{} ", role_sigil(prefix)),
                        Style::default().fg(role_color(prefix)),
                    ),
                    Span::styled(rest.to_string(), Style::default().fg(body)),
                ])
            } else {
                // Continuation: aligned under the role text, inheriting its color.
                let body = if live { SHIMMER } else { owner_body };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(raw.to_string(), Style::default().fg(body)),
                ])
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

/// Visible item-row capacity of a menu body inside a screen of `area_height`
/// rows. The overlay spends 2 rows on its border and 2 on the blank line +
/// hint footer, leaving the rest for items. Clamped to `item_count` so an
/// empty menu stays zero.
pub fn menu_max_rows(area_height: u16, item_count: usize) -> usize {
    if item_count == 0 {
        return 0;
    }
    let overhead: u16 = 4;
    ((area_height.saturating_sub(overhead)) as usize)
        .min(item_count)
        .max(1)
}

// -- Render ---------------------------------------------------------------

/// Render one frame: a ruled output pane, a hairline separator, a one-row status
/// line, and a one-row input prompt, with the confirm modal or picker overlaid
/// when active. Pure in `view`; testable with `TestBackend`.
pub fn render(frame: &mut Frame, view: &View) {
    let areas = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1), // separator
        Constraint::Length(1), // status
        Constraint::Length(1), // input
    ])
    .split(frame.area());
    let pane = areas[0];

    // Animation phase: every motion below is a pure function of elapsed millis,
    // redrawn each 100ms tick. Idle (no turn in flight) means no motion.
    let elapsed = view.pending_since.map(|since| since.elapsed());
    let pending = elapsed.is_some();
    let elapsed_ms = elapsed.map(|e| e.as_millis()).unwrap_or(0);

    // -- Output pane: ruled gutter, sigils, wrapped, scrollable ---
    let content_width = pane.width.saturating_sub(PANE_GUTTER);
    let total_lines = wrapped_line_count(&view.output, content_width as usize);
    let pane_height = pane.height as usize;

    // scroll_offset is distance-from-bottom: 0 pins to the bottom (show the last
    // `pane_height` lines); a larger value backs up, clamped to the scrollback.
    let max_scrollback = total_lines.saturating_sub(pane_height) as u16;
    let from_bottom = view.scroll_offset.min(max_scrollback);
    let scroll_row = max_scrollback - from_bottom;

    // The left rule is the Block border; one column of padding sets text off it.
    let output_widget = Paragraph::new(styled_output(&view.output, pending))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(RULE))
                .padding(Padding::new(1, 0, 0, 0)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0));

    // -- Separator: a hairline that breathes while a turn runs ---
    // A `└` corner on the first column joins the left rule to the hairline so the
    // two read as one continuous frame element rather than an abrupt tee.
    let sep_w = areas[1].width as usize;
    let sep = if sep_w > 0 {
        format!("└{}", "─".repeat(sep_w - 1))
    } else {
        String::new()
    };
    let separator = Paragraph::new(Line::from(Span::styled(
        sep,
        Style::default().fg(rule_breath(elapsed_ms, pending)),
    )));

    // -- Status line: spinner + elapsed + status, blinking scroll marker ---
    // 10-frame braille sweep at 80ms/frame: a smooth ~0.8s rotation.
    const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut status_spans: Vec<Span<'static>> = Vec::new();
    if pending {
        let frame = SPIN[(elapsed_ms / 80) as usize % SPIN.len()];
        status_spans.push(Span::styled(
            format!("{frame} "),
            Style::default().fg(ACCENT),
        ));
        // Tenths so the counter joins the spinner's motion from the first frame
        // (not frozen at "0s" for a second). SECONDARY: live data, not metadata.
        status_spans.push(Span::styled(
            format!("{}.{}s ", elapsed_ms / 1000, (elapsed_ms % 1000) / 100),
            Style::default().fg(SECONDARY),
        ));
    }
    // Vim mode tag: always shown so the user always knows which mode is active.
    // Non-Insert modes render in ACCENT (an actionable state the user entered
    // intentionally); Insert renders in DIM so it recedes in steady-state typing.
    {
        let tag = format!("-- {} -- ", view.vim.mode.tag());
        let color = if view.vim.mode == Mode::Insert {
            DIM
        } else {
            ACCENT
        };
        status_spans.push(Span::styled(tag, Style::default().fg(color)));
    }
    // Status fields, segmented by meaning so the row is readable at a glance.
    // main.rs joins fields with "  |  "; splitting here is the seam between data
    // and presentation. The model name leads in SECONDARY; the cancel hint comes
    // *before* the receding savings/ctx so it survives a narrow terminal (it is
    // the only way to abort a running turn); the rest recede in DIM behind a
    // faint middot (QUIET sits below DIM on a dark field, so it recedes furthest).
    let segs: Vec<&str> = view
        .status
        .split("  |  ")
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(model) = segs.first() {
        status_spans.push(Span::styled(
            model.to_string(),
            Style::default().fg(SECONDARY),
        ));
    }
    // Aden activity badge: "aden" in ACCENT when active, a dim "·" placeholder
    // when absent so the slot is always visible and the layout stable.
    status_spans.push(Span::styled("  ", Style::default().fg(QUIET)));
    if view.aden_active {
        status_spans.push(Span::styled("aden-cockpit", Style::default().fg(ACCENT)));
        status_spans.push(Span::styled(
            " ⊙K/gd/Ctrl+L ga / ?",
            Style::default().fg(QUIET),
        ));
        if let Some(last) = &view.last_aden {
            let display = if last.len() > 15 {
                format!("{}...", &last[..12])
            } else {
                last.clone()
            };
            status_spans.push(Span::styled(
                format!(" last: {}", display),
                Style::default().fg(QUIET),
            ));
        }
    } else {
        status_spans.push(Span::styled("·", Style::default().fg(QUIET)));
    }
    if pending {
        // Same doctrine as the overlay hints: the key (Ctrl-C) in ACCENT, the
        // verb in DIM. Ordered before the receding savings/ctx so it survives a
        // narrow terminal -- it is the only way to abort a running turn.
        status_spans.push(Span::styled("  (", Style::default().fg(DIM)));
        status_spans.push(Span::styled("Ctrl-C", Style::default().fg(ACCENT)));
        status_spans.push(Span::styled(" cancel)", Style::default().fg(DIM)));
    }
    for seg in segs.iter().skip(1) {
        status_spans.push(Span::styled(" · ", Style::default().fg(QUIET)));
        status_spans.push(Span::styled(seg.to_string(), Style::default().fg(DIM)));
    }
    // Scroll marker: a right-aligned ▾ that blinks while a turn runs (and shows
    // steady when idle). ACCENT, not DIM: it is the one actionable affordance on
    // the row (scroll back to live), matching the spinner/caret/prompt accent.
    if view.scroll_offset > 0 && (!pending || (elapsed_ms % 1000) < 600) {
        // All status content is ASCII / EAW=N, so chars().count() == display width.
        // If a CJK model name ever lands here, switch to unicode-width (a ratatui dep).
        let used: usize = status_spans.iter().map(|s| s.content.chars().count()).sum();
        let width = areas[2].width as usize;
        if width > used + 1 {
            status_spans.push(Span::raw(" ".repeat(width - used - 1)));
            status_spans.push(Span::styled("▾", Style::default().fg(ACCENT)));
        }
    }

    // -- Input prompt: accent chevron, block caret, optional visual highlight --
    // In Command mode the whole row is replaced by the ex-style `:cmdline`.
    // In Visual mode, the selection range is highlighted with reversed video in
    // the ACCENT color. In Normal mode the existing reverse-video block caret
    // marks the position. Insert mode is unchanged.
    let prompt_line = if view.vim.mode == Mode::Command {
        // Ledger-styled command line: ':' in ACCENT, cmdline text in SECONDARY,
        // a reverse-video block cursor cell at the end (where the next char goes).
        let cmdline = view.vim.cmdline.clone();
        Line::from(vec![
            Span::styled(":".to_string(), Style::default().fg(ACCENT)),
            Span::styled(cmdline, Style::default().fg(SECONDARY)),
            Span::styled(
                " ".to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
            ),
        ])
    } else {
        let sel = view.vim.selection(view.cursor);
        let input = &view.input;
        let cursor = view.cursor;

        let before = &input[..cursor];
        let (cursor_ch, after_start) = match input[cursor..].chars().next() {
            Some(c) => (c, cursor + c.len_utf8()),
            None => (' ', cursor),
        };
        let cursor_cell = cursor_ch.to_string();
        let after = &input[after_start..];

        // Build the input spans. When a Visual selection covers the cursor (or any
        // part of the line), we split the line into up to five segments:
        //   pre-selection | selection-before-cursor | cursor | selection-after-cursor | post-selection
        // Char boundary safety: sel bounds come from the Vim engine, which only
        // ever sets them at char boundaries via prev_boundary / next_boundary.
        let prompt_spans: Vec<Span<'static>> = if let Some((sel_lo, sel_hi)) = sel {
            // Clamp both bounds into the buffer before slicing. The engine only
            // emits char-boundary offsets, but a stale selection could outrun a
            // now-shorter buffer; clamping keeps every slice below in range.
            // After length-clamping we also snap to a char boundary: if the
            // buffer shrank mid-char the clamped index may land inside a
            // multi-byte sequence, making text[sel_hi..] a non-boundary slice.
            let sel_hi = crate::vim::snap_boundary_down(input, sel_hi.min(input.len()));
            let sel_lo = crate::vim::snap_boundary_down(input, sel_lo.min(input.len()));
            // Inclusive end: extend hi to include the char under it.
            let sel_end = input[sel_hi..]
                .chars()
                .next()
                .map_or(sel_hi, |c| sel_hi + c.len_utf8())
                .min(input.len());
            debug_assert!(
                input.is_char_boundary(sel_lo) && input.is_char_boundary(sel_end),
                "visual selection must slice on char boundaries"
            );

            // Segment the input around the selection, guarding all slices.
            let pre_sel = &input[..sel_lo];
            // `>` not `>=`: at cursor == sel_lo this segment is empty (the cursor
            // cell renders that position), which also avoids a backward slice.
            let sel_before_cursor = if cursor > sel_lo {
                &input[sel_lo..cursor.min(sel_end)]
            } else {
                ""
            };
            let sel_after_cursor = if after_start < sel_end {
                &input[after_start..sel_end]
            } else {
                ""
            };
            let post_sel = if sel_end < input.len() {
                &input[sel_end..]
            } else {
                ""
            };

            let sel_style = Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED);
            let cur_style = Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED);

            let chevron = if view.aden_active { "⊙ " } else { "› " };
            vec![
                Span::styled(chevron.to_string(), Style::default().fg(ACCENT)),
                Span::styled(pre_sel.to_string(), Style::default().fg(SECONDARY)),
                Span::styled(sel_before_cursor.to_string(), sel_style),
                Span::styled(cursor_cell, cur_style),
                Span::styled(sel_after_cursor.to_string(), sel_style),
                Span::styled(post_sel.to_string(), Style::default().fg(SECONDARY)),
            ]
        } else {
            // Normal / Insert: accent chevron + reverse-video block caret.
            // Typed text in SECONDARY (the composing voice), matching `you:` record
            // lines -- so the draft never outshines coxn's PRIMARY responses.
            // If cursor at end and we have a suggestion (for /commands), append
            // it dim as ghost text so it "auto populates" as you type.
            {
                let chevron = if view.aden_active { "⊙ " } else { "› " };
                let mut spans = vec![
                    Span::styled(chevron.to_string(), Style::default().fg(ACCENT)),
                    Span::styled(before.to_string(), Style::default().fg(SECONDARY)),
                    Span::styled(
                        cursor_cell,
                        Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
                    ),
                    Span::styled(after.to_string(), Style::default().fg(SECONDARY)),
                ];
                if cursor == input.len()
                    && let Some(sugg) = &view.suggestion
                {
                    spans.push(Span::styled(sugg.clone(), Style::default().fg(DIM)));
                }
                spans
            }
        };

        Line::from(prompt_spans)
    };

    frame.render_widget(output_widget, pane);
    frame.render_widget(separator, areas[1]);
    frame.render_widget(Paragraph::new(Line::from(status_spans)), areas[2]);
    frame.render_widget(Paragraph::new(prompt_line), areas[3]);

    if let Some(prompt) = &view.modal {
        let hint = "[y] proceed   [n] block";
        // Floor the inner width so the hint line is never truncated on a narrow
        // terminal (the hint is the modal's critical affordance).
        let inner_width = prompt.chars().count().max(hint.chars().count()).max(40) as u16;
        let area = centered_rect(inner_width + 4, 5, frame.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .padding(Padding::horizontal(1))
            .title(Line::from(Span::styled(
                " confirm ",
                Style::default().fg(DIM),
            )));
        let body = Text::from(vec![
            Line::from(Span::styled(prompt.clone(), Style::default().fg(PRIMARY))),
            Line::from(""),
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(ACCENT)),
                Span::styled(" proceed   ", Style::default().fg(DIM)),
                Span::styled("[n]", Style::default().fg(ACCENT)),
                Span::styled(" block", Style::default().fg(DIM)),
            ]),
        ]);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(body).block(block), area);
    } else if let Some(menu) = &view.menu {
        // The picker overlay: a windowed slice of the item list. Only rows
        // [scroll, scroll+rows) are drawn so long lists stay reachable; the
        // selected row carries a › marker and bold primary text. Overflow
        // above/below is signalled by ▴/▾ in the title so the user knows there
        // is more to scroll to.
        let hint = "j/k ↑↓  G/gg  PgUp/Dn  Enter  Esc";
        let count = menu.items.len();
        let rows = menu_max_rows(frame.area().height, count);
        let start = menu.scroll.min(count);
        let end = (start + rows).min(count);
        let width = menu
            .items
            .iter()
            .map(|i| i.label.chars().count())
            .chain([menu.title.chars().count(), hint.chars().count()])
            .max()
            .unwrap_or(0) as u16;
        let mut lines: Vec<Line<'static>> = menu
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(i, item)| {
                if i == menu.selected {
                    Line::from(vec![
                        Span::styled("› ", Style::default().fg(ACCENT)),
                        Span::styled(
                            item.label.clone(),
                            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(item.label.clone(), Style::default().fg(SECONDARY)),
                    ])
                }
            })
            .collect();
        lines.push(Line::from(""));
        // Same ACCENT-key / DIM-verb split the modal hint uses, so both overlays
        // treat affordances identically. `hint` above still sizes the box.
        lines.push(Line::from(vec![
            Span::styled("j/k ↑↓", Style::default().fg(ACCENT)),
            Span::styled(" move  ", Style::default().fg(DIM)),
            Span::styled("G/gg", Style::default().fg(ACCENT)),
            Span::styled(" jump  ", Style::default().fg(DIM)),
            Span::styled("Enter", Style::default().fg(ACCENT)),
            Span::styled(" choose  ", Style::default().fg(DIM)),
            Span::styled("Esc", Style::default().fg(ACCENT)),
            Span::styled(" cancel", Style::default().fg(DIM)),
        ]));
        let height = lines.len() as u16 + 2;
        let area = centered_rect(width + 6, height, frame.area());
        let has_above = start > 0 && rows < count;
        let has_below = end < count && rows < count;
        let mut title = String::from(" ");
        if has_above {
            title.push_str("▴ ");
        }
        title.push_str(&menu.title);
        if has_below {
            title.push_str(" ▾");
        }
        title.push(' ');
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .padding(Padding::horizontal(1))
            .title(Line::from(Span::styled(title, Style::default().fg(DIM))));
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    // Help overlay: Ledger-styled cheatsheet, topmost so it renders over any
    // other overlay. Closed by Esc / q / ? (wired in the event loop).
    if view.show_help {
        let help_lines: Vec<Line<'static>> = [
            // COCKPIT - ADEN graph harness for high velocity coding
            ("COCKPIT (ADEN graph)", None),
            ("Tab", Some("palette: ADEN symbols+actions first")),
            (
                "ADEN items",
                Some("direct: understand/view/impact + comms/doctor/audit"),
            ),
            ("Ctrl+L etc", Some("ADEN ops from Insert mode")),
            ("last: in status", Some("tracks last graph action")),
            ("", None),
            // BEGINNER section first for average users (power users scroll past)
            ("BEGINNER (no vim needed)", None),
            ("type normally to chat", Some("")),
            (
                "Ctrl+L on a word",
                Some("pulls ADEN context (also Ctrl+A/I/V/G)"),
            ),
            ("Tab", Some("opens command palette / suggestions")),
            ("? or /help", Some("this help")),
            ("mouse wheel", Some("scrolls transcript")),
            ("", None),
            ("PICKER (open menu)", None),
            ("j/k or ↑↓", Some("move selection")),
            ("G / gg", Some("jump to bottom / top")),
            ("Ctrl-D/U, PgUp/Dn", Some("scroll the list one page")),
            ("5j", Some("repeat a motion (count)")),
            ("Enter / Esc", Some("choose / cancel")),
            ("", None),
            // Section: modes
            ("MODES", None),
            ("i", Some("Insert — type freely")),
            ("a", Some("Insert after cursor")),
            ("Esc", Some("Normal mode")),
            ("v", Some("Visual mode")),
            (":", Some("Command mode")),
            ("", None),
            // Section: motions
            ("MOTIONS", None),
            ("h l", Some("left / right")),
            ("0  $", Some("line start / end")),
            ("w  e  b", Some("word forward / end / back")),
            ("j  k", Some("scroll line down / up")),
            ("gg  G", Some("top / bottom of transcript")),
            ("[n]motion", Some("repeat motion n times")),
            ("", None),
            // Section: operators
            ("OPERATORS", None),
            ("d{m}  c{m}  y{m}", Some("delete/change/yank to motion")),
            ("dd  cc  yy", Some("linewise delete/change/yank")),
            ("x", Some("delete char under cursor")),
            ("D  C", Some("delete/change to end of line")),
            ("r{c}", Some("replace char under cursor")),
            ("p  P", Some("paste after / before cursor")),
            ("", None),
            // Section: commands
            ("COMMANDS", None),
            (":q", Some("quit")),
            (":help  ?", Some("this overlay")),
            ("Tab", Some("palette (ADEN symbols + actions)")),
            (":model", Some("switch model")),
            (":tools", Some("list active tools")),
            (":clear", Some("new conversation")),
            (":understand {sym}", Some("aden: explain a symbol")),
            (":grep {pat}", Some("aden: search codebase")),
            (":ask {q}", Some("aden: architectural query")),
            (":view [sym]", Some("aden: launch browser view")),
            (":viz/:gm [sym]", Some("aden: insert mermaid diagram")),
            (":doctor", Some("aden: env + health diagnostics")),
            (":impact {sym}", Some("aden: blast radius")),
            (":communities", Some("aden: code clusters")),
            (":audit", Some("aden: security audit")),
            ("K / gd / Ctrl+L", Some("aden understand on word at cursor")),
            ("ga / Ctrl+A", Some("aden asm on word at cursor")),
            ("gi / Ctrl+I", Some("aden impact on word at cursor")),
            ("gv / Ctrl+V", Some("aden view on word at cursor")),
            ("/ / Ctrl+G", Some("aden grep on word at cursor")),
            ("]", Some("aden communities / graph nav")),
            (
                "Tab",
                Some("ADEN symbol actions (understand/view/impact + more)"),
            ),
            ("", None),
            ("Esc  q  ?", Some("close this overlay")),
        ]
        .iter()
        .map(|(key, desc)| {
            if key.is_empty() {
                // blank separator row
                Line::from("")
            } else if let Some(d) = desc {
                // key in ACCENT, separator in QUIET, description in DIM
                Line::from(vec![
                    Span::styled(format!("{key:<18}"), Style::default().fg(ACCENT)),
                    Span::styled(d.to_string(), Style::default().fg(DIM)),
                ])
            } else {
                // section header in SECONDARY (slightly brighter than DIM)
                Line::from(Span::styled(
                    key.to_string(),
                    Style::default().fg(SECONDARY),
                ))
            }
        })
        .collect();

        // Width: widest line content (key col + desc col) or a floor of 44.
        let content_width: u16 = help_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(44)
            .max(44) as u16;
        let height = help_lines.len() as u16 + 2; // +2 for the block border
        let area = centered_rect(content_width + 4, height, frame.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .padding(Padding::horizontal(1))
            .title(Line::from(Span::styled(" help ", Style::default().fg(DIM))));
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(help_lines).block(block), area);
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
    /// Move the picker selection by a signed step (j/k/↑↓ with optional count).
    MenuStep(i32),
    /// Jump the picker to the first row (gg / Home).
    MenuTop,
    /// Jump the picker to the last row (G / End).
    MenuBottom,
    /// Scroll the picker viewport up one page (Ctrl-U / PageUp).
    MenuPageUp,
    /// Scroll the picker viewport down one page (Ctrl-D / PageDown).
    MenuPageDown,
    /// Act on the selected picker item (Enter).
    MenuSelect,
    /// Close the picker without acting (Esc).
    MenuCancel,
    /// Answer a confirm modal: proceed.
    Confirm,
    /// Answer a confirm modal: block.
    Cancel,
    /// ADEN understand on word at cursor (Ctrl-L; works from Insert for casual users).
    AdenUnderstand,
    /// ADEN asm on word at cursor (Ctrl-A).
    AdenAsm,
    /// ADEN impact on word at cursor (Ctrl-I).
    AdenImpact,
    /// ADEN view launch on word at cursor (Ctrl-V).
    AdenView,
    /// ADEN grep on word at cursor (Ctrl-G).
    AdenGrep,
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
        // ADEN power keys as Ctrl shortcuts (work in Insert mode too, for users who don't want to learn Normal mode).
        (KeyCode::Char('l'), KeyModifiers::CONTROL) => Some(Action::AdenUnderstand),
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => Some(Action::AdenAsm),
        (KeyCode::Char('i'), KeyModifiers::CONTROL) => Some(Action::AdenImpact),
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => Some(Action::AdenView),
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => Some(Action::AdenGrep),
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
        let terminal = ratatui::try_init()?;
        // Enable mouse so average users can scroll the transcript with wheel
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        Ok(Self { terminal })
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
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        f();
        self.terminal = ratatui::try_init()?;
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        Ok(())
    }

    /// The current terminal size. Used to compute PageUp/PageDown scroll amounts.
    pub fn size(&self) -> Option<ratatui::layout::Size> {
        self.terminal.size().ok()
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
    fn menu_navigation_clamps_and_selects() {
        let mut v = View::new();
        v.open_menu(Menu {
            kind: MenuKind::Model,
            title: "m".to_string(),
            items: vec![menu_item("a"), menu_item("b"), menu_item("c")],
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
        });
        // Clamps at the ends (vim-like; no wrap).
        v.menu_move(-1);
        assert_eq!(v.menu_selected().unwrap().value, "a");
        v.menu_move(1);
        assert_eq!(v.menu_selected().unwrap().value, "b");
        v.menu_move(1);
        assert_eq!(v.menu_selected().unwrap().value, "c");
        v.menu_move(1); // past the last stays put
        assert_eq!(v.menu_selected().unwrap().value, "c");
        v.close_menu();
        assert!(v.menu.is_none());
        // An empty menu does not open.
        v.open_menu(Menu {
            kind: MenuKind::Session,
            title: "s".to_string(),
            items: Vec::new(),
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
        });
        assert!(v.menu.is_none());
    }

    fn open_menu_for_keys() -> View {
        let mut v = View::new();
        v.open_menu(Menu {
            kind: MenuKind::Commands,
            title: "pick".to_string(),
            items: vec![
                menu_item("0"),
                menu_item("1"),
                menu_item("2"),
                menu_item("3"),
                menu_item("4"),
            ],
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
        });
        v
    }

    #[test]
    fn menu_keys_map_to_actions() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let mut v = open_menu_for_keys();

        // Arrows, Ctrl-P/N, and bare j/k all step.
        assert_eq!(v.map_menu_key(k(KeyCode::Down)), Some(Action::MenuStep(1)));
        assert_eq!(
            v.map_menu_key(k(KeyCode::Char('j'))),
            Some(Action::MenuStep(1))
        );
        assert_eq!(v.map_menu_key(ctrl('n')), Some(Action::MenuStep(1)));
        assert_eq!(v.map_menu_key(k(KeyCode::Up)), Some(Action::MenuStep(-1)));
        assert_eq!(
            v.map_menu_key(k(KeyCode::Char('k'))),
            Some(Action::MenuStep(-1))
        );
        assert_eq!(v.map_menu_key(ctrl('p')), Some(Action::MenuStep(-1)));

        // Count prefix repeats the motion.
        v.menu.as_mut().unwrap().selected = 0;
        assert_eq!(v.map_menu_key(k(KeyCode::Char('3'))), None);
        assert_eq!(
            v.map_menu_key(k(KeyCode::Char('j'))),
            Some(Action::MenuStep(3))
        );

        // G / gg / pages / jumps.
        v.menu.as_mut().unwrap().selected = 0;
        assert_eq!(
            v.map_menu_key(k(KeyCode::Char('G'))),
            Some(Action::MenuBottom)
        );
        assert_eq!(v.map_menu_key(k(KeyCode::End)), Some(Action::MenuBottom));
        assert_eq!(v.map_menu_key(k(KeyCode::Home)), Some(Action::MenuTop));
        assert_eq!(
            v.map_menu_key(k(KeyCode::PageDown)),
            Some(Action::MenuPageDown)
        );
        assert_eq!(v.map_menu_key(k(KeyCode::PageUp)), Some(Action::MenuPageUp));
        assert_eq!(v.map_menu_key(ctrl('d')), Some(Action::MenuPageDown));
        assert_eq!(v.map_menu_key(ctrl('u')), Some(Action::MenuPageUp));

        // gg is a two-key sequence: first `g` consumes (None), second jumps top.
        v.menu.as_mut().unwrap().selected = 2;
        assert_eq!(v.map_menu_key(k(KeyCode::Char('g'))), None);
        assert!(v.menu.as_ref().unwrap().pending_g);
        assert_eq!(v.map_menu_key(k(KeyCode::Char('g'))), Some(Action::MenuTop));
        assert!(!v.menu.as_ref().unwrap().pending_g);

        // A non-g after a pending g clears the state and routes the key.
        v.menu.as_mut().unwrap().selected = 0;
        assert_eq!(v.map_menu_key(k(KeyCode::Char('g'))), None);
        assert_eq!(v.map_menu_key(k(KeyCode::Down)), Some(Action::MenuStep(1)));
        assert!(!v.menu.as_ref().unwrap().pending_g);

        // Enter / Esc / Ctrl-C.
        assert_eq!(v.map_menu_key(k(KeyCode::Enter)), Some(Action::MenuSelect));
        assert_eq!(v.map_menu_key(k(KeyCode::Esc)), Some(Action::MenuCancel));
        assert_eq!(v.map_menu_key(ctrl('c')), Some(Action::MenuCancel));
    }

    #[test]
    fn menu_viewport_follows_selection() {
        // 20 items in a 10-row-tall screen: body rows = 10 - 4 = 6.
        let items: Vec<MenuItem> = (0..20).map(|i| menu_item(&format!("row{i}"))).collect();
        let mut v = View::new();
        v.open_menu(Menu {
            kind: MenuKind::Session,
            title: "long".to_string(),
            items,
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
        });
        let rows = menu_max_rows(10, 20);
        assert_eq!(rows, 6);

        // Step past the bottom of the window: scroll follows.
        for _ in 0..6 {
            v.menu_step(1, rows);
        }
        let m = v.menu.as_ref().unwrap();
        assert_eq!(m.selected, 6);
        assert_eq!(m.scroll, 1, "scroll advances to keep selection in view");

        // Page all the way down hits the last row.
        v.menu_bottom(rows);
        let m = v.menu.as_ref().unwrap();
        assert_eq!(m.selected, 19);
        assert!(m.scroll + rows > 19);

        // Back to top.
        v.menu_top(rows);
        let m = v.menu.as_ref().unwrap();
        assert_eq!(m.selected, 0);
        assert_eq!(m.scroll, 0);

        // A half-page up from the bottom moves the selection up by `rows`.
        v.menu_bottom(rows);
        let before = v.menu.as_ref().unwrap().selected;
        v.menu_page(-1, rows);
        let after = v.menu.as_ref().unwrap().selected;
        assert_eq!(before - after, rows, "page moves one full viewport");
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
        // Width must be wide enough for the always-present mode tag plus "ready".
        // "-- INSERT -- ready  ·" is ~24 chars minimum.
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).expect("test backend");
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
        let text = styled_output(
            "you: hello\ncoxn: world\ntool: ok\nsys: info\nunknown",
            false,
        );
        assert_eq!(text.lines.len(), 5);
        // Role lines: a gutter sigil span, then the role text (prefix stripped).
        assert_eq!(text.lines[0].spans.len(), 2);
        assert_eq!(text.lines[0].spans[0].content, "▸ ");
        assert_eq!(text.lines[0].spans[1].content, "hello");
        assert_eq!(text.lines[1].spans[0].content, "♦ ");
        // Non-role lines: a blank gutter span, then the raw content.
        assert_eq!(text.lines[4].spans[0].content, "  ");
        assert_eq!(text.lines[4].spans[1].content, "unknown");
    }

    #[test]
    fn styled_output_shimmers_last_line_when_pending() {
        // While pending, the final line is brightened to SHIMMER; not otherwise.
        let pending = styled_output("coxn: a\ncoxn: b", true);
        assert_eq!(pending.lines[1].spans[1].style.fg, Some(SHIMMER));
        assert_eq!(pending.lines[0].spans[1].style.fg, Some(PRIMARY));
        let idle = styled_output("coxn: a\ncoxn: b", false);
        assert_eq!(idle.lines[1].spans[1].style.fg, Some(PRIMARY));
    }

    // -- vim wiring-seam tests -----------------------------------------------
    // These tests drive Vim::handle through a View so the integration seam is
    // covered: mode transitions, text mutations, and scroll outcomes all go
    // through the same path that drive() uses.

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }
    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn vim_esc_enters_normal_mode() {
        use crate::vim::{Mode, Outcome};
        let mut view = View::new();
        for c in "hello".chars() {
            view.input_push(c);
        }
        let out = view.vim.handle(&mut view.input, &mut view.cursor, esc());
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(view.vim.mode, Mode::Normal);
    }

    #[test]
    fn vim_j_k_in_normal_produce_scroll() {
        use crate::vim::{Outcome, Scroll};
        let mut view = View::new();
        // Enter Normal first.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let down = view.vim.handle(&mut view.input, &mut view.cursor, k('j'));
        assert_eq!(down, Outcome::Scroll(Scroll::LineDown));
        let up = view.vim.handle(&mut view.input, &mut view.cursor, k('k'));
        assert_eq!(up, Outcome::Scroll(Scroll::LineUp));
    }

    #[test]
    fn vim_x_deletes_char_under_cursor() {
        use crate::vim::Outcome;
        let mut view = View::new();
        for c in "hello".chars() {
            view.input_push(c);
        }
        // Normal mode, cursor at start.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        view.cursor = 0;
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('x'));
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(view.input, "ello");
    }

    #[test]
    fn vim_i_a_return_to_insert() {
        use crate::vim::Mode;
        let mut view = View::new();
        // Go Normal, then back to Insert with 'i'.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        assert_eq!(view.vim.mode, Mode::Normal);
        view.vim.handle(&mut view.input, &mut view.cursor, k('i'));
        assert_eq!(view.vim.mode, Mode::Insert);
        // Go Normal again, then back to Insert with 'a'.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        view.vim.handle(&mut view.input, &mut view.cursor, k('a'));
        assert_eq!(view.vim.mode, Mode::Insert);
    }

    #[test]
    fn vim_insert_typing_returns_pass() {
        use crate::vim::Outcome;
        let mut view = View::new();
        // In Insert (the default), ordinary chars return Pass.
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('z'));
        assert_eq!(out, Outcome::Pass);
        // The buffer is unchanged — the host's input_push is responsible.
        assert_eq!(view.input, "");
    }

    #[test]
    fn vim_insert_typing_inserts_via_host_path() {
        use crate::vim::Outcome;
        let mut view = View::new();
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('h'));
        // Pass means the host (input_push) runs next.
        assert_eq!(out, Outcome::Pass);
        view.input_push('h'); // simulate what drive() does on Pass
        assert_eq!(view.input, "h");
    }

    #[test]
    fn vim_normal_enter_submits() {
        use crate::vim::Outcome;
        let mut view = View::new();
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let out = view.vim.handle(&mut view.input, &mut view.cursor, enter());
        assert_eq!(out, Outcome::Submit);
    }

    #[test]
    fn vim_ctrl_c_passes_through_in_every_mode() {
        // Ctrl-C must always reach the host (quit), never be swallowed by a
        // modal catch-all. Regression for the escape hatch in EVERY mode.
        use crate::vim::{Mode, Outcome};
        let cc = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut view = View::new();
        // Insert (the default).
        assert_eq!(
            view.vim.handle(&mut view.input, &mut view.cursor, cc),
            Outcome::Pass
        );
        // Normal.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        assert_eq!(
            view.vim.handle(&mut view.input, &mut view.cursor, cc),
            Outcome::Pass
        );
        // Visual.
        view.vim.handle(&mut view.input, &mut view.cursor, k('v'));
        assert_eq!(
            view.vim.handle(&mut view.input, &mut view.cursor, cc),
            Outcome::Pass
        );
        // Command (':' entered from Normal) — the global Ctrl-C guard must fire
        // before handle_command would otherwise swallow the key.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        view.vim.handle(&mut view.input, &mut view.cursor, k(':'));
        assert_eq!(view.vim.mode, Mode::Command);
        assert_eq!(
            view.vim.handle(&mut view.input, &mut view.cursor, cc),
            Outcome::Pass
        );
    }

    #[test]
    fn vim_visual_submit_clears_selection_no_stale_anchor() {
        // Regression for the Visual-submit panic: after selecting and pressing
        // Enter, take_input() must leave a clean Insert state with no selection
        // that could slice into the now-empty buffer on the next render.
        use crate::vim::{Mode, Outcome};
        let mut view = View::new();
        for c in "hello".chars() {
            view.input_push(c);
        }
        view.cursor = 0;
        view.vim.handle(&mut view.input, &mut view.cursor, esc()); // Normal
        view.vim.handle(&mut view.input, &mut view.cursor, k('v')); // Visual
        view.vim.handle(&mut view.input, &mut view.cursor, k('l')); // extend
        let out = view.vim.handle(&mut view.input, &mut view.cursor, enter());
        assert_eq!(out, Outcome::Submit);
        let _ = view.take_input();
        assert_eq!(view.input, "");
        assert_eq!(view.vim.mode, Mode::Insert);
        // The crux: no selection survives into the empty buffer.
        assert_eq!(view.vim.selection(view.cursor), None);
    }

    // -- help overlay tests -------------------------------------------------

    #[test]
    fn toggle_help_flips_show_help() {
        let mut view = View::new();
        assert!(!view.show_help, "help starts hidden");
        view.toggle_help();
        assert!(view.show_help, "toggle once -> shown");
        view.toggle_help();
        assert!(!view.show_help, "toggle twice -> hidden");
    }

    #[test]
    fn close_help_always_hides() {
        let mut view = View::new();
        view.show_help = true;
        view.close_help();
        assert!(!view.show_help);
        // Idempotent.
        view.close_help();
        assert!(!view.show_help);
    }

    #[test]
    fn vim_question_mark_in_normal_returns_toggle_help() {
        use crate::vim::Outcome;
        let mut view = View::new();
        // Enter Normal mode first.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('?'));
        assert_eq!(out, Outcome::ToggleHelp);
    }

    #[test]
    fn render_help_overlay_shows_cheatsheet_text() {
        // When show_help is true the rendered frame must contain cheatsheet
        // keywords that prove the overlay is present.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut view = View::new();
        view.show_help = true;
        // Wide enough to accommodate the overlay content.
        let mut terminal = Terminal::new(TestBackend::new(80, 60)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("MODES"),
            "help overlay: MODES section: {text:?}"
        );
        assert!(
            text.contains("MOTIONS"),
            "help overlay: MOTIONS section: {text:?}"
        );
        assert!(
            text.contains("OPERATORS"),
            "help overlay: OPERATORS section: {text:?}"
        );
        assert!(
            text.contains("COMMANDS"),
            "help overlay: COMMANDS section: {text:?}"
        );
        assert!(
            text.contains(":help"),
            "help overlay: :help entry: {text:?}"
        );
        assert!(
            text.contains(":model"),
            "help overlay: :model entry: {text:?}"
        );
    }

    #[test]
    fn render_help_overlay_hidden_by_default() {
        // Without show_help the overlay text must not appear in the frame.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let view = View::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        // "MODES" is a section header that only exists inside the help overlay.
        assert!(!text.contains("MODES"), "overlay must be hidden: {text:?}");
    }

    // -- status line polish tests -------------------------------------------

    #[test]
    fn render_status_always_shows_mode_tag() {
        // The mode tag must appear in the status line regardless of mode.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut view = View::new();
        view.set_status("my-model");
        // Default is Insert.
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("INSERT"), "INSERT tag must appear: {text:?}");
    }

    #[test]
    fn render_status_shows_aden_active_badge() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut view = View::new();
        view.set_status("my-model");
        view.aden_active = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("aden"),
            "aden badge must appear when active: {text:?}"
        );
    }

    #[test]
    fn render_status_shows_model_name() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut view = View::new();
        view.set_status("test-model-x");
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("test-model-x"),
            "model name must appear: {text:?}"
        );
    }
}
