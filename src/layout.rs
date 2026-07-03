//! Shared frame layout math for render, mouse hit-testing, and scroll sizing.
//!
//! Keep vertical splits identical across [`crate::tui::render`], [`crate::mouse`],
//! and scroll/page helpers in [`crate::drive`].

use ratatui::layout::{Constraint, Layout, Rect};

use crate::tui::View;

/// Visible row cap for the input box before growth stops (past this the box
/// scrolls via `Paragraph`, never eating the transcript).
pub const MAX_INPUT_ROWS: u16 = 8;

/// Vertical split matching [`crate::tui::render`]: output pane, separator,
/// status, input (variable height).
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

/// Output pane (width, height) for wrapping and PageUp/PageDown scroll amounts.
/// Height excludes the separator, status, and input rows.
pub fn pane_dims(term: (u16, u16), view: &View) -> (u16, u16) {
    let input_rows = (view.input_line_count() as u16).clamp(1, MAX_INPUT_ROWS);
    let chrome = 1u16 + 1 + input_rows;
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
        // 24 - sep(1) - status(1) - input(2) = 20
        assert_eq!(h, 20);
    }
}
