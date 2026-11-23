//! Asking for what `visdom init` was not told on the command line. Only ever
//! used when both stdin and stderr are a terminal; without one, init behaves
//! as if nothing could be asked and says what to pass instead.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use tracevault_cli::agent::Agent;

use crate::mode::TracingMode;

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

/// The mode an answer names: its number in the list, or its name. An empty
/// answer takes the default, hooks. `None` when it names neither.
pub fn parse_mode_choice(answer: &str) -> Option<TracingMode> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "hooks" => Some(TracingMode::Hooks),
        "2" | "gateway" => Some(TracingMode::Gateway),
        _ => None,
    }
}

/// Ask how sessions should be traced. `None` at end of input: nothing chosen.
pub fn ask_mode() -> Option<TracingMode> {
    loop {
        let answer = ask_line(
            "How should your sessions be traced? [1] hooks  [2] through the AI Gateway  (default 1):",
        )?;
        match parse_mode_choice(&answer) {
            Some(mode) => return Some(mode),
            None => eprintln!("Answer 1 or 2."),
        }
    }
}

/// The entry of a list of `count` an answer names by its number, from 1.
pub fn parse_choice(answer: &str, count: usize) -> Option<usize> {
    let n: usize = answer.trim().parse().ok()?;
    (1..=count).contains(&n).then(|| n - 1)
}

/// Ask for one of `options`. `None` at end of input, or with nothing to
/// choose from.
pub fn ask_choice(question: &str, options: &[String]) -> Option<usize> {
    if options.is_empty() {
        return None;
    }
    eprintln!("{question}");
    for (i, option) in options.iter().enumerate() {
        eprintln!("  [{}] {option}", i + 1);
    }
    loop {
        let answer = ask_line("Number:")?;
        match parse_choice(&answer, options.len()) {
            Some(index) => return Some(index),
            None => eprintln!("Answer a number from 1 to {}.", options.len()),
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
    save_user_setting(path, "bazaar_url", url)
}

/// Record `tracing_mode` in the user config at `path`, the same way.
pub fn save_tracing_mode(path: &Path, mode: TracingMode) -> Result<(), String> {
    save_user_setting(path, "tracing_mode", mode.name())
}

/// Record `key` under `[gateway]` in the user config at `path`, creating the
/// file if needed. A file with no `[gateway]` table gets one at its end and
/// is otherwise kept byte for byte; one that has it is rewritten from its
/// parsed form.
pub fn save_gateway_setting(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut table: toml::Table =
        toml::from_str(&existing).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let value = toml::Value::String(value.to_string());
    let updated = match table.get_mut("gateway") {
        Some(toml::Value::Table(gateway)) => {
            gateway.insert(key.to_string(), value);
            toml::to_string(&table).map_err(|e| e.to_string())?
        }
        Some(_) => return Err(format!("`gateway` in {} is not a table", path.display())),
        None => {
            let gap = match existing.is_empty() || existing.ends_with("\n\n") {
                true => "",
                false if existing.ends_with('\n') => "\n",
                false => "\n\n",
            };
            format!("{existing}{gap}[gateway]\n{key} = {value}\n")
        }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, updated).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Set the top-level string `key` in the user config at `path`, creating the
/// file if needed and leaving everything else in it as it is.
fn save_user_setting(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut table: toml::Table =
        toml::from_str(&existing).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let line = format!("{key} = {}\n", toml::Value::String(value.to_string()));
    let updated = if table.contains_key(key) {
        // Already there (empty, or it would not have been asked for): the
        // file is rewritten from its parsed form.
        table.insert(key.to_string(), toml::Value::String(value.to_string()));
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
    fn mode_choice_by_number_name_or_default() {
        assert_eq!(parse_mode_choice(""), Some(TracingMode::Hooks));
        assert_eq!(parse_mode_choice("1"), Some(TracingMode::Hooks));
        assert_eq!(parse_mode_choice(" Gateway "), Some(TracingMode::Gateway));
        assert_eq!(parse_mode_choice("2"), Some(TracingMode::Gateway));
        assert_eq!(parse_mode_choice("3"), None);
        assert_eq!(parse_mode_choice("proxy"), None);
    }

    #[test]
    fn the_mode_is_saved_next_to_the_other_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::config::user_config_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = "agent = \"codex\"\n\n[gateway]\nproject = \"alpha\"\n";
        std::fs::write(&path, before).unwrap();
        save_tracing_mode(&path, TracingMode::Gateway).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("tracing_mode = \"gateway\"\n{before}")
        );
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.tracing_mode, Some(TracingMode::Gateway));
        assert_eq!(user.agent, Some(Agent::Codex));
        assert_eq!(user.gateway.project.as_deref(), Some("alpha"));
        // Choosing again replaces it.
        save_tracing_mode(&path, TracingMode::Hooks).unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.tracing_mode, Some(TracingMode::Hooks));
        assert_eq!(user.gateway.project.as_deref(), Some("alpha"));
    }

    #[test]
    fn a_choice_is_a_number_of_the_list() {
        assert_eq!(parse_choice("1", 3), Some(0));
        assert_eq!(parse_choice(" 3\n", 3), Some(2));
        for answer in ["0", "4", "", "alpha", "-1"] {
            assert_eq!(parse_choice(answer, 3), None, "{answer}");
        }
    }

    #[test]
    fn a_gateway_setting_is_saved_without_touching_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("visdom").join("config.toml");
        // A new file.
        save_gateway_setting(&path, "project", "alpha").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[gateway]\nproject = \"alpha\"\n"
        );
        // A second setting, and the first one replaced.
        save_gateway_setting(&path, "claude_code_model", "claude-sonnet").unwrap();
        save_gateway_setting(&path, "project", "beta").unwrap();
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.gateway.project.as_deref(), Some("beta"));
        assert_eq!(
            user.gateway.claude_code_model.as_deref(),
            Some("claude-sonnet")
        );

        // A file with no [gateway] keeps every byte, comments included.
        let before =
            "# mine\ntracing_mode = \"gateway\"\n\n[agents]\ncodex = \"cdx\" # local build";
        std::fs::write(&path, before).unwrap();
        save_gateway_setting(&path, "codex_model", "gpt-general").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            format!("{before}\n\n[gateway]\ncodex_model = \"gpt-general\"\n")
        );
        let user = load_user_config_in(tmp.path()).unwrap();
        assert_eq!(user.gateway.codex_model.as_deref(), Some("gpt-general"));
        assert_eq!(user.agents.codex.as_deref(), Some("cdx"));

        // One that cannot be parsed is left alone.
        std::fs::write(&path, "[gateway\n").unwrap();
        assert!(save_gateway_setting(&path, "project", "alpha").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[gateway\n");
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
