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
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
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

/// Mode cheat-sheet tip dismisses after this much idle time (M6).
pub const MODE_TIP_IDLE: Duration = Duration::from_secs(5);

/// What a [`Menu`] selects, so the event loop knows how to act on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// Switch the active model to the selected id.
    Model,
    /// Resume the selected session slug.
    Session,
    /// Command palette / slash commands (sets the input line).
    Commands,
    /// Fuzzy unified palette (M4): slash verbs, models, sessions, recent input.
    Palette,
    /// `@` file attachment picker (fuzzy project paths).
    AtFiles,
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
    /// Type-to-filter query (M4 [`MenuKind::Palette`] only).
    pub filter: String,
    /// Full catalog before filtering (M4 palette only; empty for other pickers).
    pub catalog: Vec<MenuItem>,
}

/// Lower score = tighter match. `None` = no subsequence fit.
pub fn fuzzy_score(query: &str, haystack: &str) -> Option<u32> {
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut score = 0u32;
    let mut last_pos: Option<usize> = None;
    for (pos, c) in h.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            score += match last_pos {
                Some(lp) => (pos - lp) as u32,
                None => pos as u32,
            };
            last_pos = Some(pos);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

impl Menu {
    /// Recompute [`Menu::items`] from [`Menu::catalog`] + [`Menu::filter`].
    /// Pins [`Menu::selected`] to the top match (index 0).
    pub fn apply_palette_filter(&mut self) {
        if !matches!(self.kind, MenuKind::Palette | MenuKind::AtFiles) {
            return;
        }
        let q = self.filter.trim();
        if q.is_empty() {
            self.items = if self.kind == MenuKind::AtFiles {
                self.catalog.iter().take(80).cloned().collect()
            } else {
                self.catalog.clone()
            };
        } else {
            let mut ranked: Vec<(u32, usize)> = self
                .catalog
                .iter()
                .enumerate()
                .filter_map(|(i, item)| fuzzy_score(q, &item.label).map(|s| (s, i)))
                .collect();
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            self.items = ranked
                .iter()
                .map(|(_, i)| self.catalog[*i].clone())
                .collect();
        }
        self.selected = 0;
        self.scroll = 0;
        self.count = None;
        self.pending_g = false;
    }
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

/// Which confirmation modal is active. Gates use y/n; tool approval uses o/s/d/x.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModalKind {
    /// Gate-block proceed/block (y/n).
    #[default]
    Gate,
    /// Per-tool approval before apply (o/s/d/x).
    ToolApproval,
}

/// User choice while a tool-approval modal is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalChoice {
    Once,
    Session,
    Decline,
    CancelTurn,
    Expand,
    Collapse,
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
    /// Which key mapping applies while `modal` is set.
    pub modal_kind: ModalKind,
    /// A diff body shown inside the modal, alongside `modal`. Painted line-by-
    /// line through [`paint_diff_line`] (green `+`, red `-`, cyan `@@`,
    /// context unstyled). Used by the gate-block confirmation (M3) so the
    /// user sees the rejected diff before answering; long hunks collapse via
    /// `modal_diff_expanded`.
    pub modal_diff: Option<String>,
    /// Expand/collapse state for the modal diff body when it overflows the
    /// default preview window.
    pub modal_diff_expanded: bool,
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
    /// Compact one-line mode tip under the mode tag (M6). Shown briefly after
    /// `g?` and dismissed after [`MODE_TIP_IDLE`].
    pub show_mode_tip: bool,
    /// When the mode tip auto-hides; extended on user activity while visible.
    pub mode_tip_until: Option<Instant>,
    /// Whether aden is active this session. Drives the status-line badge.
    /// Set by the event loop each time capabilities are (re-)probed.
    pub aden_active: bool,
    /// Last ADEN action performed (for cockpit status feel, e.g. "understand 'drive'").
    pub last_aden: Option<String>,
    /// Inline ghost-text suggestion (dim) for /commands as you type (Tab or
    /// Right to accept). Populated live for better discoverability of commands.
    pub suggestion: Option<String>,
    /// In-progress transcript drag-select (M5): visual line indices `(start, end)`.
    pub transcript_drag: Option<(usize, usize)>,
    /// TUI 3.0 structured state (`COXN_TUI3=1`). Conversation + activity channels.
    pub ui3: Option<crate::ui::Ui3State>,
    /// Active transcript search (M2). `None` when no search has been opened.
    /// A search is "open" (interactive prompt active) while `query_open` is
    /// true; after Enter commits, `query_open` flips to false but `matches` /
    /// `current` persist so `n`/`N` can cycle until the user cancels or opens
    /// a new search.
    pub search: Option<SearchState>,
}

/// Linear-match transcript search state (M2). Matches are record-by-line
/// substring (case-sensitive substring for MVP; can be promoted to a regex
/// or fuzzy comparator in M5 with zero View API change).
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// Live pattern the user is typing in the search prompt. Once committed
    /// (`query_open == false`) this is the pattern the matches are scored
    /// against -- editing it re-scores.
    pub query: String,
    /// True while the search prompt is editing (before Enter). After Enter,
    /// this flips to false and `current` pins to the first match found from
    /// the search direction.
    pub query_open: bool,
    /// Backward search (`?`) reverses the cycle direction of `n`. Forward
    /// search (`/`) is the default.
    pub backward: bool,
    /// Indices into the OUTPUT record lines that match the current `query`.
    /// Recomputed live as `query` changes (incremental search).
    pub matches: Vec<usize>,
    /// Index into `matches` of the current cursor; `n` advances, `N` retreats
    /// (or vice versa under backward). Wraps around the end.
    pub current: usize,
}

impl SearchState {
    /// Score the query against the output and refresh `matches`. Empty query
    /// clears matches so the search prompt shows "0 matches" honestly.
    /// Pure in `output`; `current` is clamped to the match range so a stale
    /// cursor never points past the end of `matches`.
    fn rescore(&mut self, output: &str) {
        self.matches.clear();
        if self.query.is_empty() {
            self.current = 0;
            return;
        }
        for (i, line) in output.lines().enumerate() {
            if line.contains(&self.query) {
                self.matches.push(i);
            }
        }
        if self.current >= self.matches.len() {
            self.current = 0;
        }
    }

    /// Forward / backward aware step. Returns the *next* cursor index after
    /// advancing in the search direction (`backward` means `N` advances and
    /// `n` retreats; vim lets users redefine, we keep it simple).
    /// Wraps mod-len so cycles never get stuck at the ends.
    fn step(&mut self, advance: i32) {
        if self.matches.is_empty() {
            self.current = 0;
            return;
        }
        let len = self.matches.len() as i32;
        let cur = self.current as i32;
        let next = ((cur + advance) % len + len) % len;
        self.current = next as usize;
    }
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

    /// Enable TUI 3.0 structured state when `COXN_TUI3` is set.
    pub fn init_ui3(&mut self) {
        if crate::ui::enabled() {
            self.ui3 = Some(crate::ui::Ui3State::default());
        }
    }

