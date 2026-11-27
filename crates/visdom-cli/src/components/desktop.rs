//! Will a session in the Claude Desktop app be traced, and can `visdom` open
//! the app here? Checked on top of everything the terminal is checked for:
//! the hooks, the login and the bazaar entry are in files both read.

use std::path::{Path, PathBuf};

use tracevault_cli::agent::Agent;
use tracevault_cli::desktop_evidence::Evidence;

use crate::finding::{command_line, Finding, Fix, FixAction, Severity};
use crate::mode::ResolvedMode;
use crate::surface::{refusal, supports, Platform, Surface};

const COMPONENT: &str = "desktop";

/// What was found out about the desktop app on this machine.
#[derive(Clone, Debug)]
pub struct DesktopFacts {
    pub platform: Platform,
    /// `None`: `visdom` cannot tell on this platform.
    pub app_installed: Option<bool>,
    /// The terminal CLI `visdom` opens the app with, and whether it is there.
    pub cli: String,
    pub cli_found: bool,
    pub home: PathBuf,
    /// The running `visdom`.
    pub visdom_exe: PathBuf,
    /// The `PATH` the developer's login shell exports, which is what the app
    /// reads when it starts and gives to its sessions' hooks (macOS and
    /// Linux, per the vendor's documentation). `None` when not established.
    pub hook_path: Option<Vec<PathBuf>>,
    /// The Claude Code the app downloaded for itself.
    pub bundled_version: Option<String>,
    /// What is recorded of opening the app and of traced desktop sessions.
    pub evidence: Result<Evidence, String>,
}

fn finding(id: &'static str, severity: Severity, message: impl Into<String>) -> Finding {
    let mut finding = Finding::new(COMPONENT, id, severity, message);
    finding.surface = Some(Surface::Desktop);
    finding
}

pub fn check(facts: &DesktopFacts, agent: Agent, mode: &ResolvedMode) -> Vec<Finding> {
    let mut findings = Vec::new();
    // The same decision, in the same words, as the launch's refusal.
    if let Err(why) = supports(Surface::Desktop, agent, mode, &facts.platform) {
        let text = refusal(&why);
        findings.push(finding(
            "desktop.mode",
            Severity::Error,
            text.strip_prefix("visdom: ").unwrap_or(&text),
        ));
    }
    findings.push(match facts.app_installed {
        Some(true) => finding("desktop.app", Severity::Ok, "Claude Desktop is installed"),
        Some(false) => finding(
            "desktop.app",
            Severity::Error,
            "Claude Desktop is not installed (or not where visdom looks: \
             VISDOM_CLAUDE_DESKTOP_APP names it)",
        ),
        None => finding(
            "desktop.app",
            Severity::Unknown,
            "visdom cannot tell whether Claude Desktop is installed on this platform",
        ),
    });
    findings.push(if facts.cli_found {
        finding(
            "desktop.cli",
            Severity::Ok,
            format!("the Claude Code CLI is here (`{}`)", facts.cli),
        )
    } else {
        finding(
            "desktop.cli",
            Severity::Error,
            format!(
                "`{}` was not found: install the Claude Code CLI, the desktop surface needs it \
                 to open the app and to register bazaar",
                facts.cli
            ),
        )
    });
    findings.push(hook_path(facts));
    if let Some(version) = &facts.bundled_version {
        findings.push(finding(
            "desktop.version",
            Severity::Info,
            format!(
                "Claude Desktop runs its own Claude Code ({version}), not the `{}` in the \
                 terminal",
                facts.cli
            ),
        ));
    }
    if facts.platform == Platform::Linux {
        findings.push(finding(
            "desktop.platform",
            Severity::Info,
            "best effort on Linux: the app is opened by its claude://code/new link, and the \
             folder is chosen in the app",
        ));
    }
    findings.push(evidence(&facts.evidence));
    findings
}

