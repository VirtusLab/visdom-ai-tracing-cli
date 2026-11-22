//! Where the rest of the Visdom suite is, and what its workspace allows.
//!
//! `visdom` is told one address, the tracing server's. That server publishes
//! the others (its public config's suite map); Control Plane, found that way,
//! says how the workspace's LLM traffic must flow.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::components::bazaar::is_http_url;
use crate::mode::TrafficPolicy;

/// The suite's other products, as the tracing server publishes them. A
/// product that is not there (or not published) is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuiteMap {
    /// Control Plane's public URL.
    #[serde(default)]
    pub control_plane_url: Option<String>,
    /// The AI Gateway console's public URL.
    #[serde(default)]
    pub ai_gateway_url: Option<String>,
    /// The bazaar MCP endpoint.
    #[serde(default)]
    pub bazaar_mcp_url: Option<String>,
}

impl SuiteMap {
    /// Without the entries that are not plain http(s) URLs: what a server
    /// says is not trusted to be an address.
    fn only_urls(self) -> SuiteMap {
        let url = |v: Option<String>| v.filter(|u| is_http_url(u));
        SuiteMap {
            control_plane_url: url(self.control_plane_url).map(trim_slash),
            ai_gateway_url: url(self.ai_gateway_url).map(trim_slash),
            bazaar_mcp_url: url(self.bazaar_mcp_url),
        }
    }
}

fn trim_slash(url: String) -> String {
    url.trim_end_matches('/').to_string()
}

/// The suite map of the tracing server at `server_url`. A server that
/// publishes none (an older one, or one without SSO) gives an empty map;
/// `Err` (with the reason) only when it could not be asked.
pub async fn fetch_suite_map(http: &reqwest::Client, server_url: &str) -> Result<SuiteMap, String> {
    let url = format!(
        "{}/api/v1/auth/public-config",
        server_url.trim_end_matches('/')
    );
    let failed = |why: String| format!("{server_url} could not be asked for it: {why}");
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(format!("HTTP {}", response.status().as_u16())));
    }
    let map: SuiteMap = response
        .json()
        .await
        .map_err(|e| failed(format!("unreadable answer ({e})")))?;
    Ok(map.only_urls())
}

/// The suite map last read from a tracing server, kept so that the rest of
/// the suite — and with it a workspace policy that requires the gateway — is
/// still found while that server cannot be asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CachedSuite {
    server_url: String,
    map: SuiteMap,
}

pub fn suite_cache_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("suite.json")
}

/// The remembered suite map of `server_url`, if that is the server remembered.
pub fn load_cached_suite(path: &Path, server_url: &str) -> Option<SuiteMap> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached: CachedSuite = serde_json::from_str(&text).ok()?;
    (cached.server_url == trim_slash(server_url.to_string())).then(|| cached.map.only_urls())
}

/// Best effort: a map that cannot be written only means no fallback later.
pub fn save_cached_suite(path: &Path, server_url: &str, map: &SuiteMap) {
    let cached = CachedSuite {
        server_url: trim_slash(server_url.to_string()),
        map: map.clone(),
    };
    if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&cached)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(path, text);
    }
}

/// The suite map to work with, given what the tracing server at `server_url`
/// just said: its answer (which is then remembered), or, when it could not be
/// asked, the one it gave last time.
pub fn suite_map_or_remembered(
    published: &Result<SuiteMap, String>,
    server_url: &str,
    cache: &Path,
) -> SuiteMap {
    match published {
        Ok(map) => {
            save_cached_suite(cache, server_url, map);
            map.clone()
        }
        Err(_) => load_cached_suite(cache, server_url).unwrap_or_default(),
    }
}

/// The workspace's AI traffic policy, from Control Plane at `control_plane_url`
/// with the developer's own token. Any signed-in person may read it.
pub async fn fetch_policy(
    http: &reqwest::Client,
    control_plane_url: &str,
    token: &str,
) -> Result<TrafficPolicy, String> {
    #[derive(Deserialize)]
    struct Settings {
        ai_traffic_policy: TrafficPolicy,
    }
    let url = format!(
        "{}/api/v1/workspace/settings",
        control_plane_url.trim_end_matches('/')
    );
    let response = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Control Plane ({control_plane_url}) did not answer: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "Control Plane refused this login (HTTP {}); log in again with `visdom login`",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return Err(format!("Control Plane answered HTTP {}", status.as_u16()));
    }
    response
        .json::<Settings>()
        .await
        .map(|s| s.ai_traffic_policy)
        .map_err(|e| format!("Control Plane's settings could not be read ({e})"))
}

