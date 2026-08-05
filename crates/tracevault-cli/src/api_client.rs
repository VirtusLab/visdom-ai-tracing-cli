use crate::credentials::{Credential, Credentials, KeycloakSession};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

pub struct ApiClient {
    base_url: String,
    credential: Option<ClientCredential>,
    client: reqwest::Client,
    /// Injectable "now" (unix seconds). Only the token-refresh window reads
    /// it; tests override it so the boundary is deterministic instead of
    /// depending on wall-clock time.
    now: fn() -> i64,
}

/// A [`Credential`] prepared for use by a client.
///
/// A Keycloak session sits behind an `Arc<Mutex<..>>` so several concurrent
/// in-process requests (the stream hook drains a queue with a shared client)
/// serialise on ONE refresh instead of each racing to mint its own and
/// invalidating the others' refresh token.
enum ClientCredential {
    ApiKey(String),
    Keycloak(Arc<Mutex<KeycloakSession>>),
}

/// Why a request could not be given a bearer token.
///
/// Separate from [`GetMeError`] because it is produced before any request
/// goes out, and separate from a plain string because callers must be able
/// to distinguish "this session is dead, log in again" from "we couldn't
/// reach the IdP right now".
#[derive(Debug)]
pub enum AuthError {
    /// The refresh token was rejected: the user must log in again.
    SessionExpired,
    /// The refresh attempt itself failed (IdP unreachable, 5xx, ...).
    Refresh(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionExpired => write!(
                f,
                "your session has expired — run `tracevault login` to sign in again"
            ),
            Self::Refresh(m) => write!(f, "could not refresh the access token: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Wall-clock unix seconds; the production value of [`ApiClient::now`].
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Serialize)]
pub struct RegisterRepoRequest {
    pub repo_name: String,
    pub github_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRepoResponse {
    pub repo_id: uuid::Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ResolveRemoteResponse {
    pub remote_id: uuid::Uuid,
    #[serde(default)]
    pub name: Option<String>,
    pub normalized_url: String,
    pub clone_status: String,
}

#[derive(Deserialize)]
pub struct RemoteRepoRef {
    pub id: uuid::Uuid,
    // Reserved for display (e.g. a future `repo status`/error message listing
    // a codebase's linked repos by name); not read by any caller yet.
    #[allow(dead_code)]
    pub name: String,
}

// The server's RemoteDetailResponse flattens the remote fields at top level and
// adds a `repos` array; serde ignores any other top-level fields.
#[derive(Deserialize)]
pub struct RemoteDetail {
    #[serde(default)]
    pub name: Option<String>,
    pub normalized_url: String,
    pub clone_status: String,
    pub repos: Vec<RemoteRepoRef>,
}

#[derive(Debug, Serialize)]
pub struct CheckPoliciesRequest {
    pub sessions: Vec<SessionCheckData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionCheckData {
    pub session_id: String,
    pub tool_calls: Option<serde_json::Value>,
    pub files_modified: Option<Vec<String>>,
    pub total_tool_calls: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct CheckPoliciesResponse {
    pub passed: bool,
    pub results: Vec<CheckResultItem>,
    pub blocked: bool,
}

#[derive(Debug, Deserialize)]
pub struct CheckResultItem {
    pub rule_name: String,
    pub result: String,
    pub action: String,
    pub severity: String,
    pub details: String,
}

#[derive(Debug, Deserialize)]
pub struct RepoListItem {
    pub id: uuid::Uuid,
    pub name: String,
    #[serde(default)]
    pub github_url: Option<String>,
    #[serde(default)]
    pub clone_status: Option<String>,
}

/// Response shape for the `policies/agent-instructions` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentInstructionsResponse {
    #[allow(dead_code)]
    pub format: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct MeResponse {
    #[allow(dead_code)]
    pub user_id: uuid::Uuid,
    pub email: String,
    pub name: Option<String>,
    /// The role the server derived for this caller (from the `tracing` /
    /// `tracing-admin` realm roles for a Keycloak bearer). Optional so this
    /// CLI keeps parsing a server that predates the field.
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug)]
pub enum GetMeError {
    /// 401 — token is missing or invalid.
    Unauthorized,
    /// 403 — the token is valid but the account is not authorized (for a
    /// Keycloak bearer: it lacks the `tracing` realm role). Distinct from
    /// `Unauthorized` because the fix is "an admin grants a role", not
    /// "log in again".
    Forbidden(String),
    /// Transport-level failure (DNS, TCP, TLS, timeout).
    Network(String),
    /// HTTP ≥ 400 other than 401/403, or malformed JSON.
    Server(String),
}

impl std::fmt::Display for GetMeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "unauthorized (token invalid or expired)"),
            Self::Forbidden(m) => write!(f, "forbidden (account not authorized): {m}"),
            Self::Network(m) => write!(f, "network error: {m}"),
            Self::Server(m) => write!(f, "server error: {m}"),
        }
    }
}

impl std::error::Error for GetMeError {}

#[derive(Debug, Serialize)]
pub struct CiVerifyRequest {
    pub commits: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CiVerifyResponse {
    pub status: String,
    pub total_commits: usize,
    pub registered_commits: usize,
    pub sealed_commits: usize,
    pub policy_passed_commits: usize,
    pub results: Vec<CommitVerifyResult>,
}

#[derive(Debug, Deserialize)]
pub struct CommitVerifyResult {
    pub commit_sha: String,
    pub status: String,
    pub registered: bool,
    pub sealed: bool,
    pub signature_valid: bool,
    pub chain_valid: bool,
    pub policy_results: Vec<CiPolicyResult>,
}

#[derive(Debug, Deserialize)]
pub struct CiPolicyResult {
    pub rule_name: String,
    pub result: String,
    pub action: String,
    pub severity: String,
    pub details: String,
}

/// One project in `GET /api/v1/projects`. Server sends more
/// fields; only these two are consumed today.
#[derive(Debug, Deserialize)]
pub struct ProjectListItem {
    pub id: uuid::Uuid,
    pub name: String,
}

/// A repo linked to a project (member of `ProjectDetail::repos`).
#[derive(Debug, Deserialize)]
pub struct ProjectRepoRef {
    pub id: uuid::Uuid,
}

/// Full detail for a project. Server sends more fields; only `repos` is
/// consumed today.
#[derive(Debug, Deserialize)]
pub struct ProjectDetail {
    pub repos: Vec<ProjectRepoRef>,
}

/// Outcome of `ApiClient::resolve_project`, distinguishing "no project"
/// (404) from "ambiguous, multiple candidates" (409) — unlike
/// `resolve_remote`, which only distinguishes found/not-found.
#[derive(Debug)]
pub enum ResolveProjectOutcome {
    Resolved(uuid::Uuid),
    None,
    Ambiguous,
}

/// Wire shape of a successful `resolve_project` response.
#[derive(Deserialize)]
struct ResolveProjectResponse {
    project_id: uuid::Uuid,
}

impl ApiClient {
    /// Construct a client authenticated with a raw API key (or nothing).
    ///
    /// Part of the crate's public API and the shape every test constructs, so
    /// it stays exactly as it was. Command code goes through
    /// [`ApiClient::with_credential`] instead, which is why the `tracevault`
    /// BINARY target — where nothing but the lib's tests calls this — would
    /// otherwise report it as dead.
    #[allow(dead_code)]
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self::with_credential(base_url, api_key.map(|k| Credential::ApiKey(k.to_string())))
    }

