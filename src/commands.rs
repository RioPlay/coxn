use crate::session;

/// A slash command typed into the input line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Quit,
    Clear,
    /// `/model` lists; `/model <name|#>` switches.
    Model(Option<String>),
    Tools,
    /// `/session` lists saved sessions.
    Session,
    /// `/resume <slug>` loads a saved session.
    Resume(Option<String>),
    /// `/edit [path]` opens the last-edited file (or `path`) in `$EDITOR`.
    OpenEditor(Option<String>),
    /// `/think [off|low|med|high]` sets the reasoning-effort level.
    Think(Option<String>),
    /// `/agents` shows the task partition (sub-scopes + routed models).
    Agents,
    /// `/scope` shows the active task scope from the environment.
    Scope,
    /// `/trust` toggles read_file session-auto approval.
    Trust,
    /// `/copy` writes the transcript to disk.
    Copy,
    /// `/auth status|setup [preset]|login <id>|set-key <id>` — provider auth helpers.
    Auth(Vec<String>),
    /// `/execute` runs the aden task partition sequentially (BatchIo).
    Execute {
        resume: bool,
    },
    /// `/runs` lists run ledgers; `/runs <slug>` summarizes one.
    Runs(Option<String>),
    Unknown(String),
}

/// Parse a leading-slash input into a command. Pure and testable.
pub(crate) fn parse_command(input: &str) -> Command {
    let mut words = input.trim_start_matches('/').split_whitespace();
    let word = words.next().unwrap_or("");
    let args: Vec<String> = words.map(|s| s.to_string()).collect();
    let arg = args.first().cloned();
    match word {
        "help" | "h" | "?" => Command::Help,
        "quit" | "q" | "exit" => Command::Quit,
        "clear" => Command::Clear,
        "model" => Command::Model(arg),
        "tools" => Command::Tools,
        "session" | "sessions" => Command::Session,
        "resume" => Command::Resume(arg),
        "edit" => Command::OpenEditor(arg),
        "think" => Command::Think(arg),
        "agents" => Command::Agents,
        "scope" => Command::Scope,
        "trust" => Command::Trust,
        "copy" => Command::Copy,
        "auth" => Command::Auth(args),
        "execute" | "run-agents" => Command::Execute {
            resume: args.iter().any(|a| a == "--resume"),
        },
        "runs" => Command::Runs(arg),
        other => Command::Unknown(other.to_string()),
    }
}

/// Slash command verbs, for Tab completion and the fuzzy palette (M4).
pub(crate) const COMMANDS: &[&str] = &[
    "help", "model", "auth", "think", "tools", "agents", "execute", "scope", "trust", "copy",
    "session", "resume", "runs", "edit", "clear", "quit",
];

/// The longest common prefix of `items` (empty if they share none).
fn longest_common_prefix(items: &[&str]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for s in &items[1..] {
        end = end.min(s.len());
        while !s.is_char_boundary(end) || first[..end] != s[..end] {
            end -= 1;
        }
    }
    first[..end].to_string()
}

/// Tab-complete a slash-command input: the command verb, or a `/resume` slug.
/// Returns the completed line, or `None` when there is nothing to add. Model
/// names are completed via the `/model` picker, not here.
pub(crate) fn complete_input(input: &str) -> Option<String> {
    let rest = input.strip_prefix('/')?;
    match rest.split_once(' ') {
        None => {
            let cands: Vec<&str> = COMMANDS
                .iter()
                .copied()
                .filter(|c| c.starts_with(rest))
                .collect();
            match cands.as_slice() {
                [] => None,
                [only] => Some(format!("/{only} ")),
                many => {
                    let lcp = longest_common_prefix(many);
                    (lcp.len() > rest.len()).then(|| format!("/{lcp}"))
                }
            }
        }
        Some(("resume", arg)) => {
            let slugs: Vec<String> = session::list()
                .into_iter()
                .map(|s| s.slug)
                .filter(|s| s.starts_with(arg))
                .collect();
            let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();
            match refs.as_slice() {
                [] => None,
                [only] => Some(format!("/resume {only}")),
                many => {
                    let lcp = longest_common_prefix(many);
                    (lcp.len() > arg.len()).then(|| format!("/resume {lcp}"))
                }
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_completes_command_verbs() {
        assert_eq!(complete_input("/mod").as_deref(), Some("/model "));
        assert_eq!(complete_input("/he").as_deref(), Some("/help "));
        assert_eq!(complete_input("/t"), None);
        assert_eq!(complete_input("/zzz"), None);
        assert_eq!(complete_input("hello"), None);
    }

    #[test]
    fn longest_common_prefix_of_candidates() {
        assert_eq!(longest_common_prefix(&["think", "tools"]), "t");
        assert_eq!(longest_common_prefix(&["model"]), "model");
        assert_eq!(longest_common_prefix(&["abc", "abd", "abe"]), "ab");
        assert_eq!(longest_common_prefix(&["x", "y"]), "");
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn parse_command_maps_aliases_and_unknowns() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command("/?"), Command::Help);
        assert_eq!(parse_command("/q"), Command::Quit);
        assert_eq!(parse_command("/clear"), Command::Clear);
        assert_eq!(parse_command("/tools"), Command::Tools);
        assert_eq!(
            parse_command("/auth login openrouter"),
            Command::Auth(vec!["login".to_string(), "openrouter".to_string()])
        );
        assert_eq!(
            parse_command("/auth setup openrouter-claude"),
            Command::Auth(vec!["setup".to_string(), "openrouter-claude".to_string()])
        );
        assert_eq!(parse_command("/model"), Command::Model(None));
        assert_eq!(
            parse_command("/model gpt"),
            Command::Model(Some("gpt".to_string()))
        );
        assert_eq!(
            parse_command("/model 3"),
            Command::Model(Some("3".to_string()))
        );
        assert_eq!(
            parse_command("/model @scout"),
            Command::Model(Some("@scout".to_string()))
        );
        assert_eq!(
            parse_command("/bogus"),
            Command::Unknown("bogus".to_string())
        );
        assert_eq!(parse_command("/runs"), Command::Runs(None));
        assert_eq!(
            parse_command("/runs fix-parser-1"),
            Command::Runs(Some("fix-parser-1".to_string()))
        );
        assert_eq!(
            parse_command("/execute"),
            Command::Execute { resume: false }
        );
        assert_eq!(
            parse_command("/execute --resume"),
            Command::Execute { resume: true }
        );
    }
}
