//! The AI Gateway, as a developer's agent uses it: the console issues the
//! developer a short-lived personal key for a project, and the agent calls the
//! LLM gateway with it.
//!
//! A new personal key revokes the developer's previous one for that project,
//! so a key is kept (in visdom's own config directory, readable by the user
//! only) and reused until shortly before it expires: sessions started side by
//! side share it instead of cutting each other off. A kept key belongs to the
//! account it was issued for and is not handed to another.
//!
//! A subscription key (`billing = subscription`) is the other kind: it only
//! works with the developer's own Claude subscription token, lives about 30
//! days, and a new one revokes nothing. Claude Code gets it once, in a header
//! fixed when it starts, so it is renewed a week before it expires, and the
//! older ones are kept until they expire for `visdom logout` to revoke.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracevault_cli::agent::Agent;

use crate::billing::Billing;

/// A key with less than this left is replaced rather than handed out.
pub const MIN_KEY_SECONDS_LEFT: i64 = 15 * 60;

/// A subscription key with less than this left is replaced rather than
/// handed out: a session started with it keeps it to its end. (Of a key that
/// lives shorter than twice this, half its life.)
pub const MIN_SUBSCRIPTION_KEY_SECONDS_LEFT: i64 = 7 * 24 * 60 * 60;

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
    /// The console's id of the key, to revoke it by.
    pub id: Option<String>,
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

/// Issue the developer a personal key of the kind `billing` for
/// `project_id`. An ordinary key revokes their previous ordinary one for that
/// project; a subscription key revokes nothing.
///
/// A subscription key that comes back as anything else (a console from before
/// subscription keys ignores the request's `billing`) is revoked again and is
/// an error: such a key would let the project pay.
pub async fn issue_key(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
    project_id: &str,
    billing: Billing,
    now: i64,
) -> Result<IssuedKey, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Issued {
        secret: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        expires_at: Option<String>,
        #[serde(default)]
        billing: Option<String>,
    }
    let what = match billing {
        Billing::Project => "cannot get a personal AI Gateway key",
        Billing::Subscription => "cannot get a subscription key from the AI Gateway",
    };
    // An ordinary key is asked for as before subscription keys existed.
    let body = match billing {
        Billing::Project => serde_json::json!({ "projectId": project_id }),
        Billing::Subscription => {
            serde_json::json!({ "projectId": project_id, "billing": billing.name() })
        }
    };
    let request = http
        .post(format!("{}/api/me/keys", base(console_url)))
        .bearer_auth(token)
        .json(&body);
    let issued: Issued = parse(what, &read(what, console_url, request).await?)?;
    if issued.secret.trim().is_empty() {
        return Err(format!("{what}: the answer carried no key"));
    }
    let id = issued.id.filter(|id| !id.trim().is_empty());
    if billing == Billing::Subscription && issued.billing.as_deref() != Some(billing.name()) {
        if let Some(id) = &id {
            // Best effort: the key is not used either way.
            let _ = revoke_key(http, console_url, token, id).await;
        }
        return Err(format!(
            "{what}: this AI Gateway issued an ordinary personal key instead, so it does not \
             support billing = subscription yet (its console needs a newer version). Use \
             --billing project, or set billing to project"
        ));
    }
    Ok(IssuedKey {
        expires_at: expiry_seconds(issued.expires_at.as_deref(), now),
        secret: issued.secret,
        id,
    })
}

/// Revoke the developer's key `id` at the console. A key the console no
/// longer has (expired, or revoked already) is gone, which is what was asked.
pub async fn revoke_key(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
    id: &str,
) -> Result<(), String> {
    let what = "cannot revoke an AI Gateway key";
    let mut url = url::Url::parse(&format!("{}/api/me/keys", base(console_url)))
        .map_err(|e| format!("{what}: `{console_url}` is not a URL ({e})"))?;
    url.path_segments_mut()
        .map_err(|()| format!("{what}: `{console_url}` is not an http(s) URL"))?
        .push(id);
    let response = http
        .delete(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("{what}: the AI Gateway ({console_url}) did not answer: {e}"))?;
    let status = response.status();
    // The console answers 400 for a key of yours it does not have.
    if status.is_success() || matches!(status.as_u16(), 400 | 404) {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(refusal(what, status, &body))
}

/// Why the LLM gateway's models could not be listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsError {
    pub why: String,
    /// The gateway answered that it does not take this key (HTTP 401 or 403):
    /// another key may work where this one does not.
    pub rejected: bool,
}

