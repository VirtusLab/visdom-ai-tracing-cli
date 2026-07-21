use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::Path;
use url::Url;

pub struct ApiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct DeviceAuthResponse {
    pub token: String,
}

#[derive(Deserialize)]
pub struct DeviceStatusResponse {
    pub status: String,
    pub token: Option<String>,
    pub email: Option<String>,
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
}

#[derive(Debug)]
pub enum GetMeError {
    /// 401 — token is missing or invalid.
    Unauthorized,
    /// Transport-level failure (DNS, TCP, TLS, timeout).
    Network(String),
    /// HTTP ≥ 400 other than 401, or malformed JSON.
    Server(String),
}

impl std::fmt::Display for GetMeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "unauthorized (token invalid or expired)"),
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
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(String::from),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        }
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

    pub async fn device_start(&self) -> Result<DeviceAuthResponse, Box<dyn Error>> {
        let resp = self
            .client
            // Send an explicit `Content-Length: 0`. reqwest/hyper omit the header
            // entirely for a bodyless POST, and strict frontends (e.g. Google
            // Front End) reject such requests with `411 Length Required`.
            .post(format!("{}/api/v1/auth/device", self.base_url))
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Server returned {status}: {body}").into());
        }

        Ok(resp.json().await?)
    }

    pub async fn device_status(&self, token: &str) -> Result<DeviceStatusResponse, Box<dyn Error>> {
        let resp = self
            .client
            .get(format!(
                "{}/api/v1/auth/device/{token}/status",
                self.base_url
            ))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Server returned {status}: {body}").into());
        }

        Ok(resp.json().await?)
    }

    pub async fn logout(&self) -> Result<(), Box<dyn Error>> {
        let mut builder = self
            .client
            .post(format!("{}/api/v1/auth/logout", self.base_url));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        // Explicit `Content-Length: 0` (see `device_start`): reqwest/hyper omit it
        // for a bodyless POST, which strict frontends reject with 411.
        let resp = builder
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Server returned {status}: {body}").into());
        }
        Ok(())
    }

    /// Attach the bearer token (if configured) to a request. Shared by every
    /// authenticated request builder so header attachment has exactly one
    /// implementation.
    fn attach_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.header("Authorization", format!("Bearer {key}")),
            None => builder,
        }
    }

    /// Attach the bearer token and send `builder`. The shared first step of
    /// every authenticated request; callers that need bespoke status-code
    /// handling (e.g. treating 404/409 as non-error outcomes) use this
    /// directly instead of `authed_send_json`.
    async fn send_authed(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Box<dyn Error>> {
        Ok(self.attach_auth(builder).send().await?)
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
    /// `GetMeError` (401 → `Unauthorized`, transport → `Network`, other
    /// non-2xx or bad JSON → `Server`). Shared by the credential-scoped GETs
    /// so auth/error handling lives in one place.
    async fn authed_get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, GetMeError> {
        let builder = self.attach_auth(self.client.get(format!("{}{}", self.base_url, path)));

        let resp = builder
            .send()
            .await
            .map_err(|e| GetMeError::Network(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GetMeError::Unauthorized);
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
    /// codebase isn't tracked (404).
    pub async fn resolve_remote(
        &self,
        git_url: &str,
    ) -> Result<Option<ResolveRemoteResponse>, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!("{}/api/v1/remotes/resolve", self.base_url))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let resp = self.send_authed(self.client.get(url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
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

    /// Resolve a git URL to its project, distinguishing "no project" (404)
    /// from "ambiguous, multiple candidate projects" (409).
    /// `GET /api/v1/projects/resolve?git_url=`.
    pub async fn resolve_project(
        &self,
        git_url: &str,
    ) -> Result<ResolveProjectOutcome, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!("{}/api/v1/projects/resolve", self.base_url))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let resp = self.send_authed(self.client.get(url)).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(ResolveProjectOutcome::None);
        }
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(ResolveProjectOutcome::Ambiguous);
        }
        let parsed: ResolveProjectResponse =
            Self::success_json(resp, |status| format!("resolve_project failed ({status})")).await?;
        Ok(ResolveProjectOutcome::Resolved(parsed.project_id))
    }
}

/// Resolve server URL and auth token from multiple sources.
/// Priority: env var > credentials file > project config.toml
/// Returns (server_url, auth_token).
pub fn resolve_credentials(project_root: &Path) -> (Option<String>, Option<String>) {
    use crate::credentials::Credentials;

    // 1. Env var API key
    let env_key = std::env::var("TRACEVAULT_API_KEY").ok();

    // 2. Credentials file
    let creds = Credentials::load();

    // 3. Project config
    let config_path = crate::config::TracevaultConfig::config_path(project_root);
    let config_content = std::fs::read_to_string(&config_path).unwrap_or_default();

    let config_server_url = config_content
        .lines()
        .find(|l| l.starts_with("server_url"))
        .and_then(|l| l.split('=').nth(1))
        .map(|s| s.trim().trim_matches('"').to_string());

    let config_api_key = config_content
        .lines()
        .find(|l| l.starts_with("api_key"))
        .and_then(|l| l.split('=').nth(1))
        .map(|s| s.trim().trim_matches('"').to_string());

    // Resolve server URL: env > creds > config
    let server_url = std::env::var("TRACEVAULT_SERVER_URL")
        .ok()
        .or_else(|| creds.as_ref().map(|c| c.server_url.clone()))
        .or(config_server_url);

    // Resolve token: env api key > creds token > config api key
    let token = env_key
        .or_else(|| creds.map(|c| c.token))
        .or(config_api_key);

    (server_url, token)
}

/// Resolve `project_root`'s credentials (via `resolve_credentials`) into a
/// ready `ApiClient`, or the standard "no server URL configured" error when
/// none of the credential sources yield a server URL. Shared by every
/// command that needs a client from a project root
/// (`commands::project::switch`/`status`, `commands::repo::switch`) so this
/// resolve-then-construct shape has exactly one implementation.
pub fn resolve_client(project_root: &Path) -> Result<ApiClient, Box<dyn Error>> {
    let (server_url, token) = resolve_credentials(project_root);
    let server_url = server_url
        .ok_or("no server URL configured: set TRACEVAULT_SERVER_URL or run `tracevault login`")?;
    Ok(ApiClient::new(&server_url, token.as_deref()))
}
