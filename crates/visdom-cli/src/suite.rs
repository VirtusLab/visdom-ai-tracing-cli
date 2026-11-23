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

/// A small JSON file of answers remembered per address, so that using a
/// second installation does not forget the first one's.
fn load_remembered<T: for<'de> Deserialize<'de>>(path: &Path, address: &str) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut all: std::collections::BTreeMap<String, T> = serde_json::from_str(&text).ok()?;
    all.remove(&trim_slash(address.to_string()))
}

/// Best effort: an answer that cannot be written only means no fallback
/// later. Entries that no longer read (another version's) are dropped.
///
/// Sessions starting side by side each remember their own installation's
/// answer: reading, changing and writing happen under a lock, and the file is
/// replaced in one move, so neither loses the other's entry nor reads half a
/// file. An answer already remembered is not written again.
fn save_remembered<T: Serialize + for<'de> Deserialize<'de>>(path: &Path, address: &str, value: T) {
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    // Nothing is written without the lock: it would be over what another
    // session is writing.
    let Ok(_lock) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .and_then(|file| file.lock().map(|()| file))
    else {
        return;
    };
    let mut all: std::collections::BTreeMap<String, serde_json::Value> =
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
    all.retain(|_, entry| serde_json::from_value::<T>(entry.clone()).is_ok());
    let Ok(value) = serde_json::to_value(value) else {
        return;
    };
    let address = trim_slash(address.to_string());
    if all.get(&address) == Some(&value) {
        return;
    }
    all.insert(address, value);
    if let Ok(text) = serde_json::to_string(&all) {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, path))
            .is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Where the suite maps last read from tracing servers are kept, so that the
/// rest of the suite — and with it a workspace policy that requires the
/// gateway — is still found while a server cannot be asked.
pub fn suite_cache_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("suite.json")
}

/// The remembered suite map of `server_url`.
pub fn load_cached_suite(path: &Path, server_url: &str) -> Option<SuiteMap> {
    load_remembered::<SuiteMap>(path, server_url).map(SuiteMap::only_urls)
}

pub fn save_cached_suite(path: &Path, server_url: &str, map: &SuiteMap) {
    save_remembered(path, server_url, map.clone());
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

/// Where the policies last read from Control Planes are kept, so that an
/// enforced policy still holds while Control Plane cannot be reached.
pub fn policy_cache_path_in(config_root: &Path) -> PathBuf {
    config_root.join("visdom").join("policy.json")
}

/// The remembered policy of `control_plane_url`.
pub fn load_cached_policy(path: &Path, control_plane_url: &str) -> Option<TrafficPolicy> {
    load_remembered(path, control_plane_url)
}

pub fn save_cached_policy(path: &Path, control_plane_url: &str, policy: TrafficPolicy) {
    save_remembered(path, control_plane_url, policy);
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

    /// Sessions starting side by side, each with its own installation: every
    /// answer is kept, and the file never reads as anything but whole.
    #[test]
    fn answers_remembered_at_the_same_time_are_all_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = policy_cache_path_in(tmp.path());
        let address = |i: usize| format!("https://cp-{i}.example.com");
        std::thread::scope(|scope| {
            for i in 0..16 {
                let cache = &cache;
                scope.spawn(move || {
                    save_cached_policy(cache, &address(i), TrafficPolicy::Enforced);
                    let text = std::fs::read_to_string(cache).unwrap();
                    serde_json::from_str::<serde_json::Value>(&text).expect("a whole file");
                });
            }
        });
        for i in 0..16 {
            assert_eq!(
                load_cached_policy(&cache, &address(i)),
                Some(TrafficPolicy::Enforced),
                "{i}"
            );
        }
        let left: Vec<_> = std::fs::read_dir(cache.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains("tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn nothing_is_remembered_while_the_cache_cannot_be_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = policy_cache_path_in(tmp.path());
        save_cached_policy(&cache, "https://cp.example.com", TrafficPolicy::Enforced);
        // The lock file cannot be opened: a directory is in its place.
        std::fs::remove_file(cache.with_extension("lock")).unwrap();
        std::fs::create_dir(cache.with_extension("lock")).unwrap();
        save_cached_policy(&cache, "https://other.example.com", TrafficPolicy::Direct);
        assert_eq!(
            load_cached_policy(&cache, "https://cp.example.com"),
            Some(TrafficPolicy::Enforced)
        );
        assert_eq!(
            load_cached_policy(&cache, "https://other.example.com"),
            None
        );
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
        // Another installation's answer is kept beside it, not in its place.
        let other = SuiteMap {
            control_plane_url: Some("https://cp.other.example.com".to_string()),
            ..SuiteMap::default()
        };
        let elsewhere = "https://tracing.other.example.com";
        suite_map_or_remembered(&Ok(other.clone()), elsewhere, &cache);
        assert_eq!(suite_map_or_remembered(&down, server, &cache), map);
        assert_eq!(suite_map_or_remembered(&down, elsewhere, &cache), other);
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
