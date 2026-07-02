//! Vim-style modal editing for the input line, plus Normal-mode navigation of
//! the transcript ("ledger"). Pure logic over a `(text, cursor)` pair so it is
//! unit-testable headless; the TUI wires [`Vim::handle`] into its key loop.
//!
//! Design for friendliness: **Insert is the default mode**, so typing and the
//! existing emacs-style keys (Ctrl-A/E/K/W/Y, arrows) keep working untouched --
//! [`Vim::handle`] returns [`Outcome::Pass`] for anything it does not own. `Esc`
//! drops to Normal for motions/operators; `v` enters Visual. The cursor is a
//! byte offset into `text`, always on a char boundary (the invariant the host
//! already maintains).
//!
//! Scope note: this is the engine. Phase A.1 covers modes, the common motions,
//! `x/D/C/r`, the linewise operators (`dd/cc/yy`), `p/P`, Visual select +
//! `d/c/y`, and ledger scroll. Operator-with-motion (`dw`, `c$`, counts) are
//! fully implemented: see [`Vim`] for the count/pending-operator machinery.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The editing mode. `Insert` is the default so unmodified typing just works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
    Visual,
    /// Ex-style command line: `:` entered from Normal mode.
    Command,
}

impl Mode {
    /// A short uppercase tag for the status line (`-- NORMAL --` style).
    pub fn tag(self) -> &'static str {
        match self {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
            Mode::Visual => "VISUAL",
            Mode::Command => "COMMAND",
        }
    }
}

/// A transcript-navigation request produced by Normal/Visual mode (`j/k`, `gg`,
/// `G`, `Ctrl-d/u`). The host maps these onto its existing scroll actions, so
/// vim navigates the ledger without the engine knowing the pane geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    Top,
    Bottom,
}

/// What [`Vim::handle`] decided about a key. `Pass` means "not mine" -- the host
/// applies its normal handling (insert the char, run an emacs binding, submit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The key was consumed; the buffer and/or mode may have changed.
    Consumed,
    /// Not a binding in this mode; the host should handle the key itself.
    Pass,
    /// Submit the current line (Enter).
    Submit,
    /// Navigate the transcript by one step.
    Scroll(Scroll),
    /// Navigate the transcript by `n` steps (produced when a count precedes a
    /// scroll motion, e.g. `3j`). The host applies the scroll `n` times.
    ScrollN(Scroll, u32),
    /// The user typed a `:command` and pressed Enter. The string is trimmed.
    /// Mode is reset to Normal and `cmdline` is cleared before this is returned.
    Command(String),
    /// Toggle the help overlay. Traditionally `?` in Normal mode; M2 moves it
    /// to `g?` because `?` becomes the backward transcript-search prompt.
    ToggleHelp,
    /// Open the transcript search prompt forward (`/` in Normal mode). The
    /// host installs a search field; typed characters build the query, Enter
    /// commits and jumps to the first match, Esc cancels, `n`/`N` cycle.
    SearchForward,
    /// Open the transcript search prompt backward (`?` in Normal mode).
    /// Same host handling as [`Outcome::SearchForward`] with reversed cycle.
    SearchBackward,
    /// Jump to the next active search match (`n` in Normal mode while a
    /// search is already committed; no-op if no active search).
    SearchNext,
    /// Jump to the previous active search match (`N` in Normal mode while a
    /// search is already committed; no-op if no active search).
    SearchPrev,
    /// Vim-native aden symbol lookup from input-line word at cursor.
    /// e.g. K or gd on a symbol word while composing.
    AdenLookup(String),
    /// Assemble context (asm) for word at cursor (ga).
    AdenAsm(String),
    /// Impact / blast radius for word at cursor (gi).
    AdenImpact(String),
    /// Launch aden view for word at cursor (gv).
    AdenView(String),
    /// Fuzzy-ish search via aden grep on word at cursor (/).
    AdenGrep(String),
    /// Graph nav / communities (]).
    AdenCommunities,
}

/// Modal editor state layered over the host's `(text, cursor)`.
#[derive(Debug, Clone, Default)]
pub struct Vim {
    pub mode: Mode,
    /// The unnamed register: text yanked or deleted, reused by `p`/`P`.
    register: String,
    /// Whether the register holds a whole line (linewise yank/delete), which
    /// changes how `p` pastes -- kept for parity with vim even on one line.
    register_linewise: bool,
    /// A pending operator awaiting completion (`d` waiting for a second `d`
    /// or a motion key like `w`).
    pending: Option<char>,
    /// In Visual mode, the byte offset the selection was anchored at.
    anchor: Option<usize>,
    /// Set after `r`, awaiting the replacement character.
    awaiting_replace: bool,
    /// Accumulated count prefix (e.g. `3` in `3w` or `d3w`). `None` means no
    /// count has been started yet; `Some(n)` carries the value so far.
    count: Option<u32>,
    /// True once the first non-zero motion-count digit has been consumed after
    /// a pending operator.  Used to distinguish "subsequent motion digits"
    /// (extend the accumulator with `*10 + d`) from "first motion digit"
    /// (seed with `op_count * d`), and to allow `0` to extend a motion-count
    /// that is already in progress (e.g. `d10w`) without misparsing it as the
    /// LineStart motion.  Reset whenever the pending operator resolves or is
    /// cancelled.
    motion_count_started: bool,
    /// The command-line buffer while in [`Mode::Command`].
    /// The engine owns it so it survives across redraws; the host reads it for
    /// rendering and discards it after an [`Outcome::Command`] is returned.
    pub cmdline: String,
}

impl Vim {
    /// A fresh editor in Insert mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current Visual selection as a byte range `[start, end)`, if any.
    pub fn selection(&self, cursor: usize) -> Option<(usize, usize)> {
        // No selection exists outside Visual mode, even if a stale anchor
        // lingers (e.g. between a Visual submit and the host resetting state).
        if self.mode != Mode::Visual {
            return None;
        }
        let a = self.anchor?;
        // The selection is inclusive of the char under the cursor, vim-style.
        let (lo, hi) = if a <= cursor {
            (a, cursor)
        } else {
            (cursor, a)
        };
        Some((lo, hi))
    }

    /// Handle a key. Mutates `text`/`cursor` in place; returns what the host
    /// should do next. Pure aside from the `&mut` arguments.
    pub fn handle(&mut self, text: &mut String, cursor: &mut usize, key: KeyEvent) -> Outcome {
        // A pending `r<char>` replaces the char under the cursor with the next
        // printable key, in any non-insert mode.
        if self.awaiting_replace {
            self.awaiting_replace = false;
            if let KeyCode::Char(c) = key.code {
                self.replace_char(text, *cursor, c);
            }
            return Outcome::Consumed;
        }

        // Ctrl-C is the host's quit escape hatch and must reach it in every
        // mode. Insert already passes it through; this keeps Normal/Visual from
        // swallowing it via their catch-all arms.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Pass;
        }

