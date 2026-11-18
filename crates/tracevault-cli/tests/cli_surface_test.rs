//! The `tracevault` command line as users and installed hooks see it. The
//! command definitions live in the library (shared with `visdom`); this pins
//! the shape of the binary built on top of them.

use std::process::{Command, Output};

fn tracevault(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tracevault"))
        .args(args)
        .output()
        .expect("failed to run tracevault")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Whitespace-collapsed, so the comparison does not depend on wrapping.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn top_level_help_is_unchanged() {
    let output = tracevault(&["--help"]);
    assert!(output.status.success());
    let expected = "AI code governance platform

Usage: tracevault <COMMAND>

Commands:
  init            Initialize TraceVault in current repository
  telemetry       Claude Code telemetry (cost over OpenTelemetry, on by default): `on`, `off` (remembered across upgrades) or `status`
  status          Show current session status
  check           Check session policies before pushing
  sync            Sync repo remote URL with the TraceVault server
  stats           Show local session statistics
  login           Log in to a TraceVault server
  logout          Log out from the TraceVault server
  flush           Force-sync all pending events to server
  verify          Verify commits are registered and sealed on the TraceVault server
  verify-start    Open (or re-open) a verification phase for the current session
  agent-policies  Print agent-readable instructions derived from active policies
  proxy           LLM proxy commands
  context         Manage the active context (flow ID, labels, params) attached to events
  repo            Bind/inspect the repo a detached session is working on (workspace mode)
  project         Bind/inspect the project a detached session is attributed to
  help            Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
";
    assert_eq!(squash(&stdout(&output)), squash(expected));
}

#[test]
fn a_subcommand_is_required() {
    let output = tracevault(&[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage: tracevault <COMMAND>"), "{stderr}");
}

#[test]
fn version_names_the_binary() {
    let output = tracevault(&["--version"]);
    assert_eq!(
        stdout(&output).trim(),
        format!("tracevault {}", env!("CARGO_PKG_VERSION"))
    );
}

/// The commands installed hooks call are hidden from help but must parse,
/// with the flags the hook files carry.
#[test]
fn hook_commands_still_parse() {
    for args in [
        &["stream", "--help"][..],
        &["session-start", "--help"],
        &["user-prompt", "--help"],
        &["commit-push", "--help"],
        &["otel-headers", "--help"],
    ] {
        let output = tracevault(args);
        assert!(output.status.success(), "{args:?}");
    }
    let help = stdout(&tracevault(&["stream", "--help"]));
    assert!(
        help.contains("--event <EVENT>") && help.contains("--agent <AGENT>"),
        "{help}"
    );
}

/// Hook output is parsed by agents and the notice is for people: with stderr
/// not a terminal (as here) nothing is added to it.
#[test]
fn no_deprecation_notice_when_stderr_is_not_a_terminal() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tracevault"))
        .arg("stats")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .current_dir(home.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("deprecated"), "{stderr}");
}