    /// Sync conversation turn cards from pump messages.
    pub fn sync_turns(&mut self, messages: &[crate::model::Message]) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.sync_turns(messages);
        }
    }

    pub fn set_chrome(&mut self, chrome: crate::ui::ChromeState) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.chrome = chrome;
        }
    }

    pub fn activity_push(&mut self, title: impl Into<String>, body: impl Into<String>) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.activity.push(title, body);
        }
    }

    pub fn activity_start_live(&mut self, title: impl Into<String>) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.activity.start_live(title);
        }
    }

    pub fn activity_append_live(&mut self, chunk: &str) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.activity.append_live(chunk);
        }
    }

    pub fn activity_finish_live(&mut self) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.activity.finish_live();
        }
    }

    /// Replace the live activity body (streaming `/execute`, `!cmd`).
    pub fn activity_set_live(&mut self, title: impl Into<String>, body: impl Into<String>) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.activity.live_title = Some(title.into());
            ui3.activity.live_body = body.into();
        }
    }

    /// True when structured TUI 3.0 state is active.
    pub fn ui3_active(&self) -> bool {
        self.ui3.is_some()
    }

    pub fn set_live_turn(&mut self, body: impl Into<String>, run_buf: impl Into<String>) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.live = Some(crate::ui::LiveTurn {
                body: body.into(),
                run_buf: run_buf.into(),
            });
        }
    }

    pub fn clear_live_turn(&mut self) {
        if let Some(ui3) = &mut self.ui3 {
            ui3.live = None;
        }
    }

    /// Raise a gate confirmation modal with `prompt`. Block on the user's answer
    /// (proceed / block) is the pump's job; this only sets the view state.
    pub fn confirm(&mut self, prompt: impl Into<String>) {
        self.modal_kind = ModalKind::Gate;
        self.modal = Some(prompt.into());
    }

    /// Raise a confirmation modal with `prompt` and an optional `diff` body
    /// painted line-by-line through [`paint_diff_line`] (M3). Used by the
    /// gate-block confirmation so the user sees the rejected diff before
    /// answering. Empty `diff` degrades to plain confirm-render.
    pub fn confirm_with_diff(&mut self, prompt: impl Into<String>, diff: impl Into<String>) {
        self.set_modal_diff(diff);
        self.confirm(prompt);
    }

    /// Raise a tool-approval modal with optional diff preview before apply.
    pub fn confirm_tool_approval(&mut self, prompt: impl Into<String>, diff: impl Into<String>) {
        self.modal_kind = ModalKind::ToolApproval;
        self.set_modal_diff(diff);
        self.modal = Some(prompt.into());
    }

    fn set_modal_diff(&mut self, diff: impl Into<String>) {
        let diff_text = diff.into();
        if diff_text.is_empty() {
            self.modal_diff = None;
            self.modal_diff_expanded = false;
        } else {
            self.modal_diff = Some(diff_text);
            self.modal_diff_expanded = false;
        }
    }

    /// Dismiss the modal and any diff body alongside it.
    pub fn dismiss(&mut self) {
        self.modal = None;
        self.modal_kind = ModalKind::Gate;
        self.modal_diff = None;
        self.modal_diff_expanded = false;
    }

    /// Open a picker overlay (non-empty menus only).
    pub fn open_menu(&mut self, menu: Menu) {
        if !menu.items.is_empty() {
            self.menu = Some(menu);
        }
    }

    /// Open the fuzzy unified palette (M4). `menu.catalog` must be populated.
    pub fn open_palette(&mut self, mut menu: Menu) {
        if menu.kind != MenuKind::Palette || menu.catalog.is_empty() {
            return;
        }
        menu.apply_palette_filter();
        if !menu.items.is_empty() {
            self.menu = Some(menu);
        }
    }

    /// Close the picker.
    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    // -- M2 transcript search -------------------------------------------
    /// Open the search prompt with the given direction (`/` forward, `?`
    /// backward). Clears any prior search; query starts empty so the user
    /// types a fresh pattern.
    pub fn search_open(&mut self, backward: bool) {
        let mut st = SearchState {
            query_open: true,
            backward,
            ..SearchState::default()
        };
        st.rescore(&self.output);
        self.search = Some(st);
    }

    /// Push a character into the live search query (re-scores matches).
    pub fn search_push(&mut self, c: char) {
        if let Some(st) = &mut self.search {
            st.query.push(c);
            st.rescore(&self.output);
        }
    }

    /// Delete the last character of the live search query (Backspace).
    pub fn search_backspace(&mut self) {
        if let Some(st) = &mut self.search {
            if st.query.pop().is_some() {
                st.rescore(&self.output);
            }
        }
    }

    /// Commit the search (Enter). Stops editing and pins `current` to the
    /// first match in cycle order; viewport snapping happens in `render`.
    pub fn search_commit(&mut self) {
        if let Some(st) = &mut self.search {
            st.query_open = false;
            st.current = st.current.min(st.matches.len().saturating_sub(1));
            if !st.matches.is_empty() {
                st.current = 0;
            }
        }
    }

    /// Cancel the search and drop its state entirely (Esc).
    pub fn search_cancel(&mut self) {
        self.search = None;
    }

    /// Walk the active search by one step. `n` advances along the match list
    /// in the search direction (`?` flips the polarity); `N` is the inverse.
    pub fn search_step(&mut self, advance: i32) {
        let backward = self.search.as_ref().map(|s| s.backward).unwrap_or(false);
        let effective = if backward { -advance } else { advance };
        if let Some(st) = &mut self.search {
            st.step(effective);
        }
    }

    /// True while a search prompt is still open for editing (before Enter).
    pub fn search_editing(&self) -> bool {
        self.search.as_ref().map(|s| s.query_open).unwrap_or(false)
    }

    /// The visual-line index for the current committed match, if any.
    pub fn search_match_line(&self) -> Option<usize> {
        self.search
            .as_ref()
            .filter(|s| !s.matches.is_empty())
            .and_then(|s| s.matches.get(s.current).copied())
    }

    /// Toggle the help overlay on or off.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Show the compact mode tip and start (or refresh) its idle timer.
    pub fn show_mode_tip(&mut self) {
        self.show_mode_tip = true;
        self.mode_tip_until = Some(Instant::now() + MODE_TIP_IDLE);
    }

    /// Extend the mode-tip idle timer while the user is active.
    pub fn touch_mode_tip(&mut self) {
        if self.show_mode_tip {
            self.mode_tip_until = Some(Instant::now() + MODE_TIP_IDLE);
        }
    }

    /// Hide the mode tip when its idle timer has elapsed.
    pub fn refresh_mode_tip(&mut self) {
        if self.show_mode_tip
            && self
                .mode_tip_until
                .is_some_and(|until| Instant::now() >= until)
        {
            self.show_mode_tip = false;
            self.mode_tip_until = None;
        }
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

    /// Keys while the fuzzy palette (M4) is open: type-to-filter plus j/k
    /// navigation. Returns `None` when the menu is not a palette (caller
    /// should fall back to [`map_menu_key`]).
    pub fn map_palette_key(&mut self, key: KeyEvent) -> Option<Action> {
        let menu = self.menu.as_mut()?;
        if !matches!(menu.kind, MenuKind::Palette | MenuKind::AtFiles) {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('c') && ctrl {
            menu.count = None;
            menu.pending_g = false;
            return Some(Action::MenuCancel);
        }

        match key.code {
            KeyCode::Enter => Some(Action::MenuSelect),
            KeyCode::Esc => Some(Action::MenuCancel),
            KeyCode::Backspace => {
                menu.filter.pop();
                menu.apply_palette_filter();
                None
            }
            KeyCode::Char('j') | KeyCode::Down if !ctrl => Some(Action::MenuStep(1)),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => Some(Action::MenuStep(-1)),
            KeyCode::Char('n') if ctrl => Some(Action::MenuStep(1)),
            KeyCode::Char('p') if ctrl => Some(Action::MenuStep(-1)),
            KeyCode::Char('G') if !ctrl => Some(Action::MenuBottom),
            KeyCode::Home => Some(Action::MenuTop),
            KeyCode::End => Some(Action::MenuBottom),
            KeyCode::Char('d') if ctrl => Some(Action::MenuPageDown),
            KeyCode::Char('u') if ctrl => Some(Action::MenuPageUp),
            KeyCode::PageDown => Some(Action::MenuPageDown),
            KeyCode::PageUp => Some(Action::MenuPageUp),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                menu.filter.push(c);
                menu.apply_palette_filter();
                None
            }
            _ => None,
        }
    }

    /// Append a typed character at the cursor position.
    pub fn input_push(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Bulk-insert a string at the cursor (bracketed-paste payload, or
    /// `Alt-Enter` / `Shift-Enter` which lands a single `\n` here too). Keeps
    /// the cursor on a char boundary after the inserted text. Used for the
    /// whole paste as one unit so Vim's per-key motions and mode transitions
    /// do not fire mid-paste.
    pub fn input_push_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
        while !self.input.is_char_boundary(self.cursor) && self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    /// Visible line count for the input box: one per `\n`-separated logical
    /// line, capped at `max_rows` so a runaway paste does not eat the whole
    /// screen -- clamp happens at the layout step, not here. Used by [`render`]
    /// to size the input area and by tests to assert grow-box behavior.
    pub fn input_line_count(&self) -> usize {
        self.input.matches('\n').count() + 1
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
        if let Some(ui3) = &mut self.ui3 {
            ui3.conv_scroll_offset = 0;
            ui3.activity_scroll_offset = 0;
        }
    }

    /// Primary pane text: conversation cards (ui3) or legacy output.
    pub fn primary_text(&self) -> String {
        if let Some(ui3) = &self.ui3 {
            ui3.conversation_text()
        } else {
            self.output.clone()
        }
    }

    /// Exportable transcript for `/copy` and file write.
    pub fn export_text(&self) -> String {
        if let Some(ui3) = &self.ui3 {
            ui3.export_text()
        } else {
            self.output.clone()
        }
    }

    pub fn toggle_tools_collapsed(&mut self) -> bool {
        if let Some(ui3) = &mut self.ui3 {
            ui3.tools_collapsed = !ui3.tools_collapsed;
            ui3.tools_collapsed
        } else {
            false
        }
    }

    pub fn toggle_reasoning_hidden(&mut self) -> bool {
        if let Some(ui3) = &mut self.ui3 {
            ui3.reasoning_hidden = !ui3.reasoning_hidden;
            ui3.reasoning_hidden
        } else {
            false
        }
    }

    /// Update inline suggestion (ghost text) for / slash commands based on
    /// current input. Called after any input edit so typing /hel immediately
    /// shows the dim "p " completion.
    pub fn refresh_suggestion(&mut self) {
        self.suggestion = if self.input.starts_with('/') {
            crate::commands::complete_input(&self.input).and_then(|full| {
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
        let total = wrapped_line_count(&self.primary_text(), content as usize);
        total.saturating_sub(pane_height as usize) as u16
    }

    /// Max scroll for the activity drawer (ui3 only).
    pub fn max_activity_scroll(&self, width: u16, pane_height: u16) -> u16 {
        let Some(ui3) = &self.ui3 else {
            return 0;
        };
        let content = width.saturating_sub(1);
        let total = wrapped_line_count(&ui3.activity.display_text(), content as usize);
        total.saturating_sub(pane_height as usize) as u16
    }

    fn scroll_ui3_conv(&mut self, amount: i16, max: u16) {
        if let Some(ui3) = &mut self.ui3 {
            if amount < 0 {
                ui3.conv_scroll_offset = ui3.conv_scroll_offset.saturating_sub((-amount) as u16);
            } else {
                ui3.conv_scroll_offset = ui3
                    .conv_scroll_offset
                    .saturating_add(amount as u16)
                    .min(max);
            }
        }
    }

    fn scroll_ui3_activity(&mut self, amount: i16, max: u16) {
        if let Some(ui3) = &mut self.ui3 {
            if amount < 0 {
                ui3.activity_scroll_offset =
                    ui3.activity_scroll_offset.saturating_sub((-amount) as u16);
            } else {
                ui3.activity_scroll_offset = ui3
                    .activity_scroll_offset
                    .saturating_add(amount as u16)
                    .min(max);
            }
        }
    }

    /// Scroll the primary pane (conversation or legacy output).
    pub fn scroll_primary_up(&mut self, amount: u16, max: u16) {
        if self.ui3.is_some() {
            self.scroll_ui3_conv(amount as i16, max);
        } else {
            self.scroll_up(amount, max);
        }
    }

    pub fn scroll_primary_down(&mut self, amount: u16) {
        if self.ui3.is_some() {
            self.scroll_ui3_conv(-(amount as i16), 0);
        } else {
            self.scroll_down(amount);
        }
    }

    pub fn scroll_activity_up(&mut self, amount: u16, max: u16) {
        self.scroll_ui3_activity(amount as i16, max);
    }

    pub fn scroll_activity_down(&mut self, amount: u16) {
        self.scroll_ui3_activity(-(amount as i16), 0);
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
pub(crate) fn wrapped_line_count(text: &str, width: usize) -> usize {
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
/// Diff hunk -- addition line (M3). Sage, matching the existing `OK` voice so
/// diffs read as state changes not alarms.
const DIFF_ADD: Color = Color::Rgb(122, 158, 126);
/// Diff hunk -- deletion line (M3). Terracotta, matching `ERR` for the same
/// reason; never alarm-red on a dark field.
const DIFF_DEL: Color = Color::Rgb(196, 123, 90);
/// Diff hunk -- `@@` header (M3). Cyan, the standard "hunk metadata" hue in
/// most diff tools (Codex / Grok / git).
const CYAN: Color = Color::Rgb(96, 142, 168);

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
/// Build the transcript paragraphs with optional search-match tinting.
/// Matched lines get a reverse-video ACCENT overlay on their body span so
/// the user can see where `n`/`N` will land; the *current* match (the one the
/// viewport has snapped to) is rendered with PRIMARY+REVERSED so it is the
/// brightest, while other matches are ACCENT-dim.
fn styled_output_with_search(
    output: &str,
    pending: bool,
    search: Option<&SearchState>,
) -> Text<'static> {
    let last = output.lines().count().saturating_sub(1);
    // The body color of the turn currently being rendered; continuation lines
    // adopt it until the next role line changes it.
    let mut owner_body = SECONDARY;
    let matches = search.map(|s| &s.matches).cloned().unwrap_or_default();
    let current_match_line = search.and_then(|s| {
        if s.matches.is_empty() {
            None
        } else {
            s.matches.get(s.current).copied()
        }
    });
    // Track whether the current line falls inside a ```diff fenced block (M3).
    // Toggle on a fence-open / fence-close line so every line until the close
    // gets routed through `paint_diff_line` for hunk coloring rather than the
    // role-body palette. Markdown / AsciiDoc engines are not invoked; the
    // detection is intentionally trivial (`trim_start == \`\`\`diff`).
    let mut in_diff_block = false;
    let lines: Vec<Line<'static>> = output
        .lines()
        .enumerate()
        .map(|(idx, raw)| {
            // \`\`\`diff fence tracking (M3).
            let trimmed = raw.trim_start();
            if trimmed == "```diff" || trimmed.starts_with("```diff ") {
                in_diff_block = !in_diff_block;
                // Render the fence marker as a dim accent so it stays legible
                // without competing with the hunks.
                return Line::from(vec![
                    Span::raw("  "),
                    Span::styled(raw.to_string(), Style::default().fg(DIM)),
                ]);
            }
            if trimmed == "```" && in_diff_block {
                in_diff_block = false;
                return Line::from(vec![
                    Span::raw("  "),
                    Span::styled(raw.to_string(), Style::default().fg(DIM)),
                ]);
            }

            let live = pending && idx == last;
            let matched = matches.contains(&idx);
            let current = current_match_line == Some(idx);
            let highlight = if current {
                Some(
                    Style::default()
                        .fg(PRIMARY)
                        .add_modifier(Modifier::REVERSED),
                )
            } else if matched {
                Some(Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED))
            } else {
                None
            };

            // Inside a ```diff block: paint via paint_diff_line, overriding
            // the role-body palette. Search highlight falls back to its
            // normal match part (still applied to the styled span color via
            // the `matched` overlay; for diff lines we keep search styling as
            // the primary tint so the user sees where the query hit).
            if in_diff_block {
                let painted = paint_diff_line(raw);
                let style = highlight.unwrap_or(painted.style);
                return Line::from(vec![
                    Span::raw("  "),
                    Span::styled(painted.content.clone(), style),
                ]);
            }

            // Tool-call cards (TUI 2.0): collapsed `▸ tool path` lines under coxn turns.
            if raw.starts_with("▸ ") {
                let body = if live { SHIMMER } else { MAUVE };
                let body_style = highlight.unwrap_or(Style::default().fg(body));
                return Line::from(vec![
                    Span::styled("▹ ", Style::default().fg(ACCENT)),
                    Span::styled(
                        raw.strip_prefix("▸ ").unwrap_or(raw).to_string(),
                        body_style,
                    ),
                ]);
            }

            if let Some(prefix) = ROLE_PREFIXES.iter().find(|&&p| raw.starts_with(p)) {
                owner_body = role_body_color(prefix);
                let after = &raw[prefix.len()..];
                let rest = after.strip_prefix(' ').unwrap_or(after);
                let body = if live { SHIMMER } else { owner_body };
                let body_style = highlight.unwrap_or(Style::default().fg(body));
                Line::from(vec![
                    Span::styled(
                        format!("{} ", role_sigil(prefix)),
                        Style::default().fg(role_color(prefix)),
                    ),
                    Span::styled(rest.to_string(), body_style),
                ])
            } else {
                // Continuation: aligned under the role text, inheriting its color.
                let body = if live { SHIMMER } else { owner_body };
                let body_style = highlight.unwrap_or(Style::default().fg(body));
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(raw.to_string(), body_style),
                ])
            }
        })
        .collect();
    Text::from(lines)
}

pub(crate) fn modal_hint_plain(view: &View) -> &'static str {
    match view.modal_kind {
        ModalKind::Gate if view.modal_diff.is_some() => {
            "[y] proceed   [n] block   [e] expand   [c] collapse"
        }
        ModalKind::Gate => "[y] proceed   [n] block",
        ModalKind::ToolApproval if view.modal_diff.is_some() => {
            "[o] once   [s] session   [d] decline   [x] cancel   [e] expand   [c] collapse"
        }
        ModalKind::ToolApproval => "[o] once   [s] session   [d] decline   [x] cancel",
    }
}

fn modal_hint_spans(view: &View) -> Vec<Span<'static>> {
    let accent = Style::default().fg(ACCENT);
    let dim = Style::default().fg(DIM);
    let mut spans = Vec::new();
    let push_key = |spans: &mut Vec<Span<'static>>, key: &str, label: &str| {
        spans.push(Span::styled(format!("[{key}]"), accent));
        spans.push(Span::styled(label.to_string(), dim));
    };
    match view.modal_kind {
        ModalKind::Gate => {
            push_key(&mut spans, "y", " proceed   ");
            push_key(&mut spans, "n", " block");
        }
        ModalKind::ToolApproval => {
            push_key(&mut spans, "o", " once   ");
            push_key(&mut spans, "s", " session   ");
            push_key(&mut spans, "d", " decline   ");
            push_key(&mut spans, "x", " cancel");
        }
    }
    if view.modal_diff.is_some() {
        spans.push(Span::styled("   ", dim));
        push_key(&mut spans, "e", " expand   ");
        push_key(&mut spans, "c", " collapse");
    }
    spans
}

// -- Diff hunk painting (M3) ----------------------------------------------

/// Style a single unified-diff line. Returns a styled span over the same
/// slice (no allocation, no parse):
///
/// * `+...`        -> green (addition)
/// * `-...`        -> red (deletion)
/// * `@@ ... @@`   -> cyan (hunk header)
/// * `\`...` and `\\` interpretable diff trailers -> quiet / ignored by
///   callers; treated as context here
/// * other         -> context (DEFAULT-fg plain)
///
/// Adversarial: a `-` line that begins inside an `@@` hunk (the rare case
/// where a contextual deletion includes the literal `+` byte) is still
/// classified by its leading column, never by the `+` near the line end.
///
/// A future syntax pass can route each line through [`paint_token`] for
/// language-aware styling; the stub here returns the line verbatim so the
/// design is open-ended without paying the cost.
fn paint_diff_line(line: &str) -> Span<'static> {
    let style = diff_line_style(line);
    Span::styled(line.to_string(), style)
}