/// The model aliases `key` may call. This is also the proof that the LLM
/// gateway answers and takes the key, before an agent is pointed at it.
pub async fn models(
    http: &reqwest::Client,
    llm_url: &str,
    key: &str,
) -> Result<Vec<String>, ModelsError> {
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }
    #[derive(Deserialize)]
    struct List {
        data: Vec<Model>,
    }
    let what = "cannot list the AI Gateway's models";
    let failed = |why: String, rejected: bool| ModelsError { why, rejected };
    let response = http
        .get(format!("{}/v1/models", base(llm_url)))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| {
            failed(
                format!("{what}: the AI Gateway ({llm_url}) did not answer: {e}"),
                false,
            )
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let rejected = matches!(status.as_u16(), 401 | 403);
        let why = if rejected {
            format!(
                "{what}: the AI Gateway did not accept the personal key (HTTP {})",
                status.as_u16()
            )
        } else {
            format!("{what}: HTTP {}", status.as_u16())
        };
        return Err(failed(why, rejected));
    }
    let list: List = parse(what, &body).map_err(|why| failed(why, false))?;
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
            "several AI Gateway projects are open to you ({}): set `project` under \
             `[gateway]` in .visdom/config.toml or in your visdom config, or \
             VISDOM_GATEWAY_PROJECT (`visdom init` asks and saves it)",
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

/// Whose key it is: the issuer and subject of the developer's access token.
/// A key is the person's, so one kept for somebody else who logged in on this
/// machine before must not be handed out. Empty for a token that is not a
/// readable JWT, which then cannot be told apart from another such token.
pub fn account_of(token: &str) -> String {
    use base64::Engine;
    #[derive(Deserialize)]
    struct Claims {
        #[serde(default)]
        iss: String,
        sub: String,
    }
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return String::new();
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Claims>(&bytes).ok())
        .filter(|claims| !claims.sub.is_empty())
        .map(|claims| format!("{}|{}", claims.iss, claims.sub))
        .unwrap_or_default()
}

/// One stored personal key.
#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    console_url: String,
    project_id: String,
    /// [`account_of`] the token it was issued for.
    #[serde(default)]
    account: String,
    secret: String,
    expires_at: i64,
    /// The key's kind; a key kept before there were kinds is ordinary.
    #[serde(default)]
    billing: Billing,
    /// The console's id of the key, to revoke it by on logout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Unix seconds when it was kept, which is when it was issued.
    #[serde(default)]
    issued_at: i64,
}

impl StoredKey {
    fn is_for(&self, console_url: &str, project_id: &str, account: &str, billing: Billing) -> bool {
        self.console_url == base(console_url)
            && self.project_id == project_id
            && self.account == account
            && self.billing == billing
    }

    /// Whether it is still handed out at `now`, rather than replaced.
    fn lasts(&self, now: i64) -> bool {
        self.expires_at - now >= renew_before(self.billing, self.issued_at, self.expires_at)
    }
}

/// How long before it expires a key of the kind `billing` is replaced. A
/// subscription key the console gives a short life (shorter than twice
/// [`MIN_SUBSCRIPTION_KEY_SECONDS_LEFT`]) is replaced at half its life, not
/// at once.
fn renew_before(billing: Billing, issued_at: i64, expires_at: i64) -> i64 {
    match billing {
        Billing::Project => MIN_KEY_SECONDS_LEFT,
        Billing::Subscription => {
            let life = expires_at - issued_at;
            if issued_at > 0 && life > 0 {
                MIN_SUBSCRIPTION_KEY_SECONDS_LEFT.min(life / 2)
            } else {
                MIN_SUBSCRIPTION_KEY_SECONDS_LEFT
            }
        }
    }
}

/// A kept key, as `visdom logout` revokes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptKey {
    pub console_url: String,
    pub account: String,
    pub id: Option<String>,
    pub billing: Billing,
    pub expires_at: i64,
}

/// The personal keys `visdom` holds, one per console, project and account.
#[derive(Clone)]
pub struct KeyStore {
    path: PathBuf,
}

pub fn key_store_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("gateway-keys.json")
}

