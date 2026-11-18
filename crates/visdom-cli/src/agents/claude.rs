//! Claude Code.
//!
//! MCP servers come from three places: `~/.claude.json` →
//! `projects[<project>].mcpServers` (local scope) and its top-level
//! `mcpServers` (user scope), and the repository's committed `.mcp.json`
//! (project scope). An HTTP entry is
//! `{"type":"http","url":…,"oauth":{"clientId":…}}`.
//!
//! `<project>` is the repository's primary checkout — also for a session
//! started in a subdirectory or in a linked worktree — and, outside a
//! repository, exactly the directory Claude was started in (observed with
//! Claude Code 2.1.327: `claude mcp add -s local` / `claude mcp get`).

use std::path::Path;

use tracevault_cli::agent::Agent;

use super::{read_optional, run_command, AgentAdapter, AgentPaths, McpEntry, McpScope, Workdir};
use crate::finding::FixAction;

pub struct Claude {
    paths: AgentPaths,
    program: String,
}

impl Claude {
    pub fn new(paths: AgentPaths, program_override: Option<String>) -> Self {
        Claude {
            paths,
            program: program_override.unwrap_or_else(|| "claude".to_string()),
        }
    }
}

fn scope_name(scope: McpScope) -> &'static str {
    match scope {
        McpScope::Local => "local",
        McpScope::Project => "project",
        McpScope::User => "user",
    }
}

fn parse_json(path: &Path) -> Result<Option<serde_json::Value>, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    // Claude creates the file empty before first use.
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

fn entries(servers: Option<&serde_json::Value>, scope: McpScope, out: &mut Vec<McpEntry>) {
    let Some(servers) = servers.and_then(|s| s.as_object()) else {
        return;
    };
    for (name, server) in servers {
        let text = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map(String::from);
        out.push(McpEntry {
            name: name.clone(),
            url: text(server.get("url")),
            oauth_client_id: text(server.get("oauth").and_then(|o| o.get("clientId"))),
            scope,
        });
    }
}

