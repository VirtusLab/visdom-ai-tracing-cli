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
}