/// The policy last read from a Control Plane, kept so that an enforced policy
/// still holds while Control Plane cannot be reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPolicy {
    pub control_plane_url: String,
    pub policy: TrafficPolicy,
}

pub fn policy_cache_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("policy.json")
}

/// The cached policy of `control_plane_url`, if that is the one cached.
pub fn load_cached_policy(path: &Path, control_plane_url: &str) -> Option<TrafficPolicy> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached: CachedPolicy = serde_json::from_str(&text).ok()?;
    (cached.control_plane_url == control_plane_url).then_some(cached.policy)
}

/// Best effort: a cache that cannot be written only means asking again.
pub fn save_cached_policy(path: &Path, control_plane_url: &str, policy: TrafficPolicy) {
    let cached = CachedPolicy {
        control_plane_url: control_plane_url.to_string(),
        policy,
    };
    if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&cached)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(path, text);
    }
}

/// What is known about the workspace policy for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyState {
    /// Read from Control Plane just now.
    Fresh(TrafficPolicy),
    /// Control Plane could not be asked (the reason); this is its last answer.
    Remembered(TrafficPolicy, String),
    /// Not known, and why: no Control Plane, no login it accepts, no answer.
    Unknown(String),
}

impl PolicyState {
    pub fn policy(&self) -> Option<TrafficPolicy> {
        match self {
            PolicyState::Fresh(p) | PolicyState::Remembered(p, _) => Some(*p),
            PolicyState::Unknown(_) => None,
        }
    }
}

