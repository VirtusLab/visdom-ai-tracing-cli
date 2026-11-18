//! Is the bazaar (context) MCP server registered for the agent, with the
//! OAuth client it needs?
//!
//! The entry is found by its URL, not its name: bazaar's own guide registers
//! it as `visdom`, other notes as `bazaar`. Whether the user is signed in is
//! not checked — the tokens live in the agent's own store.

use std::path::Path;

use tracevault_cli::oidc::{fetch_public_config, OidcError};

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
/// what the tracing server publishes. `Ok(None)` when none of them knows it;
/// `Err` with the reason when the server could not be asked.
pub async fn resolve_bazaar_url(
    env: Option<String>,
    user: &UserConfig,
    server_url: Option<&str>,
    http: &reqwest::Client,
) -> Result<Option<String>, String> {
    let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    if let Some(url) = set(env).or_else(|| set(user.bazaar_url.clone())) {
        return Ok(Some(url));
    }
    let Some(server_url) = server_url else {
        return Ok(None);
    };
    match fetch_public_config(http, server_url).await {
        Ok(config) => Ok(config.bazaar_mcp_url),
        // A server without SSO answers properly; it just publishes no suite map.
        Err(OidcError::NoKeycloak) => Ok(None),
        Err(e) => Err(format!("{server_url} could not be asked for it: {e}")),
    }
}

/// Whether `raw` is an absolute http(s) URL — which also means it cannot be
/// mistaken for an option by the command it is handed to.
fn is_http_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
        && !raw.starts_with('-')
        && !raw.contains(char::is_whitespace)
}

