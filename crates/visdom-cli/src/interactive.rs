//! Asking for what `visdom init` was not told on the command line. Only ever
//! used when both stdin and stderr are a terminal; without one, init behaves
//! as if nothing could be asked and says what to pass instead.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use tracevault_cli::agent::Agent;

/// Whether there is somebody to ask.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Print `question` and read one line, trimmed. `None` at end of input.
fn ask_line(question: &str) -> Option<String> {
    eprint!("{question} ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// The agent an answer names: its number in the list, or its name. An empty
/// answer takes the default, Claude Code. `None` when it names neither.
pub fn parse_agent_choice(answer: &str) -> Option<Agent> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "claude" | "claude-code" => Some(Agent::ClaudeCode),
        "2" | "codex" => Some(Agent::Codex),
        _ => None,
    }
}

/// Ask which agent to set up. End of input takes the default.
pub fn ask_agent() -> Agent {
    loop {
        let Some(answer) = ask_line("Which agent? [1] Claude Code  [2] Codex  (default 1):") else {
            return Agent::ClaudeCode;
        };
        match parse_agent_choice(&answer) {
            Some(agent) => return agent,
            None => eprintln!("Answer 1 or 2."),
        }
    }
}

/// What a typed URL answer means.
#[derive(Debug, PartialEq, Eq)]
pub enum UrlAnswer {
    Url(String),
    /// Nothing typed.
    Skipped,
    Invalid,
}

/// A bare host is taken as https; anything that is then not an absolute
/// http(s) URL is invalid.
pub fn parse_url_answer(answer: &str) -> UrlAnswer {
    let answer = answer.trim();
    if answer.is_empty() {
        return UrlAnswer::Skipped;
    }
    if answer.starts_with('-') {
        return UrlAnswer::Invalid;
    }
    let candidate = if answer.contains("://") {
        answer.to_string()
    } else {
        format!("https://{answer}")
    };
    if crate::components::bazaar::is_http_url(&candidate) {
        UrlAnswer::Url(candidate.trim_end_matches('/').to_string())
    } else {
        UrlAnswer::Invalid
    }
}

/// Ask for a URL until one is given or the question is skipped (Enter, or
/// end of input).
pub fn ask_url(question: &str) -> Option<String> {
    loop {
        match parse_url_answer(&ask_line(question)?) {
            UrlAnswer::Url(url) => return Some(url),
            UrlAnswer::Skipped => return None,
            UrlAnswer::Invalid => eprintln!("That is not an http(s) URL."),
        }
    }
}

/// Record `bazaar_url` in the user config at `path`, creating the file if
/// needed and leaving everything else in it as it is.
pub fn save_bazaar_url(path: &Path, url: &str) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut table: toml::Table =
        toml::from_str(&existing).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let line = format!("bazaar_url = {}\n", toml::Value::String(url.to_string()));
    let updated = if table.contains_key("bazaar_url") {
        // Already there (empty, or it would not have been asked for): the
        // file is rewritten from its parsed form.
        table.insert(
            "bazaar_url".to_string(),
            toml::Value::String(url.to_string()),
        );
        toml::to_string(&table).map_err(|e| e.to_string())?
    } else {
        // A top-level key must come before the first table: put it first, and
        // keep the rest of the file, comments included, byte for byte.
        format!("{line}{existing}")
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, updated).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_user_config_in;

    #[test]
    fn agent_choice_by_number_name_or_default() {
        assert_eq!(parse_agent_choice(""), Some(Agent::ClaudeCode));
        assert_eq!(parse_agent_choice("1"), Some(Agent::ClaudeCode));
        assert_eq!(parse_agent_choice(" Claude-Code "), Some(Agent::ClaudeCode));
        assert_eq!(parse_agent_choice("2"), Some(Agent::Codex));
        assert_eq!(parse_agent_choice("codex"), Some(Agent::Codex));
        assert_eq!(parse_agent_choice("3"), None);
        assert_eq!(parse_agent_choice("amp"), None);
    }

    #[test]
    fn url_answers() {
        assert_eq!(parse_url_answer("  "), UrlAnswer::Skipped);
        assert_eq!(
            parse_url_answer("https://tv.example.com/"),
            UrlAnswer::Url("https://tv.example.com".to_string())
        );
        assert_eq!(
            parse_url_answer("tv.example.com"),
            UrlAnswer::Url("https://tv.example.com".to_string())
        );
        assert_eq!(
            parse_url_answer("http://localhost:3000/mcp/remote"),
            UrlAnswer::Url("http://localhost:3000/mcp/remote".to_string())
        );
        for bad in ["ftp://x.example.com", "--flag", "two words", "https://"] {
            assert_eq!(parse_url_answer(bad), UrlAnswer::Invalid, "{bad}");
        }
    }

    #[test]
    fn bazaar_url_is_saved_into_a_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::config::user_config_path_in(tmp.path());
        save_bazaar_url(&path, "https://ctx.example.com/mcp/remote").unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(
            user.bazaar_url.as_deref(),
            Some("https://ctx.example.com/mcp/remote")
        );
    }

    #[test]
    fn bazaar_url_is_added_without_touching_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::config::user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = "# my settings\nagent = \"codex\"\n\n[agents]\n# the nightly build\ncodex = \"/opt/codex\"\n";
        std::fs::write(&path, before).unwrap();
        save_bazaar_url(&path, "https://ctx.example.com/mcp").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            format!("bazaar_url = \"https://ctx.example.com/mcp\"\n{before}")
        );
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.agent, Some(Agent::Codex));
        assert_eq!(
            user.bazaar_url.as_deref(),
            Some("https://ctx.example.com/mcp")
        );
        assert_eq!(
            user.agents.program_for(Agent::Codex).as_deref(),
            Some("/opt/codex")
        );
    }

    #[test]
    fn an_empty_bazaar_url_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::config::user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "bazaar_url = \"\"\nagent = \"codex\"\n").unwrap();
        save_bazaar_url(&path, "https://ctx.example.com/mcp").unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(
            user.bazaar_url.as_deref(),
            Some("https://ctx.example.com/mcp")
        );
        assert_eq!(user.agent, Some(Agent::Codex));
    }

    #[test]
    fn a_config_that_cannot_be_parsed_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::config::user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "agent = [broken").unwrap();
        let err = save_bazaar_url(&path, "https://ctx.example.com/mcp").unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "agent = [broken");
    }
}
