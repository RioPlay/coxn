//! Vim-style ex commands (`:cmd`) parsed from the command line.

/// An ex-style command (`:cmd`) parsed from the vim command line.
///
/// Keeps model/aden dispatch separate from the slash-command path so the two
/// can evolve independently. Every variant corresponds to a single user intent;
/// the `drive` loop calls existing functions to satisfy each one.
#[derive(Debug, PartialEq)]
pub(super) enum ExCmd {
    /// `:q` / `:quit` — exit coxn.
    Quit,
    /// `:h` / `:help` — show the help text.
    Help,
    /// `:model [name]` — list (no arg) or switch model.
    Model(Option<String>),
    /// `:tools` — list active tools.
    Tools,
    /// `:clear` / `:new` — clear the conversation and start fresh.
    Clear,
    /// `:understand <sym>` — run `aden understand` and append the result.
    Understand(String),
    /// `:grep <pattern>` — run `aden grep` and append the result.
    Grep(String),
    /// `:ask <text>` — run `aden ask` and append the result.
    Ask(String),
    /// `:view [anchor]` — launch aden browser view (centered on anchor if given).
    View(Option<String>),
    /// `:viz [anchor]` or `:mermaid [anchor]` — export Mermaid diagram text.
    Viz(Option<String>),
    /// `:doctor` — run aden doctor for env + repo diagnostics.
    Doctor,
    /// `:impact <sym>` — blast radius / downstream via aden query (gi style).
    Impact(String),
    /// `:communities` — list functional communities from aden.
    Communities,
    /// `:audit` — aden security audit.
    Audit,
    /// Unknown command — append a notice.
    Unknown(String),
}

/// Parse a `:command` string (already without the leading colon) into an
/// [`ExCmd`]. Pure and unit-testable; contains no side effects.
pub(super) fn parse_ex_command(input: &str) -> ExCmd {
    let trimmed = input.trim();
    let mut words = trimmed.splitn(2, char::is_whitespace);
    let verb = words.next().unwrap_or("");
    // The rest after the verb, trimmed of leading whitespace.
    let rest = words.next().map(|s| s.trim()).unwrap_or("");
    let arg = if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    };
    match verb {
        "q" | "quit" => ExCmd::Quit,
        "h" | "help" => ExCmd::Help,
        "model" => ExCmd::Model(arg),
        "tools" => ExCmd::Tools,
        "clear" | "new" => ExCmd::Clear,
        "understand" => match arg {
            Some(sym) => ExCmd::Understand(sym),
            None => ExCmd::Unknown("understand requires a symbol name".to_string()),
        },
        "grep" => match arg {
            Some(pat) => ExCmd::Grep(pat),
            None => ExCmd::Unknown("grep requires a pattern".to_string()),
        },
        "ask" => match arg {
            Some(text) => ExCmd::Ask(text),
            None => ExCmd::Unknown("ask requires a question".to_string()),
        },
        "view" => ExCmd::View(arg),
        "viz" | "mermaid" | "gm" => ExCmd::Viz(arg),
        "doctor" => ExCmd::Doctor,
        "impact" => match arg {
            Some(sym) => ExCmd::Impact(sym),
            None => ExCmd::Unknown("impact requires a symbol".to_string()),
        },
        "communities" => ExCmd::Communities,
        "audit" => ExCmd::Audit,
        other => ExCmd::Unknown(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ex_quit_aliases() {
        assert_eq!(parse_ex_command("q"), ExCmd::Quit);
        assert_eq!(parse_ex_command("quit"), ExCmd::Quit);
    }

    #[test]
    fn ex_help_aliases() {
        assert_eq!(parse_ex_command("h"), ExCmd::Help);
        assert_eq!(parse_ex_command("help"), ExCmd::Help);
    }

    #[test]
    fn ex_model_no_arg_and_with_arg() {
        assert_eq!(parse_ex_command("model"), ExCmd::Model(None));
        assert_eq!(
            parse_ex_command("model gpt-4"),
            ExCmd::Model(Some("gpt-4".to_string()))
        );
        assert_eq!(
            parse_ex_command("model 2"),
            ExCmd::Model(Some("2".to_string()))
        );
    }

    #[test]
    fn ex_tools() {
        assert_eq!(parse_ex_command("tools"), ExCmd::Tools);
    }

    #[test]
    fn ex_clear_and_new() {
        assert_eq!(parse_ex_command("clear"), ExCmd::Clear);
        assert_eq!(parse_ex_command("new"), ExCmd::Clear);
    }

    #[test]
    fn ex_understand_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("understand Vim"),
            ExCmd::Understand("Vim".to_string())
        );
        assert!(matches!(parse_ex_command("understand"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_grep_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("grep fn drive"),
            ExCmd::Grep("fn drive".to_string())
        );
        assert!(matches!(parse_ex_command("grep"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_ask_with_and_without_arg() {
        assert_eq!(
            parse_ex_command("ask how does the pump work"),
            ExCmd::Ask("how does the pump work".to_string())
        );
        assert!(matches!(parse_ex_command("ask"), ExCmd::Unknown(_)));
    }

    #[test]
    fn ex_view_viz_doctor() {
        assert_eq!(parse_ex_command("view"), ExCmd::View(None));
        assert_eq!(
            parse_ex_command("view Foo"),
            ExCmd::View(Some("Foo".to_string()))
        );
        assert_eq!(parse_ex_command("viz"), ExCmd::Viz(None));
        assert_eq!(
            parse_ex_command("mermaid Bar"),
            ExCmd::Viz(Some("Bar".to_string()))
        );
        assert_eq!(parse_ex_command("gm"), ExCmd::Viz(None));
        assert_eq!(parse_ex_command("doctor"), ExCmd::Doctor);
        assert_eq!(
            parse_ex_command("impact Foo"),
            ExCmd::Impact("Foo".to_string())
        );
        assert!(matches!(parse_ex_command("impact"), ExCmd::Unknown(_)));
        assert_eq!(parse_ex_command("communities"), ExCmd::Communities);
        assert_eq!(parse_ex_command("audit"), ExCmd::Audit);
    }

    #[test]
    fn ex_unknown_and_empty() {
        assert!(matches!(
            parse_ex_command("bogus"),
            ExCmd::Unknown(s) if s == "bogus"
        ));
        assert!(matches!(
            parse_ex_command(""),
            ExCmd::Unknown(s) if s.is_empty()
        ));
    }

    #[test]
    fn ex_leading_trailing_spaces_are_trimmed() {
        assert_eq!(parse_ex_command("  quit  "), ExCmd::Quit);
        assert_eq!(
            parse_ex_command("  model   some-model  "),
            ExCmd::Model(Some("some-model".to_string()))
        );
    }
}
