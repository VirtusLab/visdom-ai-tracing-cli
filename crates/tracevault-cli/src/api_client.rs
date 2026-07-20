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

#[derive(Deserialize)]
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

/// One org the authenticated credential belongs to. `org_name` is the org
/// slug (`orgs.name` server-side) used in URL paths; `display_name` is the
/// human label. Wire shape of `GET /api/v1/me/orgs`.
// Full GET /api/v1/me/orgs wire shape; only org_name (the slug) is consumed.
// Unused fields kept for the contract, allowed like MeResponse::user_id.
#[derive(Debug, Deserialize)]
pub struct OrgMembership {
    #[allow(dead_code)]
    pub org_id: uuid::Uuid,
    pub org_name: String,
    #[allow(dead_code)]
    pub display_name: Option<String>,
    #[allow(dead_code)]
    pub role: String,
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

/// One project in `GET /api/v1/orgs/{org}/projects`. Server sends more
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
        org_slug: &str,
        req: RegisterRepoRequest,
    ) -> Result<RegisterRepoResponse, Box<dyn Error>> {
        let mut builder = self
            .client
            .post(format!("{}/api/v1/orgs/{}/repos", self.base_url, org_slug));

        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.json(&req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Server returned {status}: {body}").into());
        }

        Ok(resp.json().await?)
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

    /// GET `{base}{path}` with the bearer token, mapping failures into
    /// `GetMeError` (401 → `Unauthorized`, transport → `Network`, other
    /// non-2xx or bad JSON → `Server`). Shared by the credential-scoped GETs
    /// so auth/error handling lives in one place.
    async fn authed_get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, GetMeError> {
        let mut builder = self.client.get(format!("{}{}", self.base_url, path));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

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

    /// List the orgs the authenticated credential belongs to.
    /// `GET /api/v1/me/orgs`. For a service-account key this is the service
    /// user's memberships; for a user session, the user's orgs; for an
    /// org-scoped key, an empty list.
    pub async fn list_my_orgs(&self) -> Result<Vec<OrgMembership>, GetMeError> {
        self.authed_get_json("/api/v1/me/orgs").await
    }

    pub async fn list_repos(&self, org_slug: &str) -> Result<Vec<RepoListItem>, Box<dyn Error>> {
        let mut builder = self
            .client
            .get(format!("{}/api/v1/orgs/{}/repos", self.base_url, org_slug));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to list repos ({status}): {body}").into());
        }

        let repos: Vec<RepoListItem> = resp.json().await?;
        Ok(repos)
    }

    pub async fn get_agent_instructions(
        &self,
        org_slug: &str,
        repo_id: &uuid::Uuid,
    ) -> Result<AgentInstructionsResponse, Box<dyn Error>> {
        let mut builder = self.client.get(format!(
            "{}/api/v1/orgs/{}/repos/{}/policies/agent-instructions",
            self.base_url, org_slug, repo_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to fetch agent instructions ({status}): {body}").into());
        }
        Ok(resp.json().await?)
    }

    pub async fn verify_commits(
        &self,
        org_slug: &str,
        repo_id: &uuid::Uuid,
        req: CiVerifyRequest,
    ) -> Result<CiVerifyResponse, Box<dyn Error>> {
        let mut builder = self.client.post(format!(
            "{}/api/v1/orgs/{}/repos/{}/ci/verify",
            self.base_url, org_slug, repo_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.json(&req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("CI verify failed ({status}): {body}").into());
        }

        Ok(resp.json().await?)
    }

    pub async fn push_commit(
        &self,
        org_slug: &str,
        repo_id: &str,
        req: &tracevault_protocol::streaming::CommitPushRequest,
    ) -> Result<tracevault_protocol::streaming::CommitPushResponse, Box<dyn Error>> {
        let mut builder = self.client.post(format!(
            "{}/api/v1/orgs/{}/repos/{}/commits",
            self.base_url, org_slug, repo_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.json(req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Commit push failed ({status}): {body}").into());
        }
        Ok(resp.json().await?)
    }

    pub async fn stream_event(
        &self,
        org_slug: &str,
        repo_id: &str,
        req: &tracevault_protocol::streaming::StreamEventRequest,
    ) -> Result<tracevault_protocol::streaming::StreamEventResponse, Box<dyn Error>> {
        let mut builder = self.client.post(format!(
            "{}/api/v1/orgs/{}/repos/{}/stream",
            self.base_url, org_slug, repo_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.json(req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Stream failed ({status}): {body}").into());
        }
        Ok(resp.json().await?)
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
        org_slug: &str,
        project_id: uuid::Uuid,
        repo_id: &str,
        req: &tracevault_protocol::streaming::StreamEventRequest,
    ) -> Result<tracevault_protocol::streaming::StreamEventResponse, Box<dyn Error>> {
        let mut url = Url::parse(&format!(
            "{}/api/v1/orgs/{}/projects/{}/stream",
            self.base_url, org_slug, project_id
        ))?;
        url.query_pairs_mut().append_pair("repo_id", repo_id);
        let mut builder = self.client.post(url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.json(req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("stream_event_for_project failed ({status}): {body}").into());
        }
        Ok(resp.json().await?)
    }

    pub async fn check_policies(
        &self,
        org_slug: &str,
        repo_id: &uuid::Uuid,
        req: CheckPoliciesRequest,
    ) -> Result<CheckPoliciesResponse, Box<dyn Error>> {
        let mut builder = self.client.post(format!(
            "{}/api/v1/orgs/{}/repos/{}/policies/check",
            self.base_url, org_slug, repo_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.json(&req).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Policy check failed ({status}): {body}").into());
        }

        let result: CheckPoliciesResponse = resp.json().await?;
        Ok(result)
    }

    /// Resolve a git URL to its codebase (git remote) by NORMALIZED URL —
    /// deduped, unlike an exact `github_url` match. `Ok(None)` if the
    /// codebase isn't tracked (404).
    pub async fn resolve_remote(
        &self,
        org_slug: &str,
        git_url: &str,
    ) -> Result<Option<ResolveRemoteResponse>, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!(
            "{}/api/v1/orgs/{}/remotes/resolve",
            self.base_url, org_slug
        ))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let mut builder = self.client.get(url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("resolve_remote failed ({status}): {body}").into());
        }
        let parsed: ResolveRemoteResponse = resp.json().await?;
        Ok(Some(parsed))
    }

    /// Full detail for a remote (codebase): its display name, normalized URL,
    /// clone status, and linked repos.
    pub async fn get_remote_detail(
        &self,
        org_slug: &str,
        remote_id: uuid::Uuid,
    ) -> Result<RemoteDetail, Box<dyn std::error::Error>> {
        let mut builder = self.client.get(format!(
            "{}/api/v1/orgs/{}/remotes/{}",
            self.base_url, org_slug, remote_id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("get_remote_detail failed ({status}): {body}").into());
        }
        let detail: RemoteDetail = resp.json().await?;
        Ok(detail)
    }

    /// The repos linked to a remote (the codebase's members).
    pub async fn get_remote_repos(
        &self,
        org_slug: &str,
        remote_id: uuid::Uuid,
    ) -> Result<Vec<RemoteRepoRef>, Box<dyn std::error::Error>> {
        Ok(self.get_remote_detail(org_slug, remote_id).await?.repos)
    }

    /// List the projects in an org. `GET /api/v1/orgs/{org}/projects`.
    pub async fn list_projects(
        &self,
        org_slug: &str,
    ) -> Result<Vec<ProjectListItem>, Box<dyn Error>> {
        let mut builder = self.client.get(format!(
            "{}/api/v1/orgs/{}/projects",
            self.base_url, org_slug
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to list projects ({status}): {body}").into());
        }

        let projects: Vec<ProjectListItem> = resp.json().await?;
        Ok(projects)
    }

    /// Full detail for a project. `GET /api/v1/orgs/{org}/projects/{id}`.
    pub async fn get_project(
        &self,
        org_slug: &str,
        id: uuid::Uuid,
    ) -> Result<ProjectDetail, Box<dyn Error>> {
        let mut builder = self.client.get(format!(
            "{}/api/v1/orgs/{}/projects/{}",
            self.base_url, org_slug, id
        ));
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to get project ({status}): {body}").into());
        }

        let detail: ProjectDetail = resp.json().await?;
        Ok(detail)
    }

    /// Resolve a git URL to its project, distinguishing "no project" (404)
    /// from "ambiguous, multiple candidate projects" (409).
    /// `GET /api/v1/orgs/{org}/projects/resolve?git_url=`.
    pub async fn resolve_project(
        &self,
        org_slug: &str,
        git_url: &str,
    ) -> Result<ResolveProjectOutcome, Box<dyn std::error::Error>> {
        let mut url = Url::parse(&format!(
            "{}/api/v1/orgs/{}/projects/resolve",
            self.base_url, org_slug
        ))?;
        url.query_pairs_mut().append_pair("git_url", git_url);
        let mut builder = self.client.get(url);
        if let Some(key) = &self.api_key {
            builder = builder.header("Authorization", format!("Bearer {key}"));
        }
        let resp = builder.send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(ResolveProjectOutcome::None);
        }
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(ResolveProjectOutcome::Ambiguous);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("resolve_project failed ({status}): {body}").into());
        }
        let parsed: ResolveProjectResponse = resp.json().await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_membership_deserializes_and_exposes_slug() {
        let body = r#"[
            {"org_id":"00000000-0000-0000-0000-000000000001","org_name":"acme","display_name":"Acme Inc","role":"admin"},
            {"org_id":"00000000-0000-0000-0000-000000000002","org_name":"globex","display_name":null,"role":"member"}
        ]"#;
        let orgs: Vec<OrgMembership> = serde_json::from_str(body).unwrap();
        assert_eq!(orgs.len(), 2);
        // org_name is the slug used in URL paths.
        assert_eq!(orgs[0].org_name, "acme");
        assert_eq!(orgs[1].org_name, "globex");
        assert_eq!(orgs[1].display_name, None);
    }
}
