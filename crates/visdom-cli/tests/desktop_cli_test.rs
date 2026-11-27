//! `visdom --surface desktop`: what it opens, and every way it refuses.

mod common;

use std::path::{Path, PathBuf};

use common::{stderr, Sandbox};

fn log_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// A sandbox with the app installed, a stand-in for the terminal CLI and a
/// stand-in opener. Returns the `PATH` to run with and both logs.
fn desktop_sandbox() -> (Sandbox, std::ffi::OsString, PathBuf, PathBuf) {
    let sb = Sandbox::new();
    sb.fake_app();
    let (script, agent_log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let (opener_dir, opener_log) = sb.fake_opener(0);
    let mut dirs = vec![opener_dir];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(dirs).unwrap();
    (sb, path, agent_log, opener_log)
}

#[test]
fn desktop_in_gateway_mode_is_refused_and_nothing_is_opened() {
    let (sb, path, agent_log, opener_log) = desktop_sandbox();
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", &path)
        .args(["--surface", "desktop", "--mode", "gateway"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("VIS-1396"), "{err}");
    assert!(err.contains("--surface terminal"), "{err}");
    assert!(err.contains("--mode hooks"), "{err}");
    assert!(log_lines(&opener_log).is_empty() && log_lines(&agent_log).is_empty());
}

#[test]
fn desktop_refuses_arguments_it_cannot_pass() {
    let (sb, path, agent_log, opener_log) = desktop_sandbox();
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", &path)
        .args(["--surface", "desktop", "--", "--model", "x"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("--model x"), "{err}");
    assert!(err.contains("--surface terminal"), "{err}");
    assert!(log_lines(&opener_log).is_empty() && log_lines(&agent_log).is_empty());
}

#[test]
fn desktop_for_codex_is_refused() {
    let (sb, path, _, opener_log) = desktop_sandbox();
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", &path)
        .args(["--agent", "codex", "--surface", "desktop"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not supported for Codex yet"),
        "{}",
        stderr(&output)
    );
    assert!(log_lines(&opener_log).is_empty());
}

/// A repository that asks for the desktop stays usable for a developer
/// without the app: the refusal says how to get the terminal back.
#[test]
fn a_missing_app_names_the_way_back_to_the_terminal() {
    let sb = Sandbox::new();
    let (script, agent_log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "[agents]\nclaude_code = \"{}\"\n",
        script.display()
    ));
    let visdom_dir = sb.repo.path().join(".visdom");
    std::fs::create_dir_all(&visdom_dir).unwrap();
    std::fs::write(visdom_dir.join("config.toml"), "surface = \"desktop\"\n").unwrap();
    let output = sb.visdom(sb.repo.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("Claude Desktop was not opened"), "{err}");
    assert!(err.contains("--surface terminal"), "{err}");
    assert!(err.contains("VISDOM_SURFACE"), "{err}");
    assert!(log_lines(&agent_log).is_empty());

    // And the way back works.
    let output = sb
        .visdom(sb.repo.path())
        .env("VISDOM_SURFACE", "terminal")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(log_lines(&agent_log).len(), 1);
}

#[test]
fn desktop_without_the_terminal_cli_is_refused() {
    let sb = Sandbox::new();
    sb.fake_app();
    sb.write_user_config("[agents]\nclaude_code = \"/nonexistent/claude-for-visdom-test\"\n");
    let output = sb
        .visdom(sb.repo.path())
        .args(["--surface", "desktop"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("Install the Claude Code CLI"), "{err}");
    assert!(err.contains("--surface terminal"), "{err}");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn desktop_without_a_terminal_is_refused() {
    let (sb, path, agent_log, _) = desktop_sandbox();
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", &path)
        .args(["--surface", "desktop"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("needs a terminal"),
        "{}",
        stderr(&output)
    );
    assert!(log_lines(&agent_log).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_opens_the_link_and_returns() {
    let (sb, path, agent_log, opener_log) = desktop_sandbox();
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", &path)
        // Left by the agent's shell visdom may be run from.
        .env("CLAUDE_CODE_SESSION_ID", "another-session")
        .args(["--surface", "desktop"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(log_lines(&opener_log), ["claude://code/new"]);
    assert!(
        log_lines(&agent_log).is_empty(),
        "the terminal agent is not started"
    );
    let err = stderr(&output);
    let last = err.lines().last().unwrap();
    // Nobody is logged in here, so the sessions would not be traced.
    assert!(
        last.starts_with("visdom: opened Claude Desktop, but sessions there are NOT traced: "),
        "{err}"
    );
    assert!(
        last.contains("Run visdom doctor --surface desktop."),
        "{err}"
    );
    assert!(
        last.ends_with(&format!(
            "Choose this folder in the app: {}",
            sb.repo_dir().display()
        )),
        "{err}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_opener_that_fails_is_said() {
    let (sb, _, _, _) = desktop_sandbox();
    let (opener_dir, _) = sb.fake_opener(3);
    let mut dirs = vec![opener_dir];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = sb
        .visdom(sb.repo.path())
        .env("PATH", std::env::join_paths(dirs).unwrap())
        .args(["--surface", "desktop"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.lines()
            .last()
            .unwrap()
            .starts_with("visdom: Claude Desktop was not opened: xdg-open could not open"),
        "{err}"
    );
}

/// With the terminal named in the config, a launch is what it always was.
#[test]
fn the_terminal_surface_is_untouched() {
    let run = |config_extra: &str| {
        let sb = Sandbox::new();
        let (script, log) = sb.fake_agent();
        sb.write_user_config(&format!(
            "{config_extra}[agents]\nclaude_code = \"{}\"\n",
            script.display()
        ));
        let output = sb
            .visdom(sb.repo.path())
            .args(["--", "--model", "x"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let home = sb.home.path().display().to_string();
        let repo = sb.repo_dir().display().to_string();
        let tidy = |text: String| text.replace(&home, "<home>").replace(&repo, "<repo>");
        (
            log_lines(&log).into_iter().map(&tidy).collect::<Vec<_>>(),
            tidy(stderr(&output)),
        )
    };
    let unset = run("");
    assert_eq!(unset.0, ["<repo>|--model x"]);
    assert_eq!(run("surface = \"terminal\"\n"), unset);
}
