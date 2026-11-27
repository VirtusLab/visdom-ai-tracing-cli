//! Where the coding agent opens: in the terminal `visdom` runs in, or in the
//! Claude Desktop app.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tracevault_cli::agent::Agent;

use crate::mode::{ModeReason, ResolvedMode, TracingMode, TrafficPolicy};

/// The environment variable that names the developer's choice of surface.
pub const SURFACE_ENV: &str = "VISDOM_SURFACE";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// The agent runs in this terminal.
    Terminal,
    /// The agent opens in the Claude Desktop app.
    Desktop,
}

impl Surface {
    pub fn name(self) -> &'static str {
        match self {
            Surface::Terminal => "terminal",
            Surface::Desktop => "desktop",
        }
    }
}

/// Parse a surface as the command line and the config files spell it.
pub fn parse_surface(name: &str) -> Result<Surface, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "terminal" => Ok(Surface::Terminal),
        "desktop" => Ok(Surface::Desktop),
        _ => Err(format!(
            "unknown surface `{name}` (use terminal or desktop)"
        )),
    }
}

/// The surface of a launch: the flag, then the variable, then the repository's
/// config, then the user's; the terminal when nobody chose.
pub fn resolve_surface(
    flag: Option<Surface>,
    env: Option<&str>,
    repo: Option<Surface>,
    user: Option<Surface>,
) -> Result<Surface, String> {
    if let Some(surface) = flag {
        return Ok(surface);
    }
    match env.filter(|v| !v.trim().is_empty()) {
        Some(name) => parse_surface(name).map_err(|e| format!("{SURFACE_ENV}: {e}")),
        None => Ok(repo.or(user).unwrap_or(Surface::Terminal)),
    }
}

/// The ticket that adds gateway sessions to the desktop surface.
pub const GATEWAY_ON_DESKTOP_TICKET: &str = "VIS-1396";

/// The operating system and architecture `visdom` runs on, as far as opening
/// Claude Desktop cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    WindowsX64,
    Linux,
    /// Anything else, by a readable name (`windows-aarch64`, `freebsd`).
    Other(String),
}

impl Platform {
    pub fn current() -> Platform {
        match std::env::consts::OS {
            "macos" => Platform::MacOs,
            "windows" if std::env::consts::ARCH == "x86_64" => Platform::WindowsX64,
            "windows" => Platform::Other(format!("windows-{}", std::env::consts::ARCH)),
            "linux" => Platform::Linux,
            other => Platform::Other(other.to_string()),
        }
    }
}

/// Why a surface cannot run a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
    AgentHasNoDesktop(Agent),
    GatewayOnDesktop(ModeReason),
    Platform(String),
    Arguments(Vec<String>),
}

/// The one place that says whether `surface` can run this session. Phase B
/// (VIS-1396) changes this.
pub fn supports(
    surface: Surface,
    agent: Agent,
    mode: &ResolvedMode,
    platform: &Platform,
) -> Result<(), Unsupported> {
    if surface == Surface::Terminal {
        return Ok(());
    }
    if agent != Agent::ClaudeCode {
        return Err(Unsupported::AgentHasNoDesktop(agent));
    }
    if let Platform::Other(name) = platform {
        return Err(Unsupported::Platform(name.clone()));
    }
    if mode.mode == TracingMode::Gateway {
        return Err(Unsupported::GatewayOnDesktop(mode.reason));
    }
    Ok(())
}

/// The arguments `claude --desktop` takes on `platform`, or the ones that
/// cannot be honoured. macOS and Windows take `--continue`/`-c` and a resume
/// of a session id (`--resume <id>`, `-r <id>`, `--resume=<id>`); Linux opens
/// a link, which carries no arguments.
pub fn desktop_args(platform: &Platform, args: &[String]) -> Result<Vec<String>, Unsupported> {
    match platform {
        Platform::Other(name) => Err(Unsupported::Platform(name.clone())),
        Platform::Linux if args.is_empty() => Ok(Vec::new()),
        Platform::Linux => Err(Unsupported::Arguments(args.to_vec())),
        Platform::MacOs | Platform::WindowsX64 => {
            let is_id = |value: &str| uuid::Uuid::parse_str(value).is_ok();
            let mut refused = Vec::new();
            let mut i = 0;
            while i < args.len() {
                let arg = args[i].as_str();
                let next = args.get(i + 1).map(String::as_str);
                match arg {
                    "--continue" | "-c" => i += 1,
                    "--resume" | "-r" => match next {
                        Some(id) if is_id(id) => i += 2,
                        Some(other) if !other.starts_with('-') => {
                            refused.push(arg.to_string());
                            refused.push(other.to_string());
                            i += 2;
                        }
                        _ => {
                            refused.push(arg.to_string());
                            i += 1;
                        }
                    },
                    _ if arg.strip_prefix("--resume=").is_some_and(is_id) => i += 1,
                    _ => {
                        refused.push(arg.to_string());
                        i += 1;
                    }
                }
            }
            if refused.is_empty() {
                Ok(args.to_vec())
            } else {
                Err(Unsupported::Arguments(refused))
            }
        }
    }
}