/// Style decision for a single diff line. Pure and unit-testable.
///
/// Classification is by the first non-whitespace column only -- never by a
/// later `+`/`-` byte inside the text. Shears the adversarial case where a
/// deleted context line contains a literal `+` and would otherwise mis-tint.
fn diff_line_style(line: &str) -> Style {
    let trim = line.trim_start();
    if trim.starts_with("@@") {
        Style::default().fg(CYAN)
    } else if trim.starts_with('+') {
        Style::default().fg(DIFF_ADD)
    } else if trim.starts_with('-') {
        Style::default().fg(DIFF_DEL)
    } else {
        Style::default()
    }
}

/// A stub for future per-token syntax shading. The line's classification as
/// `+/-/@@` already happens at [`paint_diff_line`]; this gives a separate
/// seam for *intra-line* token colouring (e.g. identifiers, strings).
/// Today's zero-dep policy keeps it as identity (returns the line unaltered
/// so anyone routing a styled line through here preserves it). NOTE: the
/// returned string is cloned for ownership; the caller merges with style.
#[allow(dead_code)] // future-syntax seam; intentionally stubbed
fn paint_token(line: &str, _lang: &str) -> Span<'static> {
    Span::raw(line.to_string())
}

// -- Centered-rect helper -------------------------------------------------

/// A rectangle of `width` x `height` centered in `area`, clamped to fit.
pub(crate) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
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

