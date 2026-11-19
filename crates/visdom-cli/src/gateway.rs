//! The AI Gateway, as a developer's agent uses it: the console issues the
//! developer a short-lived personal key for a project, and the agent calls the
//! LLM gateway with it.
//!
//! A new personal key revokes the developer's previous one for that project,
//! so a key is kept (in visdom's own config directory, readable by the user
//! only) and reused until shortly before it expires: sessions started side by
//! side share it instead of cutting each other off.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracevault_cli::agent::Agent;

/// A key with less than this left is replaced rather than handed out.
pub const MIN_KEY_SECONDS_LEFT: i64 = 15 * 60;

/// Assumed lifetime of a key whose expiry the console did not state in a form
/// this version reads. Short, so a wrong guess costs one early renewal.
const UNKNOWN_EXPIRY_SECONDS: i64 = 60 * 60;

/// A project the developer may issue themselves a personal key for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct OpenProject {
    /// The Control Plane project id.
    pub id: String,
    pub slug: String,
    pub name: String,
}

/// A personal key as issued.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedKey {
    pub secret: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// The console's refusal, in its own words when it gave any.
fn refusal(what: &str, status: reqwest::StatusCode, body: &str) -> String {
    #[derive(Deserialize)]
    struct Refusal {
        error: String,
    }
    let said = serde_json::from_str::<Refusal>(body)
        .map(|r| r.error)
        .unwrap_or_default();
    let code = status.as_u16();
    match (said.is_empty(), code) {
        (false, _) => format!("{what}: {said} (HTTP {code})"),
        (true, 401) => format!("{what}: the AI Gateway did not accept this login (HTTP 401)"),
        (true, _) => format!("{what}: HTTP {code}"),
    }
}

async fn read(
    what: &str,
    console_url: &str,
    request: reqwest::RequestBuilder,
) -> Result<String, String> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("{what}: the AI Gateway ({console_url}) did not answer: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(body)
    } else {
        Err(refusal(what, status, &body))
    }
}

fn parse<T: for<'de> Deserialize<'de>>(what: &str, body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| format!("{what}: unreadable answer ({e})"))
}

fn base(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// The LLM gateway's public address, from the console's settings.
pub async fn llm_gateway_url(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Settings {
        llm_gateway_url: String,
    }
    let what = "cannot read the AI Gateway's address";
    let request = http
        .get(format!("{}/api/settings", base(console_url)))
        .bearer_auth(token);
    let settings: Settings = parse(what, &read(what, console_url, request).await?)?;
    let url = base(&settings.llm_gateway_url).to_string();
    if crate::components::bazaar::is_http_url(&url) {
        Ok(url)
    } else {
        Err(format!("{what}: `{url}` is not an http(s) URL"))
    }
}

/// The projects the developer may issue a personal key for.
pub async fn open_projects(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
) -> Result<Vec<OpenProject>, String> {
    #[derive(Deserialize)]
    struct Mine {
        projects: Vec<OpenProject>,
    }
    let what = "cannot list your AI Gateway projects";
    let request = http
        .get(format!("{}/api/me/keys", base(console_url)))
        .bearer_auth(token);
    Ok(parse::<Mine>(what, &read(what, console_url, request).await?)?.projects)
}

/// Unix seconds of an expiry as the console states it (RFC 3339, or the same
/// without an offset, meaning UTC).
fn expiry_seconds(expires_at: Option<&str>, now: i64) -> i64 {
    let parsed = expires_at.and_then(|text| {
        chrono::DateTime::parse_from_rfc3339(text)
            .map(|t| t.timestamp())
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                    .map(|t| t.and_utc().timestamp())
            })
            .ok()
    });
    parsed.unwrap_or(now + UNKNOWN_EXPIRY_SECONDS)
}

/// Issue the developer a personal key for `project_id`. This revokes their
/// previous one for that project.
pub async fn issue_key(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
    project_id: &str,
    now: i64,
) -> Result<IssuedKey, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Issued {
        secret: String,
        #[serde(default)]
        expires_at: Option<String>,
    }
    let what = "cannot get a personal AI Gateway key";
    let request = http
        .post(format!("{}/api/me/keys", base(console_url)))
        .bearer_auth(token)
        .json(&serde_json::json!({ "projectId": project_id }));
    let issued: Issued = parse(what, &read(what, console_url, request).await?)?;
    if issued.secret.trim().is_empty() {
        return Err(format!("{what}: the answer carried no key"));
    }
    Ok(IssuedKey {
        expires_at: expiry_seconds(issued.expires_at.as_deref(), now),
        secret: issued.secret,
    })
}