    /// Construct a client from a resolved [`Credential`].
    ///
    /// Takes an `Option` because every caller resolves credentials that may
    /// be absent (`resolve_credentials` returns an `Option`), and an
    /// unauthenticated client is a legitimate state — several commands
    /// degrade gracefully rather than failing when nothing is configured.
    pub fn with_credential(base_url: &str, credential: Option<Credential>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            credential: credential.map(|c| match c {
                Credential::ApiKey(k) => ClientCredential::ApiKey(k),
                Credential::Keycloak(s) => ClientCredential::Keycloak(Arc::new(Mutex::new(s))),
            }),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            now: now_unix,
        }
    }

    /// Test-only clock override, so the refresh window can be exercised
    /// without waiting for a token to actually age.
    #[cfg(test)]
    fn with_now(mut self, now: fn() -> i64) -> Self {
        self.now = now;
        self
    }

    /// The bearer token to present, refreshing first if needed.
    ///
    /// * `ApiKey` → the key verbatim. Never refreshed, never rotated: this is
    ///   the automation path and it must stay a pure passthrough.
    /// * `Keycloak` → refresh when the access token is inside
    ///   [`crate::credentials::REFRESH_WINDOW_SECS`] of expiry, persist the
    ///   new tokens, and return the fresh one. The session mutex is held
    ///   across the refresh so concurrent callers wait for it instead of
    ///   each starting their own.
    async fn bearer(&self) -> Result<Option<String>, AuthError> {
        match &self.credential {
            None => Ok(None),
            Some(ClientCredential::ApiKey(key)) => Ok(Some(key.clone())),
            Some(ClientCredential::Keycloak(session)) => {
                let mut guard = session.lock().await;
                if guard.needs_refresh((self.now)()) {
                    self.refresh_locked(&mut guard).await?;
                }
                Ok(Some(guard.access_token.clone()))
            }
        }
    }

    /// Refresh the held session in place and persist it. Caller holds the
    /// session lock, which is what makes "one refresh at a time" true.
    ///
    /// # Residual cross-process race, and why it is currently benign
    ///
    /// The on-disk adopt below closes the SEQUENTIAL case (something already
    /// finished refreshing before we started) but only NARROWS the concurrent
    /// one: two hooks spawned at the same instant can both read the same
    /// session before either writes, and the window is this function's
    /// discovery GET plus its token POST. Both would then present the same
    /// refresh token.
    ///
    /// That is harmless ONLY because the realm keeps Keycloak's
    /// `revokeRefreshToken` at its default (off): a re-presented refresh token
    /// stays valid, so the worst outcome is one wasted round trip and a
    /// last-writer-wins access token, both invisible to the user. If that
    /// realm setting is ever enabled, the second presenter gets
    /// `invalid_grant` — surfacing as a spurious "your session has expired" —
    /// and this code then needs a real inter-process file lock around
    /// read-refresh-write, not just the adopt.
    ///
    /// # Both directions are scoped to this client's server
    ///
    /// There is exactly ONE credentials file, but a machine can be pointed at
    /// more than one TraceVault instance. So the adopt only reads, and
    /// `persist_session` only writes, when that file's `server_url` is this
    /// client's `base_url` (see [`crate::credentials::same_server`]).
    /// Unscoped, a long-running `tracevault stream` against instance A plus a
    /// `tracevault login` to instance B in another terminal would make A adopt
    /// B's session and send B's access token to A — disclosing a token to a
    /// host it was never minted for — and A's next refresh would then overwrite
    /// B's credentials, signing the user out of B.
    async fn refresh_locked(&self, session: &mut KeycloakSession) -> Result<(), AuthError> {
        let now = (self.now)();

        // Someone else may have refreshed already: another process (two git
        // hooks firing at once), or another `ApiClient` in this process that
        // was built from the same on-disk credential before either refreshed.
        //
        // A DIFFERENT refresh token on disk is proof that ours has been
        // superseded, so adopt unconditionally — including when the adopted
        // session is itself inside the refresh window, in which case we fall
        // through and refresh with the ADOPTED token. Gating the adopt on the
        // disk session being fresh would leave exactly the case this exists to
        // prevent: refreshing with an in-memory token another client already
        // consumed.
        //
        // ... but only if that file is still this server's (see above).
        if let Some(disk) = Credentials::load()
            .filter(|c| crate::credentials::same_server(&c.server_url, &self.base_url))
            .and_then(|c| c.auth)
        {
            if disk.refresh_token != session.refresh_token {
                let disk_needs_refresh = disk.needs_refresh(now);
                *session = disk;
                if !disk_needs_refresh {
                    return Ok(());
                }
            }
        }

        // Discovery is re-fetched per refresh rather than cached: a refresh
        // happens at most once per access-token lifetime (minutes), and
        // caching it would mean a rotated realm endpoint needs a re-login.
        let discovery = crate::oidc::discover(&self.client, &session.issuer)
            .await
            .map_err(auth_error)?;
        let tokens = crate::oidc::refresh(
            &self.client,
            &discovery,
            &session.client_id,
            &session.refresh_token,
        )
        .await
        .map_err(auth_error)?;
        session.apply(&tokens, now);

        // Best-effort, and scoped to this server (see the doc comment): a token
        // that can't be written to disk still works for THIS process, so
        // failing the request would be worse than a warning. The message
        // deliberately contains no token material.
        if let Err(e) = Credentials::persist_session(&self.base_url, session) {
            eprintln!("Warning: could not save refreshed credentials: {e}");
        }
        Ok(())
    }

    /// Force one refresh after an unexpected 401 and return the new token.
    ///
    /// `Ok(None)` means "not refreshable" (API key or no credential), which
    /// is the signal not to retry. `stale` is the token that just got
    /// rejected: if another task already refreshed while we waited for the
    /// lock, the current token is already different and is reused instead of
    /// refreshing a second time.
    async fn force_refresh(&self, stale: Option<&str>) -> Result<Option<String>, AuthError> {
        let Some(ClientCredential::Keycloak(session)) = &self.credential else {
            return Ok(None);
        };
        let mut guard = session.lock().await;
        if Some(guard.access_token.as_str()) != stale {
            return Ok(Some(guard.access_token.clone()));
        }
        self.refresh_locked(&mut guard).await?;
        Ok(Some(guard.access_token.clone()))
    }

    pub async fn register_repo(
        &self,
        req: RegisterRepoRequest,
    ) -> Result<RegisterRepoResponse, Box<dyn Error>> {
        let builder = self
            .client
            .post(format!("{}/api/v1/repos", self.base_url))
            .json(&req);
        self.authed_send_json(builder, |status| format!("Server returned {status}"))
            .await
    }

    /// Attach a bearer token (if there is one) to a request. Shared by every
    /// authenticated request builder so header attachment has exactly one
    /// implementation.
    fn attach_auth(
        builder: reqwest::RequestBuilder,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        match token {
            Some(t) => builder.header("Authorization", format!("Bearer {t}")),
            None => builder,
        }
    }

    /// Attach the bearer token and send `builder`. The shared first step of
    /// every authenticated request; callers that need bespoke status-code
    /// handling (e.g. treating 404/409 as non-error outcomes) use this
    /// directly instead of `authed_send_json`.
    ///
    /// On an unexpected 401 with a refreshable (Keycloak) credential the
    /// request is retried EXACTLY once against a force-refreshed token —
    /// covering a server-side session invalidation or a clock skew that the
    /// proactive expiry window missed. The retry is a bare `send`, not a
    /// recursive call, so a server that answers 401 unconditionally cannot
    /// make this loop. An API key never retries: a 401 on a `tvk_` key is a
    /// real rejection and re-sending it would just double every failure.
    async fn send_authed(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Box<dyn Error>> {
        // Cloned BEFORE sending: a builder is consumed by `send`. `None`
        // means the body isn't replayable, in which case we simply don't
        // retry.
        let retry = builder.try_clone();
        let token = self.bearer().await?;
        let resp = Self::attach_auth(builder, token.as_deref()).send().await?;

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let Some(retry) = retry else {
            return Ok(resp);
        };
        match self.force_refresh(token.as_deref()).await? {
            Some(fresh) => Ok(Self::attach_auth(retry, Some(&fresh)).send().await?),
            // Not refreshable — surface the original 401 unchanged.
            None => Ok(resp),
        }
    }

    /// Check that `resp`'s status is a success and deserialize its JSON
    /// body; otherwise build an error as `"{err_prefix(status)}: {body}"`.
    /// `err_prefix` receives the status so each caller can format its own
    /// distinct message (parenthesized status, bare status, ...) — this
    /// helper only supplies the shared "check status, else deserialize"
    /// shape, not the message wording.
    async fn success_json<T, F>(resp: reqwest::Response, err_prefix: F) -> Result<T, Box<dyn Error>>
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(reqwest::StatusCode) -> String,
    {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("{}: {}", err_prefix(status), body).into());
        }
        Ok(resp.json().await?)
    }

    /// Attach the bearer token, send `builder`, and deserialize a successful
    /// JSON response — the shared shape of every simple authenticated
    /// request/response method (`send_authed` + `success_json`).
    ///
    /// NOTE: callers whose `err_prefix` renders the status as `"({status})"`
    /// (e.g. `stream_event`'s `"Stream failed ({status})"`) produce error
    /// strings containing `"(404 "`/`"(403 "`/etc. —
    /// `commands::stream::is_deterministic_client_error` matches on exactly
    /// that substring shape to classify a stream-send failure. If you change
    /// how a status is rendered here, check that predicate (and its tests)
    /// still pass.
    async fn authed_send_json<T, F>(
        &self,
        builder: reqwest::RequestBuilder,
        err_prefix: F,
    ) -> Result<T, Box<dyn Error>>
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(reqwest::StatusCode) -> String,
    {
        let resp = self.send_authed(builder).await?;
        Self::success_json(resp, err_prefix).await
    }

    /// GET `{base}{path}` with the bearer token, mapping failures into
    /// `GetMeError` (401 → `Unauthorized`, 403 → `Forbidden`, transport →
    /// `Network`, other non-2xx or bad JSON → `Server`). Shared by the
    /// credential-scoped GETs so auth/error handling lives in one place.
    ///
    /// Goes through `send_authed`, so a Keycloak access token is refreshed
    /// before the call (and once after a 401) exactly like every other
    /// request. A refresh failure is mapped by cause: a dead session is
    /// `Unauthorized` ("log in again"), an unreachable IdP is `Network`
    /// ("cannot confirm") — `tracevault status` renders those very
    /// differently, so collapsing them would misreport a network blip as a
    /// revoked login.
    async fn authed_get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, GetMeError> {
        let builder = self.client.get(format!("{}{}", self.base_url, path));
        let resp = self.send_authed(builder).await.map_err(get_me_send_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GetMeError::Unauthorized);
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(GetMeError::Forbidden(body));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GetMeError::Server(format!("{status}: {body}")));
        }

        resp.json::<T>()
            .await
            .map_err(|e| GetMeError::Server(e.to_string()))
    }

    /// GET /api/v1/auth/me — validates the bearer token and returns user
    /// identity. Used by `tracevault status` to distinguish "logged out",
    /// "expired token", and "server unreachable".
    pub async fn get_me(&self) -> Result<MeResponse, GetMeError> {
        self.authed_get_json("/api/v1/auth/me").await
    }

    pub async fn list_repos(&self) -> Result<Vec<RepoListItem>, Box<dyn Error>> {
        let builder = self.client.get(format!("{}/api/v1/repos", self.base_url));
        self.authed_send_json(builder, |status| format!("Failed to list repos ({status})"))
            .await
    }

    pub async fn get_agent_instructions(
        &self,
        repo_id: &uuid::Uuid,
    ) -> Result<AgentInstructionsResponse, Box<dyn Error>> {
        let builder = self.client.get(format!(
            "{}/api/v1/repos/{}/policies/agent-instructions",
            self.base_url, repo_id
        ));
        self.authed_send_json(builder, |status| {
            format!("Failed to fetch agent instructions ({status})")
        })
        .await
    }

    pub async fn verify_commits(
        &self,
        repo_id: &uuid::Uuid,
        req: CiVerifyRequest,
    ) -> Result<CiVerifyResponse, Box<dyn Error>> {
        let builder = self
            .client
            .post(format!(
                "{}/api/v1/repos/{}/ci/verify",
                self.base_url, repo_id
            ))
            .json(&req);
        self.authed_send_json(builder, |status| format!("CI verify failed ({status})"))
            .await
    }

    pub async fn push_commit(
        &self,
        repo_id: &str,
        req: &tracevault_protocol::streaming::CommitPushRequest,
    ) -> Result<tracevault_protocol::streaming::CommitPushResponse, Box<dyn Error>> {
        let builder = self
            .client
            .post(format!(
                "{}/api/v1/repos/{}/commits",
                self.base_url, repo_id
            ))
            .json(req);
        self.authed_send_json(builder, |status| format!("Commit push failed ({status})"))
            .await
    }

    pub async fn stream_event(
        &self,
        repo_id: &str,
        req: &tracevault_protocol::streaming::StreamEventRequest,
    ) -> Result<tracevault_protocol::streaming::StreamEventResponse, Box<dyn Error>> {
        let builder = self
            .client
            .post(format!("{}/api/v1/repos/{}/stream", self.base_url, repo_id))
            .json(req);
        self.authed_send_json(builder, |status| format!("Stream failed ({status})"))
            .await
    }

    /// Project-scoped variant of `stream_event`: posts to the project's
    /// stream endpoint with `repo_id` as a query param instead of a path
    /// segment. The query is built with `Url::query_pairs_mut`, mirroring
    /// `resolve_project`'s `?git_url=`, so `repo_id` is percent-encoded
    /// rather than string-interpolated into the URL.
    ///
    /// Called from `commands::stream::send_stream_event` when a local
    /// project binding resolves for the capturing event.
    pub async fn stream_event_for_project(
        &self,
        project_id: uuid::Uuid,
        repo_id: &str,
        req: &tracevault_protocol::streaming::StreamEventRequest,
    ) -> Result<tracevault_protocol::streaming::StreamEventResponse, Box<dyn Error>> {
        let mut url = Url::parse(&format!(
            "{}/api/v1/projects/{}/stream",
            self.base_url, project_id
        ))?;
        url.query_pairs_mut().append_pair("repo_id", repo_id);
        let builder = self.client.post(url).json(req);
        self.authed_send_json(builder, |status| {
            format!("Project stream failed ({status})")
        })
        .await
    }

    pub async fn check_policies(
        &self,
        repo_id: &uuid::Uuid,
        req: CheckPoliciesRequest,
    ) -> Result<CheckPoliciesResponse, Box<dyn Error>> {
        let builder = self
            .client
            .post(format!(
                "{}/api/v1/repos/{}/policies/check",
                self.base_url, repo_id
            ))
            .json(&req);
        self.authed_send_json(builder, |status| format!("Policy check failed ({status})"))
            .await
    }

    /// Resolve a git URL to its codebase (git remote) by NORMALIZED URL —
    /// deduped, unlike an exact `github_url` match. `Ok(None)` if the
    /// codebase isn't tracked: a genuine domain 404, identified by the
    /// server's JSON error envelope (see [`is_domain_error_body`]). A bare
    /// 404 with no such body — axum's built-in "no route matched" fallback
    /// — is surfaced as `Err` instead, since it means this server doesn't
    /// have this route at all (most plausibly a CLI/server version skew,
    /// e.g. this CLI shipped ahead of a server that still uses the old
    /// org-scoped paths) rather than an absent codebase.
    pub async fn resolve_remote(
        &self,
        git_url: &str,
    ) -> Result<Option<ResolveRemoteResponse>, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!("{}/api/v1/remotes/resolve", self.base_url))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let resp = self.send_authed(self.client.get(url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            return if is_domain_error_body(&body) {
                Ok(None)
            } else {
                Err(version_skew_404_error("resolve_remote"))
            };
        }
        let parsed: ResolveRemoteResponse =
            Self::success_json(resp, |status| format!("resolve_remote failed ({status})")).await?;
        Ok(Some(parsed))
    }

    /// Full detail for a remote (codebase): its display name, normalized URL,
    /// clone status, and linked repos.
    pub async fn get_remote_detail(
        &self,
        remote_id: uuid::Uuid,
    ) -> Result<RemoteDetail, Box<dyn std::error::Error>> {
        let builder = self
            .client
            .get(format!("{}/api/v1/remotes/{}", self.base_url, remote_id));
        self.authed_send_json(builder, |status| {
            format!("get_remote_detail failed ({status})")
        })
        .await
    }

    /// The repos linked to a remote (the codebase's members).
    pub async fn get_remote_repos(
        &self,
        remote_id: uuid::Uuid,
    ) -> Result<Vec<RemoteRepoRef>, Box<dyn std::error::Error>> {
        Ok(self.get_remote_detail(remote_id).await?.repos)
    }

    /// List all projects. `GET /api/v1/projects`.
    pub async fn list_projects(&self) -> Result<Vec<ProjectListItem>, Box<dyn Error>> {
        let builder = self
            .client
            .get(format!("{}/api/v1/projects", self.base_url));
        self.authed_send_json(builder, |status| {
            format!("Failed to list projects ({status})")
        })
        .await
    }

    /// Full detail for a project. `GET /api/v1/projects/{id}`.
    pub async fn get_project(&self, id: uuid::Uuid) -> Result<ProjectDetail, Box<dyn Error>> {
        let builder = self
            .client
            .get(format!("{}/api/v1/projects/{}", self.base_url, id));
        self.authed_send_json(builder, |status| {
            format!("Failed to get project ({status})")
        })
        .await
    }

    /// Resolve a git URL to its project, distinguishing "no project" (a
    /// genuine domain 404 — see [`is_domain_error_body`]) from "ambiguous,
    /// multiple candidate projects" (409). A bare 404 with no domain-error
    /// body is surfaced as `Err` instead of `None` (see `resolve_remote`'s
    /// doc comment for why). `GET /api/v1/projects/resolve?git_url=`.
    pub async fn resolve_project(
        &self,
        git_url: &str,
    ) -> Result<ResolveProjectOutcome, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!("{}/api/v1/projects/resolve", self.base_url))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let resp = self.send_authed(self.client.get(url)).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            return if is_domain_error_body(&body) {
                Ok(ResolveProjectOutcome::None)
            } else {
                Err(version_skew_404_error("resolve_project"))
            };
        }
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(ResolveProjectOutcome::Ambiguous);
        }
        let parsed: ResolveProjectResponse =
            Self::success_json(resp, |status| format!("resolve_project failed ({status})")).await?;
        Ok(ResolveProjectOutcome::Resolved(parsed.project_id))
    }
}