        match self.mode {
            Mode::Insert => self.handle_insert(text, cursor, key),
            Mode::Normal => self.handle_normal(text, cursor, key),
            Mode::Visual => self.handle_visual(text, cursor, key),
            Mode::Command => self.handle_command(key),
        }
    }

    // -- Insert ----------------------------------------------------------

    fn handle_insert(&mut self, text: &str, cursor: &mut usize, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                // Leaving insert, vim pulls the cursor back one char (you land
                // on the char you just typed past), clamped at column 0.
                *cursor = prev_boundary(text, *cursor);
                self.mode = Mode::Normal;
                Outcome::Consumed
            }
            // Everything else (typing, emacs keys, Enter, arrows) is the host's.
            _ => Outcome::Pass,
        }
    }

    // -- Normal ----------------------------------------------------------

    fn handle_normal(&mut self, text: &mut String, cursor: &mut usize, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // --- Count accumulation -----------------------------------------
        // A leading digit run `[1-9][0-9]*` sets the repeat count.
        // `0` only contributes to the count when a count is already in progress;
        // a bare `0` (no pending count) is the start-of-line motion.
        //
        // Combined-count form: `2d3w` means `d(op_count × motion_count)w = 6`.
        // When an operator is already pending and the first motion digit arrives,
        // we seed the new accumulator with `op_count × digit` rather than
        // folding digit into the existing value (`2*10+3 = 23`, which is wrong).
        // Subsequent digits then extend that product normally, so `2d30w` gives
        // `2 × 30 = 60` correctly.  When there is no pending operator the
        // accumulator extends as usual.
        if let KeyCode::Char(d) = key.code
            && d.is_ascii_digit()
            && !ctrl
        {
            let digit = d as u32 - '0' as u32;
            // `0` with no count in progress → fall through to the motion arm
            // (bare `0` is the LineStart motion).
            // `0` with a pending operator but no motion-count started yet →
            // also fall through: `d0` means "delete to line start" in vim.
            // `0` is only a count digit when a count accumulation is already
            // in progress for the current scope (no pending op, or the motion
            // digit run has already begun after a pending op).
            if digit != 0
                || (self.count.is_some() && (self.pending.is_none() || self.motion_count_started))
            {
                // When an operator is pending and the motion-count has NOT yet
                // started, this is the first motion digit: seed the accumulator
                // with `op_count × digit` so that `2d3w` means 6 repetitions.
                // After that first digit `motion_count_started` is true and
                // subsequent digits extend the accumulator the normal way
                // (`current × 10 + digit`), so `d10w` accumulates correctly.
                let new_count = if self.pending.is_some() && !self.motion_count_started {
                    self.motion_count_started = true;
                    self.count.unwrap_or(1).saturating_mul(digit)
                } else {
                    self.count
                        .unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit)
                };
                self.count = Some(new_count);
                return Outcome::Consumed;
            }
        }

        // Take the accumulated count (defaulting to 1) so every path below
        // sees a clean `n`. We consume it here so partial-operator paths that
        // need to re-inspect it (d3w) can read `n` from the local variable.
        let n = self.count.take().unwrap_or(1);

        // --- Operator-pending -------------------------------------------
        // An operator (`d/c/y/g`) was queued; now resolve it with the next key.
        // Ctrl-modified keys (e.g. Ctrl-d, Ctrl-u) are not motion chars; cancel
        // the pending operator and let the key fall through to normal dispatch
        // so scrolling still works.
        if self.pending.is_some() && ctrl {
            self.pending = None;
            self.motion_count_started = false;
            // Fall through to the normal key dispatch below.
        } else if let Some(op) = self.pending.take() {
            self.motion_count_started = false;
            return match (op, key.code) {
                // `gg` -> top of the ledger.
                ('g', KeyCode::Char('g')) => Outcome::Scroll(Scroll::Top),

                // `g?` -> toggle help overlay (M2 rebinding: `?` now opens
                // backward transcript search, so help moves under the `g`
                // prefix that the existing `gg`/`gd`/`ga`/`gi`/`gv` family
                // already uses).
                ('g', KeyCode::Char('?')) => Outcome::ToggleHelp,
                // `gr` -> aden grep on word at cursor. Aden symbol-grep was on
                // bare `/` in Normal mode and moved here so `/` can become
                // the transcript-search prompt (vim-native / behavior).
                ('g', KeyCode::Char('r')) => {
                    let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                    Outcome::AdenGrep(sym)
                }

                // `gd` -> aden understand/locate on word at cursor (vim go-to-def style).
                ('g', KeyCode::Char('d')) => {
                    let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                    Outcome::AdenLookup(sym)
                }
                // `ga` -> aden asm (assemble) on word at cursor.
                ('g', KeyCode::Char('a')) => {
                    let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                    Outcome::AdenAsm(sym)
                }
                // `gi` -> aden impact / blast radius on word at cursor.
                ('g', KeyCode::Char('i')) => {
                    let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                    Outcome::AdenImpact(sym)
                }
                // `gv` -> aden view launch on word at cursor.
                ('g', KeyCode::Char('v')) => {
                    let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                    Outcome::AdenView(sym)
                }

                // A doubled operator (dd/cc/yy) acts linewise; count repeats
                // the effect but since there is only one line each repeat is
                // idempotent for d/c (we still honour the count for symmetry).
                (o, KeyCode::Char(c)) if c == o && matches!(o, 'd' | 'c' | 'y') => {
                    for _ in 0..n {
                        self.linewise(text, cursor, o);
                    }
                    Outcome::Consumed
                }

                // Operator + motion: operate over [cursor, motion_target) n times.
                (o, KeyCode::Char(m)) if matches!(o, 'd' | 'c' | 'y') => {
                    if let Some(motion) = motion_char(m) {
                        // Apply the motion `n` times to find the target offset.
                        let target = apply_motion_n(text, *cursor, motion, n);
                        self.apply_operator(text, cursor, o, *cursor, target);
                        Outcome::Consumed
                    } else {
                        // Unknown motion: cancel the operator (vim beeps).
                        Outcome::Consumed
                    }
                }

                // Anything else cancels the pending operator (vim-like).
                _ => Outcome::Consumed,
            };
        }

        // --- Normal key dispatch ----------------------------------------
        match key.code {
            KeyCode::Enter => Outcome::Submit,
            KeyCode::Esc => Outcome::Consumed,
            // `n` / `N` cycle the active transcript search (M2). No-op when no
            // search is committed: the host checks `view.search` state.
            KeyCode::Char('n') if !ctrl => Outcome::SearchNext,
            KeyCode::Char('N') if !ctrl => Outcome::SearchPrev,
            // Ledger navigation (may carry a count).
            KeyCode::Char('d') if ctrl => Outcome::Scroll(Scroll::HalfPageDown),
            KeyCode::Char('u') if ctrl => Outcome::Scroll(Scroll::HalfPageUp),
            KeyCode::Char('j') => {
                if n == 1 {
                    Outcome::Scroll(Scroll::LineDown)
                } else {
                    Outcome::ScrollN(Scroll::LineDown, n)
                }
            }
            KeyCode::Char('k') if !ctrl => {
                if n == 1 {
                    Outcome::Scroll(Scroll::LineUp)
                } else {
                    Outcome::ScrollN(Scroll::LineUp, n)
                }
            }
            KeyCode::Char('G') => Outcome::Scroll(Scroll::Bottom),
            KeyCode::Char('g') => {
                // `gg` -> top. A bare `g` waits for the second `g` via pending.
                // The count is stored back; it will be consumed when the `g`
                // motion resolves (gg ignores the count, so we just drop n).
                self.pending = Some('g');
                Outcome::Consumed
            }
            // Enter insert mode: i at cursor, a after it, I at start, A at end.
            KeyCode::Char('i') => {
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            KeyCode::Char('a') => {
                *cursor = next_boundary(text, *cursor);
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            KeyCode::Char('I') => {
                *cursor = 0;
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            KeyCode::Char('A') => {
                *cursor = text.len();
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            // Motions (with count).
            KeyCode::Char('h') | KeyCode::Left => {
                for _ in 0..n {
                    *cursor = prev_boundary(text, *cursor);
                }
                Outcome::Consumed
            }
            KeyCode::Char('l') | KeyCode::Right => {
                for _ in 0..n {
                    *cursor = clamp_normal(text, next_boundary(text, *cursor));
                }
                Outcome::Consumed
            }
            KeyCode::Char('0') => {
                // `0` with count == 1 is start-of-line; count is already reset.
                *cursor = 0;
                Outcome::Consumed
            }
            KeyCode::Char('$') => {
                *cursor = clamp_normal(text, text.len());
                Outcome::Consumed
            }
            KeyCode::Char('w') if !ctrl => {
                for _ in 0..n {
                    *cursor = clamp_normal(text, word_forward(text, *cursor));
                }
                Outcome::Consumed
            }
            KeyCode::Char('b') => {
                for _ in 0..n {
                    *cursor = word_back(text, *cursor);
                }
                Outcome::Consumed
            }
            KeyCode::Char('e') => {
                for _ in 0..n {
                    *cursor = clamp_normal(text, word_end(text, *cursor));
                }
                Outcome::Consumed
            }
            // Edits.
            KeyCode::Char('x') => {
                for _ in 0..n {
                    self.delete_under(text, cursor);
                }
                Outcome::Consumed
            }
            KeyCode::Char('D') => {
                self.delete_to_end(text, *cursor);
                *cursor = clamp_normal(text, *cursor);
                Outcome::Consumed
            }
            KeyCode::Char('C') => {
                self.delete_to_end(text, *cursor);
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            KeyCode::Char('r') => {
                self.awaiting_replace = true;
                Outcome::Consumed
            }
            KeyCode::Char('p') if !ctrl => {
                self.paste(text, cursor, true);
                Outcome::Consumed
            }
            KeyCode::Char('P') => {
                self.paste(text, cursor, false);
                Outcome::Consumed
            }
            KeyCode::Char('v') => {
                self.mode = Mode::Visual;
                self.anchor = Some(*cursor);
                Outcome::Consumed
            }
            KeyCode::Char(':') => {
                self.cmdline.clear();
                self.mode = Mode::Command;
                Outcome::Consumed
            }
            // Operators awaiting a motion or repeat (dd/cc/yy or dw/cw/...).
            // The count (`n`) has already been consumed above; store it back so
            // the pending-resolution arm can see it (e.g. `d3w`).
            KeyCode::Char(c @ ('d' | 'c' | 'y')) if !ctrl => {
                self.pending = Some(c);
                // Push count back so the resolution arm sees it.
                if n > 1 {
                    self.count = Some(n);
                }
                Outcome::Consumed
            }
            // Pass host-only Ctrl keys (history nav, emacs bindings) through so
            // the map_input_key path can handle them even while in Normal mode.
            // Ctrl-p/n: history; Ctrl-d/u already matched above as scroll.
            // Ctrl-k/w/y: emacs kill-to-end, word-delete, yank — must reach
            // the Insert-mode handler even when Normal mode is active.
            KeyCode::Char('p' | 'n' | 'k' | 'w' | 'y') if ctrl => Outcome::Pass,
            // Arrow scrolling and page keys: not vim Normal-mode bindings; the
            // host handles them via map_input_key (ScrollUp/Down, PageUp/Down).
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => Outcome::Pass,
            // `/` opens the transcript-search prompt forward (vim-native).
            // Aden symbol-grep moved to `gr` (above); Insert-mode `/foo`
            // slash commands are unaffected (Insert mode Passes this key).
            KeyCode::Char('/') if !ctrl => Outcome::SearchForward,
            // `?` opens transcript search backward (vim-native). The
            // help overlay moved to `g?` because `?` is taken.
            KeyCode::Char('?') if !ctrl => Outcome::SearchBackward,
            // Vim-native aden lookups (K = lookup like man/K in vim; gd like go-def).
            // Extracts word at cursor from the (chat) input line and signals host.
            KeyCode::Char('K') if !ctrl => {
                let sym = word_at_cursor(text, *cursor).unwrap_or_default();
                Outcome::AdenLookup(sym)
            }
            // ] : simple graph nav flavor - aden communities (or impact context).
            KeyCode::Char(']') if !ctrl => Outcome::AdenCommunities,
            // gd : g sets pending, d here resolves to lookup.
            // Handled in the pending g arm above for gg; extend below in caller if needed.
            // Swallow unknown keys in Normal mode (vim beeps; we ignore).
            _ => Outcome::Consumed,
        }
    }

    // -- Visual ----------------------------------------------------------

    fn handle_visual(&mut self, text: &mut String, cursor: &mut usize, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.anchor = None;
                self.mode = Mode::Normal;
                Outcome::Consumed
            }
            // Motions extend the selection (anchor stays put).
            KeyCode::Char('h') | KeyCode::Left => {
                *cursor = prev_boundary(text, *cursor);
                Outcome::Consumed
            }
            KeyCode::Char('l') | KeyCode::Right => {
                *cursor = clamp_normal(text, next_boundary(text, *cursor));
                Outcome::Consumed
            }
            KeyCode::Char('0') => {
                *cursor = 0;
                Outcome::Consumed
            }
            KeyCode::Char('$') => {
                *cursor = clamp_normal(text, text.len());
                Outcome::Consumed
            }
            // Pass host-only Ctrl keys through even in Visual mode (same as
            // Normal). These must be checked BEFORE the unguarded 'w'/'y'/'k'
            // arms so the modifiers guard correctly partitions the dispatch.
            // Ctrl-p/n: history; Ctrl-k: kill-to-end; Ctrl-w: word-delete;
            // Ctrl-y: yank (emacs ring) — must reach the host handler.
            KeyCode::Char('p' | 'n' | 'k' | 'w' | 'y')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Outcome::Pass
            }
            // Motions extend the selection (anchor stays put).
            KeyCode::Char('w') => {
                *cursor = clamp_normal(text, word_forward(text, *cursor));
                Outcome::Consumed
            }
            KeyCode::Char('b') => {
                *cursor = word_back(text, *cursor);
                Outcome::Consumed
            }
            KeyCode::Char('e') => {
                *cursor = clamp_normal(text, word_end(text, *cursor));
                Outcome::Consumed
            }
            // Operate on the selection, then return to Normal.
            KeyCode::Char('y') => {
                self.yank_selection(text, *cursor);
                self.exit_visual_to(cursor);
                Outcome::Consumed
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.delete_selection(text, cursor);
                Outcome::Consumed
            }
            KeyCode::Char('c') => {
                self.delete_selection(text, cursor);
                self.mode = Mode::Insert;
                Outcome::Consumed
            }
            KeyCode::Enter => {
                // Submitting ends the selection; clear modal state so a stale
                // anchor can't outlive the buffer it pointed into.
                self.anchor = None;
                self.mode = Mode::Normal;
                Outcome::Submit
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => Outcome::Pass,
            _ => Outcome::Consumed,
        }
    }

    // -- Command ---------------------------------------------------------

    fn handle_command(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.cmdline.clear();
                self.mode = Mode::Normal;
                Outcome::Consumed
            }
            KeyCode::Enter => {
                let cmd = self.cmdline.trim().to_string();
                self.cmdline.clear();
                self.mode = Mode::Normal;
                Outcome::Command(cmd)
            }
            KeyCode::Backspace => {
                // Pop the last UTF-8 character from cmdline.
                if let Some(last) = self.cmdline.chars().next_back() {
                    let new_len = self.cmdline.len() - last.len_utf8();
                    self.cmdline.truncate(new_len);
                }
                Outcome::Consumed
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cmdline.push(c);
                Outcome::Consumed
            }
            _ => Outcome::Consumed,
        }
    }

    // -- Helpers ---------------------------------------------------------

    fn replace_char(&mut self, text: &mut String, cursor: usize, c: char) {
        if let Some(ch) = char_at(text, cursor) {
            let end = cursor + ch.len_utf8();
            text.replace_range(cursor..end, &c.to_string());
        }
    }

    fn delete_under(&mut self, text: &mut String, cursor: &mut usize) {
        if let Some(ch) = char_at(text, *cursor) {
            let end = *cursor + ch.len_utf8();
            self.register = text[*cursor..end].to_string();
            self.register_linewise = false;
            text.replace_range(*cursor..end, "");
            *cursor = clamp_normal(text, *cursor);
        }
    }

    fn delete_to_end(&mut self, text: &mut String, cursor: usize) {
        if cursor < text.len() {
            self.register = text[cursor..].to_string();
            self.register_linewise = false;
            text.truncate(cursor);
        }
    }

    fn linewise(&mut self, text: &mut String, cursor: &mut usize, op: char) {
        // One input line, so "linewise" acts on the whole buffer.
        self.register = text.clone();
        self.register_linewise = true;
        match op {
            'y' => {} // yank leaves the line intact
            _ => {
                text.clear();
                *cursor = 0;
                if op == 'c' {
                    self.mode = Mode::Insert;
                }
            }
        }
    }

    /// Apply operator `op` over the byte range `[cursor_pos, target)` (order is
    /// normalised so backward motions work). `cursor` is updated to the start of
    /// the affected range after the operation.
    fn apply_operator(
        &mut self,
        text: &mut String,
        cursor: &mut usize,
        op: char,
        cursor_pos: usize,
        target: usize,
    ) {
        // Determine the byte range; a backward motion (db, dh) has target < cursor.
        let lo = cursor_pos.min(target);
        let hi = cursor_pos.max(target).min(text.len());

        if lo == hi {
            // Nothing to operate on (cursor at the boundary already).
            return;
        }

        // Ensure both ends are on char boundaries (they should be — each came
        // from a motion function — but clamp defensively).
        let lo = snap_boundary_down(text, lo);
        let hi = snap_boundary_up(text, hi).min(text.len());

        match op {
            'y' => {
                self.register = text[lo..hi].to_string();
                self.register_linewise = false;
                // Yank: cursor moves to lo, text unchanged.
                *cursor = clamp_normal(text, lo);
            }
            'd' => {
                self.register = text[lo..hi].to_string();
                self.register_linewise = false;
                text.replace_range(lo..hi, "");
                *cursor = clamp_normal(text, lo);
            }
            'c' => {
                self.register = text[lo..hi].to_string();
                self.register_linewise = false;
                text.replace_range(lo..hi, "");
                *cursor = lo.min(text.len());
                self.mode = Mode::Insert;
            }
            _ => {}
        }
    }

    fn paste(&mut self, text: &mut String, cursor: &mut usize, after: bool) {
        if self.register.is_empty() {
            return;
        }
        let at = if after && !text.is_empty() {
            next_boundary(text, *cursor)
        } else {
            *cursor
        };
        let reg = self.register.clone();
        text.insert_str(at, &reg);
        // Land on the last char of the pasted text, vim-style.
        // `at + reg.len()` is one past the inserted region; walk back one char
        // boundary so we sit *on* the last char even when it is multi-byte UTF-8.
        let end = (at + reg.len()).min(text.len());
        *cursor = clamp_normal(text, prev_boundary(text, end));
    }

    fn yank_selection(&mut self, text: &str, cursor: usize) {
        if let Some((lo, hi)) = self.selection(cursor) {
            let end = next_boundary(text, hi).min(text.len());
            self.register = text[lo..end].to_string();
            self.register_linewise = false;
        }
    }

    fn delete_selection(&mut self, text: &mut String, cursor: &mut usize) {
        if let Some((lo, hi)) = self.selection(*cursor) {
            let end = next_boundary(text, hi).min(text.len());
            self.register = text[lo..end].to_string();
            self.register_linewise = false;
            text.replace_range(lo..end, "");
            *cursor = clamp_normal(text, lo);
        }
        self.anchor = None;
        if self.mode == Mode::Visual {
            self.mode = Mode::Normal;
        }
    }

    fn exit_visual_to(&mut self, cursor: &mut usize) {
        if let Some((lo, _)) = self.selection(*cursor) {
            *cursor = lo;
        }
        self.anchor = None;
        self.mode = Mode::Normal;
    }
}