/// A name for a new registration that no configured server has: agents
/// replace an entry registered again under its name.
fn unused_name(entries: &[McpEntry]) -> String {
    let taken = |name: &str| entries.iter().any(|e| e.name == name);
    [BAZAAR_SERVER_NAME, "visdom-bazaar"]
        .into_iter()
        .map(String::from)
        .chain((2..).map(|n| format!("visdom-bazaar-{n}")))
        .find(|name| !taken(name))
        .unwrap_or_else(|| BAZAAR_SERVER_NAME.to_string())
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
    bazaar_url: Result<Option<&str>, &str>,
    adapter: &dyn AgentAdapter,
    workdir: &Workdir,
    user_config_path: &Path,
) -> Vec<Finding> {
    let label = adapter.agent().label();
    let bazaar_url = match bazaar_url {
        Ok(url) => url,
        Err(reason) => {
            return vec![Finding::new(
                COMPONENT,
                "bazaar.url",
                Severity::Unknown,
                format!(
                    "could not find out the bazaar URL, so its MCP registration was not checked \
                     — {reason}"
                ),
            )]
        }
    };
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
    // The URL becomes an argument of the agent's CLI, and it may come from a
    // server: only a plain http(s) URL is ever passed on.
    if !is_http_url(url) {
        return vec![Finding::new(
            COMPONENT,
            "bazaar.url",
            Severity::Warn,
            format!("the bazaar URL `{url}` is not an http(s) URL, so nothing was checked"),
        )];
    }
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
    let all_matching: Vec<&McpEntry> = entries
        .iter()
        .filter(|e| e.url.as_deref().is_some_and(|u| same_bazaar(u, url)))
        .collect();
    let matching: Vec<&McpEntry> = all_matching
        .iter()
        .copied()
        .filter(|e| !e.disabled)
        .collect();
    if let (true, Some(off)) = (matching.is_empty(), all_matching.first()) {
        return vec![Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "bazaar MCP is registered for {label} as `{}` but switched off: the agent works \
                 without bazaar context — enable it in the {label} MCP settings",
                off.name
            ),
        )];
    }

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
            &unused_name(&entries),
            url,
            BAZAAR_CLIENT_ID,
            McpScope::Local,
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
                "{message} — it comes from a configuration file in the repository; set its \
                 OAuth client to `{BAZAAR_CLIENT_ID}` there"
            ),
        )
    } else if entry.extra_settings || entry.name.starts_with('-') {
        // Removing and re-adding it would drop its other settings.
        Finding::new(
            COMPONENT,
            ID,
            Severity::Error,
            format!(
                "{message} — the entry has other settings, so it is not replaced automatically; \
                 set its OAuth client to `{BAZAAR_CLIENT_ID}` by hand"
            ),
        )
    } else {
        Finding::new(COMPONENT, ID, Severity::Error, message).with_fix(fix(FixAction::Sequence {
            steps: vec![
                adapter.remove_mcp(entry, workdir),
                // Back where it was: a user-scope entry serves every project.
                adapter.register_mcp(&entry.name, url, BAZAAR_CLIENT_ID, entry.scope, workdir),
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
        fn register_mcp(
            &self,
            name: &str,
            url: &str,
            client_id: &str,
            scope: McpScope,
            w: &Workdir,
        ) -> FixAction {
            let scope = format!("{scope:?}").to_lowercase();
            FixAction::RunCommand {
                program: "agent".to_string(),
                args: ["add", &scope, name, url, client_id]
                    .map(String::from)
                    .to_vec(),
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
            extra_settings: false,
            disabled: false,
        }
    }

    fn run(adapter: &Fake, url: Option<&str>, workdir: &Workdir) -> Finding {
        let mut findings = check(
            Ok(url),
            adapter,
            workdir,
            Path::new("/cfg/visdom/config.toml"),
        );
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
                args: ["add", "local", "bazaar", URL, BAZAAR_CLIENT_ID]
                    .map(String::from)
                    .to_vec(),
                cwd: Some(PathBuf::from("/work/repo")),
            }
        );
        assert_eq!(
            fix.description,
            format!("run `agent add local bazaar {URL} {BAZAAR_CLIENT_ID}` in /work/repo")
        );
    }

    #[test]
    fn unresolvable_url_is_unknown_with_the_reason() {
        let findings = check(
            Err("https://tv.example.com could not be asked for it: timeout"),
            &fake(vec![]),
            &repo(),
            Path::new("/cfg"),
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "bazaar.url");
        assert_eq!(findings[0].severity, Severity::Unknown);
        assert!(findings[0].message.contains("timeout"), "{findings:?}");
        assert!(findings[0].fix.is_none());
    }

    /// Registering under a name another server has would replace that server.
    #[test]
    fn registration_never_takes_a_name_in_use() {
        let name_offered = |taken: &[&str]| {
            let entries = taken
                .iter()
                .map(|n| entry(n, "https://other.example.com/x", None, McpScope::User))
                .collect();
            match run(&fake(entries), Some(URL), &repo()).fix.unwrap().action {
                FixAction::RunCommand { args, .. } => args[2].clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(name_offered(&[]), "bazaar");
        assert_eq!(name_offered(&["bazaar"]), "visdom-bazaar");
        assert_eq!(
            name_offered(&["bazaar", "visdom-bazaar"]),
            "visdom-bazaar-2"
        );
    }

    #[test]
    fn entry_with_other_settings_is_not_replaced_automatically() {
        let mut wrong = entry("bazaar", URL, None, McpScope::User);
        wrong.extra_settings = true;
        let finding = run(&fake(vec![wrong]), Some(URL), &repo());
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.fix.is_none());
        assert!(finding.message.contains("by hand"), "{finding:?}");
    }

    #[test]
    fn a_switched_off_entry_is_not_a_registration() {
        let mut off = entry("bazaar", URL, Some(BAZAAR_CLIENT_ID), McpScope::User);
        off.disabled = true;
        let finding = run(&fake(vec![off.clone()]), Some(URL), &repo());
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("switched off"), "{finding:?}");
        assert!(finding.fix.is_none(), "re-adding would replace the entry");
        // An enabled one beside it is what counts.
        let on = entry("visdom", URL, Some(BAZAAR_CLIENT_ID), McpScope::User);
        assert_eq!(
            run(&fake(vec![off, on]), Some(URL), &repo()).severity,
            Severity::Ok
        );
    }

    /// Nothing that could read as an option reaches the agent's command line.
    #[test]
    fn only_a_plain_http_url_is_passed_to_the_agent_cli() {
        for bad in [
            "--header=X: y",
            "-s",
            "file:///etc/passwd",
            "ctx.example.com/mcp",
            "https://ctx.example.com/mcp --scope user",
        ] {
            let finding = run(&fake(vec![]), Some(bad), &repo());
            assert_eq!(finding.id, "bazaar.url", "{bad}");
            assert_eq!(finding.severity, Severity::Warn, "{bad}");
            assert!(finding.fix.is_none(), "{bad}");
        }
        // An entry whose name would read as an option is not re-added.
        let finding = run(
            &fake(vec![entry("--scope", URL, None, McpScope::User)]),
            Some(URL),
            &repo(),
        );
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.fix.is_none());
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
        for (client, scope, scope_name) in [
            (None, McpScope::Local, "local"),
            // A user-scope entry serves every project: it must not come back
            // as a registration of this project only.
            (Some("something-else"), McpScope::User, "user"),
        ] {
            let adapter = fake(vec![entry("visdom", URL, client, scope)]);
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
            assert_eq!(
                args[1],
                ["add", scope_name, "visdom", URL, BAZAAR_CLIENT_ID]
            );
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
        assert!(
            finding.message.contains("file in the repository"),
            "{finding:?}"
        );
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
            .unwrap()
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
            .unwrap()
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
                .unwrap()
                .as_deref(),
            Some("https://srv.example.com/mcp/remote")
        );
        // A server that does not publish it, an unreachable one, or none at all.
        let base = serve_once(
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"c"}"#,
        );
        assert_eq!(
            resolve_bazaar_url(None, &none, Some(&base), &http).await,
            Ok(None)
        );
        // A server that cannot be reached is an error with the reason…
        let err = resolve_bazaar_url(None, &none, Some("http://127.0.0.1:1"), &http)
            .await
            .unwrap_err();
        assert!(err.contains("http://127.0.0.1:1"), "{err}");
        // …one without SSO simply does not know.
        let base = serve_once(r#"{"oidc_enabled":false}"#);
        assert_eq!(
            resolve_bazaar_url(None, &none, Some(&base), &http).await,
            Ok(None)
        );
        assert_eq!(resolve_bazaar_url(None, &none, None, &http).await, Ok(None));
    }
}
