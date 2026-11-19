//! Codex CLI.
//!
//! MCP servers live in the global `<codex home>/config.toml` and, for a
//! project, in its `.codex/config.toml`: `[mcp_servers.<name>] url = …` with
//! `[mcp_servers.<name>.oauth] client_id`; `enabled = false` switches one off.
//! `codex mcp add` writes the global file only.

use tracevault_cli::agent::Agent;

use std::path::Path;

use super::{read_optional, run_command, AgentAdapter, AgentPaths, McpEntry, McpScope, Workdir};
use crate::finding::FixAction;

pub struct Codex {
    paths: AgentPaths,
    program: String,
}

impl Codex {
    pub fn new(paths: AgentPaths, program_override: Option<String>) -> Self {
        Codex {
            paths,
            program: program_override.unwrap_or_else(|| "codex".to_string()),
        }
    }
}

/// Add the `[mcp_servers.*]` of the config file at `path` to `out`, skipping
/// names already there. A missing file has none.
fn read_servers(path: &Path, scope: McpScope, out: &mut Vec<McpEntry>) -> Result<(), String> {
    let Some(text) = read_optional(path)? else {
        return Ok(());
    };
    let config: toml::Value =
        toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    let Some(servers) = config.get("mcp_servers").and_then(|s| s.as_table()) else {
        return Ok(());
    };
    let text = |v: Option<&toml::Value>| v.and_then(|v| v.as_str()).map(String::from);
    let only = |v: Option<&toml::Value>, known: &[&str]| {
        v.and_then(|v| v.as_table())
            .is_none_or(|t| t.keys().all(|k| known.contains(&k.as_str())))
    };
    for (name, server) in servers {
        if out.iter().any(|e| &e.name == name) {
            continue;
        }
        out.push(McpEntry {
            name: name.clone(),
            url: text(server.get("url")),
            oauth_client_id: text(server.get("oauth").and_then(|o| o.get("client_id"))),
            scope,
            extra_settings: !only(Some(server), &["url", "oauth", "enabled"])
                || !only(server.get("oauth"), &["client_id"]),
            disabled: server.get("enabled").and_then(|e| e.as_bool()) == Some(false),
        });
    }
    Ok(())
}

impl AgentAdapter for Codex {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn mcp_entries(&self, workdir: &Workdir) -> Result<Vec<McpEntry>, String> {
        // A project's own `.codex/config.toml` comes first: its definition of
        // a name is the one Codex uses.
        let mut out = Vec::new();
        if let Some(checkout) = &workdir.checkout {
            read_servers(
                &checkout.join(".codex").join("config.toml"),
                McpScope::Project,
                &mut out,
            )?;
        }
        read_servers(
            &self.paths.codex_home.join("config.toml"),
            McpScope::User,
            &mut out,
        )?;
        Ok(out)
    }

    fn register_mcp(
        &self,
        name: &str,
        url: &str,
        client_id: &str,
        _scope: McpScope,
        _workdir: &Workdir,
    ) -> FixAction {
        run_command(
            &self.program,
            &[
                "mcp",
                "add",
                name,
                "--url",
                url,
                "--oauth-client-id",
                client_id,
            ],
            None,
        )
    }

    fn remove_mcp(&self, entry: &McpEntry, _workdir: &Workdir) -> FixAction {
        run_command(&self.program, &["mcp", "remove", &entry.name], None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn codex(codex_home: &Path) -> Codex {
        Codex::new(
            AgentPaths {
                home: codex_home.to_path_buf(),
                codex_home: codex_home.to_path_buf(),
            },
            None,
        )
    }

    #[test]
    fn codex_reads_url_and_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("config.toml"),
            r#"model = "gpt-6"

[mcp_servers.bazaar]
url = "https://ctx.example.com/mcp/remote"

[mcp_servers.bazaar.oauth]
client_id = "visdom-context-mcp"

[mcp_servers.local-tool]
command = "npx"
args = ["tool"]
"#,
        )
        .unwrap();
        let got = codex(tmp.path())
            .mcp_entries(&Workdir::plain("/tmp"))
            .unwrap();
        assert_eq!(
            got,
            [
                McpEntry {
                    name: "bazaar".to_string(),
                    url: Some("https://ctx.example.com/mcp/remote".to_string()),
                    oauth_client_id: Some("visdom-context-mcp".to_string()),
                    scope: McpScope::User,
                    extra_settings: false,
                    disabled: false,
                },
                McpEntry {
                    name: "local-tool".to_string(),
                    url: None,
                    oauth_client_id: None,
                    scope: McpScope::User,
                    extra_settings: true,
                    disabled: false,
                },
            ]
        );
    }

    #[test]
    fn project_config_is_read_and_wins_by_name() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            "[mcp_servers.bazaar]\nurl = \"https://ctx/mcp\"\n[mcp_servers.bazaar.oauth]\n\
             client_id = \"right\"\n[mcp_servers.other]\nurl = \"https://o/mcp\"\nenabled = false\n",
        )
        .unwrap();
        fs::create_dir_all(repo.path().join(".codex")).unwrap();
        fs::write(
            repo.path().join(".codex/config.toml"),
            "[mcp_servers.bazaar]\nurl = \"https://ctx/mcp\"\n",
        )
        .unwrap();
        let got = codex(home.path())
            .mcp_entries(&Workdir::checkout(repo.path()))
            .unwrap();
        let seen: Vec<(&str, McpScope, Option<&str>, bool)> = got
            .iter()
            .map(|e| {
                (
                    e.name.as_str(),
                    e.scope,
                    e.oauth_client_id.as_deref(),
                    e.disabled,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("bazaar", McpScope::Project, None, false),
                ("other", McpScope::User, None, true),
            ]
        );
        assert!(!got[1].extra_settings, "`enabled` alone is not an extra");
    }

    #[test]
    fn missing_codex_config_means_no_entries() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(codex(tmp.path())
            .mcp_entries(&Workdir::plain("/tmp"))
            .unwrap()
            .is_empty());
        fs::write(tmp.path().join("config.toml"), "model = \"x\"\n").unwrap();
        assert!(codex(tmp.path())
            .mcp_entries(&Workdir::plain("/tmp"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_codex_config_is_an_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "[mcp_servers").unwrap();
        let err = codex(tmp.path())
            .mcp_entries(&Workdir::plain("/tmp"))
            .unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");
    }
}