/// The model aliases `key` may call.
pub async fn models(
    http: &reqwest::Client,
    llm_url: &str,
    key: &str,
) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }
    #[derive(Deserialize)]
    struct List {
        data: Vec<Model>,
    }
    let what = "cannot list the AI Gateway's models";
    let request = http
        .get(format!("{}/v1/models", base(llm_url)))
        .bearer_auth(key);
    let list: List = parse(what, &read(what, llm_url, request).await?)?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

/// The project to use: the configured one (its slug, id or name), else the
/// only one there is. Otherwise the choice is the developer's, and the error
/// names what to choose from.
pub fn pick_project(configured: Option<&str>, open: &[OpenProject]) -> Result<OpenProject, String> {
    let names = || {
        open.iter()
            .map(|p| p.slug.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if open.is_empty() {
        return Err(
            "no AI Gateway project lets you issue a personal key: ask a project operator to \
             switch personal keys on (Budgets) and to grant you access"
                .to_string(),
        );
    }
    match configured.map(str::trim).filter(|c| !c.is_empty()) {
        Some(wanted) => open
            .iter()
            .find(|p| p.slug == wanted || p.id == wanted || p.name == wanted)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "AI Gateway project `{wanted}` does not let you issue a personal key (you \
                     can for: {})",
                    names()
                )
            }),
        None if open.len() == 1 => Ok(open[0].clone()),
        None => Err(format!(
            "several AI Gateway projects are open to you ({}): set gateway_project in \
             .visdom/config.toml or in your visdom config",
            names()
        )),
    }
}

/// The model alias an agent should ask for: the configured one, else the
/// first alias that looks made for that agent, else none (the agent's own
/// default, which the project must then allow).
pub fn pick_model(agent: Agent, configured: Option<&str>, available: &[String]) -> Option<String> {
    if let Some(model) = configured.map(str::trim).filter(|m| !m.is_empty()) {
        return Some(model.to_string());
    }
    let hints: &[&str] = match agent {
        Agent::ClaudeCode => &["claude", "sonnet", "opus"],
        Agent::Codex => &["codex", "gpt"],
        _ => &[],
    };
    hints.iter().find_map(|hint| {
        available
            .iter()
            .find(|alias| alias.to_ascii_lowercase().contains(hint))
            .cloned()
    })
}

/// One stored personal key.
#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    console_url: String,
    project_id: String,
    secret: String,
    expires_at: i64,
}

/// The personal keys `visdom` holds, one per console and project.
pub struct KeyStore {
    path: PathBuf,
}

pub fn key_store_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("gateway-keys.json")
}

impl KeyStore {
    pub fn at(path: PathBuf) -> KeyStore {
        KeyStore { path }
    }

    fn load(&self) -> Vec<StoredKey> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// The stored key for this console and project, when it has at least
    /// [`MIN_KEY_SECONDS_LEFT`] left.
    pub fn usable(&self, console_url: &str, project_id: &str, now: i64) -> Option<IssuedKey> {
        self.load()
            .into_iter()
            .find(|k| k.console_url == base(console_url) && k.project_id == project_id)
            .filter(|k| k.expires_at - now >= MIN_KEY_SECONDS_LEFT)
            .map(|k| IssuedKey {
                secret: k.secret,
                expires_at: k.expires_at,
            })
    }