/// What `visdom` says when a surface cannot run the session.
pub fn refusal(why: &Unsupported) -> String {
    match why {
        Unsupported::AgentHasNoDesktop(agent) => format!(
            "visdom: the desktop surface is not supported for {} yet: use --surface terminal",
            agent.label()
        ),
        Unsupported::GatewayOnDesktop(ModeReason::Policy(TrafficPolicy::Enforced)) => format!(
            "visdom: the workspace requires the AI Gateway, and gateway sessions in the Claude Desktop app are not supported yet ({GATEWAY_ON_DESKTOP_TICKET}): use --surface terminal"
        ),
        Unsupported::GatewayOnDesktop(_) => format!(
            "visdom: gateway mode was chosen, and gateway sessions in the Claude Desktop app are not supported yet ({GATEWAY_ON_DESKTOP_TICKET}): use --surface terminal, or --mode hooks"
        ),
        Unsupported::Platform(platform) => format!(
            "visdom: opening Claude Desktop is not supported on {platform}: use --surface terminal"
        ),
        Unsupported::Arguments(args) => format!(
            "visdom: these arguments cannot be given to the Claude Desktop app: {}. They go to the terminal agent: use --surface terminal",
            args.join(" ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "123e4567-e89b-12d3-a456-426614174000";

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn mode(mode: TracingMode, reason: ModeReason) -> ResolvedMode {
        ResolvedMode {
            mode,
            reason,
            overruled: None,
        }
    }

    const AGENTS: [Agent; 5] = [
        Agent::ClaudeCode,
        Agent::Codex,
        Agent::Gsd,
        Agent::OpenCode,
        Agent::Amp,
    ];

    fn platforms() -> [Platform; 4] {
        [
            Platform::MacOs,
            Platform::WindowsX64,
            Platform::Linux,
            Platform::Other("freebsd".into()),
        ]
    }

    #[test]
    fn the_terminal_supports_everything_it_did() {
        for agent in AGENTS {
            for m in [
                mode(TracingMode::Hooks, ModeReason::Default),
                mode(TracingMode::Gateway, ModeReason::Chosen),
                mode(
                    TracingMode::Gateway,
                    ModeReason::Policy(TrafficPolicy::Enforced),
                ),
            ] {
                for p in platforms() {
                    assert_eq!(supports(Surface::Terminal, agent, &m, &p), Ok(()));
                }
            }
        }
    }

    #[test]
    fn the_desktop_is_claude_in_hooks_mode() {
        let hooks = mode(TracingMode::Hooks, ModeReason::Default);
        for p in [Platform::MacOs, Platform::WindowsX64, Platform::Linux] {
            assert_eq!(
                supports(Surface::Desktop, Agent::ClaudeCode, &hooks, &p),
                Ok(())
            );
        }
        assert_eq!(
            supports(Surface::Desktop, Agent::Codex, &hooks, &Platform::MacOs),
            Err(Unsupported::AgentHasNoDesktop(Agent::Codex))
        );
        let chosen = mode(TracingMode::Gateway, ModeReason::Chosen);
        assert_eq!(
            supports(
                Surface::Desktop,
                Agent::ClaudeCode,
                &chosen,
                &Platform::MacOs
            ),
            Err(Unsupported::GatewayOnDesktop(ModeReason::Chosen))
        );
        let enforced = ModeReason::Policy(TrafficPolicy::Enforced);
        assert_eq!(
            supports(
                Surface::Desktop,
                Agent::ClaudeCode,
                &mode(TracingMode::Gateway, enforced),
                &Platform::Linux
            ),
            Err(Unsupported::GatewayOnDesktop(enforced))
        );
        assert_eq!(
            supports(
                Surface::Desktop,
                Agent::ClaudeCode,
                &hooks,
                &Platform::Other("freebsd".into())
            ),
            Err(Unsupported::Platform("freebsd".into()))
        );
    }

    #[test]
    fn continue_and_resume_pass_on_macos_and_windows() {
        let resume_eq = format!("--resume={ID}");
        for p in [Platform::MacOs, Platform::WindowsX64] {
            for a in [
                vec!["--continue"],
                vec!["-c"],
                vec!["--resume", ID],
                vec!["-r", ID],
                vec![resume_eq.as_str()],
            ] {
                let args = strings(&a);
                assert_eq!(desktop_args(&p, &args), Ok(args.clone()));
            }
        }
    }

    #[test]
    fn resume_without_an_id_is_refused() {
        for a in [vec!["--resume"], vec!["--resume", "my session"], vec!["-r"]] {
            let args = strings(&a);
            assert_eq!(
                desktop_args(&Platform::MacOs, &args),
                Err(Unsupported::Arguments(args))
            );
        }
    }

    #[test]
    fn every_other_argument_is_refused_by_name() {
        let args = strings(&["--model", "x", "fix the bug"]);
        assert_eq!(
            desktop_args(&Platform::WindowsX64, &args),
            Err(Unsupported::Arguments(args))
        );
        let mixed = strings(&["--continue", "--model", "x"]);
        assert_eq!(
            desktop_args(&Platform::MacOs, &mixed),
            Err(Unsupported::Arguments(strings(&["--model", "x"])))
        );
    }

    #[test]
    fn linux_takes_no_arguments() {
        let args = strings(&["--continue"]);
        assert_eq!(
            desktop_args(&Platform::Linux, &args),
            Err(Unsupported::Arguments(args))
        );
        assert_eq!(desktop_args(&Platform::Linux, &[]), Ok(Vec::new()));
    }

    #[test]
    fn each_refusal_names_the_way_out() {
        let gateway = [
            Unsupported::GatewayOnDesktop(ModeReason::Chosen),
            Unsupported::GatewayOnDesktop(ModeReason::Policy(TrafficPolicy::Enforced)),
        ];
        let others = [
            Unsupported::AgentHasNoDesktop(Agent::Codex),
            Unsupported::Platform("freebsd".into()),
            Unsupported::Arguments(strings(&["--model", "x"])),
        ];
        for why in gateway.iter().chain(others.iter()) {
            let text = refusal(why);
            assert!(text.starts_with("visdom: "), "{text}");
            assert!(text.contains("--surface terminal"), "{text}");
        }
        for why in &gateway {
            assert!(refusal(why).contains("VIS-1396"));
        }
        assert_eq!(
            refusal(&others[0]),
            "visdom: the desktop surface is not supported for Codex yet: use --surface terminal"
        );
        assert_eq!(
            refusal(&others[2]),
            "visdom: these arguments cannot be given to the Claude Desktop app: --model x. They go to the terminal agent: use --surface terminal"
        );
        assert!(refusal(&gateway[1]).contains("requires the AI Gateway"));
        assert!(refusal(&gateway[0]).contains("--mode hooks"));
    }

    #[test]
    fn the_flag_wins_then_the_variable_then_the_repo_then_the_user() {
        use Surface::*;
        assert_eq!(
            resolve_surface(
                Some(Desktop),
                Some("terminal"),
                Some(Terminal),
                Some(Terminal)
            ),
            Ok(Desktop)
        );
        assert_eq!(
            resolve_surface(None, Some("desktop"), Some(Terminal), Some(Terminal)),
            Ok(Desktop)
        );
        assert_eq!(
            resolve_surface(None, None, Some(Desktop), Some(Terminal)),
            Ok(Desktop)
        );
        assert_eq!(
            resolve_surface(None, Some("  "), None, Some(Desktop)),
            Ok(Desktop)
        );
        assert_eq!(resolve_surface(None, None, None, None), Ok(Terminal));
    }

    #[test]
    fn an_unknown_surface_in_the_variable_names_the_variable() {
        let e = resolve_surface(None, Some("web"), None, None).unwrap_err();
        assert_eq!(
            e,
            "VISDOM_SURFACE: unknown surface `web` (use terminal or desktop)"
        );
    }
}
