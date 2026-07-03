//! Shared NDJSON subprocess streaming for CLI piggyback backends.
//!
//! Spawns a child, reads stdout line-by-line on a background thread, and polls
//! the TUI for input on idle ticks (~50ms) between lines.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::model::{ModelRequest, Role, ThinkingLevel, Usage};
use crate::pump::TurnIo;
use crate::stream_idle;

pub use crate::codex_app_server::binary_installed;

const TURN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Default)]
pub struct NdjsonTurnResult {
    pub text: String,
    pub usage: Option<Usage>,
}

/// Apply per-instance env overrides. `home_path` sets `HOME` when present.
pub fn apply_instance_env(cmd: &mut Command, home_path: Option<&str>, env: &[(String, String)]) {
    if let Some(home) = home_path {
        cmd.env("HOME", home);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
}

/// Run `cmd`, parse each stdout NDJSON line with `on_line`, honour `io.on_idle`.
pub fn run_ndjson_turn<F>(
    mut cmd: Command,
    io: &mut dyn TurnIo,
    mut on_line: F,
) -> Result<NdjsonTurnResult, String>
where
    F: FnMut(
        &serde_json::Value,
        &mut NdjsonTurnResult,
        &mut dyn TurnIo,
    ) -> Result<StreamControl, String>,
{
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout unavailable".to_string())?;
    let (tx, rx) = mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = std::time::Instant::now() + TURN_TIMEOUT;
    let mut result = NdjsonTurnResult::default();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            return Err("cli turn timeout".to_string());
        }
        let line = {
            let mut idle_cb = || io.on_idle();
            let mut idle_opt = Some(&mut idle_cb as &mut dyn FnMut() -> bool);
            match stream_idle::recv_line_with_idle(&rx, &mut idle_opt) {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    let _ = child.kill();
                    return Err(format!("read stdout: {e}"));
                }
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match on_line(&value, &mut result, io)? {
            StreamControl::Continue => {}
            StreamControl::Done => break,
        }
        if child
            .try_wait()
            .ok()
            .flatten()
            .is_some_and(|status| !status.success())
            && result.text.is_empty()
        {
            return Err("cli exited before producing output".to_string());
        }
    }

    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    if result.text.is_empty() && !io.stream_cancelled() {
        return Err("cli returned empty output".to_string());
    }
    Ok(result)
}

/// Parse `input_tokens` / `output_tokens` from a CLI NDJSON usage object.
pub fn usage_from_object(usage: &serde_json::Value) -> Option<Usage> {
    let input = usage.get("input_tokens")?.as_u64()? as u32;
    let output = usage.get("output_tokens")?.as_u64()? as u32;
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(input.saturating_add(output));
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
    })
}

/// Flatten a [`ModelRequest`] into a single prompt for CLI piggyback backends.
pub fn flatten_request(request: &ModelRequest) -> String {
    let mut parts = Vec::new();
    if !request.system.is_empty() {
        parts.push(format!("System:\n{}", request.system.trim()));
    }
    if let Some(level) = request.thinking
        && level != ThinkingLevel::Off
    {
        parts.push(format!("Reasoning effort: {}", level.label()));
    }
    for message in &request.messages {
        let role = match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let mut body = message.content.trim().to_string();
        if message.role == Role::Tool {
            if let Some(id) = &message.tool_call_id {
                body = format!("(tool result for {id})\n{body}");
            }
        }
        if !message.tool_calls.is_empty() {
            let calls = message
                .tool_calls
                .iter()
                .map(|tc| format!("{}({})", tc.name, tc.arguments))
                .collect::<Vec<_>>()
                .join(", ");
            if body.is_empty() {
                body = format!("(requested tools: {calls})");
            } else {
                body.push_str(&format!("\n(requested tools: {calls})"));
            }
        }
        parts.push(format!("{role}:\n{body}"));
    }
    parts.join("\n\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamControl {
    Continue,
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pump::TurnIo;

    struct CancelOnIdle;

    impl TurnIo for CancelOnIdle {
        fn on_delta(&mut self, _delta: &str) -> bool {
            true
        }
        fn on_idle(&mut self) -> bool {
            false
        }
        fn stream_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn usage_from_object_maps_input_output_tokens() {
        let usage = serde_json::json!({
            "input_tokens": 120,
            "output_tokens": 45
        });
        let parsed = usage_from_object(&usage).expect("usage");
        assert_eq!(parsed.prompt_tokens, 120);
        assert_eq!(parsed.completion_tokens, 45);
        assert_eq!(parsed.total_tokens, 165);
    }

    #[test]
    fn run_ndjson_turn_accepts_empty_text_when_cancelled() {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let mut io = CancelOnIdle;
        let result = run_ndjson_turn(cmd, &mut io, |_v, _r, _io| Ok(StreamControl::Continue));
        assert!(
            result.is_ok(),
            "cancelled turn should not error: {result:?}"
        );
        assert!(result.unwrap().text.is_empty());
    }
}
