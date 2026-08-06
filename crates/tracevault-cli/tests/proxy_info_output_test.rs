//! `tracevault proxy info` must not print a setup script the user cannot
//! follow.
//!
//! Runs the real binary (via `CARGO_BIN_EXE_*`, so no test dependency is
//! needed) because the contract under test is about what reaches stdout, and
//! `run_proxy_info` writes with `println!` — an in-process test can assert the
//! exit code but not the output.

use std::fs;
use std::process::Command;

/// Run `tracevault proxy info` with `XDG_CONFIG_HOME` pointed at a tempdir
/// containing `creds` (when given), returning (exit code, stdout, stderr).
fn run_proxy_info(creds: Option<&str>) -> (i32, String, String) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(creds) = creds {
        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(creds_dir.join("credentials.json"), creds).unwrap();
    }

    let out = Command::new(env!("CARGO_BIN_EXE_tracevault"))
        .args(["proxy", "info"])
        .env("XDG_CONFIG_HOME", dir.path())
        // A real key/URL in the developer's environment must not leak into the
        // result: this command reads the file, but be explicit.
        .env_remove("TRACEVAULT_API_KEY")
        .env_remove("TRACEVAULT_SERVER_URL")
        .output()
        .expect("failed to run the tracevault binary");

    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A credentials file with neither a `token` nor an `auth` object: there is
/// nothing to paste into `ANTHROPIC_API_KEY`, so the command must say so and
/// stop — not sandwich the error between numbered steps the user is being told
/// to perform.
#[test]
fn no_usable_credential_reports_and_prints_no_instructions() {
    let (code, stdout, stderr) = run_proxy_info(Some(
        r#"{"server_url":"https://example.com","email":"a@b.com"}"#,
    ));

    assert_eq!(code, 1, "must exit non-zero; stdout was:\n{stdout}");
    assert!(
        stderr.contains("no usable credential"),
        "must explain what is wrong: {stderr}"
    );
    assert!(
        stderr.contains("tracevault login") && stderr.contains("TRACEVAULT_API_KEY"),
        "must name both ways to fix it: {stderr}"
    );
    for instruction in [
        "export ANTHROPIC_BASE_URL",
        "export ANTHROPIC_API_KEY",
        "Run your AI tool as usual",
        "Setup",
    ] {
        assert!(
            !stdout.contains(instruction),
            "printed setup instructions that cannot be followed ({instruction}):\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("token\" field"),
        "must not point at a `token` field this file does not have:\n{stdout}"
    );
}

/// The working cases still print the full script and succeed.
#[test]
fn a_usable_credential_still_prints_the_setup_script() {
    for creds in [
        r#"{"server_url":"https://example.com","token":"tvk_abc","email":"a@b.com"}"#,
        r#"{"server_url":"https://example.com","email":"a@b.com","auth":{"issuer":"i","client_id":"c","refresh_token":"rt","access_token":"at","access_expires_at":1}}"#,
    ] {
        let (code, stdout, _stderr) = run_proxy_info(Some(creds));
        assert_eq!(code, 0, "stdout was:\n{stdout}");
        assert!(stdout.contains("export ANTHROPIC_BASE_URL"), "{stdout}");
        assert!(stdout.contains("Run your AI tool as usual"), "{stdout}");
        // Neither variant may echo credential material.
        assert!(!stdout.contains("tvk_abc"), "api key leaked:\n{stdout}");
        assert!(!stdout.contains("\"at\""), "access token leaked:\n{stdout}");
    }
}
