//! Where the coding agent opens: in the terminal `visdom` runs in, or in the
//! Claude Desktop app.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

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
