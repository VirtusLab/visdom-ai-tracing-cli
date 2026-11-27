mod common;

use common::{snapshot, stderr, Sandbox};

const BAZAAR: &str = "https://ctx.example.com/mcp/remote";

fn log_lines(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn launch_runs_agent_with_args() {
    let sb = Sandbox::new();
    let (script, log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let sub = sb.repo.path().join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();

    // Started in a subdirectory; --yes with fixes that fail (not logged in).
    let output = sb
        .visdom(&sub)
        .args(["--yes", "--", "--model", "x"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        log_lines(&log),
        [format!(
            "{}|--model x",
            sub.canonicalize().unwrap().display()
        )],
        "the agent runs once, where visdom was run, with the arguments"
    );
    assert!(
        stderr(&output).contains("fix failed"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn launch_exit_code_is_the_agents() {
    let sb = Sandbox::new();
    sb.write_user_config("[agents]\ncodex = \"false\"\n");
    let output = sb
        .visdom(sb.repo.path())
        .args(["--agent", "codex"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn launch_missing_binary_exits_127() {
    let sb = Sandbox::new();
    sb.write_user_config("[agents]\nclaude_code = \"/nonexistent/claude-for-visdom-test\"\n");
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(127));
    let err = stderr(&output);
    assert!(
        err.contains("cannot start /nonexistent/claude-for-visdom-test"),
        "{err}"
    );
    assert!(err.contains("agents.claude_code"), "{err}");
    assert!(
        err.contains(&sb.user_config().display().to_string()),
        "{err}"
    );
}

#[test]
fn launch_not_blocked_by_findings() {
    let sb = Sandbox::new();
    let (script, log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let before = (snapshot(sb.home.path()), snapshot(sb.repo.path()));

    // Nothing wired, nobody to ask (stdin is not a terminal), no --yes.
    let output = sb
        .visdom(sb.repo.path())
        .env("VISDOM_BAZAAR_URL", BAZAAR)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(log_lines(&log).len(), 1, "the agent still starts");

    let err = stderr(&output);
    assert!(err.contains("Claude Code hooks are not installed"), "{err}");
    assert!(err.contains("bazaar MCP is not registered"), "{err}");
    assert!(err.contains("visdom doctor --fix"), "{err}");

    // Only the agent's own log is new: visdom changed nothing.
    let mut after_home = snapshot(sb.home.path());
    after_home.remove(&log);
    assert_eq!(before, (after_home, snapshot(sb.repo.path())));
}

/// With --yes the bazaar registration goes through the agent's own CLI, in
/// the repository, before the agent is started.
#[test]
fn yes_registers_bazaar_through_the_agent_cli() {
    let sb = Sandbox::new();
    let (script, log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "bazaar_url = \"{BAZAAR}\"\n[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let output = sb
        .visdom(sb.repo.path())
        .args(["--yes", "--", "--resume"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let repo = sb.repo_dir();
    assert_eq!(
        log_lines(&log),
        [
            format!(
                "{}|mcp add --transport http --scope local --client-id visdom-context-mcp \
                 bazaar {BAZAAR}",
                repo.display()
            ),
            format!("{}|--resume", repo.display()),
        ]
    );
}

#[test]
fn outside_a_repository_the_agent_starts_where_visdom_was_run() {
    let sb = Sandbox::new();
    let (script, log) = sb.fake_agent();
    sb.write_user_config(&format!("[agents]\ncodex = \"{}\"\n", script.display()));
    let outside = tempfile::tempdir().unwrap();
    let output = sb
        .visdom(outside.path())
        .env("VISDOM_AGENT", "codex")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        log_lines(&log),
        [format!(
            "{}|",
            outside.path().canonicalize().unwrap().display()
        )]
    );
}

/// The agent `visdom` starts was told about Visdom's tools on its command
/// line (or deliberately not): it is marked, so its session-start hook does
/// not tell it again.
#[test]
fn a_launched_agent_is_marked_so_its_hook_stays_quiet() {
    let sb = Sandbox::new();
    let (script, log) = sb.recording_agent(&["VISDOM_LAUNCHED"]);
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        log_lines(&log).contains(&"VISDOM_LAUNCHED=1".to_string()),
        "{:?}",
        log_lines(&log)
    );
}

/// What the session-start hook answers for a session in the sandbox's
/// repository, run with `configure` applied to the command.
fn session_start(
    sb: &Sandbox,
    configure: impl FnOnce(&mut std::process::Command),
) -> serde_json::Value {
    use std::io::Write;
    let event = serde_json::json!({
        "session_id": "0b1e1c1e-5d1c-4f6e-9a57-0d4c3a1f2b3c",
        "transcript_path": "",
        "cwd": sb.repo_dir(),
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let mut cmd = sb.visdom(sb.repo.path());
    cmd.args(["trace", "session-start"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    configure(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("the hook must print JSON ({e}): {text}"))
}

/// A session `visdom` did not start — in the Claude Desktop app, or a plain
/// `claude` — learns of Visdom's tools from its session-start hook.
#[test]
fn a_session_visdom_did_not_start_is_told_by_its_hook() {
    let sb = Sandbox::new();
    // A login to a server nobody answers at: the hook asks it nothing for
    // this text, and still answers.
    sb.log_in("http://127.0.0.1:1", "token");
    let context = |answer: &serde_json::Value| {
        answer["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(String::from)
    };

    let told = session_start(&sb, |_| {});
    let text = context(&told).expect("the hook hands the agent the instructions");
    assert!(text.starts_with("# Visdom\n"), "{text}");
    assert!(text.contains("visdom trace repo switch"), "{text}");
    assert!(!text.contains("was started with `visdom`"), "{text}");
    assert_eq!(told["hookSpecificOutput"]["hookEventName"], "SessionStart");

    // Started by visdom: told on its command line already.
    let launched = session_start(&sb, |cmd| {
        cmd.env("VISDOM_LAUNCHED", "1");
    });
    assert_eq!(context(&launched), None);

    // Turned off by the developer.
    sb.write_user_config("agent_instructions = false\n");
    assert_eq!(context(&session_start(&sb, |_| {})), None);
}

/// Nothing set up, nothing to tell: the hook's answer is what it always was.
#[test]
fn the_hook_says_nothing_where_nothing_is_set_up() {
    let sb = Sandbox::new();
    let answer = session_start(&sb, |_| {});
    assert!(answer.get("hookSpecificOutput").is_none(), "{answer}");
}