// -- Motion helpers --------------------------------------------------------

/// The motions that can follow an operator (`d`/`c`/`y`).
#[derive(Clone, Copy)]
enum Motion {
    Left,
    Right,
    WordForward,
    WordEnd,
    WordBack,
    LineStart,
    LineEnd,
}

/// Map a motion character to its [`Motion`] variant, returning `None` for
/// characters that are not recognised motions.
fn motion_char(c: char) -> Option<Motion> {
    match c {
        'h' => Some(Motion::Left),
        'l' => Some(Motion::Right),
        'w' => Some(Motion::WordForward),
        'e' => Some(Motion::WordEnd),
        'b' => Some(Motion::WordBack),
        '0' => Some(Motion::LineStart),
        '$' => Some(Motion::LineEnd),
        _ => None,
    }
}

/// Apply `motion` once to `cursor` in `text`, returning the new offset.
fn apply_motion_once(text: &str, cursor: usize, motion: Motion) -> usize {
    match motion {
        Motion::Left => prev_boundary(text, cursor),
        Motion::Right => clamp_normal(text, next_boundary(text, cursor)),
        Motion::WordForward => word_forward(text, cursor),
        Motion::WordEnd => {
            // `e` for operator includes the char under the ending cursor, so we
            // advance one past the end-of-word position.
            let end = word_end(text, cursor);
            next_boundary(text, end).min(text.len())
        }
        Motion::WordBack => word_back(text, cursor),
        Motion::LineStart => 0,
        Motion::LineEnd => text.len(),
    }
}

