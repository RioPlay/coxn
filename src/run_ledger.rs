use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::model::{ToolCall, Usage};
use crate::pump::{Approval, TurnIo};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn runs_dir() -> Option<PathBuf> {
    // Explicit override (also used by tests to avoid contending on XDG_DATA_HOME,
    // which other modules mutate concurrently).
    if let Some(dir) = std::env::var_os("COXN_RUNS_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("coxn").join("runs"))
}

fn slug_part(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == '-' || c == '_' || c.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "run".to_string()
    } else {
        out.to_string()
    }
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            if lower.contains("api_key")
                || lower.contains("auth_token")
                || lower.contains("secret")
                || lower.starts_with("sk-")
            {
                Value::String("[redacted]".to_string())
            } else {
                Value::String(s)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let lower = k.to_ascii_lowercase();
                    if lower.contains("key")
                        || lower.contains("token")
                        || lower.contains("secret")
                        || lower.contains("password")
                    {
                        (k, Value::String("[redacted]".to_string()))
                    } else {
                        (k, redact_value(v))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

pub(crate) struct RunLedger {
    run: String,
    file: Option<File>,
}

impl RunLedger {
    pub(crate) fn create(task: &str) -> Self {
        let run = format!("{}-{}", slug_part(task), now_secs());
        let file = runs_dir().and_then(|dir| {
            let _ = fs::create_dir_all(&dir);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{run}.jsonl")))
                .ok()
        });
        Self { run, file }
    }

    pub(crate) fn open(slug: &str) -> Self {
        let file = runs_dir().and_then(|dir| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{slug}.jsonl")))
                .ok()
        });
        Self {
            run: slug.to_string(),
            file,
        }
    }

    pub(crate) fn run(&self) -> &str {
        &self.run
    }

    pub(crate) fn append(
        &mut self,
        kind: &str,
        scope: Option<&str>,
        role: Option<&str>,
        data: Value,
    ) {
        let Some(file) = &mut self.file else {
            return;
        };
        let event = json!({
            "ts": now_secs(),
            "run": self.run,
            "kind": kind,
            "scope": scope,
            "role": role,
            "data": redact_value(data),
        });
        let _ = writeln!(file, "{event}");
    }
}

fn approval_label(decision: Approval) -> &'static str {
    match decision {
        Approval::Allow => "allow",
        Approval::Decline => "decline",
        Approval::CancelTurn => "cancel_turn",
    }
}

fn record_tool_result(
    ledger: &mut RunLedger,
    scope: &str,
    role: &str,
    call: &ToolCall,
    result: &str,
) {
    ledger.append(
        "tool_result",
        Some(scope),
        Some(role),
        json!({ "tool": call.name, "chars": result.chars().count() }),
    );
    if result.contains("EDIT BLOCKED") || result.contains("COMMAND BLOCKED") {
        ledger.append(
            "gate_verdict",
            Some(scope),
            Some(role),
            json!({ "tool": call.name, "verdict": "blocked" }),
        );
    } else if matches!(call.name.as_str(), "edit" | "write_file")
        && !result.contains("declined")
        && !result.contains("cancelled")
    {
        ledger.append(
            "file_edit",
            Some(scope),
            Some(role),
            json!({ "tool": call.name, "chars": result.chars().count() }),
        );
    }
}

/// Wrap any [`TurnIo`] and append pump-boundary facts to a run ledger.
pub(crate) struct LedgerTurnIo<'a, I: ?Sized> {
    inner: &'a mut I,
    ledger: &'a mut RunLedger,
    scope: &'a str,
    role: &'a str,
}

impl<'a, I: ?Sized> LedgerTurnIo<'a, I> {
    pub(crate) fn new(
        inner: &'a mut I,
        ledger: &'a mut RunLedger,
        scope: &'a str,
        role: &'a str,
    ) -> Self {
        Self {
            inner,
            ledger,
            scope,
            role,
        }
    }
}

