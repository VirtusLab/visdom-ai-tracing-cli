//! A throw-away home directory and git repository for running the built
//! `visdom` binary without touching the developer's own.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// The library's own test server, shared with these tests.
#[path = "../../src/test_http.rs"]
pub mod http;

pub struct Sandbox {
    pub home: TempDir,
    pub repo: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let repo = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(status.success());
        Sandbox {
            home: TempDir::new().unwrap(),
            repo,
        }
    }

    /// The repository as git reports it (macOS temp dirs are symlinked).
    pub fn repo_dir(&self) -> PathBuf {
        self.repo.path().canonicalize().unwrap()
    }

    /// Where `visdom` reads its user config on this platform.
    pub fn user_config(&self) -> PathBuf {
        let config_root = if cfg!(target_os = "macos") {
            self.home.path().join("Library/Application Support")
        } else {
            self.home.path().join(".config")
        };
        config_root.join("visdom/config.toml")
    }

    pub fn write_user_config(&self, content: &str) {
        let path = self.user_config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// `visdom` in `cwd`, seeing only this sandbox: no login, no variables of
    /// the developer's, nothing on stdin.
    pub fn visdom(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_visdom"));
        cmd.current_dir(cwd)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("CODEX_HOME", self.home.path().join(".codex"))
            .stdin(Stdio::null());
        for name in ["SERVER_URL", "API_KEY", "SESSION_ID", "NO_BROWSER"] {
            cmd.env_remove(format!("TRACEVAULT_{name}"));
            cmd.env_remove(format!("VISDOM_{name}"));
        }
        for name in [
            "VISDOM_AGENT",
            "VISDOM_BAZAAR_URL",
            "VISDOM_TRACING_MODE",
            "VISDOM_GATEWAY_URL",
            "VISDOM_CONTROL_PLANE_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_CUSTOM_HEADERS",
        ] {
            cmd.env_remove(name);
        }
        cmd
    }

    /// Log this sandbox in to the tracing server at `server_url` with a
    /// Visdom account whose access token is `token` (valid for an hour).
    pub fn log_in(&self, server_url: &str, token: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let credentials = serde_json::json!({
            "server_url": server_url,
            "email": "dev@example.com",
            "auth": {
                "issuer": "https://idp.invalid/realms/visdom",
                "client_id": "visdom-cli",
                "refresh_token": "refresh",
                "access_token": token,
                "access_expires_at": now + 3600,
            },
        });
        let path = self.home.path().join(".config/tracevault/credentials.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, credentials.to_string()).unwrap();
    }

    /// An executable standing in for an agent that also records the
    /// environment it was given: every invocation appends its arguments (one
    /// `arg:` line each) and the named variables (`NAME=value`, or
    /// `NAME unset`) to the returned log file.
    pub fn recording_agent(&self, variables: &[&str]) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.home.path().join("bin");
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("recording-agent");
        let log = dir.join("recording-agent.log");
        let mut text = format!(
            "#!/bin/sh\nexec >> '{}'\nfor a in \"$@\"; do printf 'arg:%s\\n' \"$a\"; done\n",
            log.display()
        );
        for name in variables {
            text.push_str(&format!(
                "if [ \"${{{name}+set}}\" = set ]; then printf '%s=%s\\n' {name} \"${name}\"; \
                 else printf '%s unset\\n' {name}; fi\n"
            ));
        }
        fs::write(&script, text).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    /// An executable standing in for an agent: every invocation appends
    /// `<cwd>|<arguments>` to the returned log file.
    pub fn fake_agent(&self) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.home.path().join("bin");
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agent");
        let log = dir.join("fake-agent.log");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `root` with its content.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.insert(path.clone(), Vec::new());
                walk(&path, out);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}