/// Map an OIDC failure onto [`AuthError`], keeping "the session is dead"
/// distinct from "the refresh attempt itself failed".
fn auth_error(e: crate::oidc::OidcError) -> AuthError {
    match e {
        crate::oidc::OidcError::SessionExpired => AuthError::SessionExpired,
        other => AuthError::Refresh(other.to_string()),
    }
}

/// Classify a `send_authed` failure for the `GetMeError`-returning callers.
/// A dead session becomes `Unauthorized`; everything else (transport, IdP
/// unreachable) becomes `Network`, which callers render as "cannot confirm".
fn get_me_send_error(e: Box<dyn Error>) -> GetMeError {
    match e.downcast::<AuthError>() {
        Ok(auth) => match *auth {
            AuthError::SessionExpired => GetMeError::Unauthorized,
            AuthError::Refresh(m) => GetMeError::Network(m),
        },
        Err(other) => GetMeError::Network(other.to_string()),
    }
}

/// Whether a 404 response body matches the server's JSON error envelope —
/// `{"error": "...", ...}`, emitted by every domain-level `AppError` (see
/// `tracevault-server`'s `error.rs`) — rather than the empty/plain body
/// axum's built-in "no route matched" fallback sends when the server has no
/// handler for the path at all. Used by `resolve_remote`/`resolve_project`
/// to tell a genuine "not tracked"/"no project" 404 apart from a 404 that
/// really means "this server doesn't have this route" (most plausibly a
/// CLI/server version skew).
fn is_domain_error_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").map(|_| ()))
        .is_some()
}