/// Held while a key is looked up, issued and stored; released when dropped.
pub struct KeyStoreLock(#[allow(dead_code)] std::fs::File);

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

    /// Wait for this store to be free of every other `visdom` process, and
    /// take it. Issuing a key revokes the previous one, so two sessions
    /// starting together must not both issue: the second waits here and then
    /// finds the first one's key.
    pub fn lock(&self) -> std::io::Result<KeyStoreLock> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        file.lock()?;
        Ok(KeyStoreLock(file))
    }

    /// The stored key of the kind `billing` of `account` for this console
    /// and project, when it has enough left: [`MIN_KEY_SECONDS_LEFT`], or for
    /// a subscription key [`MIN_SUBSCRIPTION_KEY_SECONDS_LEFT`]. Of several,
    /// the one that lasts longest.
    pub fn usable(
        &self,
        console_url: &str,
        project_id: &str,
        account: &str,
        billing: Billing,
        now: i64,
    ) -> Option<IssuedKey> {
        self.load()
            .into_iter()
            .filter(|k| k.is_for(console_url, project_id, account, billing))
            .filter(|k| k.lasts(now))
            .max_by_key(|k| k.expires_at)
            .map(|k| IssuedKey {
                secret: k.secret,
                expires_at: k.expires_at,
                id: k.id,
            })
    }

    /// Keep `key` of the kind `billing`, dropping keys that have expired. An
    /// ordinary key replaces the one for the same console and project
    /// (whoever's it was: this machine's user is now `account`), as issuing
    /// it revoked that one. A subscription key revoked nothing: `account`'s
    /// earlier ones stay until they expire, for logout to revoke; another
    /// account's go. The file is the user's alone.
    pub fn keep(
        &self,
        console_url: &str,
        project_id: &str,
        account: &str,
        billing: Billing,
        key: &IssuedKey,
        now: i64,
    ) -> Result<(), String> {
        let replaced = |k: &StoredKey| {
            k.console_url == base(console_url)
                && k.project_id == project_id
                && k.billing == billing
                && (billing == Billing::Project || k.account != account)
        };
        let mut keys: Vec<StoredKey> = self
            .load()
            .into_iter()
            .filter(|k| k.expires_at > now)
            .filter(|k| !replaced(k))
            .collect();
        keys.push(StoredKey {
            console_url: base(console_url).to_string(),
            project_id: project_id.to_string(),
            account: account.to_string(),
            secret: key.secret.clone(),
            expires_at: key.expires_at,
            billing,
            id: key.id.clone(),
            issued_at: now,
        });
        let text = serde_json::to_string(&keys).map_err(|e| e.to_string())?;
        write_private(&self.path, &text)
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    /// Every kept key that has not expired at `now`.
    pub fn kept(&self, now: i64) -> Vec<KeptKey> {
        self.load()
            .into_iter()
            .filter(|k| k.expires_at > now)
            .map(|k| KeptKey {
                console_url: k.console_url,
                account: k.account,
                id: k.id,
                billing: k.billing,
                expires_at: k.expires_at,
            })
            .collect()
    }

    /// Forget every kept key (on logout, after [`revoke_kept`]).
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
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

/// A key from [`ensure_key`].
pub struct EnsuredKey {
    pub key: IssuedKey,
    /// Issued just now, rather than found in the store.
    pub issued: bool,
}

/// The developer's key for `project_id`: the stored one while it lasts, else a
/// new one, which is then stored (a key that cannot be stored is an error). `rejected` is a key the gateway no longer
/// takes (revoked by a newer one issued elsewhere): the stored key is used
/// only if it is another one.
///
/// Looking, issuing and storing happen under the store's lock, so sessions
/// starting side by side end up sharing one key instead of revoking each
/// other's.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_key(
    http: &reqwest::Client,
    console_url: &str,
    token: &str,
    project_id: &str,
    billing: Billing,
    store: &KeyStore,
    now: i64,
    rejected: Option<&str>,
) -> Result<EnsuredKey, String> {
    let account = account_of(token);
    // Waiting for another process must not hold up this runtime's thread.
    let locking = store.clone();
    let lock = tokio::task::spawn_blocking(move || locking.lock())
        .await
        .map_err(|e| e.to_string())
        .and_then(|locked| locked.map_err(|e| e.to_string()));
    let stored = store
        .usable(console_url, project_id, &account, billing, now)
        .filter(|key| Some(key.secret.as_str()) != rejected);
    if let Some(key) = stored {
        return Ok(EnsuredKey { key, issued: false });
    }
    // A kept key is read without the lock just as well. A new one is not
    // issued without it: issuing revokes the key another session, issuing at
    // the same moment, is about to use.
    let _lock = lock.map_err(|why| {
        format!(
            "the personal AI Gateway key store {} could not be locked: {why}",
            store.path.display()
        )
    })?;
    let key = issue_key(http, console_url, token, project_id, billing, now).await?;
    // A key that is not kept is issued again by the next session, and by the
    // agent's own key helper, each time revoking the one in use: that is no
    // working gateway session.
    store
        .keep(console_url, project_id, &account, billing, &key, now)
        .map_err(|why| format!("the personal AI Gateway key could not be kept: {why}"))?;
    Ok(EnsuredKey { key, issued: true })
}

