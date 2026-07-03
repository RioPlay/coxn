//! M5 mouse hit-testing and OSC52 clipboard (gated on `COXN_CLIPBOARD=on`).
//!
//! Layout math mirrors [`crate::tui::render`] — keep in sync when render changes.

use std::io::{self, Write};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::layout;
use crate::tui::{
    Action, Menu, MenuKind, ModalKind, PANE_GUTTER, ToolApprovalChoice, View, centered_rect,
    menu_max_rows, modal_hint_plain, wrapped_line_count,
};

/// Result of routing a mouse event through the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseEffect {
    None,
    ScrollUp,
    ScrollDown,
    SetCursor(usize),
    MenuRow(usize),
    Modal(Action),
    ToolApproval(ToolApprovalChoice),
    /// Selection finalized; caller should emit OSC52 after the next frame flush.
    CopySelection(String),
}

/// Terminal areas used for hit-testing (must match [`crate::tui::render`]).
#[derive(Debug, Clone, Copy)]
pub struct FrameLayout {
    pub pane: Rect,
    pub input: Rect,
    pub menu: Option<MenuHit>,
    pub modal: Option<ModalHit>,
}

#[derive(Debug, Clone, Copy)]
pub struct MenuHit {
    pub area: Rect,
    pub scroll: usize,
    pub visible: usize,
    pub filter_row: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ModalHit {
    pub area: Rect,
    /// Screen row of the `[y] proceed / [n] block` hint line.
    pub hint_row: u16,
}

/// Split the frame the same way `render` does.
pub fn frame_layout(frame: Rect, view: &View) -> FrameLayout {
    let main = layout::main_pane(frame, view);
    let input = layout::input_area(frame, view);
    FrameLayout {
        pane: main,
        input,
        menu: view.menu.as_ref().map(|m| menu_hit(frame, m)),
        modal: view.modal.as_ref().and_then(|_| modal_hit(frame, view)),
    }
}

fn menu_hit(frame: Rect, menu: &Menu) -> MenuHit {
    let hint = if menu.kind == MenuKind::Palette {
        "type to filter  j/k  Enter  Esc"
    } else {
        "j/k ↑↓  G/gg  PgUp/Dn  Enter  Esc"
    };
    let count = menu.items.len();
    let rows = menu_max_rows(frame.height, count);
    let filter_row = matches!(menu.kind, MenuKind::Palette | MenuKind::AtFiles);
    let filter_w = if filter_row {
        format!("filter: {}", menu.filter).chars().count()
    } else {
        0
    };
    let width = menu
        .items
        .iter()
        .map(|i| i.label.chars().count())
        .chain([menu.title.chars().count(), hint.chars().count(), filter_w])
        .max()
        .unwrap_or(0) as u16;
    let body_lines = filter_row as usize + rows.min(count.saturating_sub(menu.scroll)) + 2;
    let height = body_lines as u16 + 2;
    let area = centered_rect(width + 6, height, frame);
    MenuHit {
        area,
        scroll: menu.scroll,
        visible: rows.min(count.saturating_sub(menu.scroll)),
        filter_row,
    }
}

fn modal_hit(frame: Rect, view: &View) -> Option<ModalHit> {
    let prompt = view.modal.as_ref()?;
    let hint = modal_hint_plain(view);
    const DIFF_PREVIEW_ROWS: usize = 12;
    let diff_len = view
        .modal_diff
        .as_ref()
        .map(|d| {
            let total = d.lines().count();
            if view.modal_diff_expanded {
                total + if total > DIFF_PREVIEW_ROWS { 1 } else { 0 }
            } else {
                DIFF_PREVIEW_ROWS.min(total) + if total > DIFF_PREVIEW_ROWS { 1 } else { 0 }
            }
        })
        .unwrap_or(0);
    let body_lines = 1 + if diff_len > 0 { diff_len + 1 } else { 0 } + 1 + 1;
    let widest = prompt.chars().count().max(hint.chars().count()).max(40) as u16;
    let area_height = (body_lines as u16 + 2).min(frame.height.saturating_sub(1));
    let area = centered_rect(
        widest.min(frame.width.saturating_sub(4)) + 4,
        area_height,
        frame,
    );
    let hint_row = area.y + area.height.saturating_sub(2);
    Some(ModalHit { area, hint_row })
}

fn in_rect(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

fn menu_row_at(hit: &MenuHit, menu: &Menu, col: u16, row: u16) -> Option<usize> {
    if !in_rect(hit.area, col, row) {
        return None;
    }
    let inner_row = row.saturating_sub(hit.area.y + 1) as usize;
    let mut row_idx = inner_row;
    if hit.filter_row {
        if row_idx == 0 {
            return None;
        }
        row_idx -= 1;
    }
    if row_idx >= hit.visible {
        return None;
    }
    let idx = hit.scroll + row_idx;
    menu.items.get(idx).map(|_| idx)
}

fn modal_action_at(hit: &ModalHit, view: &View, col: u16, row: u16) -> Option<MouseEffect> {
    if row != hit.hint_row || !in_rect(hit.area, col, row) {
        return None;
    }
    let rel = col.saturating_sub(hit.area.x + 1);
    let inner = hit.area.width.saturating_sub(2).max(1);
    match view.modal_kind {
        ModalKind::Gate => {
            if rel < inner / 2 {
                Some(MouseEffect::Modal(Action::Confirm))
            } else {
                Some(MouseEffect::Modal(Action::Cancel))
            }
        }
        ModalKind::ToolApproval => {
            let q = inner / 4;
            let choice = if rel < q {
                ToolApprovalChoice::Once
            } else if rel < q * 2 {
                ToolApprovalChoice::Session
            } else if rel < q * 3 {
                ToolApprovalChoice::Decline
            } else {
                ToolApprovalChoice::CancelTurn
            };
            Some(MouseEffect::ToolApproval(choice))
        }
    }
}

fn input_cursor_at(view: &View, input: Rect, col: u16, row: u16) -> usize {
    if !in_rect(input, col, row) {
        return view.cursor;
    }
    let local_y = row.saturating_sub(input.y) as usize;
    let local_x = col.saturating_sub(input.x) as usize;
    let mut line_start = 0usize;
    let mut line_idx = 0usize;
    for (i, segment) in view.input.split('\n').enumerate() {
        if i == local_y {
            line_idx = i;
            break;
        }
        line_start += segment.len() + 1;
    }
    let line = view.input.split('\n').nth(line_idx).unwrap_or("");
    let byte_col = line
        .char_indices()
        .map(|(i, _)| i)
        .nth(local_x)
        .unwrap_or(line.len());
    (line_start + byte_col).min(view.input.len())
}

fn visual_line_index(view: &View, pane: Rect, row: u16) -> Option<usize> {
    if !in_rect(pane, 0, row) {
        return None;
    }
    let content_width = pane.width.saturating_sub(PANE_GUTTER) as usize;
    let total = wrapped_line_count(&view.output, content_width);
    let pane_h = pane.height as usize;
    let max_scrollback = total.saturating_sub(pane_h) as u16;
    let from_bottom = view.scroll_offset.min(max_scrollback);
    let scroll_row = max_scrollback - from_bottom;
    let local = row.saturating_sub(pane.y) as usize;
    let idx = scroll_row as usize + local;
    (idx < total).then_some(idx)
}

/// Build wrapped visual lines for transcript selection (same wrap math as render).
fn wrapped_lines(output: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for raw in output.lines() {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut col = 0usize;
        let mut start = 0usize;
        for (byte_i, ch) in raw.char_indices() {
            let cw = if (ch as u32) > 0xFF { 2 } else { 1 };
            if col + cw > width && col > 0 {
                lines.push(raw[start..byte_i].to_string());
                start = byte_i;
                col = cw;
            } else {
                col += cw;
            }
        }
        lines.push(raw[start..].to_string());
    }
    lines
}

fn selection_text(view: &View, pane: Rect, lo: usize, hi: usize) -> String {
    let width = pane.width.saturating_sub(PANE_GUTTER) as usize;
    let lines = wrapped_lines(&view.output, width);
    if lines.is_empty() {
        return String::new();
    }
    let lo = lo.min(lines.len() - 1);
    let hi = hi.min(lines.len() - 1);
    let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    lines[a..=b].join("\n")
}

/// Route a mouse event. Updates `view` transcript-drag state in place.
pub fn handle_mouse(
    view: &mut View,
    layout: &FrameLayout,
    me: MouseEvent,
    max_scroll: u16,
) -> MouseEffect {
    let col = me.column;
    let row = me.row;

    if let Some(hit) = layout.modal
        && let Some(effect) = modal_action_at(&hit, view, col, row)
    {
        return effect;
    }

    if let (Some(hit), Some(menu)) = (layout.menu, view.menu.as_ref()) {
        if let Some(idx) = menu_row_at(&hit, menu, col, row) {
            return MouseEffect::MenuRow(idx);
        }
    }

    match me.kind {
        MouseEventKind::ScrollUp => return MouseEffect::ScrollUp,
        MouseEventKind::ScrollDown => return MouseEffect::ScrollDown,
        MouseEventKind::Down(MouseButton::Left) => {
            if in_rect(layout.input, col, row)
                && view.modal.is_none()
                && view.menu.is_none()
                && !view.show_help
            {
                let pos = input_cursor_at(view, layout.input, col, row);
                view.cursor = pos;
                view.transcript_drag = None;
                return MouseEffect::SetCursor(pos);
            }
            if let Some(vline) = visual_line_index(view, layout.pane, row) {
                if view.modal.is_none() && view.menu.is_none() {
                    view.transcript_drag = Some((vline, vline));
                    return MouseEffect::None;
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if view.transcript_drag.is_some() => {
            if let Some(vline) = visual_line_index(view, layout.pane, row)
                && let Some((_, end)) = view.transcript_drag.as_mut()
            {
                *end = vline;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some((lo, hi)) = view.transcript_drag.take() {
                let text = selection_text(view, layout.pane, lo, hi);
                if !text.is_empty() {
                    return MouseEffect::CopySelection(text);
                }
            }
        }
        _ => {}
    }

    let _ = max_scroll;
    MouseEffect::None
}

/// Minimal base64 (no extra dep) for OSC52 payloads.
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Emit OSC52 to stdout after a frame flush. Gated on `COXN_CLIPBOARD=on`.
pub fn osc52_copy(text: &str) -> io::Result<()> {
    if std::env::var("COXN_CLIPBOARD")
        .ok()
        .is_none_or(|v| v != "on" && v != "1")
    {
        return Ok(());
    }
    let b64 = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{MenuItem, MenuKind};

    #[test]
    fn base64_roundtrip_known_vector() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn menu_click_selects_visible_row() {
        let menu = Menu {
            kind: MenuKind::Session,
            title: "sessions".into(),
            items: vec![
                MenuItem {
                    value: "a".into(),
                    label: "alpha".into(),
                },
                MenuItem {
                    value: "b".into(),
                    label: "beta".into(),
                },
            ],
            selected: 0,
            scroll: 0,
            count: None,
            pending_g: false,
            filter: String::new(),
            catalog: Vec::new(),
        };
        let frame = Rect::new(0, 0, 80, 24);
        let hit = menu_hit(frame, &menu);
        let idx = menu_row_at(&hit, &menu, hit.area.x + 2, hit.area.y + 2);
        assert_eq!(idx, Some(1));
    }

    #[test]
    fn modal_hint_left_confirms_right_cancels() {
        let mut view = View::new();
        view.modal = Some("approve?".into());
        let frame = Rect::new(0, 0, 80, 24);
        let hit = modal_hit(frame, &view).unwrap();
        assert_eq!(
            modal_action_at(&hit, &view, hit.area.x + 2, hit.hint_row),
            Some(MouseEffect::Modal(Action::Confirm))
        );
        assert_eq!(
            modal_action_at(&hit, &view, hit.area.x + hit.area.width - 2, hit.hint_row),
            Some(MouseEffect::Modal(Action::Cancel))
        );
    }

    #[test]
    fn transcript_selection_joins_wrapped_range() {
        let mut view = View::new();
        view.output = "line one\nline two".into();
        let pane = Rect::new(0, 0, 40, 10);
        view.transcript_drag = Some((0, 1));
        let text = selection_text(&view, pane, 0, 1);
        assert!(text.contains("line one"));
        assert!(text.contains("line two"));
    }
}
