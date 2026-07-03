//! Structured conversation and activity state for TUI 3.0.

use crate::model::{Message, Role, ToolCall};
use crate::tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
    Tool,
    System,
    #[allow(dead_code)] // PR6: aden turn cards from graph actions
    Aden,
}

impl TurnRole {
    pub fn label(self) -> &'static str {
        match self {
            TurnRole::User => "you",
            TurnRole::Assistant => "coxn",
            TurnRole::Tool => "tool",
            TurnRole::System => "sys",
            TurnRole::Aden => "aden",
        }
    }
}

/// One conversation turn for card rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnView {
    pub role: TurnRole,
    pub body: String,
    pub tools: Vec<String>,
}

impl TurnView {
    pub fn from_message(m: &Message) -> Self {
        match m.role {
            Role::User => Self {
                role: TurnRole::User,
                body: m.content.clone(),
                tools: Vec::new(),
            },
            Role::System => Self {
                role: TurnRole::System,
                body: m.content.clone(),
                tools: Vec::new(),
            },
            Role::Tool => {
                let body = if m.content.starts_with("cmd:") {
                    m.content.clone()
                } else {
                    format!("tool: {}", m.content)
                };
                Self {
                    role: TurnRole::Tool,
                    body,
                    tools: Vec::new(),
                }
            }
            Role::Assistant => {
                let tools = m.tool_calls.iter().map(tool_call_card).collect();
                Self {
                    role: TurnRole::Assistant,
                    body: m.content.clone(),
                    tools,
                }
            }
        }
    }

    /// Format as a bordered card for the conversation pane (plain text).
    pub fn format_card(&self) -> String {
        let title = self.role.label();
        let mut lines = vec![format!("╭─ {title} ─")];
        if !self.body.is_empty() {
            for line in self.body.lines() {
                lines.push(format!("│ {line}"));
            }
        }
        for t in &self.tools {
            lines.push(format!("│ {t}"));
        }
        if lines.len() == 1 {
            lines.push("│ (empty)".to_string());
        }
        lines.push("╰────────".to_string());
        lines.join("\n")
    }
}

fn tool_call_card(call: &ToolCall) -> String {
    match call.name.as_str() {
        "read_file" | "edit" | "write_file" => {
            let path = tools::arg_preview(&call.arguments, "path");
            format!("▸ {} {}", call.name, path)
        }
        "run_command" => {
            let cmd = tools::arg_preview(&call.arguments, "command");
            let preview: String = cmd.chars().take(60).collect();
            let ell = if cmd.chars().count() > 60 { "…" } else { "" };
            format!("▸ run_command $ {preview}{ell}")
        }
        _ => {
            let preview: String = call.arguments.chars().take(40).collect();
            format!("▸ {} {preview}", call.name)
        }
    }
}

/// In-flight assistant text + optional run_command stream (conversation pane).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTurn {
    pub body: String,
    pub run_buf: String,
}

impl LiveTurn {
    pub fn format_card(&self) -> String {
        let turn = TurnView {
            role: TurnRole::Assistant,
            body: self.body.clone(),
            tools: Vec::new(),
        };
        let mut card = turn.format_card();
        if !self.run_buf.is_empty() {
            card.push('\n');
            for line in self.run_buf.lines() {
                card.push_str(&format!("│ {line}\n"));
            }
            card.push_str("╰────────");
        }
        card
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChromeState {
    pub model: String,
    pub task: String,
    pub trust: String,
    pub aden_active: bool,
}

impl ChromeState {
    pub fn format_line(&self) -> String {
        let aden = if self.aden_active { "aden" } else { "—" };
        format!(
            "{}  ·  {}  ·  {}  ·  {}",
            self.model, self.task, self.trust, aden
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub title: String,
    pub body: String,
}

/// Bottom activity drawer: execute, bang, slash output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityLog {
    pub entries: Vec<ActivityEntry>,
    /// Live body for the current entry (streaming).
    pub live_title: Option<String>,
    pub live_body: String,
    pub scroll: u16,
}

impl ActivityLog {
    pub fn push(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.entries.push(ActivityEntry {
            title: title.into(),
            body: body.into(),
        });
        self.live_title = None;
        self.live_body.clear();
    }

    pub fn start_live(&mut self, title: impl Into<String>) {
        self.live_title = Some(title.into());
        self.live_body.clear();
    }

    pub fn append_live(&mut self, chunk: &str) {
        self.live_body.push_str(chunk);
    }

    pub fn finish_live(&mut self) {
        if let Some(title) = self.live_title.take() {
            let body = std::mem::take(&mut self.live_body);
            self.entries.push(ActivityEntry { title, body });
        }
    }

    pub fn display_text(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("▸ {}\n{}", e.title, e.body));
        }
        if let Some(ref title) = self.live_title {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("▸ {title}\n{}", self.live_body));
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ui3State {
    pub chrome: ChromeState,
    pub turns: Vec<TurnView>,
    pub live: Option<LiveTurn>,
    pub activity: ActivityLog,
    pub conv_scroll: u16,
}

impl Ui3State {
    pub fn sync_turns(&mut self, messages: &[Message]) {
        self.turns = messages.iter().map(TurnView::from_message).collect();
    }

    pub fn conversation_text(&self) -> String {
        let mut parts: Vec<String> = self.turns.iter().map(|t| t.format_card()).collect();
        if let Some(ref live) = self.live {
            parts.push(live.format_card());
        }
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;

    #[test]
    fn turn_card_formats_user_message() {
        let t = TurnView::from_message(&Message::new(Role::User, "fix the bug"));
        let card = t.format_card();
        assert!(card.contains("╭─ you"));
        assert!(card.contains("fix the bug"));
    }

    #[test]
    fn activity_log_display_includes_live() {
        let mut log = ActivityLog::default();
        log.push("done", "ok");
        log.start_live("running");
        log.append_live("line1\n");
        let text = log.display_text();
        assert!(text.contains("done"));
        assert!(text.contains("running"));
        assert!(text.contains("line1"));
    }

    #[test]
    fn sync_turns_from_messages() {
        let mut ui = Ui3State::default();
        ui.sync_turns(&[
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "hello"),
        ]);
        assert_eq!(ui.turns.len(), 2);
        assert_eq!(ui.turns[0].role, TurnRole::User);
    }
}