impl<I: TurnIo + ?Sized> TurnIo for LedgerTurnIo<'_, I> {
    fn on_delta(&mut self, delta: &str) -> bool {
        self.ledger.append(
            "assistant_delta",
            Some(self.scope),
            Some(self.role),
            json!({ "chars": delta.chars().count() }),
        );
        self.inner.on_delta(delta)
    }

    fn approve(&mut self, call: &ToolCall) -> Approval {
        let decision = self.inner.approve(call);
        self.ledger.append(
            "approval",
            Some(self.scope),
            Some(self.role),
            json!({ "tool": call.name, "decision": approval_label(decision) }),
        );
        decision
    }

    fn on_run_output(&mut self, line: &str) -> bool {
        self.ledger.append(
            "command_output",
            Some(self.scope),
            Some(self.role),
            json!({ "chars": line.chars().count() }),
        );
        self.inner.on_run_output(line)
    }

    fn on_tool_call(&mut self, call: &ToolCall) {
        self.ledger.append(
            "tool_call",
            Some(self.scope),
            Some(self.role),
            json!({ "tool": call.name }),
        );
        self.inner.on_tool_call(call);
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &str) {
        record_tool_result(self.ledger, self.scope, self.role, call, result);
        self.inner.on_tool_result(call, result);
    }

    fn on_usage(&mut self, usage: Usage) {
        self.ledger.append(
            "usage",
            Some(self.scope),
            Some(self.role),
            json!({
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            }),
        );
        self.inner.on_usage(usage);
    }

    fn on_idle(&mut self) -> bool {
        self.inner.on_idle()
    }

    fn stream_cancelled(&self) -> bool {
        self.inner.stream_cancelled()
    }
}