/// Revoke at their consoles the kept keys of the person whose access token
/// is `token` (on `visdom logout`, before the store is cleared). What could
/// not be revoked is returned as warnings; such a key stays valid at the
/// gateway until it expires. Keys of another account are not this token's
/// to revoke.
pub async fn revoke_kept(
    http: &reqwest::Client,
    store: &KeyStore,
    token: &str,
    now: i64,
) -> Vec<String> {
    let account = account_of(token);
    let mut warnings = Vec::new();
    for key in store.kept(now).into_iter().filter(|k| k.account == account) {
        let Some(id) = &key.id else { continue };
        if let Err(why) = revoke_key(http, &key.console_url, token, id).await {
            warnings.push(format!(
                "{why}; the {} key stays valid until it expires (revoke it in the AI Gateway \
                 console under My API key)",
                match key.billing {
                    Billing::Project => "personal",
                    Billing::Subscription => "subscription",
                }
            ));
        }
    }
    warnings
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
        let key = issue_key(&http, &server.url, "tok", "p-1", Billing::Project, 0)
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
        let err = issue_key(&http, &server.url, "tok", "p-1", Billing::Project, 0)
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
            err.contains("alpha, beta") && err.contains("`project` under `[gateway]`"),
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
            id: None,
        };
        assert!(store
            .usable("https://gw.example.com", "p-1", "", Billing::Project, 0)
            .is_none());
        store
            .keep(
                "https://gw.example.com/",
                "p-1",
                "",
                Billing::Project,
                &key,
                0,
            )
            .unwrap();
        let again = store
            .usable("https://gw.example.com", "p-1", "", Billing::Project, 0)
            .unwrap();
        assert_eq!(again.secret, "sk-abc");
        // Another project or console has its own key.
        assert!(store
            .usable("https://gw.example.com", "p-2", "", Billing::Project, 0)
            .is_none());
        assert!(store
            .usable("https://other.example.com", "p-1", "", Billing::Project, 0)
            .is_none());
        // Close to expiry it is no longer handed out.
        let late = 10_000 - MIN_KEY_SECONDS_LEFT + 1;
        assert!(store
            .usable("https://gw.example.com", "p-1", "", Billing::Project, late)
            .is_none());
        assert!(store
            .usable(
                "https://gw.example.com",
                "p-1",
                "",
                Billing::Project,
                late - 1
            )
            .is_some());
    }

    #[test]
    fn keeping_a_key_replaces_the_old_one_and_drops_expired_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let key = |secret: &str, expires_at| IssuedKey {
            secret: secret.to_string(),
            expires_at,
            id: None,
        };
        store
            .keep(
                "https://gw",
                "p-1",
                "",
                Billing::Project,
                &key("old", 5_000),
                0,
            )
            .unwrap();
        store
            .keep(
                "https://gw",
                "p-2",
                "",
                Billing::Project,
                &key("other", 9_000),
                0,
            )
            .unwrap();
        store
            .keep(
                "https://gw",
                "p-1",
                "",
                Billing::Project,
                &key("new", 90_000),
                6_000,
            )
            .unwrap();
        assert_eq!(
            store
                .usable("https://gw", "p-1", "", Billing::Project, 6_000)
                .unwrap()
                .secret,
            "new"
        );
        assert_eq!(
            store
                .usable("https://gw", "p-2", "", Billing::Project, 6_000)
                .unwrap()
                .secret,
            "other"
        );
        store
            .keep(
                "https://gw",
                "p-3",
                "",
                Billing::Project,
                &key("third", 99_000),
                9_500,
            )
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
            id: None,
        };
        store
            .keep("https://gw", "p-1", "", Billing::Project, &key, 0)
            .unwrap();
        store
            .keep("https://gw", "p-2", "", Billing::Project, &key, 0)
            .unwrap();
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
        let first = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            now,
            None,
        )
        .await
        .unwrap();
        assert_eq!((first.key.secret.as_str(), first.issued), ("sk-abc", true));
        let second = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            now + 3600,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (second.key.secret.as_str(), second.issued),
            ("sk-abc", false)
        );
        assert_eq!(server.routes(), ["POST /api/me/keys"], "issued once");
        // A day later it has expired: a new one is issued.
        ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            now + 86_400,
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.routes().len(), 2);
    }

    /// A JWT as Keycloak issues one, unsigned: only its claims are read here.
    fn token_of(iss: &str, sub: &str) -> String {
        use base64::Engine;
        let part = |json: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        format!(
            "{}.{}.signature",
            part(r#"{"alg":"RS256"}"#.to_string()),
            part(format!(r#"{{"iss":"{iss}","sub":"{sub}","exp":1}}"#))
        )
    }

    #[test]
    fn the_account_is_the_tokens_issuer_and_subject() {
        let alice = token_of("https://idp/realms/visdom", "alice-id");
        assert_eq!(account_of(&alice), "https://idp/realms/visdom|alice-id");
        assert_ne!(
            account_of(&alice),
            account_of(&token_of("https://idp/realms/visdom", "bob-id"))
        );
        assert_ne!(
            account_of(&alice),
            account_of(&token_of("https://idp/realms/other", "alice-id"))
        );
        // Not a JWT, or one without a subject: no account.
        for opaque in ["person-access-token", "a.b.c", "a.b", "", "a.b.c.d"] {
            assert_eq!(account_of(opaque), "", "{opaque}");
        }
        assert_eq!(account_of(&token_of("https://idp", "")), "");
    }

    #[tokio::test]
    async fn a_kept_key_is_not_handed_to_another_account() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let server = MockServer::start(&[
            (
                "POST /api/me/keys",
                201,
                r#"{"secret":"sk-alice","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
            (
                "POST /api/me/keys",
                201,
                r#"{"secret":"sk-bob","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
        ]);
        let alice = token_of("https://idp", "alice-id");
        let bob = token_of("https://idp", "bob-id");
        let first = ensure_key(
            &http,
            &server.url,
            &alice,
            "p-1",
            Billing::Project,
            &store,
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.key.secret, "sk-alice");
        // Bob logs in on the same machine: Alice's key is not his.
        let second = ensure_key(
            &http,
            &server.url,
            &bob,
            "p-1",
            Billing::Project,
            &store,
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (second.key.secret.as_str(), second.issued),
            ("sk-bob", true)
        );
        let asked = server.requests();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[1].authorization, Some(format!("Bearer {bob}")));
        // And Alice's is gone from the file, not kept beside his.
        assert!(store
            .usable(&server.url, "p-1", &account_of(&alice), Billing::Project, 0)
            .is_none());
        assert_eq!(store.load().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_key_is_replaced_unless_somebody_already_did() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-new","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
        )]);
        let old = IssuedKey {
            secret: "sk-old".to_string(),
            expires_at: 4_000_000_000,
            id: None,
        };
        store
            .keep(&server.url, "p-1", "", Billing::Project, &old, 0)
            .unwrap();
        // The gateway refused sk-old: the stored copy is not offered again.
        let renewed = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            0,
            Some("sk-old"),
        )
        .await
        .unwrap();
        assert_eq!(
            (renewed.key.secret.as_str(), renewed.issued),
            ("sk-new", true)
        );
        // Another session that also saw sk-old refused finds the new one.
        let other = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            0,
            Some("sk-old"),
        )
        .await
        .unwrap();
        assert_eq!((other.key.secret.as_str(), other.issued), ("sk-new", false));
        assert_eq!(server.routes().len(), 1);
    }

    /// Issuing revokes the previous key, so sessions starting together must
    /// end up with one key: the later ones wait and find the first one's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_starting_together_issue_one_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let server = MockServer::start(&[
            (
                "POST /api/me/keys",
                201,
                r#"{"secret":"sk-first","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
            (
                "POST /api/me/keys",
                201,
                r#"{"secret":"sk-second","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
        ]);
        let starts = (0..6).map(|_| {
            let (store, url) = (store.clone(), server.url.clone());
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                ensure_key(&http, &url, "tok", "p-1", Billing::Project, &store, 0, None)
                    .await
                    .unwrap()
                    .key
                    .secret
            })
        });
        let mut secrets = Vec::new();
        for start in starts.collect::<Vec<_>>() {
            secrets.push(start.await.unwrap());
        }
        assert_eq!(secrets, ["sk-first"; 6]);
        assert_eq!(server.routes(), ["POST /api/me/keys"]);
    }

    #[tokio::test]
    async fn a_gateway_that_refuses_the_key_is_told_from_one_that_is_away() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[
            ("GET /v1/models", 401, r#"{"error":"bad key"}"#),
            ("GET /v1/models", 403, ""),
            ("GET /v1/models", 500, ""),
            ("GET /v1/models", 200, "not json"),
        ]);
        for (rejected, said) in [
            (true, "did not accept the personal key (HTTP 401)"),
            (true, "HTTP 403"),
            (false, "HTTP 500"),
            (false, "unreadable answer"),
        ] {
            let err = models(&http, &server.url, "sk").await.unwrap_err();
            assert_eq!(err.rejected, rejected, "{}", err.why);
            assert!(err.why.contains(said), "{}", err.why);
        }
        let err = models(&http, "http://127.0.0.1:1", "sk").await.unwrap_err();
        assert!(
            !err.rejected && err.why.contains("did not answer"),
            "{}",
            err.why
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_key_that_cannot_be_kept_is_an_error() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        // The store's file cannot be replaced: a directory is in its place.
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        std::fs::create_dir_all(&store.path).unwrap();
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-abc","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
        )]);
        let err = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            0,
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(err.contains("could not be kept"), "{err}");
        assert!(!err.contains("sk-abc"), "{err}");
    }

    /// Issuing revokes the key in use, so it is not done without the lock
    /// that keeps two sessions from doing it at once; a kept key needs none.
    #[tokio::test]
    async fn no_key_is_issued_while_the_store_cannot_be_locked() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        // The lock file cannot be opened: a directory is in its place.
        std::fs::create_dir_all(store.path.with_extension("lock")).unwrap();
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            201,
            r#"{"secret":"sk-abc","id":"h","name":"n","expiresAt":"2099-01-01T00:00:00Z"}"#,
        )]);
        let err = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            0,
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(err.contains("could not be locked"), "{err}");
        assert!(server.routes().is_empty(), "{:?}", server.routes());

        let kept = IssuedKey {
            secret: "sk-kept".to_string(),
            expires_at: 1_000_000,
            id: None,
        };
        let account = account_of("tok");
        store
            .keep(&server.url, "p-1", &account, Billing::Project, &kept, 0)
            .unwrap();
        let ensured = ensure_key(
            &http,
            &server.url,
            "tok",
            "p-1",
            Billing::Project,
            &store,
            0,
            None,
        )
        .await
        .unwrap();
        assert_eq!(ensured.key.secret, "sk-kept");
        assert!(!ensured.issued);
        assert!(server.routes().is_empty(), "{:?}", server.routes());
    }

    #[test]
    fn clearing_the_store_forgets_every_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        assert!(store.clear().is_ok(), "nothing to forget is fine");
        let key = IssuedKey {
            secret: "sk-abc".to_string(),
            expires_at: 10_000,
            id: None,
        };
        store
            .keep("https://gw", "p-1", "", Billing::Project, &key, 0)
            .unwrap();
        store.clear().unwrap();
        assert!(store
            .usable("https://gw", "p-1", "", Billing::Project, 0)
            .is_none());
    }

    const DAY: i64 = 24 * 60 * 60;

    fn subscription_answer(secret: &str, id: &str, expires_at: &str) -> String {
        format!(
            r#"{{"secret":"{secret}","id":"{id}","name":"subscription-dev-1","expiresAt":"{expires_at}","billing":"subscription"}}"#
        )
    }

    #[tokio::test]
    async fn a_subscription_key_is_asked_for_by_its_kind() {
        let http = reqwest::Client::new();
        let answer = subscription_answer("sk-sub", "hash-1", "2026-12-26T10:00:00Z");
        let server = MockServer::start(&[("POST /api/me/keys", 201, &answer)]);
        let key = issue_key(&http, &server.url, "tok", "p-1", Billing::Subscription, 0)
            .await
            .unwrap();
        assert_eq!(key.secret, "sk-sub");
        assert_eq!(key.id.as_deref(), Some("hash-1"));
        let asked = &server.requests()[0];
        assert_eq!(
            asked.body,
            r#"{"projectId":"p-1","billing":"subscription"}"#
        );
        assert_eq!(asked.authorization.as_deref(), Some("Bearer tok"));
    }

    /// A console from before subscription keys ignores `billing` and issues
    /// an ordinary key, which would let the project pay: it is revoked again,
    /// and the answer is an error.
    #[tokio::test]
    async fn an_ordinary_key_given_for_a_subscription_is_given_back() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[
            (
                "POST /api/me/keys",
                201,
                r#"{"secret":"sk-ordinary","id":"hash-2","name":"personal-dev-1","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
            ("DELETE /api/me/keys/hash-2", 204, ""),
        ]);
        let err = issue_key(&http, &server.url, "tok", "p-1", Billing::Subscription, 0)
            .await
            .err()
            .unwrap();
        assert!(
            err.contains("does not support billing = subscription"),
            "{err}"
        );
        assert!(!err.contains("sk-ordinary"), "{err}");
        assert_eq!(
            server.routes(),
            ["POST /api/me/keys", "DELETE /api/me/keys/hash-2"]
        );
    }

    #[tokio::test]
    async fn a_deployment_that_denies_subscription_traffic_is_quoted() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            "POST /api/me/keys",
            403,
            r#"{"error":"This gateway does not take subscription traffic (VISDOM_SUBSCRIPTION_TRAFFIC=deny), so it issues no subscription keys; use an ordinary personal key","code":"subscription_traffic_denied"}"#,
        )]);
        let err = issue_key(&http, &server.url, "tok", "p-1", Billing::Subscription, 0)
            .await
            .err()
            .unwrap();
        assert!(err.contains("does not take subscription traffic"), "{err}");
        assert!(err.contains("HTTP 403"), "{err}");
    }

    #[test]
    fn a_subscription_key_is_renewed_a_week_before_it_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let key = |secret: &str, expires_at| IssuedKey {
            secret: secret.to_string(),
            expires_at,
            id: Some(format!("id-{secret}")),
        };
        let issued = 1_000_000;
        let expires = issued + 30 * DAY;
        let sub = Billing::Subscription;
        store
            .keep(
                "https://gw",
                "p-1",
                "a",
                sub,
                &key("first", expires),
                issued,
            )
            .unwrap();
        // Kinds are kept apart.
        assert!(store
            .usable("https://gw", "p-1", "a", Billing::Project, issued)
            .is_none());
        let week = MIN_SUBSCRIPTION_KEY_SECONDS_LEFT;
        assert_eq!(
            store
                .usable("https://gw", "p-1", "a", sub, expires - week)
                .unwrap()
                .secret,
            "first"
        );
        assert!(store
            .usable("https://gw", "p-1", "a", sub, expires - week + 1)
            .is_none());
        // The next one does not replace it in the store: the first stays
        // until it expires, for logout to revoke, and the new one is used.
        let now = expires - week + 1;
        store
            .keep(
                "https://gw",
                "p-1",
                "a",
                sub,
                &key("second", now + 30 * DAY),
                now,
            )
            .unwrap();
        assert_eq!(
            store
                .usable("https://gw", "p-1", "a", sub, now)
                .unwrap()
                .secret,
            "second"
        );
        let kept = store.kept(now);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.iter().all(|k| k.billing == sub));
        // An ordinary key for the same project does not touch them.
        store
            .keep(
                "https://gw",
                "p-1",
                "a",
                Billing::Project,
                &key("plain", now + DAY),
                now,
            )
            .unwrap();
        assert_eq!(store.kept(now).len(), 3);
        // Once expired, it is dropped with the next write.
        store
            .keep(
                "https://gw",
                "p-2",
                "a",
                sub,
                &key("other", expires + 60 * DAY),
                expires + 1,
            )
            .unwrap();
        let left: Vec<_> = store.kept(expires + 1);
        assert_eq!(left.len(), 2, "{left:?}");
        // Another account's subscription keys go when this one keeps its own.
        store
            .keep(
                "https://gw",
                "p-2",
                "b",
                sub,
                &key("bobs", expires + 60 * DAY),
                expires + 2,
            )
            .unwrap();
        assert!(store
            .usable("https://gw", "p-2", "a", sub, expires + 2)
            .is_none());
    }

    #[test]
    fn a_short_lived_subscription_key_is_renewed_at_half_its_life() {
        assert_eq!(
            renew_before(Billing::Subscription, 1000, 1000 + 30 * DAY),
            MIN_SUBSCRIPTION_KEY_SECONDS_LEFT
        );
        assert_eq!(
            renew_before(Billing::Subscription, 1000, 1000 + 4 * DAY),
            2 * DAY
        );
        assert_eq!(
            renew_before(Billing::Subscription, 0, 30 * DAY),
            MIN_SUBSCRIPTION_KEY_SECONDS_LEFT,
            "issue time unknown"
        );
        assert_eq!(renew_before(Billing::Project, 0, DAY), MIN_KEY_SECONDS_LEFT);
    }

    #[tokio::test]
    async fn a_subscription_key_is_issued_once_and_shared() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let answer = subscription_answer("sk-sub", "h", "2099-01-01T00:00:00Z");
        let server = MockServer::start(&[("POST /api/me/keys", 201, &answer)]);
        let sub = Billing::Subscription;
        for issued in [true, false] {
            let ensured = ensure_key(&http, &server.url, "tok", "p-1", sub, &store, 0, None)
                .await
                .unwrap();
            assert_eq!(
                (ensured.key.secret.as_str(), ensured.issued),
                ("sk-sub", issued)
            );
        }
        assert_eq!(server.routes(), ["POST /api/me/keys"]);
        // An ordinary key for the same project is its own.
        assert!(store
            .usable(&server.url, "p-1", "", Billing::Project, 0)
            .is_none());
    }

    #[tokio::test]
    async fn logout_revokes_the_kept_keys_of_the_person_at_their_console() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let store = KeyStore::at(key_store_path_in(tmp.path()));
        let server = MockServer::start(&[
            ("DELETE /api/me/keys/sub-1", 204, ""),
            (
                "DELETE /api/me/keys/sub%2F2",
                400,
                r#"{"error":"No such personal key of yours"}"#,
            ),
            ("DELETE /api/me/keys/plain-1", 500, ""),
        ]);
        let alice = token_of("https://idp", "alice-id");
        let account = account_of(&alice);
        let key = |id: Option<&str>, expires_at| IssuedKey {
            secret: "sk".to_string(),
            expires_at,
            id: id.map(String::from),
        };
        let sub = Billing::Subscription;
        store
            .keep(
                &server.url,
                "p-1",
                &account,
                sub,
                &key(Some("sub-1"), 100),
                0,
            )
            .unwrap();
        store
            .keep(
                &server.url,
                "p-1",
                &account,
                sub,
                &key(Some("sub/2"), 200),
                0,
            )
            .unwrap();
        store
            .keep(
                &server.url,
                "p-1",
                &account,
                Billing::Project,
                &key(Some("plain-1"), 100),
                0,
            )
            .unwrap();
        // Kept before keys had ids: nothing to revoke it by.
        store
            .keep(
                &server.url,
                "p-2",
                &account,
                Billing::Project,
                &key(None, 100),
                0,
            )
            .unwrap();
        // Somebody else's, and an expired one: not asked about.
        store
            .keep(&server.url, "p-3", "other", sub, &key(Some("bobs"), 100), 0)
            .unwrap();
        let warnings = revoke_kept(&http, &store, &alice, 50).await;
        let mut asked = server.routes();
        asked.sort();
        assert_eq!(
            asked,
            [
                "DELETE /api/me/keys/plain-1",
                "DELETE /api/me/keys/sub%2F2",
                "DELETE /api/me/keys/sub-1"
            ]
        );
        for request in server.requests() {
            assert_eq!(request.authorization, Some(format!("Bearer {alice}")));
        }
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("HTTP 500"), "{warnings:?}");
        assert!(
            warnings[0].contains("personal key stays valid"),
            "{warnings:?}"
        );
    }
}
