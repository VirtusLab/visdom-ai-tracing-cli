mod common;

use common::{snapshot, stderr, stdout, Sandbox};

const BAZAAR: &str = "https://ctx.example.com/mcp/remote";

fn ids(json: &serde_json::Value) -> Vec<String> {
    json["findings"]
        .as_array()
        .expect("a findings array")
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn doctor_json_reports_missing_bazaar_and_hooks() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .env("VISDOM_BAZAAR_URL", BAZAAR)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output))
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {}", stdout(&output)));
    let ids = ids(&json);
    for id in ["tracing.auth", "tracing.agent_hooks", "bazaar.mcp"] {
        assert!(ids.iter().any(|i| i == id), "{id} missing from {ids:?}");
    }
    let bazaar = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "bazaar.mcp")
        .unwrap();
    assert_eq!(bazaar["severity"], "error");
    // The fix is the agent's own CLI, run in the repository.
    assert_eq!(bazaar["fix"]["action"]["kind"], "run_command");
    assert_eq!(bazaar["fix"]["action"]["program"], "claude");
    assert_eq!(
        bazaar["fix"]["action"]["cwd"],
        sb.repo_dir().to_string_lossy().as_ref()
    );
}

#[test]
fn doctor_text_names_every_component() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--agent", "codex"])
        .env("VISDOM_BAZAAR_URL", BAZAAR)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.contains("Tracing\n"), "{text}");
    assert!(text.contains("Bazaar\n"), "{text}");
    assert!(text.contains("Codex hooks are not installed"), "{text}");
    assert!(text.contains("codex mcp add bazaar --url"), "{text}");
}

#[test]
fn doctor_without_fix_changes_nothing() {
    let sb = Sandbox::new();
    let before = (snapshot(sb.home.path()), snapshot(sb.repo.path()));
    // Neither --yes alone nor a missing terminal turns doctor into a fixer.
    for args in [&["doctor"][..], &["doctor", "--yes"], &["doctor", "--fix"]] {
        let output = sb
            .visdom(sb.repo.path())
            .args(args)
            .env("VISDOM_BAZAAR_URL", BAZAAR)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(
            before,
            (snapshot(sb.home.path()), snapshot(sb.repo.path())),
            "{args:?} changed something"
        );
    }
}

#[test]
fn doctor_outside_a_repository_reports_and_exits() {
    let sb = Sandbox::new();
    let outside = tempfile::tempdir().unwrap();
    let output = sb
        .visdom(outside.path())
        .args(["doctor", "--json"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    // Logged in (API key), no repository, bazaar URL unknown: nothing is an error.
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        ids(&json),
        [
            "tracing.auth",
            "tracing.repo",
            "gateway.mode",
            "gateway.policy",
            "bazaar.url"
        ]
    );
}

#[test]
fn doctor_rejects_an_agent_it_cannot_launch() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--agent", "amp"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("amp is not supported yet"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn malformed_agent_state_is_reported_not_fatal() {
    let sb = Sandbox::new();
    std::fs::write(sb.home.path().join(".claude.json"), "{ not json").unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .env("VISDOM_BAZAAR_URL", BAZAAR)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let bazaar = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "bazaar.mcp")
        .unwrap();
    assert_eq!(bazaar["severity"], "unknown");
    assert!(bazaar["fix"].is_null());
}