    /// Keep `key`, replacing the one for the same console and project and
    /// dropping keys that have expired. The file is the user's alone.
    pub fn keep(
        &self,
        console_url: &str,
        project_id: &str,
        key: &IssuedKey,
        now: i64,
    ) -> Result<(), String> {
        let mut keys: Vec<StoredKey> = self
            .load()
            .into_iter()
            .filter(|k| k.expires_at > now)
            .filter(|k| !(k.console_url == base(console_url) && k.project_id == project_id))
            .collect();
        keys.push(StoredKey {
            console_url: base(console_url).to_string(),
            project_id: project_id.to_string(),
            secret: key.secret.clone(),
            expires_at: key.expires_at,
        });
        let text = serde_json::to_string(&keys).map_err(|e| e.to_string())?;
        write_private(&self.path, &text)
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
}

/// Write `text` to `path` so that it is never readable by anyone else: a new
/// file created with the user's permissions only, then moved into place.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(text.as_bytes()));
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The developer's key for `project_id`: the stored one while it lasts, else a
/// new one, which is then stored. A key that cannot be stored is still
/// returned (with the reason), as it works for this session.
pub async fn ensure_key(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
    project_id: &str,
    store: &KeyStore,
    now: i64,
) -> Result<(IssuedKey, Option<String>), String> {
    if let Some(key) = store.usable(console_url, project_id, now) {
        return Ok((key, None));
    }
    let key = issue_key(http, console_url, token, project_id, now).await?;
    let warning = store.keep(console_url, project_id, &key, now).err();
    Ok((key, warning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::MockServer;

    fn project(id: &str, slug: &str) -> OpenProject {
        OpenProject {
            id: id.to_string(),
            slug: slug.to_string(),
            name: slug.to_uppercase(),
        }
    }

    #[tokio::test]
    async fn the_llm_address_comes_from_the_consoles_settings() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            "GET /api/settings",
            200,
            r#"{"llmGatewayUrl":"https://llm.example.com/","mcpGatewayUrl":"https://mcp.example.com","suite":[],"controlPlaneUrl":null}"#,
        )]);
        assert_eq!(
            llm_gateway_url(&http, &format!("{}/", server.url), "tok").await,
            Ok("https://llm.example.com".to_string())
        );
        assert_eq!(
            server.requests()[0].authorization.as_deref(),
            Some("Bearer tok")
        );
    }

    #[tokio::test]
    async fn open_projects_are_listed_with_the_persons_token() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            "GET /api/me/keys",
            200,
            r#"{"hours":24,"projects":[{"id":"p-1","slug":"alpha","name":"Alpha","budget":null}],"keys":[]}"#,
        )]);
        assert_eq!(
            open_projects(&http, &server.url, "tok").await,
            Ok(vec![OpenProject {
                id: "p-1".to_string(),
                slug: "alpha".to_string(),
                name: "Alpha".to_string()
            }])
        );
    }

    #[tokio::test]
    async fn a_key_is_issued_for_the_project() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-abc","id":"hash","name":"personal-x-1","expiresAt":"2026-11-20T10:00:00.250000+00:00"}"#,
        )]);
        let key = issue_key(&http, &server.url, "tok", "p-1", 0)
            .await
            .unwrap();
        assert_eq!(key.secret, "sk-abc");
        assert_eq!(
            key.expires_at,
            chrono::DateTime::parse_from_rfc3339("2026-11-20T10:00:00Z")
                .unwrap()
                .timestamp()
        );
        let asked = &server.requests()[0];
        assert_eq!(asked.authorization.as_deref(), Some("Bearer tok"));
        assert_eq!(asked.body, r#"{"projectId":"p-1"}"#);
    }

    #[tokio::test]
    async fn the_consoles_refusal_is_passed_on_in_its_words() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[
            (
                "POST /api/me/keys",
                400,
                r#"{"error":"Alpha does not allow personal keys; ask an operator to switch them on (Budgets)"}"#,
            ),
            ("GET /api/me/keys", 401, ""),
        ]);
        let err = issue_key(&http, &server.url, "tok", "p-1", 0)
            .await
            .err()
            .unwrap();
        assert!(err.contains("does not allow personal keys"), "{err}");
        assert!(err.contains("HTTP 400"), "{err}");
        let err = open_projects(&http, &server.url, "tok").await.unwrap_err();
        assert!(err.contains("did not accept this login"), "{err}");
        let err = open_projects(&http, "http://127.0.0.1:1", "tok")
            .await
            .unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
    }

    #[test]
    fn expiry_is_read_in_the_forms_the_gateway_uses() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-20T10:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(expiry_seconds(Some("2026-11-20T10:00:00Z"), 0), at);
        assert_eq!(expiry_seconds(Some("2026-11-20T11:00:00+01:00"), 0), at);
        assert_eq!(expiry_seconds(Some("2026-11-20T10:00:00.123456"), 0), at);
        // Not stated, or not understood: a short assumed life.
        assert_eq!(expiry_seconds(None, 1000), 1000 + UNKNOWN_EXPIRY_SECONDS);
        assert_eq!(
            expiry_seconds(Some("tomorrow"), 1000),
            1000 + UNKNOWN_EXPIRY_SECONDS
        );
    }

    #[tokio::test]
    async fn models_are_the_aliases_the_key_may_call() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            "GET /v1/models",
            200,
            r#"{"object":"list","data":[{"id":"gpt-general","object":"model"},{"id":"claude-sonnet","object":"model"}]}"#,
        )]);
        assert_eq!(
            models(&http, &server.url, "sk-abc").await,
            Ok(vec!["gpt-general".to_string(), "claude-sonnet".to_string()])
        );
        assert_eq!(
            server.requests()[0].authorization.as_deref(),
            Some("Bearer sk-abc")
        );
    }

    #[test]
    fn the_project_is_the_configured_one_or_the_only_one() {
        let one = [project("p-1", "alpha")];
        let two = [project("p-1", "alpha"), project("p-2", "beta")];
        assert_eq!(pick_project(None, &one).unwrap().id, "p-1");
        assert_eq!(pick_project(Some(" beta "), &two).unwrap().id, "p-2");
        assert_eq!(pick_project(Some("p-2"), &two).unwrap().slug, "beta");
        assert_eq!(pick_project(Some("BETA"), &two).unwrap().id, "p-2");
        let err = pick_project(None, &two).unwrap_err();
        assert!(
            err.contains("alpha, beta") && err.contains("gateway_project"),
            "{err}"
        );
        let err = pick_project(Some("gamma"), &two).unwrap_err();
        assert!(
            err.contains("`gamma`") && err.contains("alpha, beta"),
            "{err}"
        );
        let err = pick_project(Some("alpha"), &[]).unwrap_err();
        assert!(err.contains("personal keys on"), "{err}");
    }

    #[test]
    fn the_model_is_the_configured_one_or_one_named_for_the_agent() {
        let aliases: Vec<String> = ["demo-assistant", "gpt-general", "claude-sonnet"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            pick_model(Agent::ClaudeCode, None, &aliases).as_deref(),
            Some("claude-sonnet")
        );
        assert_eq!(
            pick_model(Agent::Codex, None, &aliases).as_deref(),
            Some("gpt-general")
        );
        assert_eq!(
            pick_model(Agent::Codex, Some(" local-coder "), &aliases).as_deref(),
            Some("local-coder")
        );
        assert_eq!(
            pick_model(Agent::ClaudeCode, None, &["local-coder".to_string()]),
            None
        );
    }

    #[test]
    fn a_stored_key_is_reused_until_shortly_before_it_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let key = IssuedKey {
            secret: "sk-abc".to_string(),
            expires_at: 10_000,
        };
        assert!(store.usable("https://gw.example.com", "p-1", 0).is_none());
        store
            .keep("https://gw.example.com/", "p-1", &key, 0)
            .unwrap();
        let again = store.usable("https://gw.example.com", "p-1", 0).unwrap();
        assert_eq!(again.secret, "sk-abc");
        // Another project or console has its own key.
        assert!(store.usable("https://gw.example.com", "p-2", 0).is_none());
        assert!(store
            .usable("https://other.example.com", "p-1", 0)
            .is_none());
        // Close to expiry it is no longer handed out.
        let late = 10_000 - MIN_KEY_SECONDS_LEFT + 1;
        assert!(store
            .usable("https://gw.example.com", "p-1", late)
            .is_none());
        assert!(store
            .usable("https://gw.example.com", "p-1", late - 1)
            .is_some());
    }

    #[test]
    fn keeping_a_key_replaces_the_old_one_and_drops_expired_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let key = |secret: &str, expires_at| IssuedKey {
            secret: secret.to_string(),
            expires_at,
        };
        store
            .keep("https://gw", "p-1", &key("old", 5_000), 0)
            .unwrap();
        store
            .keep("https://gw", "p-2", &key("other", 9_000), 0)
            .unwrap();
        store
            .keep("https://gw", "p-1", &key("new", 90_000), 6_000)
            .unwrap();
        assert_eq!(
            store.usable("https://gw", "p-1", 6_000).unwrap().secret,
            "new"
        );
        assert_eq!(
            store.usable("https://gw", "p-2", 6_000).unwrap().secret,
            "other"
        );
        store
            .keep("https://gw", "p-3", &key("third", 99_000), 9_500)
            .unwrap();
        assert_eq!(store.load().len(), 2, "the expired p-2 key is gone");
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = key_store_path_in(tmp.path());
        let store = KeyStore::at(path.clone());
        let key = IssuedKey {
            secret: "sk-abc".to_string(),
            expires_at: 10_000,
        };
        store.keep("https://gw", "p-1", &key, 0).unwrap();
        store.keep("https://gw", "p-2", &key, 0).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_key_is_issued_once_and_then_reused() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-abc","id":"h","name":"n","expiresAt":"2026-11-20T10:00:00Z"}"#,
        )]);
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z")
            .unwrap()
            .timestamp();
        let (first, warning) = ensure_key(&http, &server.url, "tok", "p-1", &store, now)
            .await
            .unwrap();
        assert_eq!((first.secret.as_str(), warning), ("sk-abc", None));
        let (second, _) = ensure_key(&http, &server.url, "tok", "p-1", &store, now + 3600)
            .await
            .unwrap();
        assert_eq!(second.secret, "sk-abc");
        assert_eq!(server.routes(), ["POST /api/me/keys"], "issued once");
        // A day later it has expired: a new one is issued.
        ensure_key(&http, &server.url, "tok", "p-1", &store, now + 86_400)
            .await
            .unwrap();
        assert_eq!(server.routes().len(), 2);
    }
}
