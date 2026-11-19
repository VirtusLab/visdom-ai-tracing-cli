//! A throw-away home directory and git repository for running the built
//! `visdom` binary without touching the developer's own.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

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
        cmd.env_remove("VISDOM_AGENT")
            .env_remove("VISDOM_BAZAAR_URL");
        cmd
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