/// Hooks are installed as `visdom trace …`, by bare name: a desktop session
/// runs them with the `PATH` the app got, which is not this terminal's.
fn hook_path(facts: &DesktopFacts) -> Finding {
    const ID: &str = "desktop.hook_path";
    let Some(path) = &facts.hook_path else {
        return finding(
            ID,
            Severity::Unknown,
            format!(
                "visdom cannot tell which PATH desktop sessions get on {}: their hooks run \
                 `visdom` by name, and it is at {}",
                platform_name(&facts.platform),
                facts.visdom_exe.display()
            ),
        );
    };
    let exe_name = facts
        .visdom_exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "visdom".into());
    if path.iter().any(|dir| dir.join(&exe_name).is_file()) {
        return finding(
            ID,
            Severity::Ok,
            "hooks in desktop sessions will find `visdom` (it is on the PATH your login shell \
             exports, which the app reads when it starts)",
        );
    }
    let message = format!(
        "hooks in desktop sessions will not find `visdom`: {} is not on the PATH your login \
         shell exports, which is the one Claude Desktop reads when it starts. Add its directory \
         to your shell profile, then quit and reopen the app",
        facts.visdom_exe.display()
    );
    let found = finding(ID, Severity::Error, message);
    match link_target(path, &facts.visdom_exe, &facts.home) {
        Some(link) => {
            let args = vec![
                "-s".to_string(),
                facts.visdom_exe.display().to_string(),
                link.display().to_string(),
            ];
            found.with_fix(Fix {
                description: format!(
                    "run `{}` (a link to this visdom in a directory the app does search), then \
                     quit and reopen Claude Desktop",
                    command_line("ln", &args)
                ),
                action: FixAction::RunCommand {
                    program: "ln".to_string(),
                    args,
                    cwd: None,
                },
            })
        }
        None => found,
    }
}

/// Where a link to `exe` can be made: the first directory on `path` that is
/// the developer's own to write in and has no such name yet.
fn link_target(path: &[PathBuf], exe: &Path, home: &Path) -> Option<PathBuf> {
    if !cfg!(unix) {
        return None;
    }
    let name = exe.file_name()?;
    path.iter()
        .filter(|dir| dir.starts_with(home) && dir.is_dir())
        .filter(|dir| {
            dir.metadata()
                .is_ok_and(|meta| !meta.permissions().readonly())
        })
        .map(|dir| dir.join(name))
        .find(|link| link.symlink_metadata().is_err())
}

fn platform_name(platform: &Platform) -> &str {
    match platform {
        Platform::MacOs => "macOS",
        Platform::WindowsX64 => "Windows",
        Platform::Linux => "Linux",
        Platform::Other(name) => name,
    }
}

fn at(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| seconds.to_string())
}

/// What did happen, as against what should: was a desktop session's event
/// accepted by the server since the app was last opened by `visdom`?
fn evidence(evidence: &Result<Evidence, String>) -> Finding {
    const ID: &str = "desktop.evidence";
    let evidence = match evidence {
        Ok(evidence) => evidence,
        Err(why) => return finding(ID, Severity::Unknown, why.clone()),
    };
    match (evidence.opened_at, evidence.traced_at) {
        (Some(opened), Some(traced)) if traced < opened => finding(
            ID,
            Severity::Warn,
            format!(
                "Claude Desktop was opened at {} and no traced session has been seen since \
                 (the last one: {}). If you sent a prompt there since, check the hooks' PATH \
                 above first",
                at(opened),
                at(traced)
            ),
        ),
        (_, Some(traced)) => finding(
            ID,
            Severity::Ok,
            format!("a desktop session was traced at {}", at(traced)),
        ),
        (Some(opened), None) => finding(
            ID,
            Severity::Warn,
            format!(
                "Claude Desktop was opened at {} and no traced session has been seen since. If \
                 you sent a prompt there since, check the hooks' PATH above first",
                at(opened)
            ),
        ),
        (None, None) => finding(ID, Severity::Info, "no desktop session has been traced yet"),
    }
}

/// The newest Claude Code the app keeps under `<config dir>/Claude/claude-code/`.
pub fn bundled_version(config_root: &Path) -> Option<String> {
    let numbers = |name: &str| -> Option<Vec<u64>> {
        name.split('.').map(|part| part.parse().ok()).collect()
    };
    std::fs::read_dir(config_root.join("Claude").join("claude-code"))
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| numbers(&name).map(|n| (n, name)))
        .max()
        .map(|(_, name)| name)
}