/// Build the multi-line input prompt Text. One `Line` per `\n`-separated
/// logical line of `view.input`.
///
/// The cursor cell (reverse-video block caret) and Visual-selection styles
/// apply only on the line that hosts the cursor -- a Visual selection in this
/// engine is single-line, so non-cursor lines render plain. Continuation
/// lines (rows > 0) get a `↳` accent prefix instead of the chevron so the
/// whole prompt reads as one composed unit across rows.
///
/// Pure in `view`. Tested via `TestBackend` render in M1.
fn build_input_prompt_text(view: &View) -> Text<'static> {
    let input = &view.input;
    let cursor = view.cursor;
    let sel = view.vim.selection(cursor);

    // Walk the input line-by-line; track byte ranges so we can resolve which
    // line holds the cursor and offset cursor spans into the substring that
    // line owns.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line_start = 0usize;
    let chevron = if view.aden_active { "⊙ " } else { "› " };
    let cont = "↳ ";
    let plain = Style::default().fg(SECONDARY);
    let accent = Style::default().fg(ACCENT);

    // Loop over each \n-delimited slice. `chunks` would split on graphemes,
    // not bytes; split_terminator keeps the byte ranges we get from `match`.
    for (idx, segment) in input.split('\n').enumerate() {
        let line_end = line_start + segment.len();
        let is_cursor_line = (line_start..=line_end).contains(&cursor);

        let prefix = if idx == 0 { chevron } else { cont };

        if is_cursor_line {
            // Cursor-relative slices scoped to THIS line so a `\n` elsewhere
            // in the buffer does not corrupt the spans below.
            let sub_before = &input[line_start..cursor];
            let (cursor_ch, after_start) = match input[cursor..line_end.max(cursor)].chars().next()
            {
                Some(c) if cursor < line_end => (c, cursor + c.len_utf8()),
                _ => (' ', cursor),
            };
            let cursor_cell = cursor_ch.to_string();
            let sub_after = &input[after_start..line_end];

            // Visual selection: clamp to the cursor-bearing line. The engine
            // selects single lines in this baseline; if a stale selection
            // crosses `\n` boundaries we clamp both ends into this line.
            let prefix_span = Span::styled(prefix.to_string(), accent);
            let mut spans = vec![prefix_span];

            if let Some((sel_lo, sel_hi)) = sel {
                let sel_lo_b = sel_lo.max(line_start).min(line_end);
                let sel_hi_b = sel_hi.max(line_start).min(line_end);
                let sel_lo_b = crate::vim::snap_boundary_down(input, sel_lo_b);
                let sel_hi_b = crate::vim::snap_boundary_down(input, sel_hi_b);
                let sel_end = input[sel_hi_b.min(line_end)..]
                    .chars()
                    .next()
                    .map_or(sel_hi_b, |c| sel_hi_b + c.len_utf8())
                    .min(line_end);
                debug_assert!(
                    input.is_char_boundary(sel_lo_b) && input.is_char_boundary(sel_end),
                    "visual selection must slice on char boundaries"
                );

                let pre_sel = &input[line_start..sel_lo_b];
                let sel_before_cursor = if cursor > sel_lo_b {
                    &input[sel_lo_b..cursor.min(sel_end)]
                } else {
                    ""
                };
                let sel_after_cursor = if after_start < sel_end {
                    &input[after_start..sel_end]
                } else {
                    ""
                };
                let post_sel = if sel_end < line_end {
                    &input[sel_end..line_end]
                } else {
                    ""
                };
                let sel_style = Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED);
                let cur_style = Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED);
                // Only `pre_sel` is offset from `line_start`; emit it and skip
                // the standalone `sub_before` render below to avoid duplicating
                // text. (The cursor-cell split below uses cursor-relative spans.)
                let _ = sub_before; // pre_sel subsumes it.
                spans.push(Span::styled(pre_sel.to_string(), plain));
                spans.push(Span::styled(sel_before_cursor.to_string(), sel_style));
                spans.push(Span::styled(cursor_cell, cur_style));
                spans.push(Span::styled(sel_after_cursor.to_string(), sel_style));
                spans.push(Span::styled(post_sel.to_string(), plain));
            } else {
                // Normal / Insert: accent chevron + reverse-video block caret,
                // typed text in SECONDARY. If cursor at end-of-input and we
                // have a suggestion (for /commands), append a dim ghost on the
                // final line so it auto-populates as you type.
                spans.push(Span::styled(sub_before.to_string(), plain));
                spans.push(Span::styled(
                    cursor_cell,
                    Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::styled(sub_after.to_string(), plain));
                if cursor == input.len() && idx == view.input_line_count().saturating_sub(1) {
                    if let Some(sugg) = &view.suggestion {
                        spans.push(Span::styled(sugg.clone(), Style::default().fg(DIM)));
                    }
                }
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), accent),
                Span::styled(segment.to_string(), plain),
            ]));
        }

        line_start = line_end + 1; // skip the `\n` byte
    }

    if lines.is_empty() {
        // Edge case: empty input still renders a chevron + cursor cell so the
        // box never collapses to zero height.
        lines.push(Line::from(vec![
            Span::styled(chevron.to_string(), accent),
            Span::styled(
                " ".to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
            ),
        ]));
    }

    Text::from(lines)
}

/// Render one frame: a ruled output pane, a hairline separator, a one-row status
/// line, and a one-row input prompt, with the confirm modal or picker overlaid
/// when active. Pure in `view`; testable with `TestBackend`.
/// One-line mode cheat-sheet copy (M6). Chat-first when vim is off.
pub(crate) fn mode_tip_text(mode: Mode) -> &'static str {
    if !crate::vim::enabled() {
        return "Enter send. Ctrl-Space palette. @ files. ? help. Ctrl-F search.";
    }
    match mode {
        Mode::Insert => "type. Enter send. Alt-Enter newline. Up recalls history when empty.",
        Mode::Normal => "h/j/k/l. / search. gr grep. g? help.",
        Mode::Visual => "v motion. d/y operate.",
        Mode::Command => ":view :grep :doctor :impact.",
    }
}

/// Status-line mode label: CHAT when vim is off, else the vim mode tag.
pub(crate) fn mode_status_label(mode: Mode) -> String {
    if crate::vim::enabled() {
        format!("mode: {} ", mode.tag())
    } else {
        "mode: CHAT ".to_string()
    }
}

fn modal_title(view: &View) -> &'static str {
    match view.modal_kind {
        ModalKind::Gate => " gate ",
        ModalKind::ToolApproval => " approve ",
    }
}

