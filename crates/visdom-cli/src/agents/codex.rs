//! Codex CLI.
//!
//! MCP servers live only in the global `<codex home>/config.toml`:
//! `[mcp_servers.<name>] url = …` with `[mcp_servers.<name>.oauth] client_id`.

use tracevault_cli::agent::Agent;

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

impl AgentAdapter for Codex {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn mcp_entries(&self, _workdir: &Workdir) -> Result<Vec<McpEntry>, String> {
        let path = self.paths.codex_home.join("config.toml");
        let Some(text) = read_optional(&path)? else {
            return Ok(Vec::new());
        };
        let config: toml::Value =
            toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        let Some(servers) = config.get("mcp_servers").and_then(|s| s.as_table()) else {
            return Ok(Vec::new());
        };
        let text = |v: Option<&toml::Value>| v.and_then(|v| v.as_str()).map(String::from);
        Ok(servers
            .iter()
            .map(|(name, server)| McpEntry {
                name: name.clone(),
                url: text(server.get("url")),
                oauth_client_id: text(server.get("oauth").and_then(|o| o.get("client_id"))),
                scope: McpScope::User,
            })
            .collect())
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
                },
                McpEntry {
                    name: "local-tool".to_string(),
                    url: None,
                    oauth_client_id: None,
                    scope: McpScope::User,
                },
            ]
        );
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