#[test]
fn trace_carries_the_tracevault_commands() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["trace", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = stdout(&output);
    for command in [
        "status",
        "verify-start",
        "agent-policies",
        "context",
        "repo",
    ] {
        assert!(
            help.contains(command),
            "trace --help lacks {command}: {help}"
        );
    }
    // A hook command, hidden from help, still runs under `trace`.
    let output = sb
        .visdom(sb.repo.path())
        .args(["trace", "stream", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

/// `VISDOM_API_KEY` reaches the tracing library, which only knows
/// `TRACEVAULT_API_KEY`.
#[test]
fn visdom_env_names_reach_the_tracing_library() {
    let sb = Sandbox::new();
    let run = |key: Option<&str>| {
        let mut cmd = sb.visdom(sb.repo.path());
        cmd.args(["doctor", "--json"]);
        if let Some(key) = key {
            cmd.env("VISDOM_API_KEY", key);
        }
        let output = cmd.output().unwrap();
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        json["findings"][0]["severity"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(run(None), "error");
    assert_eq!(run(Some("tvk_test")), "ok");
}

#[test]
fn visdom_init_in_temp_repo_installs_hooks() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["init", "--agent", "codex"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let hooks = std::fs::read_to_string(sb.repo.path().join(".codex/hooks.json")).unwrap();
    assert!(hooks.contains("visdom trace stream"), "{hooks}");
    assert!(sb.repo.path().join(".visdom/tracing.toml").exists());
    // Bazaar is part of init; here its URL is unknown, which is said, not fatal.
    assert!(
        stderr(&output).contains("bazaar URL is unknown"),
        "{}",
        stderr(&output)
    );

    // And doctor now finds tracing in order.
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--agent", "codex"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(stdout(&output).contains("✓ Codex hooks installed"));
}

/// A credential file that is there but broken is not "not logged in": no
/// login is offered over it.
#[test]
fn broken_credential_file_is_unknown() {
    let sb = Sandbox::new();
    let dir = sb
        .user_config()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("visdom");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("credentials.json"), "{ truncated").unwrap();
    let output = sb
        .visdom(sb.repo.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let auth = &json["findings"][0];
    assert_eq!(auth["id"], "tracing.auth");
    assert_eq!(auth["severity"], "unknown", "{auth}");
    assert!(auth["fix"].is_null());
}

/// `visdom init` sets up bazaar too: the registration runs through the
/// agent's own CLI without a separate question, even with nobody to ask.
#[test]
fn visdom_init_registers_bazaar() {
    let sb = Sandbox::new();
    let (script, log) = sb.fake_agent();
    sb.write_user_config(&format!(
        "bazaar_url = \"{BAZAAR}\"\n[agents]\ncodex = \"{}\"\n",
        script.display()
    ));
    let output = sb
        .visdom(sb.repo.path())
        .args(["init", "--agent", "codex"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(sb.repo.path().join(".codex/hooks.json").exists());
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        [format!(
            "{}|mcp add bazaar --url {BAZAAR} --oauth-client-id visdom-context-mcp",
            sb.repo_dir().display()
        )]
    );
    // What is about to change is shown before it is done.
    assert!(
        stderr(&output).contains("applying: run `"),
        "{}",
        stderr(&output)
    );
}

/// A bazaar registration that fails makes `init` fail: half an init must not
/// look like a whole one.
#[test]
fn visdom_init_fails_when_bazaar_registration_fails() {
    let sb = Sandbox::new();
    sb.write_user_config(&format!(
        "bazaar_url = \"{BAZAAR}\"\n[agents]\ncodex = \"false\"\n"
    ));
    let output = sb
        .visdom(sb.repo.path())
        .args(["init", "--agent", "codex"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("fix failed"),
        "{}",
        stderr(&output)
    );
    // Tracing was still set up.
    assert!(sb.repo.path().join(".codex/hooks.json").exists());
}

/// A checkout the `tracevault` binary set up: `visdom init` makes everything
/// it installed call `visdom trace …`, so `tracevault` is no longer needed on
/// `PATH`, and leaves the user's own hooks alone.
#[test]
fn visdom_init_takes_over_hooks_that_call_tracevault() {
    let sb = Sandbox::new();
    let repo = sb.repo.path();
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"tracevault stream --event stop"},{"type":"command","command":"my-own-hook"}]}]}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    std::fs::write(
        repo.join(".git/hooks/pre-push"),
        "#!/bin/sh\nmy-own-check\n# tracevault:enforce\ntracevault sync 2>/dev/null || true\ntracevault check || exit 1\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".git/hooks/post-commit"),
        "#!/bin/sh\n# tracevault:post-commit\ntracevault commit-push 2>/dev/null &\n",
    )
    .unwrap();

    let output = sb
        .visdom(repo)
        .args(["init", "--agent", "claude-code"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );

    let settings = std::fs::read_to_string(repo.join(".claude/settings.local.json")).unwrap();
    assert!(
        settings.contains("visdom trace stream --event stop"),
        "{settings}"
    );
    // The full set went into the default file, calling `visdom` as well.
    let shared = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    assert!(shared.contains("visdom trace session-start"), "{shared}");
    assert!(!shared.contains("\"tracevault "), "{shared}");
    assert!(settings.contains("my-own-hook"), "{settings}");
    assert!(!settings.contains("\"tracevault "), "{settings}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".git/hooks/pre-push")).unwrap(),
        "#!/bin/sh\nmy-own-check\n# visdom:enforce\nvisdom trace sync 2>/dev/null || true\nvisdom trace check || exit 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(".git/hooks/post-commit")).unwrap(),
        "#!/bin/sh\n# visdom:post-commit\nvisdom trace commit-push 2>/dev/null &\n"
    );
}

/// `visdom trace init` for an agent that takes a plugin installs it under the
/// `visdom` name, running `visdom trace …`, and removes the `tracevault` one
/// an earlier `init` left — the agent would load both.
#[test]
fn visdom_installs_plugins_under_its_own_name_and_removes_the_earlier_ones() {
    let init = |agent: &str, sb: &Sandbox| {
        let output = sb
            .visdom(sb.repo.path())
            .args(["trace", "init", "--agent", agent])
            .env("VISDOM_API_KEY", "tvk_test")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            stdout(&output),
            stderr(&output)
        );
    };

    let sb = Sandbox::new();
    let plugins = sb.repo.path().join(".amp/plugins");
    std::fs::create_dir_all(plugins.join("tracevault")).unwrap();
    std::fs::write(plugins.join("tracevault/index.ts"), "// earlier").unwrap();
    init("amp", &sb);
    let plugin = std::fs::read_to_string(plugins.join("visdom/index.ts")).unwrap();
    assert!(plugin.contains("const CLI = ['visdom', 'trace']"));
    assert!(!plugins.join("tracevault").exists());
    let gitignore = std::fs::read_to_string(sb.repo.path().join(".gitignore")).unwrap();
    assert!(
        gitignore.lines().any(|l| l == ".amp/plugins/visdom/"),
        "{gitignore}"
    );

    let sb = Sandbox::new();
    let plugins = sb.repo.path().join(".opencode/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("tracevault.ts"), "// earlier").unwrap();
    init("opencode", &sb);
    let plugin = std::fs::read_to_string(plugins.join("visdom.ts")).unwrap();
    assert!(plugin.contains("const CLI = [\"visdom\", \"trace\"];"));
    assert!(!plugins.join("tracevault.ts").exists());
}

/// What `visdom init --global` leaves in the agent's own files names nothing
/// `tracevault`: the hooks, and the workspace-mode block with its marker.
#[test]
fn a_machine_wide_visdom_setup_names_only_visdom() {
    let sb = Sandbox::new();
    let output = sb
        .visdom(sb.repo.path())
        .args(["trace", "init", "--global", "--no-claude-telemetry"])
        .env("VISDOM_API_KEY", "tvk_test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let said = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!said.to_lowercase().contains("tracevault"), "{said}");
    for file in [".claude/settings.json", ".claude/CLAUDE.md"] {
        let text = std::fs::read_to_string(sb.home.path().join(file)).unwrap();
        assert!(
            !text.to_lowercase().contains("tracevault"),
            "{file}: {text}"
        );
    }
    let md = std::fs::read_to_string(sb.home.path().join(".claude/CLAUDE.md")).unwrap();
    assert!(md.contains("<!-- visdom:workspace-mode -->"), "{md}");
    assert!(md.contains("`visdom trace repo switch <path>`"), "{md}");
}