pub fn render(frame: &mut Frame, view: &View) {
    let input_rows = (view.input_line_count() as u16).clamp(1, crate::layout::MAX_INPUT_ROWS);

    let (pane, areas) = if let Some(ref ui3) = view.ui3 {
        let v3 = crate::layout::areas_v3(frame.area(), view);
        let elapsed = view.pending_since.map(|since| since.elapsed());
        let pending = elapsed.is_some();
        crate::ui::render_chrome(frame, v3.chrome, &ui3.chrome);
        crate::ui::render_conversation(frame, v3.conversation, ui3, pending);
        crate::ui::render_activity(
            frame,
            v3.activity,
            &ui3.activity,
            ui3.activity_scroll_offset,
        );
        (
            v3.conversation,
            [v3.conversation, v3.separator, v3.status, v3.input],
        )
    } else {
        let areas = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(input_rows),
        ])
        .split(frame.area());
        (areas[0], [areas[0], areas[1], areas[2], areas[3]])
    };

    // Animation phase: every motion below is a pure function of elapsed millis,
    // redrawn each 100ms tick. Idle (no turn in flight) means no motion.
    let elapsed = view.pending_since.map(|since| since.elapsed());
    let pending = elapsed.is_some();
    let elapsed_ms = elapsed.map(|e| e.as_millis()).unwrap_or(0);

    // -- Output pane (legacy): ruled gutter, sigils, wrapped, scrollable ---
    if view.ui3.is_none() {
        let content_width = pane.width.saturating_sub(PANE_GUTTER);
        let total_lines = wrapped_line_count(&view.output, content_width as usize);
        let pane_height = pane.height as usize;
        let max_scrollback = total_lines.saturating_sub(pane_height) as u16;
        let from_bottom = view.scroll_offset.min(max_scrollback);
        let scroll_row = max_scrollback - from_bottom;
        let search_is_committed = view
            .search
            .as_ref()
            .map(|s| !s.query_open && s.matches.len() > 1)
            .unwrap_or(false);
        let snapped_scroll_row = if search_is_committed {
            view.search_match_line()
                .map(|target_line| {
                    let target = (target_line as u16).saturating_sub((pane_height as u16) / 2);
                    target.min(max_scrollback)
                })
                .unwrap_or(scroll_row)
        } else {
            scroll_row
        };
        let output_widget = Paragraph::new(styled_output_with_search(
            &view.output,
            pending,
            view.search.as_ref(),
        ))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(RULE))
                .padding(Padding::new(1, 0, 0, 0)),
        )
        .wrap(Wrap { trim: false })
        .scroll((snapped_scroll_row, 0));
        frame.render_widget(output_widget, pane);
    }

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
        let tag = mode_status_label(view.vim.mode);
        let color = if crate::vim::enabled() && view.vim.mode != Mode::Insert {
            ACCENT
        } else {
            DIM
        };
        status_spans.push(Span::styled(tag, Style::default().fg(color)));
    }
    // M6: one-line cheat-sheet tip under the mode tag; brief after `g?`.
    if view.show_mode_tip {
        status_spans.push(Span::styled(
            format!("{} ", mode_tip_text(view.vim.mode)),
            Style::default().fg(DIM),
        ));
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
    //
    // Multi-line: `Alt-Enter` / `Shift-Enter` insert `\n` (M1), so the input
    // area grows from one to N rows and the prompt becomes a `Text` (one Line
    // per `\n`-separated logical line). The cursor and visual-selection styles
    // only apply on the line that hosts the cursor; continuation lines get a
    // `↳` accent marker so the chevron reads as one prompt across rows.
    let prompt_text: Text<'static> = if view.vim.mode == Mode::Command {
        // Ledger-styled command line: ':' in ACCENT, cmdline text in SECONDARY,
        // a reverse-video block cursor cell at the end (where the next char goes).
        let cmdline = view.vim.cmdline.clone();
        Text::from(Line::from(vec![
            Span::styled(":".to_string(), Style::default().fg(ACCENT)),
            Span::styled(cmdline, Style::default().fg(SECONDARY)),
            Span::styled(
                " ".to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
            ),
        ]))
    } else {
        build_input_prompt_text(view)
    };

    frame.render_widget(separator, areas[1]);
    frame.render_widget(Paragraph::new(Line::from(status_spans)), areas[2]);
    // Search prompt (M2): an editing search overlays the input row with its
    // own prompt symbol (`/` or `?`) + the live query + match counter. A
    // committed-but-still-active search also gets a 1-row hint reading
    // "[n] next [N] prev [Esc] clear" so cycling is discoverable.
    if let Some(st) = &view.search {
        let prompt_sym = if st.backward { "?" } else { "/" };
        let mut spans = vec![
            Span::styled(prompt_sym.to_string(), Style::default().fg(ACCENT)),
            Span::styled(" ", Style::default().fg(ACCENT)),
            Span::styled(st.query.clone(), Style::default().fg(SECONDARY)),
            Span::styled(
                " ".to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::REVERSED),
            ),
        ];
        if st.query_open {
            // Live edit: show running match count.
            spans.push(Span::styled(
                format!("  [{} match]", st.matches.len()),
                Style::default().fg(DIM),
            ));
        } else {
            // Committed: cycling hint.
            spans.push(Span::styled(
                format!("  [{} of {}]  ", st.current + 1, st.matches.len()),
                Style::default().fg(DIM),
            ));
            spans.push(Span::styled("[n]", Style::default().fg(ACCENT)));
            spans.push(Span::styled(" next ", Style::default().fg(DIM)));
            spans.push(Span::styled("[N]", Style::default().fg(ACCENT)));
            spans.push(Span::styled(" prev ", Style::default().fg(DIM)));
            spans.push(Span::styled("[Esc]", Style::default().fg(ACCENT)));
            spans.push(Span::styled(" clear", Style::default().fg(DIM)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false }),
            areas[3],
        );
    } else {
        frame.render_widget(
            Paragraph::new(prompt_text).wrap(Wrap { trim: false }),
            areas[3],
        );
    }

    if let Some(prompt) = &view.modal {
        // Hunk preview budget (lines) before the diff body collapses with an
        // `[e] expand` hint. Chosen so a typical 6-10 line change shows in full;
        // longer changes (a `write_file` over a big file) start collapsed.
        const DIFF_PREVIEW_ROWS: usize = 12;

        let hint = modal_hint_plain(view);

        // Build the diff body lines if any, painting each line via
        // [`paint_diff_line`]. Long hunks collapse past DIFF_PREVIEW_ROWS.
        let mut diff_lines: Vec<Line<'static>> = Vec::new();
        let mut collapsed = false;
        if let Some(diff) = &view.modal_diff {
            let all: Vec<&str> = diff.lines().collect();
            let total = all.len();
            let expanded = view.modal_diff_expanded;
            let show_end = if expanded {
                total
            } else {
                DIFF_PREVIEW_ROWS.min(total)
            };
            for line in all.iter().take(show_end) {
                diff_lines.push(Line::from(paint_diff_line(line)));
            }
            if total > DIFF_PREVIEW_ROWS {
                collapsed = !expanded;
                let line = if expanded {
                    format!(
                        "… +{} more lines (collapse: [c])",
                        total - DIFF_PREVIEW_ROWS
                    )
                } else {
                    format!("… +{} more lines (expand: [e])", total - DIFF_PREVIEW_ROWS)
                };
                diff_lines.push(Line::from(Span::styled(line, Style::default().fg(DIM))));
            }
        }
        let _ = collapsed; // signalled in the `… +N more` line above

        // Floor the inner width so the hint line is never truncated on a narrow
        // terminal (the hint is the modal's critical affordance). The diff body
        // is allowed to wrap inside the box (Paragraph::Wrap below).
        let widest = prompt
            .chars()
            .count()
            .max(hint.chars().count())
            .max(diff_lines.iter().map(|l| l.width()).max().unwrap_or(0))
            .max(40) as u16;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .padding(Padding::horizontal(1))
            .title(Line::from(Span::styled(
                modal_title(view),
                Style::default().fg(DIM),
            )));

        let mut body_lines: Vec<Line<'static>> = Vec::with_capacity(2 + diff_lines.len() + 1);
        body_lines.push(Line::from(Span::styled(
            prompt.clone(),
            Style::default().fg(PRIMARY),
        )));
        if !diff_lines.is_empty() {
            body_lines.push(Line::from(""));
            body_lines.extend(diff_lines);
        }
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(modal_hint_spans(view)));
        let body = Text::from(body_lines.clone());
        let body_height = body_lines.len() as u16;
        let _ = body_height; // see immediately below for area sizing.

        // The modal area = body + 2 rows of border. Capped to the frame height
        // so very long diffs do not run off-screen (the body then wraps inside).
        let area_height = (body_height + 2).min(frame.area().height.saturating_sub(1));
        let area = centered_rect(
            widest.min(frame.area().width.saturating_sub(4)) + 4,
            area_height,
            frame.area(),
        );

        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(block),
            area,
        );
    } else if let Some(menu) = &view.menu {
        // The picker overlay: a windowed slice of the item list. Only rows
        // [scroll, scroll+rows) are drawn so long lists stay reachable; the
        // selected row carries a › marker and bold primary text. Overflow
        // above/below is signalled by ▴/▾ in the title so the user knows there
        // is more to scroll to.
        let hint = if matches!(menu.kind, MenuKind::Palette | MenuKind::AtFiles) {
            "type to filter  j/k  Enter  Esc"
        } else {
            "j/k ↑↓  G/gg  PgUp/Dn  Enter  Esc"
        };
        let count = menu.items.len();
        let rows = menu_max_rows(frame.area().height, count);
        let start = menu.scroll.min(count);
        let end = (start + rows).min(count);
        let filter_line = if matches!(menu.kind, MenuKind::Palette | MenuKind::AtFiles) {
            format!("filter: {}", menu.filter)
        } else {
            String::new()
        };
        let width = menu
            .items
            .iter()
            .map(|i| i.label.chars().count())
            .chain([
                menu.title.chars().count(),
                hint.chars().count(),
                filter_line.chars().count(),
            ])
            .max()
            .unwrap_or(0) as u16;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if matches!(menu.kind, MenuKind::Palette | MenuKind::AtFiles) {
            lines.push(Line::from(vec![
                Span::styled("filter: ", Style::default().fg(DIM)),
                Span::styled(menu.filter.clone(), Style::default().fg(ACCENT)),
                Span::styled("_", Style::default().fg(ACCENT)),
            ]));
        }
        lines.extend(
            menu.items
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
                }),
        );
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
        let mut help_entries: Vec<(&str, Option<&str>)> = vec![
            // COCKPIT - ADEN graph harness for high velocity coding
            ("COCKPIT (ADEN graph)", None),
            (
                "Ctrl-Space / Ctrl-P",
                Some("fuzzy palette: commands, models, sessions"),
            ),
            ("Tab", Some("slash commands + ADEN shortcuts")),
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
            ("Ctrl-Space / Ctrl-P / Tab", Some("palette / commands")),
            ("Ctrl-F / Ctrl-Shift-F", Some("transcript search (vim off)")),
            ("@", Some("attach project file (fuzzy picker)")),
            (
                "!cmd",
                Some("run shell locally — y/n gate, sandboxed when bwrap present"),
            ),
            (
                "? / g? / /help",
                Some("help overlay (? when input empty in chat mode)"),
            ),
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
            (":help  g?", Some("this overlay")),
            (
                "Ctrl-Space / Ctrl-P",
                Some("fuzzy palette; Tab for commands"),
            ),
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
        ];
        if view.ui3_active() {
            help_entries.extend([
                ("STRUCTURED SHELL", None),
                (
                    "chrome bar",
                    Some("model · scope · trust · aden always visible"),
                ),
                (
                    "conversation",
                    Some("turn cards — chat never replaced by /commands"),
                ),
                (
                    "activity drawer",
                    Some("/execute, !cmd, slash output, aden results"),
                ),
                ("Ctrl-T", Some("collapse multi-tool assistant cards")),
                (
                    "Ctrl-Shift-R",
                    Some("show/hide reasoning blocks in assistant text"),
                ),
                (
                    "wheel on activity",
                    Some("scroll activity drawer; wheel elsewhere scrolls chat"),
                ),
                ("", None),
            ]);
        }
        help_entries.push(("Esc  q  ?", Some("close this overlay")));
        let help_lines: Vec<Line<'static>> = help_entries
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
//
// Not `Copy` because `Paste(String)` owns its text. Callers below move the
// enum out of `Option<Action>` exactly once, so dropping `Copy` is harmless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Leave the pump.
    Quit,
    /// Submit the current input line as a user turn. Bare `Enter`.
    Submit,
    /// Insert a literal newline at the cursor (`Alt-Enter`, `Shift-Enter` when
    /// the terminal reports it). The input buffer holds multiple lines until
    /// Submit sends the whole thing.
    Newline,
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
    /// Expand a modal diff body past the preview row budget (M3; `[e]`).
    ModalExpand,
    /// Collapse an expanded modal diff back to the preview window (M3; `[c]`).
    ModalCollapse,
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
    /// Open transcript search forward (`Ctrl-F` when vim is off).
    SearchForward,
    /// Toggle collapsed tool cards in conversation (ui3; `Ctrl-T`).
    ToggleTools,
    /// Toggle hidden reasoning blocks in assistant cards (ui3; `Ctrl-Shift-R`).
    ToggleReasoning,
}

