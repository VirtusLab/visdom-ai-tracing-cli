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