/// The `PATH` `shell` exports as a login shell, or `None` when it cannot be
/// asked within a few seconds. Marked in the output, so whatever a profile
/// prints around it does not matter.
pub fn login_shell_path(shell: &str) -> Option<Vec<PathBuf>> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    const START: &str = "__VISDOM_PATH_START__";
    const END: &str = "__VISDOM_PATH_END__";
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-c",
            &format!("printf '%s%s%s' {START} \"$PATH\" {END}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let text = reader.join().ok()?;
    let path = text.split(START).nth(1)?.split(END).next()?;
    (!path.is_empty()).then(|| std::env::split_paths(path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::{ModeReason, TracingMode, TrafficPolicy};
    use crate::surface::Unsupported;

    fn mode(mode: TracingMode, reason: ModeReason) -> ResolvedMode {
        ResolvedMode {
            mode,
            reason,
            overruled: None,
        }
    }

    fn hooks_mode() -> ResolvedMode {
        mode(TracingMode::Hooks, ModeReason::Default)
    }

    /// A machine where everything is as it should be: `visdom` in `dir`,
    /// which the login shell has on its `PATH`.
    fn ok_facts(dir: &Path) -> DesktopFacts {
        let exe = dir.join("visdom");
        std::fs::write(&exe, "").unwrap();
        DesktopFacts {
            platform: Platform::MacOs,
            app_installed: Some(true),
            cli: "claude".into(),
            cli_found: true,
            home: PathBuf::from("/nonexistent/home"),
            visdom_exe: exe,
            hook_path: Some(vec![PathBuf::from("/usr/bin"), dir.to_path_buf()]),
            bundled_version: Some("2.1.330".into()),
            evidence: Ok(Evidence::default()),
        }
    }

    fn by_id<'a>(findings: &'a [Finding], id: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| panic!("no {id} in {findings:?}"))
    }

    #[test]
    fn a_good_setup_has_nothing_to_attend_to() {
        let dir = tempfile::tempdir().unwrap();
        let found = check(&ok_facts(dir.path()), Agent::ClaudeCode, &hooks_mode());
        for f in &found {
            assert!(matches!(f.severity, Severity::Ok | Severity::Info), "{f:?}");
            assert_eq!(f.component, "desktop");
            assert_eq!(f.surface, Some(Surface::Desktop));
        }
        let ids: Vec<&str> = found.iter().map(|f| f.id).collect();
        assert_eq!(
            ids,
            [
                "desktop.app",
                "desktop.cli",
                "desktop.hook_path",
                "desktop.version",
                "desktop.evidence"
            ]
        );
        assert!(by_id(&found, "desktop.version").message.contains("2.1.330"));
    }

    #[test]
    fn nothing_is_ok_that_was_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        let facts = DesktopFacts {
            app_installed: None,
            hook_path: None,
            evidence: Err("/x/desktop-evidence.json cannot be read: bad".into()),
            ..ok_facts(dir.path())
        };
        let found = check(&facts, Agent::ClaudeCode, &hooks_mode());
        for id in ["desktop.app", "desktop.hook_path", "desktop.evidence"] {
            assert_eq!(by_id(&found, id).severity, Severity::Unknown, "{id}");
        }
        assert!(by_id(&found, "desktop.evidence")
            .message
            .contains("/x/desktop-evidence.json"));
    }

    #[test]
    fn a_missing_app_or_cli_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let facts = DesktopFacts {
            app_installed: Some(false),
            cli_found: false,
            ..ok_facts(dir.path())
        };
        let found = check(&facts, Agent::ClaudeCode, &hooks_mode());
        assert_eq!(by_id(&found, "desktop.app").severity, Severity::Error);
        assert_eq!(by_id(&found, "desktop.cli").severity, Severity::Error);
    }

    #[test]
    fn gateway_mode_is_the_launch_refusal_word_for_word() {
        let dir = tempfile::tempdir().unwrap();
        for reason in [
            ModeReason::Chosen,
            ModeReason::Policy(TrafficPolicy::Enforced),
        ] {
            let found = check(
                &ok_facts(dir.path()),
                Agent::ClaudeCode,
                &mode(TracingMode::Gateway, reason),
            );
            let f = by_id(&found, "desktop.mode");
            assert_eq!(f.severity, Severity::Error);
            assert_eq!(
                format!("visdom: {}", f.message),
                refusal(&Unsupported::GatewayOnDesktop(reason))
            );
        }
        // And nothing of the kind in hooks mode.
        let found = check(&ok_facts(dir.path()), Agent::ClaudeCode, &hooks_mode());
        assert!(found.iter().all(|f| f.id != "desktop.mode"));
    }

    #[test]
    fn a_visdom_the_app_will_not_find_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let facts = DesktopFacts {
            hook_path: Some(vec![PathBuf::from("/usr/bin")]),
            ..ok_facts(dir.path())
        };
        let found = check(&facts, Agent::ClaudeCode, &hooks_mode());
        let f = by_id(&found, "desktop.hook_path");
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains(&facts.visdom_exe.display().to_string()));
        assert!(f.message.contains("quit and reopen"), "{}", f.message);
        // Nowhere of the developer's own to link it from.
        assert!(f.fix.is_none());
    }

    #[test]
    fn a_link_is_offered_only_where_none_is_in_the_way() {
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("elsewhere/visdom");
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = [PathBuf::from("/usr/bin"), bin.clone()];
        if cfg!(unix) {
            assert_eq!(
                link_target(&path, &exe, home.path()),
                Some(bin.join("visdom"))
            );
            let facts = DesktopFacts {
                home: home.path().to_path_buf(),
                visdom_exe: exe.clone(),
                hook_path: Some(path.to_vec()),
                ..ok_facts(home.path())
            };
            let found = check(&facts, Agent::ClaudeCode, &hooks_mode());
            let fix = by_id(&found, "desktop.hook_path").fix.clone().unwrap();
            assert_eq!(
                fix.action,
                FixAction::RunCommand {
                    program: "ln".into(),
                    args: vec![
                        "-s".into(),
                        exe.display().to_string(),
                        bin.join("visdom").display().to_string()
                    ],
                    cwd: None,
                }
            );
        }
        // Not the developer's own directory: nothing is offered there.
        assert_eq!(
            link_target(&path, &exe, Path::new("/nonexistent/home")),
            None
        );
        // Something of that name is there already: it is not replaced.
        std::fs::write(bin.join("visdom"), "").unwrap();
        assert_eq!(link_target(&path, &exe, home.path()), None);
    }

    #[test]
    fn linux_is_labelled_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let facts = DesktopFacts {
            platform: Platform::Linux,
            ..ok_facts(dir.path())
        };
        let found = check(&facts, Agent::ClaudeCode, &hooks_mode());
        let f = by_id(&found, "desktop.platform");
        assert_eq!(f.severity, Severity::Info);
        assert!(f.message.contains("best effort"));
    }

    #[test]
    fn opened_after_the_last_trace_is_a_warning() {
        let with = |opened_at, traced_at| {
            evidence(&Ok(Evidence {
                opened_at,
                traced_at,
                ..Evidence::default()
            }))
            .severity
        };
        assert_eq!(with(Some(20), Some(10)), Severity::Warn);
        assert_eq!(with(Some(20), None), Severity::Warn);
        assert_eq!(with(Some(10), Some(20)), Severity::Ok);
        assert_eq!(with(None, Some(20)), Severity::Ok);
        assert_eq!(with(None, None), Severity::Info);
    }

    #[test]
    fn the_newest_bundled_version_is_reported() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(bundled_version(root.path()), None);
        for name in ["2.1.99", "2.1.330", "2.1.9", "staging"] {
            std::fs::create_dir_all(root.path().join("Claude/claude-code").join(name)).unwrap();
        }
        assert_eq!(bundled_version(root.path()).as_deref(), Some("2.1.330"));
    }

    #[cfg(unix)]
    #[test]
    fn the_login_shell_is_asked_for_its_path() {
        let path = login_shell_path("/bin/sh").expect("sh answers");
        assert!(!path.is_empty());
        assert_eq!(login_shell_path("/nonexistent/shell"), None);
    }
}