/// Map a key event while typing (base table). Up/Down scroll the transcript;
/// [`map_insert_key`] upgrades empty-input Up/Down to history recall.
/// Ctrl-P / Ctrl-N also browse history. Pure and testable.
pub fn map_input_key(key: KeyEvent) -> Option<Action> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
        // Bare Enter always submits. Alt-Enter and Shift-Enter insert a
        // newline so the input buffer can grow into a multi-line prompt
        // (Codex/Claude muscle memory). Other Enter modifiers (CONTROL, etc)
        // are dropped: Ctrl-Enter is not reliably distinguishable from plain
        // Enter across terminal decoders, so we do not promise a behavior.
        (KeyCode::Enter, KeyModifiers::NONE) => Some(Action::Submit),
        (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT) => Some(Action::Newline),
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
        (KeyCode::Char('f'), KeyModifiers::CONTROL) => Some(Action::SearchForward),
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => Some(Action::ToggleTools),
        (KeyCode::Char('r'), KeyModifiers::CONTROL | KeyModifiers::SHIFT) => {
            Some(Action::ToggleReasoning)
        }
        (KeyCode::Up, _) => Some(Action::ScrollUp),
        (KeyCode::Down, _) => Some(Action::ScrollDown),
        (KeyCode::PageUp, _) => Some(Action::PageUp),
        (KeyCode::PageDown, _) => Some(Action::PageDown),
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => Some(Action::Append(c)),
        _ => None,
    }
}

/// Insert-mode key map with shell-style history recall: when the input is empty,
/// Up/Down browse submitted lines; otherwise arrows scroll the transcript (so a
/// mouse wheel mapped to arrow keys still scrolls chat while typing).
pub fn map_insert_key(view: &View, key: KeyEvent) -> Option<Action> {
    let action = map_input_key(key)?;
    match action {
        Action::ScrollUp => {
            if view.hist_pos.is_some() || (view.input.is_empty() && !view.history.is_empty()) {
                Some(Action::HistoryPrev)
            } else {
                Some(Action::ScrollUp)
            }
        }
        Action::ScrollDown => {
            if view.hist_pos.is_some() {
                Some(Action::HistoryNext)
            } else {
                Some(Action::ScrollDown)
            }
        }
        other => Some(other),
    }
}

/// Map a key event while a confirm modal is up. `y`/Enter proceed; `n`/Esc
/// block. `e`/`c` expand/collapse the modal diff body when one is attached
/// (M3); they are no-ops when there is no diff. The pump selects this
/// mapping when [`View::modal`] is set.
pub fn map_modal_key(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Action::Cancel),
        KeyCode::Char('e') | KeyCode::Char('E') => Some(Action::ModalExpand),
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::ModalCollapse),
        _ => None,
    }
}