pub(crate) fn list() -> Vec<String> {
    let Some(dir) = runs_dir() else {
        return Vec::new();
    };
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(Result::ok)
            .filter_map(|e| {
                let path = e.path();
                let slug = path.file_stem()?.to_str()?.to_string();
                let modified = e.metadata().and_then(|m| m.modified()).ok();
                Some((slug, modified))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    entries.into_iter().map(|(slug, _)| slug).take(20).collect()
}

pub(crate) fn latest_for_task(task: &str) -> Option<String> {
    for slug in list() {
        let path = runs_dir()?.join(format!("{slug}.jsonl"));
        let file = File::open(path).ok()?;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("kind").and_then(Value::as_str) == Some("run_started")
                && v.get("data")
                    .and_then(|d| d.get("task"))
                    .and_then(Value::as_str)
                    == Some(task)
            {
                return Some(slug);
            }
        }
    }
    None
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScopeStatus {
    pub(crate) status: String,
    pub(crate) result: String,
}

pub(crate) fn scope_statuses(slug: &str) -> std::collections::BTreeMap<String, ScopeStatus> {
    let Some(dir) = runs_dir() else {
        return std::collections::BTreeMap::new();
    };
    let Ok(file) = File::open(dir.join(format!("{slug}.jsonl"))) else {
        return std::collections::BTreeMap::new();
    };
    let mut statuses = std::collections::BTreeMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(scope) = v.get("scope").and_then(Value::as_str) else {
            continue;
        };
        match v.get("kind").and_then(Value::as_str) {
            Some("scope_started") => {
                statuses.entry(scope.to_string()).or_insert(ScopeStatus {
                    status: "interrupted".to_string(),
                    result: String::new(),
                });
            }
            Some("scope_finished") => {
                let data = v.get("data").unwrap_or(&Value::Null);
                statuses.insert(
                    scope.to_string(),
                    ScopeStatus {
                        status: data
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        result: data
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    },
                );
            }
            _ => {}
        }
    }
    statuses
}

pub(crate) fn summarize(slug: &str) -> Result<String, String> {
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid run slug".to_string());
    }
    let path = runs_dir()
        .ok_or_else(|| "run directory unavailable".to_string())?
        .join(format!("{slug}.jsonl"));
    let file = File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    let mut scopes = std::collections::BTreeSet::<String>::new();
    let mut final_status = "unknown".to_string();
    let mut task = String::new();
    let mut mode = String::new();
    let mut models = std::collections::BTreeSet::<String>::new();
    let mut approvals_allow = 0usize;
    let mut approvals_decline = 0usize;
    let mut gate_blocks = 0usize;
    let mut file_edits = 0usize;
    let mut usage_total = 0u64;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let data = v.get("data").unwrap_or(&Value::Null);
        if let Some(kind) = v.get("kind").and_then(Value::as_str) {
            *kinds.entry(kind.to_string()).or_default() += 1;
            match kind {
                "run_started" => {
                    if let Some(t) = data.get("task").and_then(Value::as_str) {
                        task = t.to_string();
                    }
                    if let Some(m) = data.get("mode").and_then(Value::as_str) {
                        mode = m.to_string();
                    }
                }
                "run_finished" => {
                    if let Some(status) = data.get("status").and_then(Value::as_str) {
                        final_status = status.to_string();
                    }
                }
                "model_selected" => {
                    if let Some(m) = data.get("model").and_then(Value::as_str) {
                        models.insert(m.to_string());
                    }
                }
                "approval" => match data.get("decision").and_then(Value::as_str) {
                    Some("allow") => approvals_allow += 1,
                    Some("decline") => approvals_decline += 1,
                    _ => {}
                },
                "gate_verdict" => gate_blocks += 1,
                "file_edit" => file_edits += 1,
                "usage" => {
                    if let Some(t) = data.get("total_tokens").and_then(Value::as_u64) {
                        usage_total += t;
                    }
                }
                _ => {}
            }
        }
        if let Some(scope) = v.get("scope").and_then(Value::as_str) {
            scopes.insert(scope.to_string());
        }
    }
    let counts = kinds
        .into_iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!("run: {slug}\nstatus: {final_status}");
    if !task.is_empty() {
        out.push_str(&format!("\ntask: {task}"));
    }
    if !mode.is_empty() {
        out.push_str(&format!("\nmode: {mode}"));
    }
    out.push_str(&format!(
        "\nscopes: {}",
        scopes.into_iter().collect::<Vec<_>>().join(", ")
    ));
    if !models.is_empty() {
        out.push_str(&format!(
            "\nmodels: {}",
            models.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    out.push_str(&format!(
        "\napprovals: allow={approvals_allow} decline={approvals_decline}  gate_blocks={gate_blocks}  file_edits={file_edits}"
    ));
    if usage_total > 0 {
        out.push_str(&format!("\nusage_total_tokens: {usage_total}"));
    }
    out.push_str(&format!("\nevents: {counts}"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ledger_appends_and_summarizes_events() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("coxn-runs-{}", std::process::id()));
        unsafe { std::env::set_var("COXN_RUNS_DIR", &tmp) };
        let mut ledger = RunLedger::create("Fix Parser");
        let slug = ledger.run().to_string();
        ledger.append("run_started", None, None, json!({"api_key":"sk-test"}));
        ledger.append("scope_started", Some("s1"), Some("scout"), json!({}));
        ledger.append(
            "scope_finished",
            Some("s1"),
            Some("scout"),
            json!({"status":"success", "result":"done"}),
        );
        ledger.append("run_finished", None, None, json!({"status":"success"}));
        drop(ledger);

        let summary = summarize(&slug).expect("summary");
        assert!(summary.contains("status: success"));
        assert!(summary.contains("s1"));
        assert!(summary.contains("gate_blocks="));
        let path = runs_dir().unwrap().join(format!("{slug}.jsonl"));
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("[redacted]"));
        assert!(!raw.contains("sk-test"));
        unsafe { std::env::remove_var("COXN_RUNS_DIR") };
        let _ = std::fs::remove_dir_all(tmp);
    }

    struct FakeIo {
        deltas: usize,
    }

    impl TurnIo for FakeIo {
        fn on_delta(&mut self, _delta: &str) -> bool {
            self.deltas += 1;
            true
        }
        fn approve(&mut self, _call: &ToolCall) -> Approval {
            Approval::Decline
        }
    }

    #[test]
    fn ledger_turn_io_records_approval_and_gate_block() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("coxn-ledger-io-{}", std::process::id()));
        unsafe { std::env::set_var("COXN_RUNS_DIR", &tmp) };
        let mut ledger = RunLedger::create("ledger-io-test");
        let slug = ledger.run().to_string();
        let mut inner = FakeIo { deltas: 0 };
        let call = ToolCall {
            id: "t1".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        };
        {
            let mut io = LedgerTurnIo::new(&mut inner, &mut ledger, "chat", "main");
            assert!(io.on_delta("hi"));
            assert_eq!(io.approve(&call), Approval::Decline);
            io.on_tool_result(
                &call,
                "EDIT BLOCKED by aden gate: scope-escape. The change was reverted.",
            );
        }
        drop(ledger);
        let summary = summarize(&slug).expect("summary");
        assert!(summary.contains("approvals: allow=0 decline=1"));
        assert!(summary.contains("gate_blocks=1"));
        unsafe { std::env::remove_var("COXN_RUNS_DIR") };
        let _ = std::fs::remove_dir_all(tmp);
    }
}
