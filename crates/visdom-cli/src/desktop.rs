//! Opening the Claude Desktop app instead of the terminal agent.
//!
//! macOS and x64 Windows hand the terminal to `claude --desktop`, which opens
//! the app on the directory it is run in. Linux has no such command: the app
//! is opened through its `claude://code/new` link, which the vendor documents
//! without a way to name a local folder, so the developer is told which one to
//! choose. `visdom` does not become the agent here: it opens the app, says in
//! one line what state that leaves the developer in, and exits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::finding::{Finding, Severity};
use crate::surface::Platform;

/// The link that opens the app's new-session page.
pub const CODE_NEW_LINK: &str = "claude://code/new";

/// Names the app itself (a `.app` bundle or a `.desktop` file), for a place
/// `visdom` does not look in by itself.
pub const APP_ENV: &str = "VISDOM_CLAUDE_DESKTOP_APP";

/// Variables of the session `visdom` runs in. The app keeps the environment
/// it was started with and gives it to every later session, so none of these
/// may reach it: a desktop session would be attributed to this one, or have
/// its hooks told to record nothing.
const SESSION_VARIABLES: [&str; 10] = [
    "VISDOM_SESSION_ID",
    "TRACEVAULT_SESSION_ID",
    "VISDOM_GATEWAY_SESSION",
    "TRACEVAULT_GATEWAY_SESSION",
    "VISDOM_TRACING_MODE",
    "TRACEVAULT_TRACING_MODE",
    "VISDOM_LAUNCHED",
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

fn is_session_variable(name: &str) -> bool {
    SESSION_VARIABLES.contains(&name) || name.starts_with("CLAUDE_CODE_")
}

/// The command that opens the app, and how it is to be run.
#[derive(Debug)]
pub struct DesktopOpen {
    pub command: Command,
    /// Started and left running: the opener may not return while the app runs.
    pub detached: bool,
    /// `claude --desktop` refuses redirected output.
    pub needs_terminal: bool,
    /// The folder the developer must choose in the app themselves.
    pub choose_folder: Option<PathBuf>,
}

/// The command that opens the app on `start_dir`. `claude` is the terminal
/// CLI, `args` what [`crate::surface::desktop_args`] let through, and
/// `env_names` the variables set now, for the cleaning.
pub fn open_command(
    platform: &Platform,
    claude: &str,
    start_dir: &Path,
    args: &[String],
    env_names: &[String],
) -> DesktopOpen {
    let mut open = match platform {
        Platform::Linux => {
            let mut command = Command::new("xdg-open");
            command.arg(link_for(start_dir, None));
            DesktopOpen {
                command,
                detached: true,
                needs_terminal: false,
                choose_folder: Some(start_dir.to_path_buf()),
            }
        }
        // `supports` has refused every other platform before this is asked.
        _ => {
            let mut command = Command::new(claude);
            command.arg("--desktop").args(args);
            DesktopOpen {
                command,
                detached: false,
                needs_terminal: true,
                choose_folder: None,
            }
        }
    };
    open.command.current_dir(start_dir);
    for name in SESSION_VARIABLES {
        open.command.env_remove(name);
    }
    for name in env_names.iter().filter(|name| is_session_variable(name)) {
        open.command.env_remove(name);
    }
    open
}

/// The new-session link, naming `start_dir` in `folder_param` when the app
/// has a parameter for it (none is documented or was seen to work).
pub fn link_for(start_dir: &Path, folder_param: Option<&str>) -> String {
    match folder_param {
        None => CODE_NEW_LINK.to_string(),
        Some(param) => format!(
            "{CODE_NEW_LINK}?{param}={}",
            percent_encode(&start_dir.to_string_lossy())
        ),
    }
}

/// Everything but RFC 3986's unreserved characters, as `%XX` of its UTF-8.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Whether Claude Desktop is installed. `None` where `visdom` cannot tell.
/// `env` reads the environment: [`APP_ENV`], and on Linux the XDG data
/// directories, whose `applications/*.desktop` files say who handles
/// `claude://` links.
pub fn app_installed(
    platform: &Platform,
    home: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<bool> {
    if let Some(named) = env(APP_ENV).filter(|v| !v.trim().is_empty()) {
        return Some(Path::new(&named).exists());
    }
    match platform {
        Platform::MacOs => Some(
            Path::new("/Applications/Claude.app").exists()
                || home.join("Applications/Claude.app").exists(),
        ),
        Platform::Linux => {
            let set = |name: &str| env(name).filter(|v| !v.trim().is_empty());
            let mut dirs = vec![set("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))];
            let system =
                set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
            dirs.extend(std::env::split_paths(&system));
            Some(
                dirs.iter()
                    .any(|dir| handles_claude_links(&dir.join("applications"))),
            )
        }
        Platform::WindowsX64 | Platform::Other(_) => None,
    }
}

/// Whether a `.desktop` file in `dir` registers the `claude://` scheme.
fn handles_claude_links(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension().is_some_and(|e| e == "desktop")
            && std::fs::read_to_string(&path).is_ok_and(|text| {
                text.lines().any(|line| {
                    line.strip_prefix("MimeType=").is_some_and(|types| {
                        types
                            .split(';')
                            .any(|t| t.trim() == "x-scheme-handler/claude")
                    })
                })
            })
    })
}

/// Where `program` is, the way a shell would find it: as given when it names
/// a path, else the first executable of that name on `path`.
pub fn find_program(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let is_executable = |candidate: &Path| -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            candidate.is_file()
        }
    };
    if Path::new(program).components().count() > 1 {
        let candidate = PathBuf::from(program);
        return is_executable(&candidate).then_some(candidate);
    }
    let names: Vec<String> = if cfg!(windows) {
        ["", ".exe", ".cmd", ".bat"]
            .iter()
            .map(|ext| format!("{program}{ext}"))
            .collect()
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(path?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_executable(candidate))
}

/// The state `visdom` leaves the developer in.
#[derive(Debug, PartialEq, Eq)]
pub enum Closing {
    OpenedTraced,
    /// Opened, with what keeps its sessions from being traced.
    OpenedNotTraced(Vec<String>),
    NotOpened(String),
}

/// The last line `visdom` prints on the desktop surface.
pub fn closing_line(closing: &Closing, choose_folder: Option<&Path>) -> String {
    let mut line = match closing {
        Closing::OpenedTraced => {
            "visdom: opened Claude Desktop; sessions there are traced by hooks.".to_string()
        }
        Closing::OpenedNotTraced(why) => format!(
            "visdom: opened Claude Desktop, but sessions there are NOT traced: {}. Run visdom \
             doctor --surface desktop.",
            why.join("; ")
        ),
        Closing::NotOpened(why) => format!("visdom: Claude Desktop was not opened: {why}"),
    };
    if let (Some(folder), false) = (choose_folder, matches!(closing, Closing::NotOpened(_))) {
        line.push_str(&format!(
            " Choose this folder in the app: {}",
            folder.display()
        ));
    }
    line
}

/// What an opened app's sessions are left as: not traced while an error
/// remains in what hooks tracing needs.
pub fn closing_for(findings: &[Finding]) -> Closing {
    let why: Vec<String> = findings
        .iter()
        .filter(|f| f.severity == Severity::Error && matches!(f.component, "tracing" | "desktop"))
        .map(|f| f.message.clone())
        .collect();
    if why.is_empty() {
        Closing::OpenedTraced
    } else {
        Closing::OpenedNotTraced(why)
    }
}

/// How long a detached opener is given to fail. One that is still running
/// after this is the app itself, starting.
const OPENER_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Start a detached opener and leave it running. `Err` says why the app was
/// not opened: the opener is missing, or it gave up at once.
pub fn spawn_detached(mut command: Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing this terminal must not take the app down.
        // SAFETY: `setsid` is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "{program} could not open the {CODE_NEW_LINK} link ({status}): is Claude \
                     Desktop installed?"
                ))
            }
            Ok(None) if started.elapsed() >= OPENER_GRACE => return Ok(()),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn removed(command: &Command) -> Vec<String> {
        command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn macos_and_windows_hand_the_terminal_to_claude_desktop() {
        for platform in [Platform::MacOs, Platform::WindowsX64] {
            let o = open_command(
                &platform,
                "claude",
                Path::new("/r"),
                &["--continue".into()],
                &[],
            );
            assert_eq!(o.command.get_program(), "claude");
            assert_eq!(args_of(&o.command), ["--desktop", "--continue"]);
            assert_eq!(o.command.get_current_dir(), Some(Path::new("/r")));
            assert!(o.needs_terminal && !o.detached && o.choose_folder.is_none());
        }
    }

    #[test]
    fn linux_opens_the_link_detached_and_names_the_folder() {
        let o = open_command(&Platform::Linux, "claude", Path::new("/r"), &[], &[]);
        assert_eq!(o.command.get_program(), "xdg-open");
        assert_eq!(args_of(&o.command), [CODE_NEW_LINK]);
        assert!(o.detached && !o.needs_terminal);
        assert_eq!(o.choose_folder.as_deref(), Some(Path::new("/r")));
    }

    #[test]
    fn a_link_encodes_every_reserved_character() {
        assert_eq!(
            link_for(Path::new("/a b/#c&d%/ż"), Some("path")),
            "claude://code/new?path=%2Fa%20b%2F%23c%26d%25%2F%C5%BC"
        );
        assert_eq!(link_for(Path::new("/a"), None), CODE_NEW_LINK);
    }

    #[test]
    fn the_opener_gets_none_of_this_sessions_variables() {
        let present: Vec<String> = [
            "PATH",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CONFIG_DIR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for platform in [Platform::MacOs, Platform::WindowsX64, Platform::Linux] {
            let o = open_command(&platform, "claude", Path::new("/r"), &[], &present);
            let gone = removed(&o.command);
            for name in [
                "VISDOM_SESSION_ID",
                "TRACEVAULT_SESSION_ID",
                "VISDOM_GATEWAY_SESSION",
                "VISDOM_TRACING_MODE",
                "VISDOM_LAUNCHED",
                "CLAUDECODE",
                "CLAUDE_CODE_SESSION_ID",
                "CLAUDE_CODE_ENTRYPOINT",
            ] {
                assert!(gone.contains(&name.to_string()), "{name} on {platform:?}");
            }
            // The developer's own settings stay.
            assert!(!gone.contains(&"PATH".to_string()));
            assert!(!gone.contains(&"CLAUDE_CONFIG_DIR".to_string()));
        }
    }

    #[test]
    fn the_closing_line_has_three_states() {
        assert_eq!(
            closing_line(&Closing::OpenedTraced, None),
            "visdom: opened Claude Desktop; sessions there are traced by hooks."
        );
        assert_eq!(
            closing_line(
                &Closing::OpenedNotTraced(vec!["not logged in".into(), "no hooks".into()]),
                Some(Path::new("/r"))
            ),
            "visdom: opened Claude Desktop, but sessions there are NOT traced: not logged in; no \
             hooks. Run visdom doctor --surface desktop. Choose this folder in the app: /r"
        );
        // Nothing to choose in an app that was not opened.
        assert_eq!(
            closing_line(&Closing::NotOpened("no app".into()), Some(Path::new("/r"))),
            "visdom: Claude Desktop was not opened: no app"
        );
    }

    #[test]
    fn only_errors_in_tracing_or_the_desktop_keep_sessions_untraced() {
        let mk = |component, severity| Finding::new(component, "x", severity, "m");
        assert_eq!(
            closing_for(&[
                mk("tracing", Severity::Warn),
                mk("bazaar", Severity::Error),
                mk("desktop", Severity::Unknown),
            ]),
            Closing::OpenedTraced
        );
        assert_eq!(
            closing_for(&[
                mk("tracing", Severity::Error),
                mk("desktop", Severity::Error)
            ]),
            Closing::OpenedNotTraced(vec!["m".into(), "m".into()])
        );
    }

    #[test]
    fn the_app_is_found_by_its_link_handler_on_linux() {
        let home = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        let data = home.path().join("data");
        let env = |name: &str| match name {
            "XDG_DATA_HOME" => Some(data.display().to_string()),
            "XDG_DATA_DIRS" => Some(home.path().join("sys").display().to_string()),
            _ => None,
        };
        assert_eq!(
            app_installed(&Platform::Linux, home.path(), &env),
            Some(false)
        );
        let apps = data.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(
            apps.join("other.desktop"),
            "[Desktop Entry]\nMimeType=x-scheme-handler/claude-cli;\n",
        )
        .unwrap();
        assert_eq!(
            app_installed(&Platform::Linux, home.path(), &env),
            Some(false)
        );
        std::fs::write(
            apps.join("claude.desktop"),
            "[Desktop Entry]\nMimeType=text/html;x-scheme-handler/claude;\n",
        )
        .unwrap();
        assert_eq!(
            app_installed(&Platform::Linux, home.path(), &env),
            Some(true)
        );
        // Where visdom cannot look, it does not say.
        assert_eq!(
            app_installed(&Platform::WindowsX64, home.path(), &none),
            None
        );
        // A named app is believed on every platform.
        let named = |name: &str| (name == APP_ENV).then(|| "/nonexistent/Claude.app".to_string());
        assert_eq!(
            app_installed(&Platform::WindowsX64, home.path(), &named),
            Some(false)
        );
    }

    #[test]
    fn a_program_is_found_by_path_or_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(find_program("tool", Some(&path)), Some(exe.clone()));
        assert_eq!(find_program("absent", Some(&path)), None);
        assert_eq!(find_program("tool", None), None);
        assert_eq!(
            find_program(&exe.display().to_string(), None),
            Some(exe.clone())
        );
        assert_eq!(find_program("/nonexistent/tool", Some(&path)), None);
    }
}