/// Map a key event while a tool-approval modal is up.
pub fn map_tool_approval_key(key: KeyEvent) -> Option<ToolApprovalChoice> {
    match key.code {
        KeyCode::Char('o' | 'O') => Some(ToolApprovalChoice::Once),
        KeyCode::Char('s' | 'S') => Some(ToolApprovalChoice::Session),
        KeyCode::Char('d' | 'D') => Some(ToolApprovalChoice::Decline),
        KeyCode::Char('x' | 'X') | KeyCode::Esc => Some(ToolApprovalChoice::CancelTurn),
        KeyCode::Char('e' | 'E') => Some(ToolApprovalChoice::Expand),
        KeyCode::Char('c' | 'C') => Some(ToolApprovalChoice::Collapse),
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
        // Enable bracketed paste so a multi-line paste arrives as one
        // `Event::Paste` (a single bulk insert, one undo unit) instead of a
        // stream of per-character key events (which would trip Vim's per-key
        // motions and mode transitions like the literal `Esc\ni` adversarial
        // paste). Disabled on Drop so the user's shell gets it back.
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
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
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        f();
        self.terminal = ratatui::try_init()?;
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
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
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        ratatui::try_restore().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Mutex;

    /// Serialize tests that mutate process-global `COXN_VIM`.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_coxn_vim(enabled: bool, f: impl FnOnce()) {
        let _guard = ENV_TEST_LOCK.lock().expect("env test lock");
        if enabled {
            unsafe { std::env::set_var("COXN_VIM", "1") };
        } else {
            unsafe { std::env::remove_var("COXN_VIM") };
        }
        f();
        unsafe { std::env::remove_var("COXN_VIM") };
    }

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
            filter: String::new(),
            catalog: Vec::new(),
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
            filter: String::new(),
            catalog: Vec::new(),
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
            filter: String::new(),
            catalog: Vec::new(),
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
            filter: String::new(),
            catalog: Vec::new(),
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
    fn fuzzy_subsequence_ranks_tighter_matches_first() {
        assert!(fuzzy_score("mdl", "model  qwen2.5-coder").is_some());
        assert!(fuzzy_score("zzz", "model  qwen2.5-coder").is_none());
        assert!(fuzzy_score("", "anything").is_some());
        let tight = fuzzy_score("ses", "session  foo").unwrap();
        let loose = fuzzy_score("s", "session  foo").unwrap();
        assert!(tight >= loose);
    }

    #[test]
    fn palette_filter_repins_selection_to_top_match() {
        let catalog = vec![
            MenuItem {
                value: "a".into(),
                label: "model alpha".into(),
            },
            MenuItem {
                value: "b".into(),
                label: "session beta".into(),
            },
            MenuItem {
                value: "c".into(),
                label: "/help".into(),
            },
        ];
        let mut menu = Menu {
            kind: MenuKind::Palette,
            title: "palette".into(),
            items: catalog.clone(),
            catalog,
            selected: 2,
            scroll: 1,
            count: None,
            pending_g: false,
            filter: String::new(),
        };
        menu.filter.push_str("ses");
        menu.apply_palette_filter();
        assert_eq!(menu.selected, 0);
        assert_eq!(menu.scroll, 0);
        assert_eq!(menu.items.len(), 1);
        assert!(menu.items[0].label.contains("session"));
    }

    #[test]
    fn palette_key_types_filter_without_submit_action() {
        let catalog = vec![menu_item("model one"), menu_item("session two")];
        let mut v = View::new();
        v.open_palette(Menu {
            kind: MenuKind::Palette,
            title: "palette".into(),
            items: catalog.clone(),
            catalog,
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
            filter: String::new(),
        });
        let k = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(v.map_palette_key(k('m')), None);
        assert_eq!(v.menu.as_ref().unwrap().filter, "m");
        assert_eq!(v.map_palette_key(k('j')), Some(Action::MenuStep(1)));
        assert_eq!(
            v.map_palette_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::MenuSelect)
        );
    }

    #[test]
    fn ctrl_space_does_not_map_to_submit() {
        let cs = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_ne!(map_input_key(cs), Some(Action::Submit));
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
        // Base table: arrows scroll; Ctrl-P/N history.
        assert_eq!(map_input_key(up), Some(Action::ScrollUp));
        assert_eq!(map_input_key(down), Some(Action::ScrollDown));
        assert_eq!(map_input_key(pgup), Some(Action::PageUp));
        assert_eq!(map_input_key(pgdn), Some(Action::PageDown));
        assert_eq!(map_input_key(ctrl_p), Some(Action::HistoryPrev));
        assert_eq!(map_input_key(ctrl_n), Some(Action::HistoryNext));
    }

    #[test]
    fn insert_key_up_recalls_history_when_input_empty() {
        let mut v = View::new();
        v.push_history("prior prompt".to_string());
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            map_insert_key(&v, up),
            Some(Action::HistoryPrev),
            "empty input + history → recall"
        );
        v.input = "typing".to_string();
        assert_eq!(
            map_insert_key(&v, up),
            Some(Action::ScrollUp),
            "non-empty input → scroll chat"
        );
    }

    #[test]
    fn insert_key_down_browses_history_while_navigating() {
        let mut v = View::new();
        v.push_history("a".to_string());
        v.push_history("b".to_string());
        v.history_prev();
        assert!(v.hist_pos.is_some());
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(map_insert_key(&v, down), Some(Action::HistoryNext));
        let empty = View::new();
        assert_eq!(
            map_insert_key(&empty, down),
            Some(Action::ScrollDown),
            "not browsing history → scroll"
        );
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
    fn tool_approval_modal_renders_osdx_hints() {
        let mut view = View::new();
        view.confirm_tool_approval("Approve edit src/foo.rs?", "@@\n-old\n+new\n");
        assert_eq!(view.modal_kind, ModalKind::ToolApproval);
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("[o]"), "once hint: {text:?}");
        assert!(text.contains("[s]"), "session hint: {text:?}");
        assert!(text.contains("[d]"), "decline hint: {text:?}");
        assert!(text.contains("[x]"), "cancel hint: {text:?}");
        assert!(!text.contains("[y] proceed"), "gate hint absent: {text:?}");
    }

    #[test]
    fn tool_approval_keys_map_to_choices() {
        let o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE);
        let s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        let d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(map_tool_approval_key(o), Some(ToolApprovalChoice::Once));
        assert_eq!(map_tool_approval_key(s), Some(ToolApprovalChoice::Session));
        assert_eq!(map_tool_approval_key(d), Some(ToolApprovalChoice::Decline));
        assert_eq!(
            map_tool_approval_key(x),
            Some(ToolApprovalChoice::CancelTurn)
        );
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
        // M3: expand/collapse keys on the modal's diff body.
        let e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(map_modal_key(e), Some(Action::ModalExpand));
        assert_eq!(map_modal_key(c), Some(Action::ModalCollapse));
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
        let text = styled_output_with_search(
            "you: hello\ncoxn: world\ntool: ok\nsys: info\nunknown",
            false,
            None,
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
        let pending = styled_output_with_search("coxn: a\ncoxn: b", true, None);
        assert_eq!(pending.lines[1].spans[1].style.fg, Some(SHIMMER));
        assert_eq!(pending.lines[0].spans[1].style.fg, Some(PRIMARY));
        let idle = styled_output_with_search("coxn: a\ncoxn: b", false, None);
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
    fn vim_question_mark_in_normal_opens_search_backward() {
        // M2 rebinding: `?` is backward transcript search (was help).
        // Help moved to `g?`.
        use crate::vim::Outcome;
        let mut view = View::new();
        // Enter Normal mode first.
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('?'));
        assert_eq!(out, Outcome::SearchBackward);
    }

    #[test]
    fn vim_g_question_mark_toggles_help() {
        // M2: help overlay moved to `g?` so `?` can become the backward
        // search prompt.
        use crate::vim::Outcome;
        let mut view = View::new();
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        view.vim.handle(&mut view.input, &mut view.cursor, k('g'));
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('?'));
        assert_eq!(out, Outcome::ToggleHelp);
    }

    #[test]
    fn vim_gr_in_normal_emits_aden_grep() {
        // M2: aden symbol-grep moved from bare `/` to `gr`.
        use crate::vim::Outcome;
        let mut view = View::new();
        view.input_push_str("foo_bar");
        view.cursor_home();
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        view.vim.handle(&mut view.input, &mut view.cursor, k('g'));
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('r'));
        assert!(matches!(out, Outcome::AdenGrep(s) if s.contains("foo")));
    }

    #[test]
    fn vim_slash_in_normal_opens_search_forward() {
        // M2: transcript forward search instead of aden-grep.
        use crate::vim::Outcome;
        let mut view = View::new();
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('/'));
        assert_eq!(out, Outcome::SearchForward);
    }

    #[test]
    fn vim_n_in_normal_cycles_search() {
        // M2: `n` cycles the active transcript search when there is one.
        use crate::vim::Outcome;
        let mut view = View::new();
        view.vim.handle(&mut view.input, &mut view.cursor, esc());
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('n'));
        assert_eq!(out, Outcome::SearchNext);
        let out = view.vim.handle(&mut view.input, &mut view.cursor, k('N'));
        assert_eq!(out, Outcome::SearchPrev);
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
        with_coxn_vim(true, || {
            let mut view = View::new();
            view.set_status("my-model");
            let mut terminal = Terminal::new(TestBackend::new(80, 6)).expect("test backend");
            terminal
                .draw(|frame| render(frame, &view))
                .expect("draw succeeds");
            let text = buffer_text(&terminal);
            assert!(text.contains("INSERT"), "INSERT tag must appear: {text:?}");
        });
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
    fn chat_first_mode_label_when_vim_off() {
        with_coxn_vim(false, || {
            assert_eq!(mode_status_label(Mode::Insert), "mode: CHAT ");
            assert!(mode_tip_text(Mode::Normal).contains("? help"));
        });
    }

    #[test]
    fn tool_card_lines_get_accent_gutter() {
        let text = styled_output_with_search("coxn:\n▸ edit src/foo.rs", false, None);
        let line = &text.lines[1];
        assert_eq!(line.spans[0].content, "▹ ");
        assert!(line.spans[1].content.contains("edit src/foo.rs"));
    }

    #[test]
    fn m6_mode_tip_text_per_mode() {
        with_coxn_vim(true, || {
            assert!(mode_tip_text(Mode::Insert).contains("Enter send"));
            assert!(mode_tip_text(Mode::Normal).contains("g? help"));
            assert!(mode_tip_text(Mode::Visual).contains("d/y"));
            assert!(mode_tip_text(Mode::Command).contains(":doctor"));
        });
    }

    #[test]
    fn m6_mode_tip_dismisses_after_idle() {
        let mut view = View::new();
        view.show_mode_tip();
        assert!(view.show_mode_tip);
        view.mode_tip_until = Some(Instant::now() - Duration::from_millis(1));
        view.refresh_mode_tip();
        assert!(!view.show_mode_tip);
    }

    #[test]
    fn m6_status_chips_render_in_order() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut view = View::new();
        view.set_status(
            "model: gpt-4  |  scope: task 'foo'  |  ctx: ~1.2k ctx  |  trust: supervised+scope",
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 6)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        let model_pos = text.find("model:").expect("model chip");
        let scope_pos = text.find("scope:").expect("scope chip");
        let ctx_pos = text.find("ctx:").expect("ctx chip");
        assert!(model_pos < scope_pos, "{text}");
        assert!(scope_pos < ctx_pos, "{text}");
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

    // -- M1: multi-line input + submit semantics --------------------------------

    #[test]
    fn m1_enter_submits_alt_enter_newlines() {
        // Bare Enter submits (no modifier).
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(map_input_key(enter), Some(Action::Submit));
        // Alt-Enter and Shift-Enter both insert a newline.
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(map_input_key(alt), Some(Action::Newline));
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(map_input_key(shift), Some(Action::Newline));
    }

    #[test]
    fn m1_enter_with_control_does_nothing() {
        // Ctrl-Enter is dropped: decoders cannot reliably distinguish it from
        // plain Enter; we refuse to promise a behaviour we can't deliver.
        let ctrl = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(map_input_key(ctrl), None);
        // CONTROL+ALT+ENTER drops as well (we don't double up).
        let ctrl_alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::CONTROL);
        assert_eq!(map_input_key(ctrl_alt), None);
    }

    #[test]
    fn m1_input_push_str_inserts_one_unit() {
        // Bracketed-paste payload lands as one bulk insert; cursor advances
        // past the inserted text and stays on a char boundary.
        let mut view = View::new();
        view.input_push_str("foo\nbar");
        assert_eq!(&view.input, "foo\nbar");
        assert_eq!(view.cursor, "foo\nbar".len()); // boundary-checked
        assert_eq!(view.input_line_count(), 2);

        // Mid-buffer insert keeps the cursor on a char boundary AFTER the
        // inserted text (e.g. yank-in-place semantics).
        let mut view2 = View::new();
        view2.input_push_str("a日");
        view2.cursor_left(); // land on '日' boundary
        view2.input_push_str("\nx");
        assert_eq!(&view2.input, "a\nx日");
        assert_eq!(&view2.input[view2.cursor..], "日");
    }

    #[test]
    fn m1_input_push_str_empty_is_noop() {
        let mut view = View::new();
        view.input_push_str("");
        assert_eq!(&view.input, "");
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn m1_input_line_count_counts_logical_lines() {
        let mut view = View::new();
        assert_eq!(view.input_line_count(), 1); // empty = one logical line

        view.input_push_str("only line");
        assert_eq!(view.input_line_count(), 1);

        view.input_push('\n');
        view.input_push_str("second");
        assert_eq!(view.input_line_count(), 2);

        view.input_push('\n');
        view.input_push_str("third");
        assert_eq!(view.input_line_count(), 3);
    }

    #[test]
    fn m1_render_grows_input_box_for_multiline() {
        // Three logical lines => the input widget occupies 3 rows in the
        // vertical layout, eating from the pane above and not crawling across
        // the status or separator. Verified via the rendered buffer's height
        // for the input row block (last N rows of the layout, given the
        // ordering pane > separator > status > input).
        let mut view = View::new();
        view.input_push_str("line one\nline two\nline three");

        // Build wide+modest-high and assert the third-from-bottom row carries
        // the continuation marker `↳` for line two (rows 1 and 2 of the multi-
        // line prompt). The presence of `↳` proves the box grew past 1 row.
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("↳"),
            "multi-line input must render a continuation marker: {text:?}"
        );
        assert!(
            text.contains("line three"),
            "last sub-line must render: {text:?}"
        );
    }

    #[test]
    fn m1_render_caps_input_box_at_max_rows() {
        // 50 logical lines would overflow the screen; the layout cap (8 rows)
        // means the input widget stays bounded. Hard-to-assert counter, so we
        // verify zero panics on a very long draft -- the proof is the
        // `render` not panicking plus the pane (row 0) still rendering.
        let mut view = View::new();
        for _ in 0..50 {
            view.input_push_str("x");
            view.input_push('\n');
        }
        // Cursor at the very bottom; render must not panic.
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds on capped input");
    }

    /// Adversarial paste (R-equivalent): a paste containing the literal bytes
    /// `Escape\ni`. Reaching the vim engine at the key level would flip Normal
    /// mode and `i` would re-enter Insert; instead `Event::Paste` is applied
    /// as a single bulk insert with ZERO mode change. Tested at the
    /// `View::input_push_str` primitive level here; the wiring (main.rs drive)
    /// tests assert the Event::Paste dispatch itself.
    #[test]
    fn m1_adversarial_paste_does_not_trip_per_key_motions() {
        let mut view = View::new();
        // Paste containing bytes that look like key sequences.
        view.input_push_str("foo\x1b\nibar");
        // The whole payload lives verbatim in the buffer (no key action ate
        // the Escape; no mode entered; nothing stripped). The `i` is part of
        // the buffer text, not a mode keystroke.
        assert_eq!(&view.input, "foo\x1b\nibar");
        assert_eq!(view.cursor, "foo\x1b\nibar".len());
    }

    /// Drive-loop integration: ensure the vim path is bypassed for newline
    /// keys. This is tested at the wiring layer (`Outcome::Pass` synthesized
    /// for Alt/Shift Enter); the pure path here asserts the buffer ends up
    /// with the newline, not submits. Equivalent to assert that `Outcome::Pass`
    /// synthesized does indeed land `\n` in the buffer.
    #[test]
    fn m1_newline_inserts_into_existing_buffer_in_place() {
        let mut view = View::new();
        view.input_push_str("hello");
        view.cursor_home(); // cursor at 'h'
        // Mimic the dispatch arm: Newline action => input_push('\n').
        view.input_push('\n');
        assert_eq!(&view.input, "\nhello");
        // Multi-line render must not panic for an embedded \n at the start.
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).expect("test backend");
        terminal.draw(|frame| render(frame, &view)).expect("draw");
    }

    // -- M2: transcript search -------------------------------------------------

    #[test]
    fn m2_search_open_is_editing_empty() {
        let mut view = View::new();
        view.search_open(false);
        assert!(
            view.search_editing(),
            "freshly opened search is in edit mode"
        );
        assert_eq!(
            view.search.as_ref().unwrap().query,
            "",
            "query starts empty"
        );
        assert!(
            view.search.as_ref().unwrap().matches.is_empty(),
            "empty query matches nothing"
        );
    }

    #[test]
    fn m2_search_push_rescords_matches_live() {
        let mut view = View::new();
        view.push("you: hello world\ncoxn: hello there\nyou: bye\n");
        view.search_open(false);
        // Type 'h' -- should match the two lines containing 'h'.
        view.search_push('h');
        let st = view.search.as_ref().unwrap();
        assert_eq!(st.matches.len(), 2, "h matches two lines");
        assert!(st.matches.contains(&0), "line 0 matches: {:?}", st.matches);
        assert!(st.matches.contains(&1));
    }

    #[test]
    fn m2_search_commit_pins_to_first_match() {
        let mut view = View::new();
        view.push("you: foo\ncoxn: foo bar\nyou: baz\n");
        view.search_open(false);
        view.search_push('f');
        view.search_push('o');
        view.search_push('o');
        view.search_commit();
        let st = view.search.as_ref().unwrap();
        assert!(!st.query_open, "commit drops the edit bit");
        assert_eq!(st.current, 0, "current indexes match 0");
        assert_eq!(view.search_match_line(), Some(0), "match line is line 0");
    }

    #[test]
    fn m2_search_step_cycles_wraps() {
        let mut view = View::new();
        view.push("you: foo\ncoxn: foo bar\nyou: baz foo\n");
        view.search_open(false);
        view.search_push('f');
        view.search_push('o');
        view.search_push('o');
        view.search_commit();
        // Three matches: lines 0/1/2 (all contain 'foo'). Stepping through
        // them wraps 0 -> 1 -> 2 -> 0.
        let got = |v: &View| v.search_match_line();
        assert_eq!(got(&view), Some(0));
        view.search_step(1);
        assert_eq!(got(&view), Some(1));
        view.search_step(1);
        assert_eq!(got(&view), Some(2));
        view.search_step(1);
        assert_eq!(got(&view), Some(0), "wrap-around afterlast");
    }

    #[test]
    fn m2_search_backward_flips_n_direction() {
        let mut view = View::new();
        view.push("you: foo\ncoxn: foo bar\nyou: baz foo\n");
        view.search_open(true); // ? -> backward
        view.search_push('f');
        view.search_push('o');
        view.search_push('o');
        view.search_commit();
        let got = |v: &View| v.search_match_line();
        assert_eq!(got(&view), Some(0));
        // `n` in backward retreats; under our model that means stepping -1
        // against advancing match list (wraps to last).
        view.search_step(1); // n under backward == retreat
        assert_eq!(
            got(&view),
            Some(2),
            "n under backward retreats/wraps to last"
        );
    }

    #[test]
    fn m2_search_cancel_drops_state() {
        let mut view = View::new();
        view.push("you: foo\n");
        view.search_open(false);
        view.search_push('f');
        view.search_cancel();
        assert!(view.search.is_none());
        assert!(!view.search_editing());
    }

    #[test]
    fn m2_search_backspace_edits_query() {
        let mut view = View::new();
        view.push("you: foo\ncoxn: foo bar\n");
        view.search_open(false);
        view.search_push('f');
        view.search_push('o');
        view.search_push('o');
        // Backspace twice cuts 'foo' to 'f'.
        view.search_backspace();
        view.search_backspace();
        let st = view.search.as_ref().unwrap();
        assert_eq!(st.query, "f");
    }

    #[test]
    fn m2_render_paints_search_prompt_when_editing() {
        // The search-prompt bar replaces the input row while editing.
        let mut view = View::new();
        view.push("you: hello world\n");
        view.search_open(false);
        view.search_push('h');
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        // The 1 match counter and the live query 'h' should both be present
        // in the rendered text.
        let text = buffer_text(&terminal).replace('\u{1b}', "");
        assert!(text.contains("/ h"), "live query 'h' shown: {text:?}");
        assert!(text.contains("match"), "match counter: {text:?}");
    }

    #[test]
    fn m2_render_tints_match_lines_when_committed() {
        // A committed search highlights matched transcript lines. We assert
        // the highlighted signature via the render buffer's reverse-video
        // cells on a match-bearing line. The clearest signal is the count
        // of cells with REVERSED modifier on the row of the matched line.
        let mut view = View::new();
        view.push("you: foo\ncoxn: bar\n");
        view.search_open(false);
        view.search_push('f');
        view.search_push('o');
        view.search_push('o');
        view.search_commit();
        // 'foo' appears once (line 0); render must show it highlighted.
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        // Walk row 0 of the output pane area and assert at least one cell
        // receives REVERSED. The output pane area starts at the top of the
        // frame; row 0 corresponds to line 0 of the output ("you: foo").
        let buffer = terminal.backend().buffer();
        let reversed_in_top: usize = (0..40)
            .map(|x| &buffer[(x, 0)])
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .count();
        assert!(
            reversed_in_top > 0,
            "matched line must highlight with REVERSED cells, found {reversed_in_top}"
        );
    }

    // -- M3: diff hunk painting ------------------------------------------------

    #[test]
    fn m3_paint_diff_line_classifies_by_leading_column_only() {
        // Pure classifier -- no render needed.
        let add = diff_line_style("+fn foo() {");
        assert_eq!(add.fg, Some(DIFF_ADD));
        let del = diff_line_style("-let old = 5;");
        assert_eq!(del.fg, Some(DIFF_DEL));
        let head = diff_line_style("@@ -1,3 +1,4 @@");
        assert_eq!(head.fg, Some(CYAN));
        let ctx = diff_line_style(" fn bar() {}");
        assert_eq!(ctx.fg, None);
        // Adversarial: a deletion line whose body contains a literal '+'
        // (e.g. Rust arithmetic) must not be mis-tinted as an addition.
        let minus_with_plus = diff_line_style("-let z = x + 1;");
        assert_eq!(minus_with_plus.fg, Some(DIFF_DEL));
        // Hunk header with a trailing `-` must not classify as deletion.
        let head_with_dash = diff_line_style("@@ -3,1 +3,2 @@");
        assert_eq!(head_with_dash.fg, Some(CYAN));
    }

    #[test]
    fn m3_confirm_with_diff_paints_modal_diff_body() {
        let mut view = View::new();
        let diff = "@@ -1,1 +1,1 @@\n-old\n+new\n";
        view.confirm_with_diff("GATE BLOCKED: scope-escape", diff);
        assert!(view.modal.is_some());
        assert!(view.modal_diff.is_some());
        // The hint advertises the expand/collapse keys when a diff is attached.
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(text.contains("[e]"), "expand hint present: {text:?}");
        assert!(text.contains("[c]"), "collapse hint present: {text:?}");
        // Both deletion and addition lines render inside the modal area.
        assert!(text.contains("-old"), "deletion line in modal: {text:?}");
        assert!(text.contains("+new"), "addition line in modal: {text:?}");
    }

    #[test]
    fn m3_confirm_with_diff_collapses_long_hunks() {
        // > DIFF_PREVIEW_ROWS triggers the ".. +N more lines" row.
        let mut view = View::new();
        let mut diff = String::from("@@ -1,1 +1,16 @@\n");
        for i in 0..20 {
            diff.push_str(&format!("+line{i}\n"));
        }
        view.confirm_with_diff("GATE BLOCKED", diff);
        // Tall enough terminal to fit the FULL expanded diff (24+ rows), so
        // both the collapse-state and the expand-state hints render.
        let mut terminal = Terminal::new(TestBackend::new(60, 32)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            text.contains("more lines"),
            "long diff collapses with a hint: {text:?}"
        );
        // Expanding flips the hint.
        view.modal_diff_expanded = true;
        terminal.draw(|frame| render(frame, &view)).expect("redraw");
        let text2 = buffer_text(&terminal);
        assert!(
            text2.contains("collapse"),
            "expanded diff shows the collapse hint: {text2:?}"
        );
    }

    #[test]
    fn m3_confirm_with_empty_diff_degrades_to_plain_modal() {
        let mut view = View::new();
        view.confirm_with_diff("just a prompt", "");
        assert!(view.modal_diff.is_none());
        // No [e]/[c] hint when there is no diff body attached.
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        let text = buffer_text(&terminal);
        assert!(
            !text.contains("[e]"),
            "no expand hint when diff empty: {text:?}"
        );
    }

    #[test]
    fn m3_render_paints_transcript_diff_fences() {
        // Aden output and tool results often embed ``` ```diff ``` code
        // fences; the transcript renderer should paint those lines.
        let mut view = View::new();
        view.push("coxn: here's the change\n```diff\n@@ -1,1 +1,1 @@\n-a\n+b\n```\n");
        let mut terminal = Terminal::new(TestBackend::new(50, 12)).expect("test backend");
        terminal
            .draw(|frame| render(frame, &view))
            .expect("draw succeeds");
        // Walk the buffer by COLUMN (row_text byte indices would be wrong when
        // a wide glyph like the gutter rule `|` eats 3 bytes for 1 column).
        // Find the column where `-`/`+` sits, then assert the cell carries
        // the DIFF_DEL / DIFF_ADD fg.
        let buffer = terminal.backend().buffer();
        let total_width: u16 = 50;
        let mut found_del = false;
        let mut found_add = false;
        for y in 0..12u16 {
            for x in 0..total_width.saturating_sub(1) {
                let sym_dash = buffer[(x, y)].symbol() == "-";
                let sym_plus = buffer[(x, y)].symbol() == "+";
                let next_a = buffer[(x + 1, y)].symbol() == "a";
                let next_b = buffer[(x + 1, y)].symbol() == "b";
                let fg = buffer[(x, y)].style().fg;
                if sym_dash && next_a && fg == Some(DIFF_DEL) {
                    found_del = true;
                }
                if sym_plus && next_b && fg == Some(DIFF_ADD) {
                    found_add = true;
                }
            }
        }
        assert!(found_del, "deletion line tinted: diff in transcript");
        assert!(found_add, "addition line tinted: diff in transcript");
    }
}
