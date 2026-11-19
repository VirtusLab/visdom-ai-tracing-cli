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

/// Run the capture hook for one event in `dir`, with its own home and state
/// directories, and return its stdout.
fn stream_event(dir: &std::path::Path, home: &std::path::Path, mode: Option<&str>) -> String {
    use std::io::Write;
    let event = serde_json::json!({
        "session_id": "11111111-2222-3333-4444-555555555555",
        "cwd": dir,
        "hook_event_name": "Stop",
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracevault"));
    cmd.args(["stream", "--event", "stop"])
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("TRACEVAULT_API_KEY")
        .env_remove("TRACEVAULT_SERVER_URL")
        .env_remove("VISDOM_TRACING_MODE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(mode) = mode {
        cmd.env("VISDOM_TRACING_MODE", mode);
    }
    let mut child = cmd.spawn().expect("failed to run tracevault");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "a hook never fails");
    stdout(&output)
}

/// An agent `visdom` started through the AI Gateway carries
/// `VISDOM_TRACING_MODE=gateway`; its capture hook then answers and records
/// nothing — not even the session's local state.
#[test]
fn the_capture_hook_records_nothing_for_a_gateway_session() {
    let home = tempfile::tempdir().unwrap();

    let gateway = tempfile::tempdir().unwrap();
    let out = stream_event(gateway.path(), home.path(), Some("gateway"));
    let answer: serde_json::Value = serde_json::from_str(out.trim()).expect("a hook response");
    assert!(answer.is_object(), "{out}");
    assert!(
        !gateway.path().join(".tracevault").exists(),
        "nothing may be written for a gateway session"
    );

    // The same event in a hooks session leaves the session's local state.
    for mode in [None, Some("hooks")] {
        let hooks = tempfile::tempdir().unwrap();
        stream_event(hooks.path(), home.path(), mode);
        assert!(
            hooks.path().join(".tracevault").exists(),
            "{mode:?}: a hooks session is recorded locally"
        );
    }
}
