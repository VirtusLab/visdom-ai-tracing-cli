//! Is the bazaar (context) MCP server registered for the agent, with the
//! OAuth client it needs?
//!
//! The entry is found by its URL, not its name: bazaar's own guide registers
//! it as `visdom`, other notes as `bazaar`. Whether the user is signed in is
//! not checked — the tokens live in the agent's own store.

use std::path::Path;

use tracevault_cli::oidc::fetch_public_config;

use crate::agents::{AgentAdapter, McpEntry, McpScope, Workdir};
use crate::config::UserConfig;
use crate::finding::{command_line, Finding, Fix, FixAction, Severity};

const COMPONENT: &str = "bazaar";

/// The pre-registered OAuth client of the bazaar MCP endpoint. Without it the
/// agent tries Dynamic Client Registration, which the realm refuses.
pub const BAZAAR_CLIENT_ID: &str = "visdom-context-mcp";

/// The name a new registration gets.
pub const BAZAAR_SERVER_NAME: &str = "bazaar";

/// The bazaar MCP endpoint: `VISDOM_BAZAAR_URL`, then the user config, then
/// what the tracing server publishes. `None` when none of them knows it (a
/// server that cannot be reached counts as not knowing).
pub async fn resolve_bazaar_url(
    env: Option<String>,
    user: &UserConfig,
    server_url: Option<&str>,
    http: &reqwest::Client,
) -> Option<String> {
    let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    if let Some(url) = set(env).or_else(|| set(user.bazaar_url.clone())) {
        return Some(url);
    }
    let config = fetch_public_config(http, server_url?).await.ok()?;
    config.bazaar_mcp_url
}

