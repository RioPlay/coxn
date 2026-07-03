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

/// Strip model reasoning blocks from assistant text when the user hides them.
pub fn strip_reasoning(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["think", "thinking", "reasoning"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = out.find(&open) {
            if let Some(end) = out[start..].find(&close) {
                let end = start + end + close.len();
                out.replace_range(start..end, "");
            } else {
                break;
            }
        }
    }
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out.trim().to_string()
}

impl TurnView {
    pub fn from_message(m: &Message, hide_reasoning: bool) -> Self {
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
                let body = if hide_reasoning {
                    strip_reasoning(&m.content)
                } else {
                    m.content.clone()
                };
                Self {
                    role: TurnRole::Assistant,
                    body,
                    tools,
                }
            }
        }
    }

    /// Format as a bordered card for the conversation pane (plain text).
    pub fn format_card(&self, collapse_tools: bool) -> String {
        let title = self.role.label();
        let mut lines = vec![format!("╭─ {title} ─")];
        if !self.body.is_empty() {
            for line in self.body.lines() {
                lines.push(format!("│ {line}"));
            }
        }
        if !self.tools.is_empty() {
            if collapse_tools && self.tools.len() > 1 {
                lines.push(format!(
                    "│ {}  … +{} tools (Ctrl+T expand)",
                    self.tools[0],
                    self.tools.len() - 1
                ));
            } else {
                for t in &self.tools {
                    lines.push(format!("│ {t}"));
                }
            }
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
    pub fn format_card(&self, hide_reasoning: bool, collapse_tools: bool) -> String {
        let body = if hide_reasoning {
            strip_reasoning(&self.body)
        } else {
            self.body.clone()
        };
        let turn = TurnView {
            role: TurnRole::Assistant,
            body,
            tools: Vec::new(),
        };
        let mut card = turn.format_card(collapse_tools);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ui3State {
    pub chrome: ChromeState,
    pub turns: Vec<TurnView>,
    pub live: Option<LiveTurn>,
    pub activity: ActivityLog,
    /// Conversation scroll: distance-from-bottom in visual lines (0 = pinned).
    pub conv_scroll_offset: u16,
    /// Activity drawer scroll: distance-from-bottom in visual lines.
    pub activity_scroll_offset: u16,
    /// Collapse multi-tool assistant turns to one line + count.
    pub tools_collapsed: bool,
    /// Hide `<think>`-style reasoning blocks in assistant cards.
    pub reasoning_hidden: bool,
}

impl Default for Ui3State {
    fn default() -> Self {
        Self {
            chrome: ChromeState::default(),
            turns: Vec::new(),
            live: None,
            activity: ActivityLog::default(),
            conv_scroll_offset: 0,
            activity_scroll_offset: 0,
            tools_collapsed: true,
            reasoning_hidden: true,
        }
    }
}

impl Ui3State {
    pub fn sync_turns(&mut self, messages: &[Message]) {
        self.turns = messages
            .iter()
            .map(|m| TurnView::from_message(m, self.reasoning_hidden))
            .collect();
    }

    pub fn conversation_text(&self) -> String {
        let mut parts: Vec<String> = self
            .turns
            .iter()
            .map(|t| t.format_card(self.tools_collapsed))
            .collect();
        if let Some(ref live) = self.live {
            parts.push(live.format_card(self.reasoning_hidden, self.tools_collapsed));
        }
        parts.join("\n\n")
    }

    /// Full export: conversation cards + activity log (for `/copy`).
    pub fn export_text(&self) -> String {
        let conv = self.conversation_text();
        let activity = self.activity.display_text();
        if activity.is_empty() {
            conv
        } else if conv.is_empty() {
            activity
        } else {
            format!("{conv}\n\n--- activity ---\n{activity}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;

    #[test]
    fn turn_card_formats_user_message() {
        let t = TurnView::from_message(&Message::new(Role::User, "fix the bug"), false);
        let card = t.format_card(false);
        assert!(card.contains("╭─ you"));
        assert!(card.contains("fix the bug"));
    }

    #[test]
    fn strip_reasoning_removes_think_blocks() {
        let raw = "answer<think>secret</think> tail";
        assert_eq!(strip_reasoning(raw), "answer tail");
    }

    #[test]
    fn collapsed_tools_show_count() {
        let t = TurnView {
            role: TurnRole::Assistant,
            body: "ok".into(),
            tools: vec!["▸ read a".into(), "▸ edit b".into()],
        };
        let card = t.format_card(true);
        assert!(card.contains("+1 tools"));
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
    fn export_text_includes_activity_section() {
        let mut ui = Ui3State::default();
        ui.sync_turns(&[Message::new(Role::User, "hi")]);
        ui.activity.push("/model", "models...");
        let text = ui.export_text();
        assert!(text.contains("hi"));
        assert!(text.contains("activity"));
        assert!(text.contains("models"));
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