/// The error for a 404 whose body doesn't match the server's JSON error
/// envelope (see [`is_domain_error_body`]) — surfaced instead of silently
/// treating the call as "not tracked"/"no project", since this shape is the
/// CLI's best signal that the server doesn't recognize `what`'s route at
/// all, rather than a genuine absence of the resource.
fn version_skew_404_error(what: &str) -> Box<dyn std::error::Error> {
    format!(
        "{what} got a 404 with no recognizable error body — this endpoint may not exist on \
         this server yet. This usually means the CLI is newer than the server (a CLI/server \
         version mismatch); confirm the server has been upgraded and retry."
    )
    .into()
}

/// Resolve server URL and credential from multiple sources.
/// Priority: env var > credentials file > project config.toml
/// Returns (server_url, credential).
///
/// Only the credentials file can yield a [`Credential::Keycloak`]: an env var
/// or a `config.toml` `api_key` is always a `tvk_` API key, and an API key is
/// never treated as refreshable. That asymmetry is the whole contract — CI
/// and automation stay on the unchanging key path.
///
/// # Errors
///
/// Fails when the resolved server URL and a credential read from the
/// credentials file are for different TraceVault instances (see
/// [`server_mismatch_error`]). This covers EITHER kind of file-sourced
/// credential — the distinction that matters is not Keycloak-vs-key but
/// named-on-this-invocation vs. read-from-the-file. Failing is deliberate
/// rather than silently dropping the credential: every "Not logged in, run
/// `tracevault login`" message downstream would be actively WRONG advice — the
/// user IS logged in, and logging in again to the same instance would not
/// change anything.
#[allow(clippy::type_complexity)]
pub fn resolve_credentials(
    project_root: &Path,
) -> Result<(Option<String>, Option<Credential>), Box<dyn Error>> {
    // 1. Env var API key
    let env_key = std::env::var("TRACEVAULT_API_KEY").ok();

    // 2. Credentials file
    let creds = Credentials::load();

    // 3. Project config — parsed as TOML by its own loader, not by hand.
    //
    // The previous line-scanning version split on `=` and took field 1, so a
    // perfectly legal `api_key = "tvk_abc=="` resolved to `tvk_abc`: a silently
    // truncated key, surfacing later as an unexplained 401 that (by design) is
    // never retried. It also matched any line merely STARTING with the field
    // name (`server_url_backup`) and read keys nested under a `[table]` as
    // though they were top-level. `TracevaultConfig` already declares both
    // fields, so the hand-parse only existed to duplicate it — badly. Using
    // `load` also means a malformed config now warns instead of silently
    // yielding nothing.
    let config = crate::config::TracevaultConfig::load(project_root);
    let config_server_url = config.as_ref().and_then(|c| c.server_url.clone());
    let config_api_key = config.as_ref().and_then(|c| c.api_key.clone());

    // Resolve server URL: env > creds > config
    let server_url = std::env::var("TRACEVAULT_SERVER_URL")
        .ok()
        .or_else(|| creds.as_ref().map(|c| c.server_url.clone()))
        .or(config_server_url);

    // Resolve the credential: env api key > credentials file > config api key
    let credential = match env_key {
        // Named explicitly on this invocation: the operator's choice of
        // key/URL pairing is deliberate and is not second-guessed.
        Some(key) => Some(Credential::ApiKey(key)),
        None => {
            let from_file = creds.as_ref().and_then(Credentials::credential);
            if from_file.is_some() {
                // The two precedence chains above disagree: the URL can come
                // from the env while the credential comes from the file. Every
                // credential in that file — a Keycloak access token or a `tvk_`
                // API key alike — was issued by ONE TraceVault instance, so
                // handing it to a client pointed elsewhere transmits a secret to
                // a host it was never issued for, which then has it in its logs.
                //
                // This is the FIRST request, before any refresh, so neither
                // server-scoping guard in `refresh_locked` would see it.
                //
                // Only reachable when TRACEVAULT_SERVER_URL is set: with it
                // unset the URL comes from this same file, so the two match by
                // construction.
                let file_url = creds.as_ref().map_or("", |c| c.server_url.as_str());
                if let Some(target) = server_url.as_deref() {
                    if !crate::credentials::same_server(file_url, target) {
                        return Err(server_mismatch_error(file_url, target));
                    }
                }
            }
            // A `config.toml` `api_key` is NOT guarded: committing a key to a
            // project config, next to that config's own `server_url`, is as
            // deliberate as naming it in the environment. It is also only
            // reachable when the file yielded nothing.
            from_file.or_else(|| config_api_key.map(Credential::ApiKey))
        }
    };

    Ok((server_url, credential))
}

