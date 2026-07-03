//! Region render helpers for TUI 3.0 (chrome, conversation, activity).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::transcript::{ActivityLog, ChromeState, Ui3State};

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

pub fn render_conversation(frame: &mut Frame, area: Rect, ui3: &Ui3State, pending: bool) {
    let text = ui3.conversation_text();
    let style = if pending {
        Style::default().fg(SECONDARY)
    } else {
        Style::default().fg(PRIMARY)
    };
    let scroll = ui3.conv_scroll;
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

pub fn render_activity(frame: &mut Frame, area: Rect, activity: &ActivityLog) {
    let text = activity.display_text();
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
            .scroll((activity.scroll, 0)),
        area,
    );
}
