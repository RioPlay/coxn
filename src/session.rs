//! Append-only JSONL session persistence.
//!
//! A session is one file under the per-user data dir: one JSON line per
//! [`Message`], appended as the conversation grows. Append-only gives crash
//! safety (a kill loses nothing committed) and trivial loading. The aden context
//! is never stored inline -- only the user/assistant/tool turns -- so files stay
//! small. Flat by design: no tree/branch (aden's scope-per-task already gives
//! each task its own context).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::Message;

/// Seconds since the Unix epoch, or 0 if the clock is before it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a slug is safe to use as a filename: non-empty and only alphanumeric,
/// `-`, or `_`. Rejects path-traversal (`..`, `/`, absolute) so a `/resume <slug>`
/// cannot escape the sessions directory and create/append to an arbitrary file.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The sessions directory: `$XDG_DATA_HOME/coxn/sessions` or
/// `~/.local/share/coxn/sessions`. `None` if neither is set.
fn sessions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("coxn").join("sessions"))
}

/// An open append-only session file. Created at startup; the drive loop appends
/// each new message. A write failure is swallowed (persistence is best-effort and
/// must never break the live loop).
pub struct Session {
    path: PathBuf,
    file: Option<File>,
}

impl Session {
    /// Open a fresh session file named by the current epoch seconds. Returns a
    /// no-op session (no file) if the data dir is unavailable or uncreatable, so
    /// the caller never has to handle persistence failure.
    pub fn create() -> Session {
        let path = match sessions_dir() {
            Some(dir) => {
                let _ = fs::create_dir_all(&dir);
                dir.join(format!("{}.jsonl", now_secs()))
            }
            None => PathBuf::new(),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Session { path, file }
    }

    /// Reopen an existing session by slug for continued appending (`/resume`).
    /// Falls back to a no-op session if the slug is unsafe or the data dir is
    /// unavailable.
    pub fn open(slug: &str) -> Session {
        let path = match sessions_dir() {
            Some(dir) if valid_slug(slug) => dir.join(format!("{slug}.jsonl")),
            _ => PathBuf::new(),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Session { path, file }
    }

    /// The session slug (file stem), for display and `/resume`.
    pub fn slug(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    }

    /// Append one message as a JSON line. Best-effort: a serialize or write error
    /// is ignored rather than disturbing the turn.
    pub fn append(&mut self, message: &Message) {
        if let (Some(file), Ok(line)) = (self.file.as_mut(), serde_json::to_string(message)) {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// A persisted session, for the `/session` picker.
pub struct SessionInfo {
    pub slug: String,
    /// Seconds since the session file was last modified.
    pub age_secs: u64,
    /// The first user line, as a preview.
    pub preview: String,
}

/// List saved sessions, most-recently-modified first. Empty when the dir is
/// missing or unreadable.
pub fn list() -> Vec<SessionInfo> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = now_secs();
    let mut sessions: Vec<(u64, SessionInfo)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let slug = path.file_stem()?.to_str()?.to_string();
            let modified = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some((
                modified,
                SessionInfo {
                    slug,
                    age_secs: now.saturating_sub(modified),
                    preview: first_user_line(&path),
                },
            ))
        })
        .collect();
    sessions.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    sessions.into_iter().map(|(_, info)| info).collect()
}

/// Load a session's messages by slug. Lines that do not parse are skipped, so a
/// partially-written tail (from a crash) does not abort the load.
pub fn load(slug: &str) -> Vec<Message> {
    if !valid_slug(slug) {
        return Vec::new();
    }
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let path = dir.join(format!("{slug}.jsonl"));
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Message>(&l).ok())
        .collect()
}

/// The first user message's text in a session file, for the picker preview.
fn first_user_line(path: &std::path::Path) -> String {
    let Ok(file) = File::open(path) else {
        return String::new();
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(m) = serde_json::from_str::<Message>(&line)
            && m.role == crate::model::Role::User
        {
            return m.content.chars().take(60).collect();
        }
    }
    String::new()
}

/// Format an age in seconds as a compact relative label (now, 5m, 3h, 2d, 1w).
pub fn relative_age(secs: u64) -> String {
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        86400..=604799 => format!("{}d", secs / 86400),
        _ => format!("{}w", secs / 604800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    #[test]
    fn slug_validation_rejects_path_traversal() {
        assert!(valid_slug("1782360134"));
        assert!(valid_slug("my-session_2"));
        // Traversal / separators / empties are refused.
        assert!(!valid_slug("../../.bashrc"));
        assert!(!valid_slug("a/b"));
        assert!(!valid_slug("..")); // would escape the sessions dir
        assert!(!valid_slug(""));
        // An unsafe slug opens a no-op session (no file) and loads nothing.
        assert!(Session::open("../escape").slug().is_empty());
        assert!(load("../escape").is_empty());
    }

    #[test]
    fn relative_age_buckets() {
        assert_eq!(relative_age(10), "now");
        assert_eq!(relative_age(300), "5m");
        assert_eq!(relative_age(7200), "2h");
        assert_eq!(relative_age(172800), "2d");
        assert_eq!(relative_age(1209600), "2w");
    }

    #[test]
    fn append_then_load_round_trips() {
        // Point the data dir at a temp location for this test.
        let tmp = std::env::temp_dir().join(format!("coxn-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: single-threaded test; set XDG_DATA_HOME so sessions_dir resolves here.
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

        let mut s = Session::create();
        let slug = s.slug();
        s.append(&Message::new(Role::User, "hello"));
        s.append(&Message::new(Role::Assistant, "hi there"));

        let loaded = load(&slug);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[0].content, "hello");
        assert_eq!(loaded[1].content, "hi there");

        let listed = list();
        assert!(
            listed
                .iter()
                .any(|i| i.slug == slug && i.preview == "hello")
        );

        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
    }
}