/// Apply `motion` `n` times to `cursor`, returning the final offset.
fn apply_motion_n(text: &str, cursor: usize, motion: Motion, n: u32) -> usize {
    let mut pos = cursor;
    for _ in 0..n {
        pos = apply_motion_once(text, pos, motion);
    }
    pos
}

// -- Char/word navigation (byte offsets, UTF-8 safe) ----------------------

fn prev_boundary(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |c| cursor + c.len_utf8())
}

fn char_at(text: &str, cursor: usize) -> Option<char> {
    text[cursor..].chars().next()
}

/// In Normal/Visual the cursor rests *on* a char, so it cannot sit past the last
/// char (unlike Insert, where it may sit at `text.len()` to append).
fn clamp_normal(text: &str, cursor: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    let last = prev_boundary(text, text.len());
    cursor.min(last)
}

/// Walk backward from `pos` to find the nearest char boundary <= `pos`.
pub(crate) fn snap_boundary_down(text: &str, pos: usize) -> usize {
    let mut i = pos.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Walk forward from `pos` to find the nearest char boundary >= `pos`.
fn snap_boundary_up(text: &str, pos: usize) -> usize {
    let mut i = pos.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[derive(PartialEq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// `w`: start of the next word (Word or Punct run) after the cursor.
fn word_forward(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    let start_class = char_at(text, i).map(class);
    // Move off the current run.
    if let Some(sc) = start_class {
        while let Some(c) = char_at(text, i) {
            if class(c) != sc {
                break;
            }
            i = next_boundary(text, i);
        }
    }
    // Skip whitespace to the next run's first char.
    while let Some(c) = char_at(text, i) {
        if class(c) != Class::Space {
            break;
        }
        i = next_boundary(text, i);
    }
    i
}

/// `b`: start of the word at or before the cursor.
fn word_back(text: &str, cursor: usize) -> usize {
    let mut i = cursor;
    // Step back one, then over any whitespace.
    i = prev_boundary(text, i);
    while i > 0 {
        if char_at(text, i).map(class) != Some(Class::Space) {
            break;
        }
        i = prev_boundary(text, i);
    }
    // Back to the start of this run.
    let run = char_at(text, i).map(class);
    while i > 0 {
        let p = prev_boundary(text, i);
        if char_at(text, p).map(class) != run {
            break;
        }
        i = p;
    }
    i
}

/// `e`: end of the next word from the cursor.
fn word_end(text: &str, cursor: usize) -> usize {
    let mut i = next_boundary(text, cursor);
    // Skip whitespace.
    while let Some(c) = char_at(text, i) {
        if class(c) != Class::Space {
            break;
        }
        i = next_boundary(text, i);
    }
    // To the last char of this run.
    let run = char_at(text, i).map(class);
    loop {
        let n = next_boundary(text, i);
        if char_at(text, n).map(class) != run || n == i {
            break;
        }
        i = n;
    }
    i
}

/// Extract a "symbol" word around the cursor for aden lookups (K/gd).
/// Uses the same word classing as motions; returns None if no alphanum word.
pub(crate) fn word_at_cursor(text: &str, cursor: usize) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut start = cursor.min(text.len());
    // back up to start of current or prev word run
    while start > 0 {
        let p = prev_boundary(text, start);
        if let Some(c) = char_at(text, p)
            && !c.is_alphanumeric()
            && c != '_'
        {
            break;
        }
        start = p;
    }
    // now forward to end of the run
    let mut end = start;
    while let Some(c) = char_at(text, end) {
        if c.is_alphanumeric() || c == '_' {
            end = next_boundary(text, end);
        } else {
            break;
        }
    }
    if start < end {
        let w = &text[start..end];
        if w.chars().any(|c| c.is_alphanumeric()) {
            return Some(w.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    /// Drive a key sequence over a fresh editor; return (vim, text, cursor).
    fn run(text: &str, cursor: usize, keys: &[KeyEvent]) -> (Vim, String, usize) {
        let mut v = Vim::new();
        let mut t = text.to_string();
        let mut c = cursor;
        for &key in keys {
            v.handle(&mut t, &mut c, key);
        }
        (v, t, c)
    }

    #[test]
    fn insert_passes_typing_and_esc_enters_normal() {
        let mut v = Vim::new();
        let mut t = "hi".to_string();
        let mut c = 2;
        assert_eq!(v.handle(&mut t, &mut c, k('x')), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, esc()), Outcome::Consumed);
        assert_eq!(v.mode, Mode::Normal);
        // Esc pulled the cursor back onto the last char.
        assert_eq!(c, 1);
    }

    #[test]
    fn normal_motions_hl_0_dollar() {
        let (_, _, c) = run("hello", 0, &[esc(), k('l'), k('l')]);
        assert_eq!(c, 2);
        let (_, _, c) = run("hello", 0, &[esc(), k('$')]);
        assert_eq!(c, 4); // on 'o', not past it
        let (_, _, c) = run("hello", 0, &[esc(), k('$'), k('0')]);
        assert_eq!(c, 0);
        // h clamps at column 0.
        let (_, _, c) = run("hello", 0, &[esc(), k('h')]);
        assert_eq!(c, 0);
    }

    #[test]
    fn word_motions() {
        // "foo bar baz", cursor at 0.
        let (_, _, c) = run("foo bar baz", 0, &[esc(), k('w')]);
        assert_eq!(c, 4); // start of "bar"
        let (_, _, c) = run("foo bar baz", 0, &[esc(), k('w'), k('w')]);
        assert_eq!(c, 8); // start of "baz"
        let (_, _, c) = run("foo bar", 0, &[esc(), k('e')]);
        assert_eq!(c, 2); // end of "foo"
        let (_, _, c) = run("foo bar", 6, &[esc(), k('b')]);
        assert_eq!(c, 4); // back to start of "bar"
    }

    #[test]
    fn x_deletes_under_cursor() {
        let (v, t, c) = run("hello", 0, &[esc(), k('x')]);
        assert_eq!(t, "ello");
        assert_eq!(c, 0);
        // Deleted char goes to the register; p pastes it back after the cursor.
        let _ = v;
    }

    #[test]
    fn dd_clears_line_and_p_pastes() {
        // dd yanks the whole line and clears it.
        let (mut v, mut t, mut c) = run("hello world", 0, &[esc(), k('d'), k('d')]);
        assert_eq!(t, "");
        assert_eq!(c, 0);
        // Now type something, escape, and paste the yanked line after the cursor.
        v.handle(&mut t, &mut c, k('p'));
        assert_eq!(t, "hello world");
    }

    #[test]
    fn dw_via_visual_yank_and_paste() {
        // Visual select "foo", yank, move to end, paste.
        let (mut v, mut t, mut c) = run("foo bar", 0, &[esc(), k('v'), k('l'), k('l'), k('y')]);
        assert_eq!(v.mode, Mode::Normal);
        // register holds "foo"
        v.handle(&mut t, &mut c, k('$'));
        v.handle(&mut t, &mut c, k('p'));
        assert_eq!(t, "foo barfoo");
    }

    #[test]
    fn visual_delete_removes_selection() {
        let (_, t, c) = run("hello", 0, &[esc(), k('v'), k('l'), k('l'), k('d')]);
        assert_eq!(t, "lo"); // removed "hel"
        assert_eq!(c, 0);
    }

    #[test]
    fn r_replaces_char() {
        let (_, t, _) = run("cat", 0, &[esc(), k('r'), k('b')]);
        assert_eq!(t, "bat");
    }

    #[test]
    fn capital_d_deletes_to_end() {
        let (_, t, c) = run("hello", 0, &[esc(), k('l'), k('l'), k('D')]);
        assert_eq!(t, "he");
        assert_eq!(c, 1); // clamped onto last remaining char
    }

    #[test]
    fn a_appends_after_cursor() {
        let mut v = Vim::new();
        let mut t = "hi".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // normal, cursor clamps to 0
        v.handle(&mut t, &mut c, k('a')); // append after char 0
        assert_eq!(v.mode, Mode::Insert);
        assert_eq!(c, 1);
    }

    #[test]
    fn word_at_cursor_extracts_symbols() {
        assert_eq!(word_at_cursor("hello world", 0), Some("hello".to_string()));
        assert_eq!(word_at_cursor("hello world", 3), Some("hello".to_string()));
        assert_eq!(word_at_cursor("hello world", 6), Some("world".to_string()));
        assert_eq!(
            word_at_cursor("foo_bar baz", 4),
            Some("foo_bar".to_string())
        );
        assert_eq!(word_at_cursor("123 + abc", 6), Some("abc".to_string()));
        assert!(word_at_cursor("   ", 1).is_none());
    }

    #[test]
    fn enter_submits_in_normal_and_insert() {
        let mut v = Vim::new();
        let mut t = "go".to_string();
        let mut c = 2;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        // Insert mode passes Enter to the host (which submits).
        assert_eq!(v.handle(&mut t, &mut c, enter), Outcome::Pass);
        v.handle(&mut t, &mut c, esc());
        assert_eq!(v.handle(&mut t, &mut c, enter), Outcome::Submit);
    }

    #[test]
    fn k_gd_ga_emit_aden_ops() {
        let mut v = Vim::new();
        let mut t = "parse_config".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        let res_k = v.handle(&mut t, &mut c, k('K'));
        assert!(matches!(res_k, Outcome::AdenLookup(s) if s == "parse_config"));
        // gd
        let mut v2 = Vim::new();
        let mut t2 = "seed_foo".to_string();
        let mut c2 = 5;
        v2.handle(&mut t2, &mut c2, esc());
        v2.handle(&mut t2, &mut c2, k('g'));
        let res_gd = v2.handle(&mut t2, &mut c2, k('d'));
        assert!(matches!(res_gd, Outcome::AdenLookup(s) if s.contains("seed")));
        // ga
        let mut v3 = Vim::new();
        let mut t3 = "MyStruct".to_string();
        let mut c3 = 2;
        v3.handle(&mut t3, &mut c3, esc());
        v3.handle(&mut t3, &mut c3, k('g'));
        let res_ga = v3.handle(&mut t3, &mut c3, k('a'));
        assert!(matches!(res_ga, Outcome::AdenAsm(s) if s == "MyStruct"));
        // gi / gv
        let mut v4 = Vim::new();
        let mut t4 = "parse_config".to_string();
        let mut c4 = 0;
        v4.handle(&mut t4, &mut c4, esc());
        v4.handle(&mut t4, &mut c4, k('g'));
        let res_gi = v4.handle(&mut t4, &mut c4, k('i'));
        assert!(matches!(res_gi, Outcome::AdenImpact(s) if s.contains("parse")));
        v4.handle(&mut t4, &mut c4, k('g'));
        let res_gv = v4.handle(&mut t4, &mut c4, k('v'));
        assert!(matches!(res_gv, Outcome::AdenView(s) if s.contains("parse")));
        // /
        // M2: bare `/` in Normal mode opens transcript search forward.
        let mut v5 = Vim::new();
        let mut t5 = "foo_bar".to_string();
        let mut c5 = 0;
        v5.handle(&mut t5, &mut c5, esc());
        let res_slash = v5.handle(&mut t5, &mut c5, k('/'));
        assert_eq!(res_slash, Outcome::SearchForward);
        // `gr` is the new aden-grep binding.
        v5.handle(&mut t5, &mut c5, esc());
        v5.handle(&mut t5, &mut c5, k('g'));
        let res_gr = v5.handle(&mut t5, &mut c5, k('r'));
        assert!(matches!(res_gr, Outcome::AdenGrep(s) if s.contains("foo")));
        // `g?` toggles help (was bare `?`).
        v5.handle(&mut t5, &mut c5, esc());
        v5.handle(&mut t5, &mut c5, k('g'));
        let res_gq = v5.handle(&mut t5, &mut c5, k('?'));
        assert_eq!(res_gq, Outcome::ToggleHelp);
        // `?` opens backward search.
        v5.handle(&mut t5, &mut c5, esc());
        let res_q = v5.handle(&mut t5, &mut c5, k('?'));
        assert_eq!(res_q, Outcome::SearchBackward);
        // `n`/`N` cycle the active search (no-op without one at the engine
        // level; the host checks search state).
        let res_n = v5.handle(&mut t5, &mut c5, k('n'));
        assert_eq!(res_n, Outcome::SearchNext);
        let res_capn = v5.handle(&mut t5, &mut c5, k('N'));
        assert_eq!(res_capn, Outcome::SearchPrev);
        // ]
        let mut v6 = Vim::new();
        let mut t6 = String::new();
        let mut c6 = 0;
        v6.handle(&mut t6, &mut c6, esc());
        let res_rbracket = v6.handle(&mut t6, &mut c6, k(']'));
        assert!(matches!(res_rbracket, Outcome::AdenCommunities));
    }

    #[test]
    fn ledger_navigation_keys() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        assert_eq!(
            v.handle(&mut t, &mut c, k('j')),
            Outcome::Scroll(Scroll::LineDown)
        );
        assert_eq!(
            v.handle(&mut t, &mut c, k('k')),
            Outcome::Scroll(Scroll::LineUp)
        );
        assert_eq!(
            v.handle(&mut t, &mut c, ctrl('d')),
            Outcome::Scroll(Scroll::HalfPageDown)
        );
        assert_eq!(
            v.handle(&mut t, &mut c, k('G')),
            Outcome::Scroll(Scroll::Bottom)
        );
        // gg -> top (two keystrokes).
        v.handle(&mut t, &mut c, k('g'));
        assert_eq!(
            v.handle(&mut t, &mut c, k('g')),
            Outcome::Scroll(Scroll::Top)
        );
    }

    // ---- New tests: operator+motion ----------------------------------------

    #[test]
    fn dw_deletes_word_forward() {
        // "foo bar" at 0, dw removes "foo "
        let (_, t, c) = run("foo bar", 0, &[esc(), k('d'), k('w')]);
        assert_eq!(t, "bar");
        assert_eq!(c, 0);
    }

    #[test]
    fn de_deletes_to_end_of_word() {
        // "foo bar" at 0, de removes "foo" (up to and including the 'o')
        let (_, t, c) = run("foo bar", 0, &[esc(), k('d'), k('e')]);
        assert_eq!(t, " bar");
        assert_eq!(c, 0);
    }

    #[test]
    fn db_deletes_word_backward() {
        // "foo bar": esc from cursor=7 (past-end) lands on 'r' (index 6).
        // db from 'r' goes back to index 4 ('b'); deletes "ba" → "foo r".
        let (_, t, c) = run("foo bar", 7, &[esc(), k('d'), k('b')]);
        assert_eq!(t, "foo r");
        assert_eq!(c, 4);
    }

    #[test]
    fn d_dollar_deletes_to_end_of_line() {
        // "hello world" — esc from cursor=6 lands at 5 (' '), d$ deletes " world"
        let (_, t, c) = run("hello world", 6, &[esc(), k('d'), k('$')]);
        assert_eq!(t, "hello");
        // clamp_normal("hello", 5) → prev_boundary("hello", 5) = 4 ('o')
        assert_eq!(c, 4);
    }

    #[test]
    fn d_zero_deletes_to_line_start() {
        // "hello": esc from cursor=4 lands on 'l' (index 3).
        // d0: target=0; deletes bytes [0..3) = "hel" → "lo". cursor=0.
        let (_, t, c) = run("hello", 4, &[esc(), k('d'), k('0')]);
        assert_eq!(t, "lo");
        assert_eq!(c, 0);
    }

    #[test]
    fn dh_deletes_char_to_left() {
        // "hello": esc from cursor=3 lands on index 2 ('l').
        // dh: target = prev_boundary("hello", 2) = 1 ('e'). Deletes text[1..2]='e' → "hllo".
        let (_, t, c) = run("hello", 3, &[esc(), k('d'), k('h')]);
        assert_eq!(t, "hllo");
        assert_eq!(c, 1);
    }

    #[test]
    fn dl_deletes_char_under_cursor() {
        // "hello": esc from cursor=2 lands on index 1 ('e').
        // dl: Right motion → target = clamp_normal("hello", 2) = 2. Deletes text[1..2]='e' → "hllo".
        let (_, t, c) = run("hello", 2, &[esc(), k('d'), k('l')]);
        assert_eq!(t, "hllo");
        assert_eq!(c, 1);
    }

    #[test]
    fn cw_deletes_word_and_enters_insert() {
        // "foo bar" at 0, cw removes "foo " and enters Insert
        let (v, t, c) = run("foo bar", 0, &[esc(), k('c'), k('w')]);
        assert_eq!(t, "bar");
        assert_eq!(c, 0);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn c_dollar_deletes_to_end_and_enters_insert() {
        // "hello": esc from cursor=3 lands on 'l' (index 2).
        // c$: target=5 (text.len()). Deletes "llo" → "he". cursor=2 (=lo, clamped to text.len()=2). Insert.
        let (v, t, c) = run("hello", 3, &[esc(), k('c'), k('$')]);
        assert_eq!(t, "he");
        assert_eq!(c, 2);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn yw_yanks_word_forward() {
        // "foo bar" at 0, yw yanks "foo " into register, text unchanged
        let (v, t, c) = run("foo bar", 0, &[esc(), k('y'), k('w')]);
        assert_eq!(t, "foo bar");
        assert_eq!(v.register, "foo ");
        assert_eq!(c, 0); // cursor moves to lo (same as start)
    }

    #[test]
    fn yb_yanks_word_backward() {
        // "foo bar": esc from cursor=7 (past-end) lands on 'r' (index 6).
        // yb: word_back("foo bar", 6) = 4 ('b'). Range [4..6) = "ba" yanked.
        // Text unchanged.
        let (v, t, _c) = run("foo bar", 7, &[esc(), k('y'), k('b')]);
        assert_eq!(t, "foo bar");
        assert_eq!(v.register, "ba");
    }

    // ---- New tests: counts -------------------------------------------------

    #[test]
    fn count_2_dw_deletes_two_words() {
        // "one two three" at 0, 2dw removes "one two "
        let (_, t, c) = run("one two three", 0, &[esc(), k('2'), k('d'), k('w')]);
        assert_eq!(t, "three");
        assert_eq!(c, 0);
    }

    #[test]
    fn d_count_3_w_deletes_three_words() {
        // "a b c d" at 0, d3w removes "a b c "
        let (_, t, c) = run("a b c d", 0, &[esc(), k('d'), k('3'), k('w')]);
        assert_eq!(t, "d");
        assert_eq!(c, 0);
    }

    #[test]
    fn count_3_dd_linewise_clears_line() {
        // 3dd on "hello" still clears (only one line)
        let (_, t, c) = run("hello", 0, &[esc(), k('3'), k('d'), k('d')]);
        assert_eq!(t, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn count_3_j_produces_scroll_n() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        let out = [k('3'), k('j')]
            .iter()
            .map(|&key| v.handle(&mut t, &mut c, key))
            .last()
            .unwrap();
        assert_eq!(out, Outcome::ScrollN(Scroll::LineDown, 3));
    }

    #[test]
    fn count_5_l_moves_five_right() {
        // "abcdefgh" at 0, 5l moves to index 5
        let (_, _, c) = run("abcdefgh", 0, &[esc(), k('5'), k('l')]);
        assert_eq!(c, 5);
    }

    #[test]
    fn count_past_end_clamps() {
        // "hi" at 0, 99l clamps at last char index 1
        let (_, _, c) = run("hi", 0, &[esc(), k('9'), k('9'), k('l')]);
        assert_eq!(c, 1);
    }

    #[test]
    fn bare_zero_is_line_start_not_count() {
        // "hello" at 4, 0 goes to start (not count)
        let (_, _, c) = run("hello", 4, &[esc(), k('0')]);
        assert_eq!(c, 0);
    }

    #[test]
    fn count_with_zero_extension() {
        // "abcdefghij" at 0, pressing 1 then 0 gives count=10, then l moves 10
        // (but string only has 10 chars, indices 0-9, so clamps at 9)
        let (_, _, c) = run("abcdefghij", 0, &[esc(), k('1'), k('0'), k('l')]);
        assert_eq!(c, 9);
    }

    // ---- UTF-8 safety tests ------------------------------------------------

    #[test]
    fn dw_on_multibyte_text() {
        // "日本語 test" — word "日本語" is 9 bytes (3 bytes each)
        let text = "日本語 test";
        let (_, t, c) = run(text, 0, &[esc(), k('d'), k('w')]);
        assert_eq!(t, "test");
        assert_eq!(c, 0);
    }

    #[test]
    fn db_on_multibyte_text() {
        // "test 語": esc from past-end (cursor=8) lands on '語' (byte 5).
        // db skips the space and deletes the preceding word "test" → " 語"...
        // but actually also skips the space: word_back("test 語", 5) = 0.
        // So we delete bytes [0..5) = "test " and the result is "語".
        let text = "test 語";
        let (_, t, _c) = run(text, 8, &[esc(), k('d'), k('b')]);
        assert_eq!(t, "語");
    }

    #[test]
    fn operator_motion_at_line_start_dh_noop() {
        // dh at col 0 is a no-op (nothing to the left)
        let (_, t, c) = run("hello", 0, &[esc(), k('d'), k('h')]);
        assert_eq!(t, "hello");
        assert_eq!(c, 0);
    }

    #[test]
    fn operator_motion_at_line_end_dl_noop() {
        // "hi": esc from cursor=2 lands on index 1 ('i', the last char).
        // dl: Right → clamp_normal("hi", next_boundary("hi",1)) = clamp_normal("hi", 2) = 1.
        // apply_operator with cursor_pos=1, target=1 → lo=hi=1 → noop.
        let (_, t, c) = run("hi", 2, &[esc(), k('d'), k('l')]);
        assert_eq!(t, "hi");
        assert_eq!(c, 1);
    }

    #[test]
    fn c_motion_leaves_insert_mode() {
        // "foobar": esc from cursor=4 lands on 'b' (index 3).
        // cb goes back to word start (0), deletes "foo" → "bar"; cursor=0; Insert mode.
        let (v, t, c) = run("foobar", 4, &[esc(), k('c'), k('b')]);
        assert_eq!(t, "bar");
        assert_eq!(c, 0);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn yank_motion_does_not_change_text() {
        let (v, t, _c) = run("hello world", 0, &[esc(), k('y'), k('$')]);
        assert_eq!(t, "hello world");
        assert_eq!(v.register, "hello world");
    }

    #[test]
    fn dw_at_last_word_clears_to_end() {
        // "hello" at 0, dw removes "hello"
        let (_, t, c) = run("hello", 0, &[esc(), k('d'), k('w')]);
        assert_eq!(t, "");
        assert_eq!(c, 0);
    }

    // ---- Regression tests for review findings ------------------------------

    // Finding 1 (blocker): paste cursor lands on char boundary for multi-byte
    // registers.  Previously `at + reg.len() - 1` would put the cursor in the
    // interior of a multi-byte char; the corrected code walks back one full
    // char boundary.
    #[test]
    fn paste_multibyte_cursor_on_char_boundary() {
        // Yank "日" (3 bytes) then paste-after onto "x".
        // After paste the string is "x日"; cursor must land on '日' (byte 1),
        // not byte 3 (interior) or byte 2 (interior).
        let mut v = Vim::new();
        v.register = "日".to_string();
        let mut t = "x".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        // paste after: inserts at byte 1, region is [1..4). prev_boundary(4) = 1.
        v.handle(&mut t, &mut c, k('p'));
        assert_eq!(t, "x日");
        assert!(
            t.is_char_boundary(c),
            "cursor {c} is not on a char boundary"
        );
        assert_eq!(&t[c..], "日"); // landed on the pasted char
    }

    #[test]
    fn paste_multibyte_register_two_chars_cursor_on_last() {
        // Register = "ab日" (5 bytes). After paste-after on "x":
        // string "xab日", pasted region bytes 1..6, prev_boundary(6)=3 ('日').
        let mut v = Vim::new();
        v.register = "ab日".to_string();
        let mut t = "x".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k('p'));
        assert_eq!(t, "xab日");
        assert!(
            t.is_char_boundary(c),
            "cursor {c} is not on a char boundary"
        );
        assert_eq!(&t[c..], "日"); // on the last pasted char
    }

    // Finding 2 (major): j/k produce Scroll::LineDown/Up (single-step), not
    // a multiplied step. This test lives in vim.rs; the host side (step=1 vs
    // SCROLL_STEP) is exercised by tui integration tests.
    #[test]
    fn j_k_produce_single_line_scroll_outcome() {
        // Verify j produces LineDown and k produces LineUp, not any counted variant.
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        assert_eq!(
            v.handle(&mut t, &mut c, k('j')),
            Outcome::Scroll(Scroll::LineDown)
        );
        assert_eq!(
            v.handle(&mut t, &mut c, k('k')),
            Outcome::Scroll(Scroll::LineUp)
        );
    }

    // Finding 3 (major): ctrl-d/u while an operator is pending cancels the
    // operator and then performs the scroll (the key is NOT swallowed as a
    // motion attempt).
    #[test]
    fn ctrl_d_u_cancel_pending_operator_and_pass_scroll() {
        let mut v = Vim::new();
        let mut t = "hello".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());

        // Start a `d` operator, then press Ctrl-d.  The operator must be
        // cancelled (text unchanged) and the outcome must be the half-page
        // scroll — NOT Consumed with a silent cancel.
        v.handle(&mut t, &mut c, k('d')); // pending = Some('d')
        let out = v.handle(&mut t, &mut c, ctrl('d'));
        assert_eq!(
            t, "hello",
            "text must be unchanged after ctrl-d cancels operator"
        );
        assert_eq!(out, Outcome::Scroll(Scroll::HalfPageDown));

        // Same for Ctrl-u.
        v.handle(&mut t, &mut c, k('d')); // pending = Some('d')
        let out = v.handle(&mut t, &mut c, ctrl('u'));
        assert_eq!(t, "hello");
        assert_eq!(out, Outcome::Scroll(Scroll::HalfPageUp));
    }

    // Finding 4 (major): Ctrl-P/N pass through in Normal and Visual mode so
    // the host's history navigation is reachable even after pressing Esc.
    #[test]
    fn ctrl_p_n_pass_in_normal_mode() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // enter Normal
        assert_eq!(v.handle(&mut t, &mut c, ctrl('p')), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, ctrl('n')), Outcome::Pass);
    }

    #[test]
    fn ctrl_p_n_pass_in_visual_mode() {
        let mut v = Vim::new();
        let mut t = "hi".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // Normal
        v.handle(&mut t, &mut c, k('v')); // Visual
        assert_eq!(v.mode, Mode::Visual);
        assert_eq!(v.handle(&mut t, &mut c, ctrl('p')), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, ctrl('n')), Outcome::Pass);
    }

    // Finding 5 (major): Up/Down/PageUp/PageDown pass through in Normal and
    // Visual mode so transcript scrolling via arrow keys is not broken.
    #[test]
    fn arrow_and_page_keys_pass_in_normal_mode() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(v.handle(&mut t, &mut c, up), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, down), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, pgup), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, pgdn), Outcome::Pass);
    }

    // Finding 1 (major): After `d<Ctrl-d>` the count must be cleanly consumed;
    // a subsequent `dw` must delete exactly one word (no leaked count).
    #[test]
    fn count_not_leaked_after_ctrl_cancel() {
        let mut v = Vim::new();
        let mut t = "one two three".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // Normal mode

        // Press `d` (operator pending), then Ctrl-d (cancel + scroll).
        v.handle(&mut t, &mut c, k('d'));
        let out = v.handle(&mut t, &mut c, ctrl('d'));
        assert_eq!(out, Outcome::Scroll(Scroll::HalfPageDown));
        assert_eq!(t, "one two three", "text unchanged after ctrl-d cancels d");

        // Now `dw` — must delete exactly one word, not leave count debris.
        v.handle(&mut t, &mut c, k('d'));
        v.handle(&mut t, &mut c, k('w'));
        assert_eq!(
            t, "two three",
            "dw after cancel must delete exactly one word"
        );
    }

    // Finding 2 (major): `2d3w` must delete 6 words (real-vim semantics), not 23.
    // After the operator is pending, a digit resets (not extends) the count so
    // the motion count is 3 and the combined result is op_count × motion_count = 6.
    #[test]
    fn combined_count_2d3w_deletes_six_words() {
        // "a b c d e f g h" — 2d3w should remove 6 words: "a b c d e f "
        let (_, t, _) = run(
            "a b c d e f g h",
            0,
            &[esc(), k('2'), k('d'), k('3'), k('w')],
        );
        assert_eq!(t, "g h", "2d3w must delete 2×3=6 words");
    }

    // Finding 3 (regression): Ctrl-K / Ctrl-W / Ctrl-Y must pass through in
    // Normal mode so emacs bindings remain reachable without pressing `i` first.
    #[test]
    fn ctrl_k_w_y_pass_in_normal_mode() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // enter Normal
        assert_eq!(v.handle(&mut t, &mut c, ctrl('k')), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, ctrl('w')), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, ctrl('y')), Outcome::Pass);
    }

    #[test]
    fn arrow_and_page_keys_pass_in_visual_mode() {
        let mut v = Vim::new();
        let mut t = "hi".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k('v'));
        assert_eq!(v.mode, Mode::Visual);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(v.handle(&mut t, &mut c, up), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, down), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, pgup), Outcome::Pass);
        assert_eq!(v.handle(&mut t, &mut c, pgdn), Outcome::Pass);
    }

    // ---- Command mode tests ------------------------------------------------

    fn enter_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }
    fn backspace_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
    }

    #[test]
    fn colon_enters_command_mode_from_normal() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // Normal
        let out = v.handle(&mut t, &mut c, k(':'));
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(v.mode, Mode::Command);
        assert_eq!(v.cmdline, "");
    }

    #[test]
    fn command_mode_typing_builds_cmdline() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        v.handle(&mut t, &mut c, k('q'));
        v.handle(&mut t, &mut c, k('u'));
        v.handle(&mut t, &mut c, k('i'));
        v.handle(&mut t, &mut c, k('t'));
        assert_eq!(v.cmdline, "quit");
        assert_eq!(v.mode, Mode::Command);
    }

    #[test]
    fn command_mode_backspace_pops_last_char() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        v.handle(&mut t, &mut c, k('q'));
        v.handle(&mut t, &mut c, k('q'));
        let out = v.handle(&mut t, &mut c, backspace_key());
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(v.cmdline, "q");
    }

    #[test]
    fn command_mode_backspace_on_empty_is_noop() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        // cmdline is empty; backspace should not panic and stays in Command
        let out = v.handle(&mut t, &mut c, backspace_key());
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(v.cmdline, "");
        assert_eq!(v.mode, Mode::Command);
    }

    #[test]
    fn command_mode_enter_returns_command_outcome() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        v.handle(&mut t, &mut c, k('q'));
        let out = v.handle(&mut t, &mut c, enter_key());
        assert_eq!(out, Outcome::Command("q".to_string()));
        assert_eq!(v.mode, Mode::Normal);
        assert_eq!(v.cmdline, "");
    }

    #[test]
    fn command_mode_enter_trims_whitespace() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        // Simulate " quit " — spaces typed around "quit".
        for ch in " quit ".chars() {
            v.handle(&mut t, &mut c, k(ch));
        }
        let out = v.handle(&mut t, &mut c, enter_key());
        assert_eq!(out, Outcome::Command("quit".to_string()));
    }

    #[test]
    fn command_mode_esc_cancels_to_normal() {
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        v.handle(&mut t, &mut c, k('q'));
        let out = v.handle(&mut t, &mut c, esc());
        assert_eq!(out, Outcome::Consumed);
        assert_eq!(v.mode, Mode::Normal);
        assert_eq!(v.cmdline, "");
    }

    #[test]
    fn command_mode_backspace_utf8_safe() {
        // "日" is 3 bytes; backspace must not leave the string on an interior byte.
        let mut v = Vim::new();
        let mut t = String::new();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc());
        v.handle(&mut t, &mut c, k(':'));
        // Push '日' directly into cmdline (can't send it as a single k() easily
        // because KeyCode::Char takes a char, so we push via k()).
        v.handle(&mut t, &mut c, k('日'));
        assert_eq!(v.cmdline, "日");
        v.handle(&mut t, &mut c, backspace_key());
        assert_eq!(v.cmdline, "");
        // Must be a valid UTF-8 string.
        assert!(std::str::from_utf8(v.cmdline.as_bytes()).is_ok());
    }

    // ---- Regression tests for this review cycle ----------------------------

    // Finding B-vim-1 (major): `d0` with an operator pending must treat `0`
    // as the line-start motion, not as a zero motion-count.  Before the fix,
    // the count accumulator ran `op_count.saturating_mul(0) = 0`, so the
    // motion was applied 0 times and the text was silently unchanged.
    #[test]
    fn d_zero_with_pending_op_is_line_start_motion() {
        // "hello": esc from cursor=4 lands at index 3 ('l').
        // d0 must delete bytes [0..3) = "hel", leaving "lo" at cursor 0.
        let (_, t, c) = run("hello", 4, &[esc(), k('d'), k('0')]);
        assert_eq!(t, "lo", "d0 must delete to line start, not be a no-op");
        assert_eq!(c, 0);
    }

    // Corollary: an op-count such as `2d0` must also treat 0 as the motion.
    // The op-count (2) is meaningful only for operators that repeat the motion;
    // with d+LineStart the motion is applied once regardless, so the result is
    // the same as plain d0: everything before the cursor is deleted.
    #[test]
    fn count_d_zero_is_still_line_start_not_multiply_by_zero() {
        // "hello": cursor=3 (lands on 'l' at index 2 after esc from 3).
        // 2d0: op_count=2 stored, then `0` must fall through as LineStart motion.
        // Deletes [0..2) = "he" → "llo".
        let (_, t, c) = run("hello", 3, &[esc(), k('2'), k('d'), k('0')]);
        assert_eq!(t, "llo", "2d0 must delete to line start");
        assert_eq!(c, 0);
    }

    // Finding B-vim-4 (regression): Ctrl-K / Ctrl-W / Ctrl-Y must pass through
    // in Visual mode just as they do in Normal mode.  Before the fix, the
    // Visual catch-all returned Consumed, silently swallowing those bindings.
    #[test]
    fn ctrl_k_w_y_pass_in_visual_mode() {
        let mut v = Vim::new();
        let mut t = "hi".to_string();
        let mut c = 0;
        v.handle(&mut t, &mut c, esc()); // Normal
        v.handle(&mut t, &mut c, k('v')); // Visual
        assert_eq!(v.mode, Mode::Visual);
        assert_eq!(
            v.handle(&mut t, &mut c, ctrl('k')),
            Outcome::Pass,
            "Ctrl-K must pass through in Visual mode"
        );
        assert_eq!(
            v.handle(&mut t, &mut c, ctrl('w')),
            Outcome::Pass,
            "Ctrl-W must pass through in Visual mode"
        );
        assert_eq!(
            v.handle(&mut t, &mut c, ctrl('y')),
            Outcome::Pass,
            "Ctrl-Y must pass through in Visual mode"
        );
    }

    // Finding B-tui-3 (major): snap_boundary_down must snap an interior byte
    // of a multi-byte char down to the preceding char boundary.
    #[test]
    fn snap_boundary_down_snaps_interior_byte() {
        // "日" is 3 bytes (0xE6 0x97 0xA5). Indices 1 and 2 are interior.
        let s = "日";
        assert_eq!(snap_boundary_down(s, 0), 0);
        assert_eq!(
            snap_boundary_down(s, 1),
            0,
            "byte 1 is interior; must snap to 0"
        );
        assert_eq!(
            snap_boundary_down(s, 2),
            0,
            "byte 2 is interior; must snap to 0"
        );
        assert_eq!(
            snap_boundary_down(s, 3),
            3,
            "byte 3 is the next char boundary"
        );
        // Past-end clamping.
        assert_eq!(snap_boundary_down(s, 99), 3);
    }

    // Regression tests for finding B-command-line-1 (major):
    // multi-digit motion counts after an operator were broken because
    // `0` fell through the digit guard when `pending.is_some()`.

    #[test]
    fn d10w_deletes_ten_words() {
        // "a b c d e f g h i j k" at 0; d10w must delete the first 10 words.
        // Before the fix, `0` was dispatched as Motion::LineStart, deleting to
        // column 0 (no-op on the first word start) instead of 10 words.
        let text = "a b c d e f g h i j k";
        let (_, t, c) = run(text, 0, &[esc(), k('d'), k('1'), k('0'), k('w')]);
        assert_eq!(t, "k", "d10w should delete 10 words, leaving only the last");
        assert_eq!(c, 0);
    }

    #[test]
    fn d20w_deletes_twenty_words_clamped() {
        // 20-word delete on a shorter string: motion clamps at end, so the
        // whole string is deleted.
        let text = "one two three four five";
        let (_, t, c) = run(text, 0, &[esc(), k('d'), k('2'), k('0'), k('w')]);
        assert_eq!(t, "", "d20w on a 5-word string should clear it entirely");
        assert_eq!(c, 0);
    }

    #[test]
    fn d13w_deletes_thirteen_words() {
        // Also verifies that subsequent motion digits after the first extend
        // the accumulator decimally (not multiplicatively), so d13w = 13 not 3.
        let words: Vec<&str> = (1..=15).map(|_| "x").collect();
        let text = words.join(" "); // "x x x x x x x x x x x x x x x" (15 words)
        let (_, t, _) = run(&text, 0, &[esc(), k('d'), k('1'), k('3'), k('w')]);
        // After deleting 13 words ("x " * 13), "x x" remains (two words: space-sep).
        assert_eq!(t, "x x", "d13w should delete exactly 13 words");
    }

    #[test]
    fn d0_is_still_line_start_not_zero_count() {
        // `d0` must remain "delete to line start", not "delete 0 times".
        // run() starts in Insert mode; esc() pulls cursor back one char
        // (prev_boundary), so pass cursor=4 to land on index 3 in Normal mode.
        // d0 at index 3: target=0, deletes bytes [0..3) = "hel" → "lo".
        let (_, t, c) = run("hello", 4, &[esc(), k('d'), k('0')]);
        assert_eq!(t, "lo", "d0 must delete to line start, not be a zero-count");
        assert_eq!(c, 0);
    }

    #[test]
    fn op_count_zero_is_line_start_even_when_count_exists() {
        // `2d0`: op-count=2, then `0` arrives after the operator but before any
        // motion digit — must still be LineStart, not extend the op-count.
        // cursor=4 → esc() → cursor=3 in Normal; 2d0 deletes [0..3) = "hel" → "lo".
        let (_, t, c) = run("hello", 4, &[esc(), k('2'), k('d'), k('0')]);
        assert_eq!(
            t, "lo",
            "2d0 must delete to line start (0 is motion, not count digit)"
        );
        assert_eq!(c, 0);
    }
}