/// The error for "the saved credential is for a different TraceVault instance".
///
/// Applies to any FILE-SOURCED credential, Keycloak session or `tvk_` key
/// alike: both are secrets issued by one instance, and a key would not even
/// work against another instance — so without this the user gets a confusing
/// 401 from the wrong host instead of an explanation.
///
/// Names BOTH URLs, because the whole failure is that two of them disagree and
/// the user can see neither: `tracevault status` reports the file's URL, and
/// the env var is often set in a shell profile or a CI job definition rather
/// than on the command line.
///
/// The third suggestion matters: pointing at the SAME instance through another
/// address (a `kubectl port-forward`, an internal vs. external hostname) is a
/// legitimate workflow that this guard cannot distinguish from a genuine
/// mismatch, so the message names the ways to proceed deliberately.
fn server_mismatch_error(file_url: &str, target: &str) -> Box<dyn Error> {
    format!(
        "refusing to use the saved credentials: the credentials file is for '{file_url}', but \
         this command is targeting '{target}'. A credential issued by one TraceVault instance \
         must not be sent to another.\n\
         Either unset TRACEVAULT_SERVER_URL to use '{file_url}', or run `tracevault login \
         --server-url {target}`. If '{target}' is the same instance reached through a different \
         address (e.g. a port-forward), set TRACEVAULT_API_KEY to a `tvk_` key for it instead."
    )
    .into()
}

