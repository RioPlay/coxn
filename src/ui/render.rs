//! Region render helpers for TUI 3.0 (chrome, conversation, activity).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::transcript::{ActivityLog, ChromeState, Ui3State};
use crate::tui::wrapped_line_count;

const ACCENT: ratatui::style::Color = ratatui::style::Color::Rgb(91, 127, 166);
const DIM: ratatui::style::Color = ratatui::style::Color::Rgb(107, 100, 88);
const PRIMARY: ratatui::style::Color = ratatui::style::Color::Rgb(212, 201, 176);
const SECONDARY: ratatui::style::Color = ratatui::style::Color::Rgb(168, 158, 138);

pub fn render_chrome(frame: &mut Frame, area: Rect, chrome: &ChromeState) {
    let line = Line::from(vec![Span::styled(
        chrome.format_line(),
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(ACCENT)),
        ),
        area,
    );
}

fn scroll_from_bottom(text: &str, area: Rect, offset_from_bottom: u16) -> u16 {
    let content_width = area.width.saturating_sub(1) as usize;
    let total = wrapped_line_count(text, content_width);
    let pane_h = area.height as usize;
    let max_scrollback = total.saturating_sub(pane_h) as u16;
    let from_bottom = offset_from_bottom.min(max_scrollback);
    max_scrollback.saturating_sub(from_bottom)
}

pub fn render_conversation(frame: &mut Frame, area: Rect, ui3: &Ui3State, pending: bool) {
    let text = ui3.conversation_text();
    let style = if pending {
        Style::default().fg(SECONDARY)
    } else {
        Style::default().fg(PRIMARY)
    };
    let scroll = scroll_from_bottom(&text, area, ui3.conv_scroll_offset);
    frame.render_widget(
        Paragraph::new(text)
            .style(style)
            .block(
                Block::default()
                    .title(Span::styled(" conversation ", Style::default().fg(DIM)))
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(ACCENT)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub fn render_activity(
    frame: &mut Frame,
    area: Rect,
    activity: &ActivityLog,
    offset_from_bottom: u16,
) {
    let text = activity.display_text();
    let scroll = scroll_from_bottom(&text, area, offset_from_bottom);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(SECONDARY))
            .block(
                Block::default()
                    .title(Span::styled(" activity ", Style::default().fg(DIM)))
                    .borders(Borders::TOP | Borders::LEFT)
                    .border_style(Style::default().fg(DIM)),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}
