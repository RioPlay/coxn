//! Shared frame layout math for render, mouse hit-testing, and scroll sizing.
//!
//! Keep vertical splits identical across [`crate::tui::render`], [`crate::mouse`],
//! and scroll/page helpers in [`crate::drive`].

use ratatui::layout::{Constraint, Layout, Rect};

use crate::tui::View;

/// Visible row cap for the input box before growth stops (past this the box
/// scrolls via `Paragraph`, never eating the transcript).
pub const MAX_INPUT_ROWS: u16 = 8;

/// TUI 3.0 activity drawer height (rows).
pub const ACTIVITY_ROWS: u16 = 8;

/// TUI 3.0 chrome bar height (rows).
pub const CHROME_ROWS: u16 = 1;

fn structured(view: &View) -> bool {
    crate::ui::enabled() && view.ui3.is_some()
}

/// Areas for TUI 3.0: chrome, conversation, activity, separator, status, input.
pub struct AreasV3 {
    pub chrome: Rect,
    pub conversation: Rect,
    pub activity: Rect,
    pub separator: Rect,
    pub status: Rect,
    pub input: Rect,
}

pub fn areas_v3(frame: Rect, view: &View) -> AreasV3 {
    let input_rows = (view.input_line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    let outer = Layout::vertical([
        Constraint::Length(CHROME_ROWS),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(input_rows),
    ])
    .split(frame);
    let inner =
        Layout::vertical([Constraint::Min(1), Constraint::Length(ACTIVITY_ROWS)]).split(outer[1]);
    AreasV3 {
        chrome: outer[0],
        conversation: inner[0],
        activity: inner[1],
        separator: outer[2],
        status: outer[3],
        input: outer[4],
    }
}

/// Vertical split (legacy): output pane, separator, status, input.
pub fn frame_areas(frame: Rect, view: &View) -> [Rect; 4] {
    let input_rows = (view.input_line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    let areas = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(input_rows),
    ])
    .split(frame);
    [areas[0], areas[1], areas[2], areas[3]]
}

/// Primary scrollable pane: conversation (v3) or output (legacy).
pub fn main_pane(frame: Rect, view: &View) -> Rect {
    if structured(view) {
        areas_v3(frame, view).conversation
    } else {
        frame_areas(frame, view)[0]
    }
}

/// Composer/input area.
pub fn input_area(frame: Rect, view: &View) -> Rect {
    if structured(view) {
        areas_v3(frame, view).input
    } else {
        frame_areas(frame, view)[3]
    }
}

/// Output pane (width, height) for wrapping and PageUp/PageDown scroll amounts.
pub fn pane_dims(term: (u16, u16), view: &View) -> (u16, u16) {
    let input_rows = (view.input_line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    let chrome = if structured(view) {
        CHROME_ROWS + ACTIVITY_ROWS + 1 + 1 + input_rows
    } else {
        1 + 1 + input_rows
    };
    (term.0.max(1), term.1.saturating_sub(chrome).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_dims_grows_with_multiline_input() {
        let mut view = View::new();
        view.input = "line one\nline two".to_string();
        assert_eq!(view.input_line_count(), 2);
        let (w, h) = pane_dims((80, 24), &view);
        assert_eq!(w, 80);
        assert_eq!(h, 20);
    }

    #[test]
    fn structured_layout_reserves_activity_rows() {
        let mut view = View::new();
        view.init_ui3();
        let areas = areas_v3(Rect::new(0, 0, 80, 30), &view);
        assert_eq!(areas.chrome.height, CHROME_ROWS);
        assert_eq!(areas.activity.height, ACTIVITY_ROWS);
        assert!(areas.conversation.height >= 1);
    }
}