/// Resolve `project_root`'s credentials (via `resolve_credentials`) into a
/// ready `ApiClient`, or the standard "no server URL configured" error when
/// none of the credential sources yield a server URL. Shared by every
/// command that needs a client from a project root, so this
/// resolve-then-construct shape has exactly one implementation.
pub fn resolve_client(project_root: &Path) -> Result<ApiClient, Box<dyn Error>> {
    let (server_url, credential) = resolve_credentials(project_root)?;
    let server_url = server_url
        .ok_or("no server URL configured: set TRACEVAULT_SERVER_URL or run `tracevault login`")?;
    Ok(ApiClient::with_credential(&server_url, credential))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::KeycloakSession;
    use crate::test_helpers::{http_json, spawn_seq, spawn_seq_with, RECV_TIMEOUT};
    use std::time::Duration;

    /// Fixed "now" for the refresh-window tests. Using a constant instead of
    /// the wall clock is what makes the boundary assertions deterministic.
    const NOW: i64 = 1_800_000_000;

    fn fixed_now() -> i64 {
        NOW
    }

    /// A fake IdP that answers discovery (self-reporting its own URL as the
    /// issuer, as a real one must) and then one token request. Returns the
    /// issuer URL to put in the session.
    fn spawn_idp(token_response: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        spawn_seq_with(move |base| {
            vec![
                http_json(
                    "200 OK",
                    &format!(
                        r#"{{"issuer":"{base}","token_endpoint":"{base}/token","device_authorization_endpoint":"{base}/device","revocation_endpoint":"{base}/revoke"}}"#
                    ),
                ),
                http_json("200 OK", token_response),
            ]
        })
    }

    /// Write a Keycloak credentials file for `server_url`/`issuer` into
    /// `dir`, so a refresh has something to adopt from and persist into.
    ///
    /// `server_url` is explicit because the adopt and the persist are both
    /// scoped to the client's `base_url`: a helper that hardcoded it would
    /// silently disable persistence in tests whose client points elsewhere.
    fn write_keycloak_file(dir: &std::path::Path, server_url: &str, issuer: &str, expires_at: i64) {
        let creds_dir = dir.join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            format!(
                r#"{{"server_url":"{server_url}","email":"a@b.com","auth":{{"issuer":"{issuer}","client_id":"tracevault-cli","refresh_token":"old-rt","access_token":"old-at","access_expires_at":{expires_at}}}}}"#
            ),
        )
        .unwrap();
    }

    fn session(issuer: &str, expires_at: i64) -> KeycloakSession {
        KeycloakSession {
            issuer: issuer.to_string(),
            client_id: "tracevault-cli".into(),
            refresh_token: "old-rt".into(),
            access_token: "old-at".into(),
            access_expires_at: expires_at,
        }
    }

    /// An access token inside the 60s window is refreshed BEFORE the request
    /// goes out, and the new tokens are written back to disk so the next
    /// process doesn't repeat the refresh.
    #[tokio::test]
    async fn bearer_refreshes_inside_the_sixty_second_window() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, rx) =
            spawn_idp(r#"{"access_token":"fresh-at","refresh_token":"fresh-rt","expires_in":300}"#);
        // 30s of life left — inside the window.
        write_keycloak_file(dir.path(), "https://example.com", &issuer, NOW + 30);

        let client = ApiClient::with_credential(
            "https://example.com",
            Some(Credential::Keycloak(session(&issuer, NOW + 30))),
        )
        .with_now(fixed_now);

        let token = client.bearer().await.unwrap();
        assert_eq!(token.as_deref(), Some("fresh-at"));

        // Discovery, then the refresh_token grant.
        let disc = rx.recv_timeout(RECV_TIMEOUT).expect("no discovery request");
        assert!(disc.contains("/.well-known/openid-configuration"), "{disc}");
        let refresh = rx.recv_timeout(RECV_TIMEOUT).expect("no token request");
        assert!(refresh.contains("grant_type=refresh_token"), "{refresh}");
        assert!(refresh.contains("refresh_token=old-rt"), "{refresh}");
        assert!(
            !refresh.contains("client_secret"),
            "the CLI client is public; no secret may be sent: {refresh}"
        );

        // Persisted, so a git hook running a second later doesn't refresh again.
        let saved = Credentials::load().expect("credentials must still exist");
        let auth = saved.auth.expect("auth block must survive a refresh");
        assert_eq!(auth.access_token, "fresh-at");
        assert_eq!(auth.refresh_token, "fresh-rt");
        assert_eq!(auth.access_expires_at, NOW + 300);
        assert_eq!(saved.email, "a@b.com", "unrelated fields must be preserved");
    }

    /// Two git hooks can run at once, and `tracevault status` builds two
    /// clients from one credential. If another process already refreshed, this
    /// one must ADOPT that session rather than present the refresh token that
    /// was just consumed — with refresh-token rotation enabled that would come
    /// back `invalid_grant` and log the user out for no reason.
    ///
    /// The issuer points at a dead port, so any actual refresh attempt would
    /// fail the call: reaching the assertion proves no round trip happened.
    #[tokio::test]
    async fn bearer_adopts_a_newer_session_written_by_another_process() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        // On disk: a session refreshed by "another process" — different
        // refresh token, plenty of life left.
        let creds_dir = dir.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            format!(
                r#"{{"server_url":"https://example.com","email":"a@b.com","auth":{{"issuer":"http://127.0.0.1:1","client_id":"tracevault-cli","refresh_token":"rotated-rt","access_token":"other-process-at","access_expires_at":{}}}}}"#,
                NOW + 3600
            ),
        )
        .unwrap();

        // In memory: the stale session this client was built from.
        let client = ApiClient::with_credential(
            "https://example.com",
            Some(Credential::Keycloak(session(
                "http://127.0.0.1:1",
                NOW + 10,
            ))),
        )
        .with_now(fixed_now);

        let token = client
            .bearer()
            .await
            .expect("adopting the on-disk session must not require the IdP");
        assert_eq!(token.as_deref(), Some("other-process-at"));
    }

    /// `ApiClient` trims trailing slashes off its `base_url`, but
    /// `credentials.json` stores `server_url` exactly as it was passed to
    /// `tracevault login`. Comparing raw strings would therefore skip the
    /// adopt for a perfectly legitimate same-server file.
    #[tokio::test]
    async fn adopt_matches_a_stored_server_url_with_a_trailing_slash() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        // Stored WITH a trailing slash; the client's base_url has none.
        write_keycloak_file(
            dir.path(),
            "https://example.com/",
            "http://127.0.0.1:1",
            NOW + 3600,
        );
        let mut on_disk = Credentials::load().unwrap();
        on_disk.auth.as_mut().unwrap().refresh_token = "rotated-rt".into();
        on_disk.auth.as_mut().unwrap().access_token = "other-process-at".into();
        on_disk.save().unwrap();

        let client = ApiClient::with_credential(
            "https://example.com",
            Some(Credential::Keycloak(session(
                "http://127.0.0.1:1",
                NOW + 10,
            ))),
        )
        .with_now(fixed_now);

        // The issuer is a dead port, so an un-adopted session could only fail.
        let token = client
            .bearer()
            .await
            .expect("a trailing slash must not prevent the adopt");
        assert_eq!(token.as_deref(), Some("other-process-at"));
    }

    /// There is one credentials file but a machine can target several
    /// TraceVault instances. A file belonging to instance B must NOT be adopted
    /// by a client talking to instance A (that would send B's token to A), and
    /// A's refreshed session must NOT overwrite B's file (that would sign the
    /// user out of B).
    #[tokio::test]
    async fn a_credentials_file_for_another_server_is_neither_adopted_nor_overwritten() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, rx) = spawn_idp(
            r#"{"access_token":"a-fresh-at","refresh_token":"a-fresh-rt","expires_in":300}"#,
        );

        // On disk: instance B's session, freshly written by `tv login` in
        // another terminal. Different refresh token, plenty of life left — so
        // an unscoped adopt WOULD take it.
        let creds_dir = dir.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        let b_file = format!(
            r#"{{"server_url":"https://instance-b.example.com","email":"b@b.com","auth":{{"issuer":"{issuer}","client_id":"tracevault-cli","refresh_token":"b-rt","access_token":"b-at","access_expires_at":{}}}}}"#,
            NOW + 3600
        );
        std::fs::write(creds_dir.join("credentials.json"), &b_file).unwrap();

        // This client talks to instance A and holds A's own session.
        let client = ApiClient::with_credential(
            "https://instance-a.example.com",
            Some(Credential::Keycloak(session(&issuer, NOW + 10))),
        )
        .with_now(fixed_now);

        let token = client.bearer().await.expect("A's own refresh must succeed");
        assert_eq!(token.as_deref(), "a-fresh-at".into());

        // The refresh used A's OWN in-memory token — B's was never presented.
        let _discovery = rx.recv_timeout(RECV_TIMEOUT).expect("no discovery request");
        let refresh = rx.recv_timeout(RECV_TIMEOUT).expect("no token request");
        assert!(
            refresh.contains("refresh_token=old-rt"),
            "A must refresh with its own token: {refresh}"
        );
        assert!(
            !refresh.contains("b-rt"),
            "another server's refresh token must never be presented: {refresh}"
        );

        // And B's credentials file is untouched: byte-identical to what
        // `tv login` wrote for B.
        let after = std::fs::read_to_string(creds_dir.join("credentials.json")).unwrap();
        assert_eq!(
            after, b_file,
            "instance A's refresh overwrote instance B's credentials"
        );
    }

    /// The interleaving that makes the adopt unconditional: the on-disk
    /// session has a NEWER refresh token AND its access token is itself inside
    /// the refresh window. A refresh is genuinely needed, so it must be
    /// performed with the ADOPTED refresh token — refreshing with the
    /// in-memory one, which another client already consumed, is the
    /// guaranteed-`invalid_grant` case the adopt exists to prevent.
    #[tokio::test]
    async fn a_refresh_after_adopting_uses_the_disk_refresh_token() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, rx) = spawn_idp(
            r#"{"access_token":"newest-at","refresh_token":"newest-rt","expires_in":300}"#,
        );

        // On disk: another client already rotated the refresh token, but its
        // access token is ALSO within 60s of expiry.
        let creds_dir = dir.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            format!(
                r#"{{"server_url":"https://example.com","email":"a@b.com","auth":{{"issuer":"{issuer}","client_id":"tracevault-cli","refresh_token":"rotated-rt","access_token":"rotated-at","access_expires_at":{}}}}}"#,
                NOW + 5
            ),
        )
        .unwrap();

        // In memory: the superseded session, also inside the window.
        let client = ApiClient::with_credential(
            "https://example.com",
            Some(Credential::Keycloak(session(&issuer, NOW + 10))),
        )
        .with_now(fixed_now);

        let token = client.bearer().await.expect("the refresh must succeed");
        assert_eq!(token.as_deref(), Some("newest-at"));

        let _discovery = rx.recv_timeout(RECV_TIMEOUT).expect("no discovery request");
        let refresh = rx.recv_timeout(RECV_TIMEOUT).expect("no token request");
        assert!(
            refresh.contains("refresh_token=rotated-rt"),
            "the refresh must use the adopted on-disk token: {refresh}"
        );
        assert!(
            !refresh.contains("refresh_token=old-rt"),
            "the superseded in-memory token must NOT be presented: {refresh}"
        );
    }

    /// Outside the window the stored token is used as-is. The issuer points
    /// at a port nothing listens on, so ANY refresh attempt would fail the
    /// call — proving no network round trip happened.
    #[tokio::test]
    async fn bearer_does_not_refresh_outside_the_window() {
        let client = ApiClient::with_credential(
            "https://example.com",
            Some(Credential::Keycloak(session(
                "http://127.0.0.1:1",
                NOW + 3600,
            ))),
        )
        .with_now(fixed_now);

        let token = client
            .bearer()
            .await
            .expect("a still-valid token must not trigger any IdP call");
        assert_eq!(token.as_deref(), Some("old-at"));
    }

    /// The API-key path is a pure passthrough: no clock, no IdP, no refresh.
    #[tokio::test]
    async fn bearer_returns_an_api_key_verbatim() {
        let client = ApiClient::new("https://example.com", Some("tvk_abc"));
        assert_eq!(client.bearer().await.unwrap().as_deref(), Some("tvk_abc"));

        let anonymous = ApiClient::new("https://example.com", None);
        assert_eq!(anonymous.bearer().await.unwrap(), None);
    }

    /// A 401 on an API key is a real rejection: retrying would double every
    /// failure and could re-run a non-idempotent write. Exactly ONE request
    /// must reach the server.
    #[tokio::test]
    async fn a_401_with_an_api_key_does_not_refresh_or_retry() {
        let (base, rx) = spawn_seq(vec![
            http_json("401 Unauthorized", r#"{"error":"invalid token"}"#),
            // A second response is queued precisely to prove it is unused.
            http_json("200 OK", "[]"),
        ]);
        let client = ApiClient::new(&base, Some("tvk_abc"));
        client
            .list_repos()
            .await
            .expect_err("a 401 must surface as an error");

        let first = rx.recv_timeout(RECV_TIMEOUT).expect("no request captured");
        assert!(first.contains("GET /api/v1/repos"), "{first}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "an API key must never be retried after a 401"
        );
    }

    /// A Keycloak credential that gets an unexpected 401 (server-side session
    /// invalidation, clock skew) force-refreshes and retries ONCE.
    #[tokio::test]
    async fn a_401_with_a_keycloak_credential_refreshes_and_retries_once() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, _idp_rx) =
            spawn_idp(r#"{"access_token":"fresh-at","refresh_token":"fresh-rt","expires_in":300}"#);
        let (base, rx) = spawn_seq(vec![
            http_json("401 Unauthorized", r#"{"error":"expired"}"#),
            http_json("200 OK", "[]"),
        ]);
        // The file is for THIS client's server, so the refreshed session is
        // persisted rather than skipped by the same-server guard.
        write_keycloak_file(dir.path(), &base, &issuer, NOW + 3600);

        // Far from expiry: the proactive window must NOT be what triggers the
        // refresh here — the 401 must.
        let client = ApiClient::with_credential(
            &base,
            Some(Credential::Keycloak(session(&issuer, NOW + 3600))),
        )
        .with_now(fixed_now);

        let repos = client
            .list_repos()
            .await
            .expect("the retry after a forced refresh must succeed");
        assert!(repos.is_empty());

        // The point of the retry is that it presents the REFRESHED token; a
        // retry that re-sent the rejected one would be pointless and would
        // still 401 in production.
        let first = rx.recv_timeout(RECV_TIMEOUT).expect("no first request");
        assert!(first.contains("GET /api/v1/repos"), "{first}");
        assert!(
            first.contains("Bearer old-at"),
            "the first attempt must carry the stored token: {first}"
        );
        let second = rx.recv_timeout(RECV_TIMEOUT).expect("no retry request");
        assert!(second.contains("GET /api/v1/repos"), "{second}");
        assert!(
            second.contains("Bearer fresh-at"),
            "the retry must carry the REFRESHED token, not the rejected one: {second}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the retry must happen at most once, or a 401-always server would loop"
        );
    }

    /// A 401 that survives the retry must not send the request a third time:
    /// a server answering 401 unconditionally has to terminate.
    #[tokio::test]
    async fn a_persistent_401_stops_after_one_retry() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, _idp_rx) =
            spawn_idp(r#"{"access_token":"fresh-at","refresh_token":"fresh-rt","expires_in":300}"#);
        let (base, rx) = spawn_seq(vec![
            http_json("401 Unauthorized", r#"{"error":"nope"}"#),
            http_json("401 Unauthorized", r#"{"error":"nope"}"#),
            http_json("200 OK", "[]"),
        ]);
        write_keycloak_file(dir.path(), &base, &issuer, NOW + 3600);
        let client = ApiClient::with_credential(
            &base,
            Some(Credential::Keycloak(session(&issuer, NOW + 3600))),
        )
        .with_now(fixed_now);

        client
            .list_repos()
            .await
            .expect_err("a persistent 401 must surface as an error");

        assert!(rx.recv_timeout(RECV_TIMEOUT).is_ok());
        assert!(rx.recv_timeout(RECV_TIMEOUT).is_ok());
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "a third attempt means the retry can loop"
        );
    }

    /// A dead refresh token must surface as "log in again", not as a bland
    /// 401 or a network error — `tracevault status` renders those differently.
    #[tokio::test]
    async fn get_me_maps_a_dead_refresh_token_to_unauthorized() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (issuer, _rx) = spawn_seq_with(|base| {
            vec![
                http_json(
                    "200 OK",
                    &format!(r#"{{"issuer":"{base}","token_endpoint":"{base}/token"}}"#),
                ),
                http_json("400 Bad Request", r#"{"error":"invalid_grant"}"#),
            ]
        });
        write_keycloak_file(dir.path(), "http://127.0.0.1:1", &issuer, NOW + 10);

        let client = ApiClient::with_credential(
            "http://127.0.0.1:1",
            // Inside the window, so the (failing) refresh happens before any
            // request to the server is attempted.
            Some(Credential::Keycloak(session(&issuer, NOW + 10))),
        )
        .with_now(fixed_now);

        match client.get_me().await {
            Err(GetMeError::Unauthorized) => {}
            other => panic!("expected Unauthorized for a dead session, got {other:?}"),
        }
    }

    /// An unreachable IdP is NOT a dead session: it must stay distinguishable
    /// so status says "cannot confirm" rather than "you were logged out".
    #[tokio::test]
    async fn get_me_maps_an_unreachable_idp_to_network() {
        // The session is inside the refresh window, so `refresh_locked` calls
        // `Credentials::load()`. Without this redirect that reads the
        // DEVELOPER's real `~/.config/tracevault/credentials.json` — which on a
        // machine logged in with Keycloak means reading a live token, and makes
        // the test's outcome depend on the developer's own login state.
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let client = ApiClient::with_credential(
            "http://127.0.0.1:1",
            Some(Credential::Keycloak(session(
                "http://127.0.0.1:1",
                NOW + 10,
            ))),
        )
        .with_now(fixed_now);

        match client.get_me().await {
            Err(GetMeError::Network(_)) => {}
            other => panic!("expected Network for an unreachable IdP, got {other:?}"),
        }
    }

    /// A 403 is its own outcome: the token is fine, the account isn't
    /// authorized, and "log in again" would be the wrong advice.
    #[tokio::test]
    async fn get_me_maps_403_to_forbidden() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "403 Forbidden",
            r#"{"error":"missing role"}"#,
        )]);
        let client = ApiClient::new(&base, Some("tvk_abc"));
        match client.get_me().await {
            Err(GetMeError::Forbidden(_)) => {}
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    // ---- resolve_credentials: a credential read from the FILE must not follow
    // ---- a TRACEVAULT_SERVER_URL override to another instance. Applies to a
    // ---- Keycloak session and a `tvk_` key alike; an env- or config-supplied
    // ---- key is a deliberate operator act and is left alone.

    type ResolveFixture = (
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
        crate::test_helpers::EnvVarGuard,
    );

    /// Set up an empty project root with `TRACEVAULT_SERVER_URL` set to
    /// `env_url` (unset when `None`) and `credentials.json` containing
    /// `file_body` (no file when `None`). Returns the tempdir (which doubles as
    /// the project root) plus the guards the caller must keep alive.
    fn resolve_fixture_with(file_body: Option<&str>, env_url: Option<&str>) -> ResolveFixture {
        let env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", dir.path());
        // A stray key in the developer's environment would take precedence and
        // mask what these tests are checking.
        guard.remove("TRACEVAULT_API_KEY");
        match env_url {
            Some(u) => guard.set("TRACEVAULT_SERVER_URL", u),
            None => guard.remove("TRACEVAULT_SERVER_URL"),
        }
        if let Some(body) = file_body {
            let creds_dir = dir.path().join("tracevault");
            std::fs::create_dir_all(&creds_dir).unwrap();
            std::fs::write(creds_dir.join("credentials.json"), body).unwrap();
        }
        (dir, env_lock, guard)
    }

    /// [`resolve_fixture_with`] for a file holding a Keycloak session.
    fn resolve_fixture(file_server_url: &str, env_url: Option<&str>) -> ResolveFixture {
        let fixture = resolve_fixture_with(None, env_url);
        write_keycloak_file(
            fixture.0.path(),
            file_server_url,
            "https://idp.test/realms/v",
            NOW + 3600,
        );
        fixture
    }

    /// [`resolve_fixture_with`] for a file holding a `tvk_` API key.
    fn resolve_fixture_api_key(file_server_url: &str, env_url: Option<&str>) -> ResolveFixture {
        resolve_fixture_with(
            Some(&format!(
                r#"{{"server_url":"{file_server_url}","token":"tvk_from_file","email":"a@b.com"}}"#
            )),
            env_url,
        )
    }

    /// The precedence chains disagree: the URL comes from the env, the
    /// credential from the file. Handing over the session would send instance
    /// B's access token to instance A on the FIRST request, before any refresh,
    /// so neither guard in `refresh_locked` would ever see it.
    #[test]
    fn a_mismatched_server_url_refuses_the_files_keycloak_session() {
        let (dir, _lock, _guard) = resolve_fixture(
            "https://instance-b.example.com",
            Some("https://instance-a.example.com"),
        );

        let err = resolve_credentials(dir.path())
            .expect_err("a session for another instance must not be handed out");
        let msg = err.to_string();
        // Both URLs must be named: the user can see neither (status prints the
        // file's, and the env var is usually set in a profile or CI job).
        assert!(
            msg.contains("https://instance-b.example.com"),
            "must name the file's server: {msg}"
        );
        assert!(
            msg.contains("https://instance-a.example.com"),
            "must name the targeted server: {msg}"
        );
        // And it must point at the deliberate ways forward, including the
        // same-instance-different-address case this guard cannot distinguish.
        assert!(msg.contains("TRACEVAULT_SERVER_URL"), "{msg}");
        assert!(msg.contains("tracevault login"), "{msg}");
        assert!(msg.contains("TRACEVAULT_API_KEY"), "{msg}");
    }

    /// The two sides are formatted differently by construction, so a trailing
    /// slash must not be read as "a different instance".
    #[test]
    fn a_matching_server_url_modulo_trailing_slash_still_resolves() {
        let (dir, _lock, _guard) =
            resolve_fixture("https://example.com/", Some("https://example.com"));

        let (url, credential) =
            resolve_credentials(dir.path()).expect("a trailing slash is not a different instance");
        assert_eq!(url.as_deref(), Some("https://example.com"));
        assert!(matches!(credential, Some(Credential::Keycloak(_))));
    }

    /// The normal case: no env override, so the URL comes from the same file as
    /// the credential and they match by construction.
    #[test]
    fn no_env_override_resolves_the_files_session_unchanged() {
        let (dir, _lock, _guard) = resolve_fixture("https://example.com", None);

        let (url, credential) = resolve_credentials(dir.path()).expect("the normal case must work");
        assert_eq!(url.as_deref(), Some("https://example.com"));
        assert!(matches!(credential, Some(Credential::Keycloak(_))));
    }

    /// A `tvk_` key in the credentials file is instance-bound too: keys are
    /// per-instance, so sending it to another host both leaks it into that
    /// host's logs AND cannot succeed — today's outcome without the guard is a
    /// confusing 401 from the wrong server.
    #[test]
    fn a_mismatched_server_url_refuses_the_files_api_key() {
        let (dir, _lock, _guard) = resolve_fixture_api_key(
            "https://instance-b.example.com",
            Some("https://instance-a.example.com"),
        );

        let err = resolve_credentials(dir.path())
            .expect_err("a key for another instance must not be handed out");
        let msg = err.to_string();
        assert!(
            msg.contains("https://instance-b.example.com"),
            "must name the file's server: {msg}"
        );
        assert!(
            msg.contains("https://instance-a.example.com"),
            "must name the targeted server: {msg}"
        );
        assert!(msg.contains("TRACEVAULT_SERVER_URL"), "{msg}");
        assert!(msg.contains("tracevault login"), "{msg}");
        assert!(msg.contains("TRACEVAULT_API_KEY"), "{msg}");
        assert!(
            !msg.contains("tvk_from_file"),
            "the error must not echo the key itself: {msg}"
        );
    }

    /// Same normalisation for a file-sourced key as for a session: the file
    /// keeps what login was given, `ApiClient` trims trailing slashes.
    #[test]
    fn a_file_api_key_with_a_matching_url_modulo_trailing_slash_still_resolves() {
        let (dir, _lock, _guard) =
            resolve_fixture_api_key("https://example.com/", Some("https://example.com"));

        let (url, credential) =
            resolve_credentials(dir.path()).expect("a trailing slash is not a different instance");
        assert_eq!(url.as_deref(), Some("https://example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_from_file"),
            other => panic!("expected the file's API key, got {other:?}"),
        }
    }

    /// The config is parsed as TOML, not scanned line by line. A base64-ish key
    /// containing `=` used to be truncated at the first `=` (`tvk_abc==` ->
    /// `tvk_abc`), producing an unexplained 401 that the `ApiKey` path
    /// deliberately never retries.
    #[test]
    fn a_config_api_key_containing_equals_signs_is_not_truncated() {
        let (dir, _lock, _guard) = resolve_fixture_with(None, None);
        let config_dir = dir.path().join(".tracevault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://example.com\"\napi_key = \"tvk_YWJjZA==\"\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(
                k, "tvk_YWJjZA==",
                "the key must survive verbatim, `=` padding and all"
            ),
            other => panic!("expected the config API key, got {other:?}"),
        }
    }

    /// Line-scanning also matched any field whose name merely STARTED with the
    /// one being looked for, and read a key nested in a `[table]` as top-level.
    /// TOML parsing gets both right.
    #[test]
    fn a_config_lookalike_field_is_not_mistaken_for_the_real_one() {
        let (dir, _lock, _guard) = resolve_fixture_with(None, None);
        let config_dir = dir.path().join(".tracevault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://real.example.com\"\n\
             [user_context]\n\
             enable = false\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://real.example.com"));
        assert!(
            credential.is_none(),
            "no api_key is configured anywhere, so there is no credential"
        );
    }

    /// A `config.toml` `api_key` is committed next to that config's own
    /// `server_url`: as deliberate as naming it in the environment, so a URL
    /// override must not break it. (It is also only consulted when the
    /// credentials file yielded nothing, as here.)
    #[test]
    fn a_config_api_key_is_unaffected_by_a_mismatched_server_url() {
        let (dir, _lock, _guard) =
            resolve_fixture_with(None, Some("https://instance-a.example.com"));
        let config_dir = dir.path().join(".tracevault");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://instance-b.example.com\"\napi_key = \"tvk_from_config\"\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path())
            .expect("a config-supplied key must never be second-guessed");
        assert_eq!(url.as_deref(), Some("https://instance-a.example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_from_config"),
            other => panic!("expected the config API key, got {other:?}"),
        }
    }

    /// `TRACEVAULT_API_KEY` plus a URL pointing anywhere is a deliberate,
    /// explicit act by the operator (and the CI path). It must be unaffected.
    #[test]
    fn an_env_api_key_is_unaffected_by_a_mismatched_server_url() {
        let (dir, _lock, mut guard) = resolve_fixture(
            "https://instance-b.example.com",
            Some("https://instance-a.example.com"),
        );
        guard.set("TRACEVAULT_API_KEY", "tvk_abc");

        let (url, credential) = resolve_credentials(dir.path())
            .expect("an explicitly supplied API key must never be second-guessed");
        assert_eq!(url.as_deref(), Some("https://instance-a.example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_abc"),
            other => panic!("expected the env API key, got {other:?}"),
        }
    }

    /// `resolve_client` is the funnel the project/repo commands use; the guard
    /// has to hold there too, not just in `resolve_credentials`.
    #[test]
    fn resolve_client_also_refuses_a_mismatched_server_url() {
        let (dir, _lock, _guard) = resolve_fixture(
            "https://instance-b.example.com",
            Some("https://instance-a.example.com"),
        );

        // `ApiClient` deliberately has no `Debug` (it holds a credential), so
        // match instead of `expect_err`.
        let err = match resolve_client(dir.path()) {
            Err(e) => e,
            Ok(_) => panic!("resolve_client must apply the guard"),
        };
        assert!(err.to_string().contains("instance-b.example.com"));
    }
}
