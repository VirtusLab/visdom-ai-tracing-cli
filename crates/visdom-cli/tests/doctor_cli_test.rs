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
    assert_eq!(ids(&json), ["tracing.auth", "tracing.repo", "bazaar.url"]);
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
        .args(["init", "--agent", "codex", "--yes"])
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
    assert!(hooks.contains("tracevault stream"), "{hooks}");
    assert!(sb.repo.path().join(".tracevault/config.toml").exists());
    // The rest of the wiring is checked after init: bazaar's URL is unknown here.
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
