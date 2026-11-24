//! Which command the hooks this CLI installs call.
//!
//! The library runs as two binaries: the `tracevault` command and, under
//! `visdom trace`, the `visdom` one that replaces it. Whatever one of them
//! installs for an agent or for git to run later — hooks, the telemetry
//! helper, plugins — has to name a command, and it names the one that
//! installed it: a machine set up with `visdom` alone has no `tracevault` on
//! `PATH`.
//!
//! The move goes one way. `visdom` rewrites `tracevault …` commands it finds
//! to `visdom trace …`; `tracevault` leaves `visdom trace …` commands as they
//! are (see [`installs_visdom`]), so an automatic refresh started by one
//! binary never undoes what the other set up.

use std::sync::atomic::{AtomicBool, Ordering};

static VISDOM: AtomicBool = AtomicBool::new(false);

/// How the `tracevault` binary spells a tracing command.
pub const TRACEVAULT: &str = "tracevault";
/// How the `visdom` binary spells one.
pub const VISDOM_TRACE: &str = "visdom trace";

/// Called once by the `visdom` binary before it runs anything.
pub fn run_as_visdom() {
    VISDOM.store(true, Ordering::Relaxed);
}

/// Whether this process is the `visdom` binary.
pub fn is_visdom() -> bool {
    VISDOM.load(Ordering::Relaxed)
}

/// What comes after the command name when `command` is one of ours
/// (`tracevault stream …` or `visdom trace stream …` → `stream …`).
pub fn subcommand(command: &str) -> Option<&str> {
    [VISDOM_TRACE, TRACEVAULT].iter().find_map(|name| {
        command
            .strip_prefix(name)
            .filter(|rest| rest.starts_with(' '))
            .map(str::trim_start)
    })
}

/// Whether `command` is one of ours and names the `visdom` binary.
pub fn is_visdom_command(command: &str) -> bool {
    command
        .strip_prefix(VISDOM_TRACE)
        .is_some_and(|rest| rest.starts_with(' '))
}

/// Whether what is being installed should call `visdom`: this is the `visdom`
/// binary, or what is already there (`existing_is_visdom`) does.
pub fn installs_visdom(existing_is_visdom: bool) -> bool {
    is_visdom() || existing_is_visdom
}

/// The command name to install.
pub fn name(visdom: bool) -> &'static str {
    if visdom {
        VISDOM_TRACE
    } else {
        TRACEVAULT
    }
}

/// `command` (one of ours, under either name) spelled for the binary chosen
/// by `visdom`. Anything else comes back unchanged.
pub fn spelled(command: &str, visdom: bool) -> String {
    match subcommand(command) {
        Some(rest) => format!("{} {rest}", name(visdom)),
        None => command.to_string(),
    }
}

/// The command name in a message, for the running binary: `{CLI} repo
/// switch` reads `tracevault repo switch` or `visdom trace repo switch`.
pub struct Cli;
/// See [`Cli`].
pub const CLI: Cli = Cli;

impl std::fmt::Display for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(name(is_visdom()))
    }
}

/// The binary's own name in a message, for the commands `visdom` has at its
/// top level and for message prefixes: `{TOP} login`, `{TOP}: warning: …`.
pub struct Top;
/// See [`Top`].
pub const TOP: Top = Top;

impl std::fmt::Display for Top {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if is_visdom() { "visdom" } else { TRACEVAULT })
    }
}

/// The prefix of the environment variables in a message: `{ENV}_API_KEY`
/// reads `TRACEVAULT_API_KEY` or `VISDOM_API_KEY` (which `visdom` accepts
/// for it).
pub struct Env;
/// See [`Env`].
pub const ENV: Env = Env;

impl std::fmt::Display for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if is_visdom() { "VISDOM" } else { "TRACEVAULT" })
    }
}

/// `text` — a message written with `tracevault …` commands and
/// `TRACEVAULT_…` variables — as the running binary says it. For messages
/// that are not format strings; those use [`CLI`], [`TOP`] and [`ENV`].
pub fn say(text: &str) -> String {
    say_as(text, is_visdom())
}

fn say_as(text: &str, visdom: bool) -> String {
    if !visdom {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(at) = rest.find(TRACEVAULT) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let after = &from[TRACEVAULT.len()..];
        let word_start = !before
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/');
        let sub: String = after
            .strip_prefix(' ')
            .map(|a| {
                a.chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect()
            })
            .unwrap_or_default();
        if word_start && ["login", "logout", "init"].contains(&sub.as_str()) {
            out.push_str("visdom");
        } else if word_start && (!sub.is_empty() || after.starts_with(": ")) {
            out.push_str(if sub.is_empty() {
                "visdom"
            } else {
                VISDOM_TRACE
            });
        } else {
            out.push_str(TRACEVAULT);
        }
        rest = after;
    }
    out.push_str(rest);
    out.replace("TRACEVAULT_", "VISDOM_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_commands_are_recognised_under_either_name() {
        assert_eq!(
            subcommand("tracevault stream --event stop"),
            Some("stream --event stop")
        );
        assert_eq!(
            subcommand("visdom trace stream --event stop"),
            Some("stream --event stop")
        );
        assert_eq!(subcommand("tracevaultx stream"), None);
        assert_eq!(subcommand("visdom doctor"), None);
        assert_eq!(subcommand("visdom tracer stream"), None);
        assert_eq!(subcommand("echo tracevault stream"), None);
        assert!(is_visdom_command("visdom trace user-prompt"));
        assert!(!is_visdom_command("tracevault user-prompt"));
    }

    #[test]
    fn a_command_is_spelled_for_the_chosen_binary() {
        assert_eq!(
            spelled("tracevault session-start", true),
            "visdom trace session-start"
        );
        assert_eq!(
            spelled("visdom trace session-start", false),
            "tracevault session-start"
        );
        assert_eq!(
            spelled("visdom trace session-start", true),
            "visdom trace session-start"
        );
        assert_eq!(spelled("my-own-hook --flag", true), "my-own-hook --flag");
    }

    #[test]
    fn a_message_is_said_with_the_running_binarys_names() {
        let text = "tracevault: no login. Set TRACEVAULT_API_KEY or run `tracevault login`, \
                    then `tracevault repo switch <path>` (see .tracevault/ and my-tracevault tool)";
        assert_eq!(say_as(text, false), text);
        assert_eq!(
            say_as(text, true),
            "visdom: no login. Set VISDOM_API_KEY or run `visdom login`, \
             then `visdom trace repo switch <path>` (see .tracevault/ and my-tracevault tool)"
        );
        assert_eq!(
            say_as("run 'tracevault init' first", true),
            "run 'visdom init' first"
        );
        assert_eq!(
            format!("`{CLI} sync`, `{TOP} login`, {ENV}_PROJECT"),
            "`tracevault sync`, `tracevault login`, TRACEVAULT_PROJECT"
        );
    }
}
