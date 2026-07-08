//! The `repo switch` `<path>` / `--name` arg group is exactly-one-required.
//! clap enforces this at parse time (before any command logic), so these run
//! without a server or session.
use std::process::Command;

fn tv() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tracevault"))
}

#[test]
fn repo_switch_rejects_neither_path_nor_name() {
    let out = tv().args(["repo", "switch"]).output().expect("run");
    assert!(!out.status.success(), "neither should be a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--name") || stderr.to_lowercase().contains("required"),
        "stderr: {stderr}"
    );
}

#[test]
fn repo_switch_rejects_both_path_and_name() {
    let out = tv()
        .args(["repo", "switch", "/tmp/x", "--name", "foo"])
        .output()
        .expect("run");
    assert!(!out.status.success(), "both should be a conflict error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("cannot be used with") || stderr.contains("--name"),
        "stderr: {stderr}"
    );
}
