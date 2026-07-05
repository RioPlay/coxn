//! Input draining during long-running turns (streaming, `/execute`, `!cmd`).

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::layout;
use crate::tui::{Action, Tui, View, map_input_key, map_insert_key};
use crate::vim::{Outcome, Scroll};

/// Lines the transcript scrolls per Up/Down (a wheel notch in most terminals).
pub(super) const SCROLL_STEP: u16 = 3;

/// Result of draining the event queue while a long operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputDrainResult {
    /// No input edits were applied.
    None,
    /// The input buffer or scroll position changed.
    Edited,
    /// The user pressed Ctrl-C to cancel the background operation.
    Cancel,
}

pub(super) fn pane_dims(tui: &Tui, view: &View) -> (u16, u16) {
    tui.size()
        .map(|s| layout::pane_dims((s.width, s.height), view))
        .unwrap_or((80, 1))
}

/// Drain pending keyboard/paste events into the input buffer while inference,
/// streaming, or `/execute` runs. Does not submit messages or open modals.
pub(super) fn drain_input_edits(tui: &Tui, view: &mut View) -> InputDrainResult {
    if view.modal.is_some() || view.menu.is_some() || view.show_help {
        return InputDrainResult::None;
    }
    let mut result = InputDrainResult::None;
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let Ok(ev) = event::read() else {
            break;
        };
        match ev {
            Event::Paste(s) => {
                view.input_push_str(&s);
                result = InputDrainResult::Edited;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if matches!(map_input_key(key), Some(Action::Quit)) {
                    return InputDrainResult::Cancel;
                }
                let newline_enter = matches!(key.code, KeyCode::Enter)
                    && (key.modifiers.contains(KeyModifiers::ALT)
                        || key.modifiers.contains(KeyModifiers::SHIFT));
                let vim_outcome = if newline_enter || !crate::vim::enabled() {
                    Outcome::Pass
                } else {
                    view.vim.handle(&mut view.input, &mut view.cursor, key)
                };
                if vim_outcome == Outcome::Consumed {
                    result = InputDrainResult::Edited;
                    continue;
                }
                if let Outcome::Scroll(dir) = vim_outcome {
                    let (w, h) = pane_dims(tui, view);
                    match dir {
                        Scroll::LineUp => view.scroll_primary_up(1, view.max_scroll(w, h)),
                        Scroll::LineDown => view.scroll_primary_down(1),
                        Scroll::HalfPageUp => view.scroll_primary_up(h / 2, view.max_scroll(w, h)),
                        Scroll::HalfPageDown => view.scroll_primary_down(h / 2),
                        Scroll::Top => {
                            view.scroll_primary_up(view.max_scroll(w, h), view.max_scroll(w, h));
                        }
                        Scroll::Bottom => view.scroll_primary_down(view.max_scroll(w, h)),
                    }
                    result = InputDrainResult::Edited;
                    continue;
                }
                if let Outcome::ScrollN(dir, n) = vim_outcome {
                    let (w, h) = pane_dims(tui, view);
                    for _ in 0..n {
                        match dir {
                            Scroll::LineUp => view.scroll_primary_up(1, view.max_scroll(w, h)),
                            Scroll::LineDown => view.scroll_primary_down(1),
                            Scroll::HalfPageUp => {
                                view.scroll_primary_up(h / 2, view.max_scroll(w, h))
                            }
                            Scroll::HalfPageDown => view.scroll_primary_down(h / 2),
                            Scroll::Top => {
                                view.scroll_primary_up(
                                    view.max_scroll(w, h),
                                    view.max_scroll(w, h),
                                );
                            }
                            Scroll::Bottom => view.scroll_primary_down(view.max_scroll(w, h)),
                        }
                    }
                    result = InputDrainResult::Edited;
                    continue;
                }
                if view.search_editing() || view.search.is_some() {
                    continue;
                }
                if vim_outcome == Outcome::Submit {
                    continue;
                }
                let action = map_insert_key(view, key);
                match action {
                    Some(Action::Quit) => return InputDrainResult::Cancel,
                    Some(Action::Submit) | Some(Action::Complete) => {}
                    Some(Action::Append(c)) => {
                        view.input_push(c);
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::Newline) => {
                        view.input_push('\n');
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::Backspace) => {
                        view.input_backspace();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::CursorLeft) => {
                        view.cursor_left();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::CursorRight) => {
                        if view.cursor == view.input.len() {
                            if let Some(sugg) = &view.suggestion {
                                view.input.push_str(sugg);
                                view.cursor_end();
                                result = InputDrainResult::Edited;
                                continue;
                            }
                        }
                        view.cursor_right();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::CursorHome) => {
                        view.cursor_home();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::CursorEnd) => {
                        view.cursor_end();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::WordDelete) => {
                        view.word_delete();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::KillToEnd) => {
                        view.kill_to_end();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::KillToStart) => {
                        view.kill_to_start();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::Yank) => {
                        view.yank();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::HistoryPrev) => {
                        view.history_prev();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::HistoryNext) => {
                        view.history_next();
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::ScrollUp) => {
                        let (w, h) = pane_dims(tui, view);
                        view.scroll_primary_up(SCROLL_STEP, view.max_scroll(w, h));
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::ScrollDown) => {
                        view.scroll_primary_down(SCROLL_STEP);
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::PageUp) => {
                        let (w, h) = pane_dims(tui, view);
                        view.scroll_primary_up(h, view.max_scroll(w, h));
                        result = InputDrainResult::Edited;
                    }
                    Some(Action::PageDown) => {
                        let (_, h) = pane_dims(tui, view);
                        view.scroll_primary_down(h);
                        result = InputDrainResult::Edited;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if result == InputDrainResult::Edited {
        view.refresh_suggestion();
    }
    result
}

/// Ctrl-C / quit only — used where `tui`/`view` are already borrowed elsewhere.
pub(super) fn poll_user_quit_only() -> bool {
    if let Ok(true) = event::poll(Duration::ZERO)
        && let Ok(Event::Key(key)) = event::read()
        && key.kind == KeyEventKind::Press
    {
        return matches!(map_input_key(key), Some(Action::Quit));
    }
    false
}

pub(super) fn apply_input_drain(tui: &mut Tui, view: &mut View) -> bool {
    match drain_input_edits(tui, view) {
        InputDrainResult::Cancel => false,
        InputDrainResult::Edited => {
            let _ = tui.draw(view);
            true
        }
        InputDrainResult::None => true,
    }
}