impl AgentAdapter for Claude {
    fn agent(&self) -> Agent {
        Agent::ClaudeCode
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn mcp_entries(&self, workdir: &Workdir) -> Result<Vec<McpEntry>, String> {
        let mut out = Vec::new();
        let state = parse_json(&self.paths.home.join(".claude.json"))?;
        if let Some(state) = &state {
            let key = workdir.primary.as_deref().unwrap_or(&workdir.start_dir);
            let project = state
                .get("projects")
                .and_then(|p| p.get(key.to_string_lossy().as_ref()));
            entries(
                project.and_then(|p| p.get("mcpServers")),
                McpScope::Local,
                &mut out,
            );
        }
        let committed_dir = workdir.checkout.as_deref().unwrap_or(&workdir.start_dir);
        let committed = parse_json(&committed_dir.join(".mcp.json"))?;
        entries(
            committed.as_ref().and_then(|c| c.get("mcpServers")),
            McpScope::Project,
            &mut out,
        );
        entries(
            state.as_ref().and_then(|s| s.get("mcpServers")),
            McpScope::User,
            &mut out,
        );
        Ok(out)
    }

    fn register_mcp(&self, name: &str, url: &str, client_id: &str, workdir: &Workdir) -> FixAction {
        // Options before the positionals: `claude mcp add` treats what follows
        // the URL as arguments of a stdio server.
        run_command(
            &self.program,
            &[
                "mcp",
                "add",
                "--transport",
                "http",
                "--scope",
                "local",
                "--client-id",
                client_id,
                name,
                url,
            ],
            Some(&workdir.start_dir),
        )
    }

    fn remove_mcp(&self, entry: &McpEntry, workdir: &Workdir) -> FixAction {
        run_command(
            &self.program,
            &[
                "mcp",
                "remove",
                "--scope",
                scope_name(entry.scope),
                &entry.name,
            ],
            Some(&workdir.start_dir),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn claude(home: &Path) -> Claude {
        Claude::new(
            AgentPaths {
                home: home.to_path_buf(),
                codex_home: home.join(".codex"),
            },
            None,
        )
    }

    fn state_for(project: &Path) -> String {
        serde_json::json!({
            "numStartups": 3,
            "mcpServers": {
                "global-tool": { "type": "http", "url": "https://global.example.com/mcp" }
            },
            "projects": {
                project.to_string_lossy(): {
                    "mcpServers": {
                        "bazaar": {
                            "type": "http",
                            "url": "https://ctx.example.com/mcp/remote",
                            "oauth": { "clientId": "visdom-context-mcp" }
                        }
                    }
                },
                "/somewhere/else": {
                    "mcpServers": { "other": { "type": "http", "url": "https://other/mcp" } }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn claude_reads_local_user_and_project_entries() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".claude.json"), state_for(repo.path())).unwrap();
        fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers":{"team":{"command":"npx","args":["team-mcp"]}}}"#,
        )
        .unwrap();

        let got = claude(home.path())
            .mcp_entries(&Workdir::checkout(repo.path()))
            .unwrap();
        assert_eq!(
            got,
            [
                McpEntry {
                    name: "bazaar".to_string(),
                    url: Some("https://ctx.example.com/mcp/remote".to_string()),
                    oauth_client_id: Some("visdom-context-mcp".to_string()),
                    scope: McpScope::Local,
                },
                McpEntry {
                    name: "team".to_string(),
                    url: None,
                    oauth_client_id: None,
                    scope: McpScope::Project,
                },
                McpEntry {
                    name: "global-tool".to_string(),
                    url: Some("https://global.example.com/mcp".to_string()),
                    oauth_client_id: None,
                    scope: McpScope::User,
                },
            ]
        );
    }

    /// Claude keys the local entry by the repository's primary checkout, so
    /// it is found from a subdirectory and from a linked worktree, whose own
    /// paths appear nowhere in `~/.claude.json`.
    #[test]
    fn claude_local_entry_found_from_subdirectory() {
        let home = tempfile::tempdir().unwrap();
        let repo = PathBuf::from("/work/repo");
        fs::write(home.path().join(".claude.json"), state_for(&repo)).unwrap();
        let has_bazaar = |workdir: &Workdir| {
            claude(home.path())
                .mcp_entries(workdir)
                .unwrap()
                .iter()
                .any(|e| e.name == "bazaar" && e.scope == McpScope::Local)
        };

        assert!(has_bazaar(&Workdir {
            start_dir: repo.join("src/deep"),
            checkout: Some(repo.clone()),
            primary: Some(repo.clone()),
        }));
        assert!(has_bazaar(&Workdir {
            start_dir: PathBuf::from("/work/repo-wt/src"),
            checkout: Some(PathBuf::from("/work/repo-wt")),
            primary: Some(repo.clone()),
        }));
        // Another repository does not see it.
        assert!(!has_bazaar(&Workdir::checkout("/work/other")));
    }

    /// Outside a repository the key is the start directory itself: an entry
    /// of a parent directory does not apply.
    #[test]
    fn outside_a_repository_the_start_directory_is_the_project() {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join(".claude.json"),
            state_for(Path::new("/work/umbrella")),
        )
        .unwrap();
        let scopes = |dir: &str| -> Vec<McpScope> {
            claude(home.path())
                .mcp_entries(&Workdir::plain(dir))
                .unwrap()
                .iter()
                .map(|e| e.scope)
                .collect()
        };
        assert_eq!(scopes("/work/umbrella"), [McpScope::Local, McpScope::User]);
        assert_eq!(scopes("/work/umbrella/sub"), [McpScope::User]);
    }

    #[test]
    fn malformed_claude_json_is_unknown() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let workdir = Workdir::checkout(repo.path());
        let path = home.path().join(".claude.json");
        fs::write(&path, "{ not json").unwrap();
        let err = claude(home.path()).mcp_entries(&workdir).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");

        // A malformed committed .mcp.json is named the same way.
        fs::write(&path, "{}").unwrap();
        let committed = repo.path().join(".mcp.json");
        fs::write(&committed, "[").unwrap();
        let err = claude(home.path()).mcp_entries(&workdir).unwrap_err();
        assert!(err.contains(&committed.display().to_string()), "{err}");
    }

    #[test]
    fn missing_claude_json_means_no_entries() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let workdir = Workdir::checkout(repo.path());
        let entries = || claude(home.path()).mcp_entries(&workdir).unwrap();
        assert!(entries().is_empty());
        // So does an empty file, and one whose shape is not what we expect.
        fs::write(home.path().join(".claude.json"), "").unwrap();
        assert!(entries().is_empty());
        fs::write(
            home.path().join(".claude.json"),
            r#"{"projects":[],"mcpServers":"x"}"#,
        )
        .unwrap();
        assert!(entries().is_empty());
    }
}