/// Whether two URLs name the same bazaar developer endpoint: same scheme, host
/// and port, and each path one of `/mcp/remote` and its alias `/mcp`.
pub fn same_bazaar(a: &str, b: &str) -> bool {
    fn parts(raw: &str) -> Option<(String, String, Option<u16>)> {
        let url = url::Url::parse(raw.trim()).ok()?;
        let path = url.path().trim_end_matches('/');
        if path != "/mcp" && path != "/mcp/remote" {
            return None;
        }
        Some((
            url.scheme().to_string(),
            url.host_str()?.to_ascii_lowercase(),
            url.port_or_known_default(),
        ))
    }
    match (parts(a), parts(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn describe(action: &FixAction) -> String {
    match action {
        FixAction::RunCommand { program, args, cwd } => {
            let line = format!("`{}`", command_line(program, args));
            match cwd {
                Some(dir) => format!("{line} in {}", dir.display()),
                None => line,
            }
        }
        FixAction::Sequence { steps } => steps
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", then "),
        other => format!("{other:?}"),
    }
}

fn fix(action: FixAction) -> Fix {
    Fix {
        description: format!("run {}", describe(&action)),
        action,
    }
}

pub fn check(
    bazaar_url: Option<&str>,
    adapter: &dyn AgentAdapter,
    workdir: &Workdir,
    user_config_path: &Path,
) -> Vec<Finding> {
    let label = adapter.agent().label();
    let Some(url) = bazaar_url else {
        return vec![Finding::new(
            COMPONENT,
            "bazaar.url",
            Severity::Warn,
            format!(
                "the bazaar URL is unknown, so its MCP registration was not checked — set \
                 `bazaar_url` in {} or VISDOM_BAZAAR_URL",
                user_config_path.display()
            ),
        )];
    };
    const ID: &str = "bazaar.mcp";
    let entries = match adapter.mcp_entries(workdir) {
        Ok(entries) => entries,
        Err(e) => {
            return vec![Finding::new(
                COMPONENT,
                ID,
                Severity::Unknown,
                format!("could not read the {label} MCP configuration — {e}"),
            )]
        }
    };
    let matching: Vec<&McpEntry> = entries
        .iter()
        .filter(|e| e.url.as_deref().is_some_and(|u| same_bazaar(u, url)))
        .collect();

    if let Some(entry) = matching
        .iter()
        .find(|e| e.oauth_client_id.as_deref() == Some(BAZAAR_CLIENT_ID))
    {
        return vec![Finding::new(
            COMPONENT,
            ID,
            Severity::Ok,
            format!(
                "bazaar MCP registered for {label} as `{}` (sign in with /mcp if the agent asks)",
                entry.name
            ),
        )];
    }

    let Some(entry) = matching.first() else {
        return vec![Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "bazaar MCP is not registered for {label}: the agent works without bazaar context"
            ),
        )
        .with_fix(fix(adapter.register_mcp(
            BAZAAR_SERVER_NAME,
            url,
            BAZAAR_CLIENT_ID,
            workdir,
        )))];
    };
    let found = match &entry.oauth_client_id {
        Some(id) => format!("with OAuth client `{id}`"),
        None => "without an OAuth client".to_string(),
    };
    let message = format!(
        "bazaar MCP is registered for {label} as `{}` {found}; it needs `{BAZAAR_CLIENT_ID}`, \
         otherwise sign-in fails with a 403 on client registration",
        entry.name
    );
    vec![if entry.scope == McpScope::Project {
        // A committed file: the team's, not visdom's to rewrite.
        Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "{message} — it comes from the committed .mcp.json; set `oauth.clientId` to \
                 `{BAZAAR_CLIENT_ID}` there"
            ),
        )
    } else {
        Finding::new(COMPONENT, ID, Severity::Error, message).with_fix(fix(FixAction::Sequence {
            steps: vec![
                adapter.remove_mcp(entry, workdir),
                adapter.register_mcp(&entry.name, url, BAZAAR_CLIENT_ID, workdir),
            ],
        }))
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tracevault_cli::agent::Agent;

    const URL: &str = "https://ctx.example.com/mcp/remote";

    struct Fake {
        entries: Result<Vec<McpEntry>, String>,
    }

    impl AgentAdapter for Fake {
        fn agent(&self) -> Agent {
            Agent::ClaudeCode
        }
        fn program(&self) -> &str {
            "agent"
        }
        fn mcp_entries(&self, _: &Workdir) -> Result<Vec<McpEntry>, String> {
            self.entries.clone()
        }
        fn register_mcp(&self, name: &str, url: &str, client_id: &str, w: &Workdir) -> FixAction {
            FixAction::RunCommand {
                program: "agent".to_string(),
                args: ["add", name, url, client_id].map(String::from).to_vec(),
                cwd: Some(w.start_dir.clone()),
            }
        }
        fn remove_mcp(&self, entry: &McpEntry, w: &Workdir) -> FixAction {
            FixAction::RunCommand {
                program: "agent".to_string(),
                args: ["remove", &entry.name].map(String::from).to_vec(),
                cwd: Some(w.start_dir.clone()),
            }
        }
    }

    fn fake(entries: Vec<McpEntry>) -> Fake {
        Fake {
            entries: Ok(entries),
        }
    }

    fn entry(name: &str, url: &str, client: Option<&str>, scope: McpScope) -> McpEntry {
        McpEntry {
            name: name.to_string(),
            url: Some(url.to_string()),
            oauth_client_id: client.map(String::from),
            scope,
        }
    }

    fn run(adapter: &Fake, url: Option<&str>, workdir: &Workdir) -> Finding {
        let mut findings = check(url, adapter, workdir, Path::new("/cfg/visdom/config.toml"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        findings.remove(0)
    }

    fn repo() -> Workdir {
        Workdir::checkout("/work/repo")
    }

    #[test]
    fn url_variants_match_bazaar() {
        for other in [
            "https://ctx.example.com/mcp/remote/",
            "https://ctx.example.com/mcp",
            "https://ctx.example.com/mcp/",
            "https://CTX.example.com/mcp/remote",
            "https://ctx.example.com:443/mcp/remote",
            " https://ctx.example.com/mcp/remote ",
        ] {
            assert!(same_bazaar(URL, other), "{other} is the same server");
            assert!(same_bazaar(other, URL), "{other} is the same server");
        }
        for other in [
            "https://ctx.example.com/mcp/workspace",
            "https://other.example.com/mcp/remote",
            "http://ctx.example.com/mcp/remote",
            "https://ctx.example.com:8443/mcp/remote",
            "https://ctx.example.com/",
            "not a url",
            "",
        ] {
            assert!(!same_bazaar(URL, other), "{other} is a different server");
        }
        // Two workspace endpoints are not "the same bazaar" either.
        assert!(!same_bazaar(
            "https://ctx.example.com/mcp/workspace",
            "https://ctx.example.com/mcp/workspace"
        ));
    }

    #[test]
    fn unknown_url_is_warn_only() {
        let finding = run(&fake(vec![]), None, &repo());
        assert_eq!(finding.id, "bazaar.url");
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.fix.is_none());
        assert!(finding.message.contains("/cfg/visdom/config.toml"));
        assert!(finding.message.contains("VISDOM_BAZAAR_URL"));
    }

    #[test]
    fn missing_entry_offers_register_with_client_id() {
        let other = entry("sentry", "https://mcp.sentry.dev/mcp", None, McpScope::User);
        let finding = run(&fake(vec![other]), Some(URL), &repo());
        assert_eq!(finding.id, "bazaar.mcp");
        assert_eq!(finding.severity, Severity::Error);
        let fix = finding.fix.expect("a fix");
        assert_eq!(
            fix.action,
            FixAction::RunCommand {
                program: "agent".to_string(),
                args: ["add", "bazaar", URL, BAZAAR_CLIENT_ID]
                    .map(String::from)
                    .to_vec(),
                cwd: Some(PathBuf::from("/work/repo")),
            }
        );
        assert_eq!(
            fix.description,
            format!("run `agent add bazaar {URL} {BAZAAR_CLIENT_ID}` in /work/repo")
        );
    }

    #[test]
    fn entry_under_other_name_is_found() {
        let adapter = fake(vec![entry(
            "visdom",
            "https://ctx.example.com/mcp",
            Some(BAZAAR_CLIENT_ID),
            McpScope::User,
        )]);
        let finding = run(&adapter, Some(URL), &repo());
        assert_eq!(finding.severity, Severity::Ok);
        assert!(finding.message.contains("`visdom`"), "{finding:?}");
        assert!(finding.fix.is_none());
    }

    #[test]
    fn wrong_client_id_offers_remove_then_add() {
        for client in [None, Some("something-else")] {
            let adapter = fake(vec![entry("visdom", URL, client, McpScope::Local)]);
            let finding = run(&adapter, Some(URL), &repo());
            assert_eq!(finding.severity, Severity::Error);
            assert!(finding.message.contains("403"), "{finding:?}");
            let steps = match finding.fix.expect("a fix").action {
                FixAction::Sequence { steps } => steps,
                other => panic!("expected a sequence, got {other:?}"),
            };
            let args: Vec<Vec<String>> = steps
                .iter()
                .map(|s| match s {
                    FixAction::RunCommand { args, .. } => args.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            // Removed, then added back under the name it had.
            assert_eq!(args[0], ["remove", "visdom"]);
            assert_eq!(args[1], ["add", "visdom", URL, BAZAAR_CLIENT_ID]);
        }
    }

    #[test]
    fn a_correct_entry_wins_over_a_wrong_one() {
        let adapter = fake(vec![
            entry("old", URL, None, McpScope::User),
            entry("bazaar", URL, Some(BAZAAR_CLIENT_ID), McpScope::Local),
        ]);
        assert_eq!(run(&adapter, Some(URL), &repo()).severity, Severity::Ok);
    }

    #[test]
    fn wrong_client_id_in_committed_file_has_no_automatic_fix() {
        let adapter = fake(vec![entry("bazaar", URL, None, McpScope::Project)]);
        let finding = run(&adapter, Some(URL), &repo());
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.fix.is_none());
        assert!(finding.message.contains(".mcp.json"), "{finding:?}");
        assert!(finding.message.contains("oauth.clientId"), "{finding:?}");
    }

    #[test]
    fn adapter_error_is_unknown() {
        let adapter = Fake {
            entries: Err("cannot parse /home/u/.claude.json: eof".to_string()),
        };
        let finding = run(&adapter, Some(URL), &repo());
        assert_eq!(finding.severity, Severity::Unknown);
        assert!(finding.fix.is_none());
        assert!(finding.message.contains(".claude.json"), "{finding:?}");
    }

    /// Outside a repository the agent still has a project (the directory it
    /// starts in), so the registration is offered there.
    #[test]
    fn outside_a_repository_registration_is_offered_in_the_start_directory() {
        let finding = run(&fake(vec![]), Some(URL), &Workdir::plain("/work/umbrella"));
        assert_eq!(finding.severity, Severity::Error);
        match finding.fix.expect("a fix").action {
            FixAction::RunCommand { cwd, .. } => {
                assert_eq!(cwd, Some(PathBuf::from("/work/umbrella")))
            }
            other => panic!("{other:?}"),
        }
    }

    /// One HTTP response on a local port; returns the base URL.
    fn serve_once(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    #[tokio::test]
    async fn url_precedence_env_user_server() {
        let http = reqwest::Client::new();
        let user = UserConfig {
            bazaar_url: Some("https://user.example.com/mcp".to_string()),
            ..Default::default()
        };
        let none = UserConfig::default();
        // The environment wins; no server is contacted (the URL is unusable).
        assert_eq!(
            resolve_bazaar_url(
                Some("https://env.example.com/mcp".to_string()),
                &user,
                Some("http://127.0.0.1:1"),
                &http
            )
            .await
            .as_deref(),
            Some("https://env.example.com/mcp")
        );
        // An empty variable is unset; the user config is next.
        assert_eq!(
            resolve_bazaar_url(
                Some(" ".to_string()),
                &user,
                Some("http://127.0.0.1:1"),
                &http
            )
            .await
            .as_deref(),
            Some("https://user.example.com/mcp")
        );
        // Last, what the tracing server publishes.
        let base = serve_once(
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"c","bazaar_mcp_url":"https://srv.example.com/mcp/remote"}"#,
        );
        assert_eq!(
            resolve_bazaar_url(None, &none, Some(&base), &http)
                .await
                .as_deref(),
            Some("https://srv.example.com/mcp/remote")
        );
        // A server that does not publish it, an unreachable one, or none at all.
        let base = serve_once(
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"c"}"#,
        );
        assert_eq!(
            resolve_bazaar_url(None, &none, Some(&base), &http).await,
            None
        );
        assert_eq!(
            resolve_bazaar_url(None, &none, Some("http://127.0.0.1:1"), &http).await,
            None
        );
        assert_eq!(resolve_bazaar_url(None, &none, None, &http).await, None);
    }
}