/// Read the policy, falling back to the remembered one, and remember a fresh
/// answer. `token` is the developer's Keycloak access token; `None` when they
/// have none (not logged in, or an API key, which is never sent elsewhere).
pub async fn resolve_policy(
    http: &reqwest::Client,
    control_plane_url: Option<&str>,
    token: Option<&str>,
    cache: &Path,
) -> PolicyState {
    let Some(cp) = control_plane_url else {
        return PolicyState::Unknown("this installation names no Control Plane".to_string());
    };
    let outcome = match token {
        Some(token) => fetch_policy(http, cp, token).await,
        None => Err("not logged in with a Visdom account".to_string()),
    };
    match outcome {
        Ok(policy) => {
            save_cached_policy(cache, cp, policy);
            PolicyState::Fresh(policy)
        }
        Err(why) => match load_cached_policy(cache, cp) {
            Some(policy) => PolicyState::Remembered(policy, why),
            None => PolicyState::Unknown(why),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::MockServer;

    const CONFIG: &str = "GET /api/v1/auth/public-config";
    const SETTINGS: &str = "GET /api/v1/workspace/settings";

    #[tokio::test]
    async fn the_suite_map_is_what_the_server_publishes() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            CONFIG,
            200,
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"c",
                "control_plane_url":"https://cp.example.com/",
                "ai_gateway_url":"https://gw.example.com",
                "bazaar_mcp_url":"https://ctx.example.com/mcp/"}"#,
        )]);
        assert_eq!(
            fetch_suite_map(&http, &format!("{}/", server.url)).await,
            Ok(SuiteMap {
                control_plane_url: Some("https://cp.example.com".to_string()),
                ai_gateway_url: Some("https://gw.example.com".to_string()),
                // An endpoint keeps its path as published.
                bazaar_mcp_url: Some("https://ctx.example.com/mcp/".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn a_server_without_a_suite_map_gives_an_empty_one() {
        let http = reqwest::Client::new();
        for body in [
            r#"{"oidc_enabled":false}"#,
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"c"}"#,
        ] {
            let server = MockServer::start(&[(CONFIG, 200, body)]);
            assert_eq!(
                fetch_suite_map(&http, &server.url).await,
                Ok(SuiteMap::default())
            );
        }
    }

    #[tokio::test]
    async fn what_is_not_an_http_url_is_dropped() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            CONFIG,
            200,
            r#"{"oidc_enabled":true,"control_plane_url":"javascript:alert(1)",
                "ai_gateway_url":"--flag","bazaar_mcp_url":"https://ctx.example.com/mcp"}"#,
        )]);
        assert_eq!(
            fetch_suite_map(&http, &server.url).await,
            Ok(SuiteMap {
                bazaar_mcp_url: Some("https://ctx.example.com/mcp".to_string()),
                ..SuiteMap::default()
            })
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_asked_is_an_error_with_the_reason() {
        let http = reqwest::Client::new();
        let err = fetch_suite_map(&http, "http://127.0.0.1:1")
            .await
            .unwrap_err();
        assert!(err.contains("http://127.0.0.1:1"), "{err}");
        let server = MockServer::start(&[(CONFIG, 500, "{}")]);
        let err = fetch_suite_map(&http, &server.url).await.unwrap_err();
        assert!(err.contains("HTTP 500"), "{err}");
    }

    #[tokio::test]
    async fn the_policy_is_read_with_the_persons_token() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(
            SETTINGS,
            200,
            r#"{"timezone":"Europe/Warsaw","ai_traffic_policy":"enforced"}"#,
        )]);
        assert_eq!(
            fetch_policy(&http, &server.url, "tok").await,
            Ok(TrafficPolicy::Enforced)
        );
        assert_eq!(
            server.requests()[0].authorization.as_deref(),
            Some("Bearer tok")
        );
    }

    #[tokio::test]
    async fn a_refused_login_and_a_broken_answer_are_told_apart() {
        let http = reqwest::Client::new();
        let server = MockServer::start(&[(SETTINGS, 401, "{}")]);
        let err = fetch_policy(&http, &server.url, "tok").await.unwrap_err();
        assert!(err.contains("log in again"), "{err}");
        let server = MockServer::start(&[(SETTINGS, 200, r#"{"timezone":"UTC"}"#)]);
        let err = fetch_policy(&http, &server.url, "tok").await.unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
        let server = MockServer::start(&[(SETTINGS, 200, r#"{"ai_traffic_policy":"new"}"#)]);
        assert!(fetch_policy(&http, &server.url, "tok").await.is_err());
    }

    #[tokio::test]
    async fn a_fresh_policy_is_remembered_for_when_control_plane_is_away() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let cache = policy_cache_path_in(tmp.path());
        let server = MockServer::start(&[
            (SETTINGS, 200, r#"{"ai_traffic_policy":"enforced"}"#),
            (SETTINGS, 503, "{}"),
        ]);
        let cp = Some(server.url.as_str());
        assert_eq!(
            resolve_policy(&http, cp, Some("tok"), &cache).await,
            PolicyState::Fresh(TrafficPolicy::Enforced)
        );
        // Control Plane is down: its last answer still holds.
        match resolve_policy(&http, cp, Some("tok"), &cache).await {
            PolicyState::Remembered(TrafficPolicy::Enforced, why) => {
                assert!(why.contains("HTTP 503"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        // Logged out: the same.
        assert_eq!(
            resolve_policy(&http, cp, None, &cache).await.policy(),
            Some(TrafficPolicy::Enforced)
        );
        // Another Control Plane's policy was never read.
        match resolve_policy(&http, Some("http://127.0.0.1:1"), Some("tok"), &cache).await {
            PolicyState::Unknown(_) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_server_that_cannot_be_asked_leaves_its_last_suite_map() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = suite_cache_path_in(tmp.path());
        let server = "https://tracing.example.com";
        let down = Err("no answer".to_string());
        // Never asked successfully: nothing to fall back to.
        assert_eq!(
            suite_map_or_remembered(&down, server, &cache),
            SuiteMap::default()
        );
        let map = SuiteMap {
            control_plane_url: Some("https://cp.example.com".to_string()),
            ai_gateway_url: Some("https://gw.example.com".to_string()),
            bazaar_mcp_url: None,
        };
        assert_eq!(
            suite_map_or_remembered(&Ok(map.clone()), server, &cache),
            map
        );
        // Now it is away: what it said last holds, for that server only.
        assert_eq!(suite_map_or_remembered(&down, server, &cache), map);
        assert_eq!(
            suite_map_or_remembered(&down, &format!("{server}/"), &cache),
            map
        );
        assert_eq!(
            suite_map_or_remembered(&down, "https://other.example.com", &cache),
            SuiteMap::default()
        );
        // A later answer replaces it, an empty one included.
        suite_map_or_remembered(&Ok(SuiteMap::default()), server, &cache);
        assert_eq!(
            suite_map_or_remembered(&down, server, &cache),
            SuiteMap::default()
        );
    }

    #[tokio::test]
    async fn no_control_plane_means_no_policy() {
        let http = reqwest::Client::new();
        let tmp = tempfile::tempdir().unwrap();
        let state = resolve_policy(&http, None, Some("tok"), &tmp.path().join("p.json")).await;
        assert_eq!(state.policy(), None);
        assert!(matches!(state, PolicyState::Unknown(_)));
    }
}
