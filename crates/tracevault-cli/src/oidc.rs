//! OIDC / OAuth 2.0 plumbing for interactive (human) login.
//!
//! The CLI authenticates humans with Keycloak's RFC 8628 device
//! authorization grant: it asks the TraceVault server which realm to talk to
//! (`/api/v1/auth/public-config`), discovers the realm's endpoints, starts a
//! device authorization, polls the token endpoint until the user finishes in
//! the browser, and thereafter keeps the access token fresh with the refresh
//! token. The TraceVault server itself is not part of the token exchange —
//! it only publishes the issuer and client id.
//!
//! Everything here is transport + protocol only: no printing, no credential
//! file access, no command-layer concerns. That keeps the whole module
//! testable against a raw one-shot HTTP server.
//!
//! ## Secret hygiene
//!
//! `device_code`, `access_token` and `refresh_token` are secrets. [`DeviceAuth`]
//! and [`TokenSet`] therefore implement `Debug` **by hand**, redacting those
//! fields, so no `{:?}` anywhere (including a panicking `unwrap()` inside a
//! test) can leak them into a log. For the same reason no error variant in
//! [`OidcError`] carries a response body: an IdP error body is not supposed
//! to contain a token, but a *success* body always does, and one shared
//! "here is the body" error path would eventually be reached from a success
//! response that failed to deserialize.

use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// Scopes requested for the device flow. `offline_access` is what makes
/// Keycloak issue a long-lived refresh token, which is the whole point of
/// this flow for a CLI: unattended git hooks must never prompt.
pub const LOGIN_SCOPE: &str = "openid profile email offline_access";

/// RFC 8628 grant type for exchanging a device code for tokens.
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Extra seconds added to the poll interval each time the IdP answers
/// `slow_down` (RFC 8628 §3.5 specifies exactly 5).
const SLOW_DOWN_INCREMENT_SECS: u64 = 5;

/// Fallback poll interval when the device authorization response omits
/// `interval` (RFC 8628 §3.2 default).
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// What the TraceVault server publishes about its identity provider.
/// Deliberately minimal: the CLI needs the realm issuer and the public
/// client id registered for it, nothing else.
#[derive(Debug, Clone, Deserialize)]
pub struct PublicConfig {
    pub issuer: String,
    pub cli_client_id: String,
}

/// The subset of the realm's OpenID discovery document the CLI uses.
///
/// `device_authorization_endpoint` and `revocation_endpoint` are optional
/// because a non-Keycloak (or ancient Keycloak) issuer may not advertise
/// them; the operations that need them fail with a specific message rather
/// than discovery failing wholesale, so `tv logout` still works against an
/// issuer with no revocation endpoint.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub device_authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
}

/// Wire shape of the discovery document. `issuer` is read back so we can
/// verify it matches the issuer we were told to trust.
#[derive(Deserialize)]
struct DiscoveryDoc {
    issuer: String,
    #[serde(default)]
    device_authorization_endpoint: Option<String>,
    token_endpoint: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

/// A started device authorization (RFC 8628 §3.2).
#[derive(Clone, Deserialize)]
pub struct DeviceAuth {
    /// SECRET. Never print this — it is the bearer of the pending login.
    pub device_code: String,
    /// The short code the human types into the browser. This is the one
    /// value in the whole flow that MUST be shown to the user.
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_device_expires_in")]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_device_expires_in() -> u64 {
    600
}

fn default_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

/// Hand-written so `device_code` cannot leak through `{:?}`.
impl fmt::Debug for DeviceAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceAuth")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field(
                "verification_uri_complete",
                &self.verification_uri_complete.is_some(),
            )
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// A successful token response. `expires_in` is relative seconds as sent by
/// the IdP; callers turn it into an absolute instant via
/// [`TokenSet::access_expires_at`].
///
/// `refresh_token` is optional because a token endpoint is allowed to omit
/// it on a refresh (no rotation); callers keep the previous one in that case.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    /// SECRET.
    pub access_token: String,
    /// SECRET.
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: i64,
}

impl TokenSet {
    /// Absolute unix-seconds expiry of `access_token`, given the current
    /// time. `now` is a parameter (not read from the clock here) so callers
    /// and tests share one deterministic notion of "now".
    pub fn access_expires_at(&self, now: i64) -> i64 {
        now.saturating_add(self.expires_in)
    }
}

/// Hand-written so neither token can leak through `{:?}`.
impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Wire shape of an OAuth error response (RFC 6749 §5.2).
#[derive(Deserialize)]
struct OauthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Wire shape of a TraceVault error envelope, as far as this module cares:
/// the machine-readable `code` that tells one 503 from another.
#[derive(Deserialize)]
struct ServerErrorBody {
    #[serde(default)]
    code: Option<String>,
}

/// The `code` the TraceVault server sends with its `public-config` 503 when
/// it has no Keycloak configured at all. Any OTHER 503 from that endpoint is
/// a transient IdP/JWKS outage and is retryable — see
/// [`fetch_public_config`].
const CODE_OIDC_NOT_CONFIGURED: &str = "oidc_not_configured";

/// Failures of the login/refresh machinery. Every variant that a caller has
/// to *behave* differently about is its own variant; anything else collapses
/// into [`OidcError::Oauth`] (the IdP said no, with a code) or
/// [`OidcError::Transport`] (we never got a usable answer).
#[derive(Debug)]
pub enum OidcError {
    /// `/auth/public-config` answered 503 with `code: "oidc_not_configured"`:
    /// this TraceVault instance has no Keycloak at all, so interactive login
    /// is impossible and an API key must be used instead. Permanent until an
    /// operator changes the deployment.
    NoKeycloak,
    /// `/auth/public-config` answered 503 for some OTHER reason — the server
    /// has a Keycloak but currently can't talk to it (JWKS fetch failing, IdP
    /// restarting). Retryable, and explicitly NOT a reason to go get an API
    /// key.
    IdpUnavailable { detail: String },
    /// The discovery document's own `issuer` disagrees with the issuer the
    /// server told us to use — a misconfiguration or a redirect to a
    /// different realm. Both values are named so it is diagnosable.
    IssuerMismatch { expected: String, found: String },
    /// The refresh token is no longer accepted (`invalid_grant`).
    SessionExpired,
    /// The user rejected the login in the browser.
    AccessDenied,
    /// The device code expired before the user finished.
    DeviceCodeExpired,
    /// We polled for the full `expires_in` budget without a verdict.
    PollTimeout { waited_secs: u64 },
    /// The issuer does not advertise an endpoint this operation needs.
    EndpointUnavailable { what: &'static str },
    /// An OAuth error code we don't special-case.
    Oauth {
        error: String,
        description: Option<String>,
    },
    /// Network/HTTP-level failure, or a response we could not parse.
    Transport(String),
}

impl fmt::Display for OidcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoKeycloak => write!(
                f,
                "this TraceVault server has no Keycloak/SSO configured, so interactive login is \
                 unavailable"
            ),
            Self::IdpUnavailable { detail } => write!(
                f,
                "the identity provider is temporarily unavailable ({detail}) — this is not a \
                 configuration problem; retry `tracevault login` in a moment"
            ),
            Self::IssuerMismatch { expected, found } => write!(
                f,
                "issuer mismatch: the server said the issuer is '{expected}', but its discovery \
                 document identifies itself as '{found}'"
            ),
            Self::SessionExpired => write!(
                f,
                "your session has expired — run `tracevault login` to sign in again"
            ),
            Self::AccessDenied => write!(f, "the login was denied in the browser (access_denied)"),
            Self::DeviceCodeExpired => write!(
                f,
                "the login code expired before it was approved — run `tracevault login` again"
            ),
            Self::PollTimeout { waited_secs } => write!(
                f,
                "gave up waiting for browser approval after {waited_secs}s — run \
                 `tracevault login` again"
            ),
            Self::EndpointUnavailable { what } => write!(
                f,
                "the identity provider does not advertise a {what} endpoint"
            ),
            Self::Oauth { error, description } => match description {
                Some(d) => write!(f, "identity provider rejected the request: {error} ({d})"),
                None => write!(f, "identity provider rejected the request: {error}"),
            },
            Self::Transport(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for OidcError {}

/// Canonical form of an issuer URL for comparison: no surrounding
/// whitespace, no trailing slashes. Both the configured issuer and the one
/// the discovery document self-reports go through this before being
/// compared, so `https://x/realms/v` and `https://x/realms/v/` match.
pub fn canonical_issuer(issuer: &str) -> String {
    issuer.trim().trim_end_matches('/').to_string()
}

/// Ask the TraceVault server which realm to authenticate against.
///
/// The endpoint uses 503 for TWO different situations, and they must not be
/// conflated:
///
/// * `code: "oidc_not_configured"` → the instance has no Keycloak at all.
///   Permanent; the user needs a `tvk_` API key ([`OidcError::NoKeycloak`]).
/// * any other/absent `code` → the server has Keycloak but can't reach it
///   right now (JWKS fetch failing, IdP restarting). Retryable
///   ([`OidcError::IdpUnavailable`]).
///
/// Keying on the status alone would tell a user in the middle of a 30-second
/// Keycloak restart to go get an API key — misleading and unactionable. Body
/// parsing is deliberately tolerant: an empty or unparseable 503 body falls
/// into the retryable branch, because "no Keycloak configured" is the
/// stronger claim and must be positively evidenced by the `code`.
pub async fn fetch_public_config(
    client: &reqwest::Client,
    server_url: &str,
) -> Result<PublicConfig, OidcError> {
    let url = format!(
        "{}/api/v1/auth/public-config",
        server_url.trim_end_matches('/')
    );
    let resp = client.get(&url).send().await.map_err(transport)?;
    let status = resp.status();

    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        let body = resp.text().await.unwrap_or_default();
        let code = serde_json::from_str::<ServerErrorBody>(&body)
            .ok()
            .and_then(|b| b.code);
        return Err(match code.as_deref() {
            Some(CODE_OIDC_NOT_CONFIGURED) => OidcError::NoKeycloak,
            Some(other) => OidcError::IdpUnavailable {
                detail: format!("server returned 503 {other}"),
            },
            None => OidcError::IdpUnavailable {
                detail: "server returned 503 with no error code".to_string(),
            },
        });
    }

    if !status.is_success() {
        return Err(OidcError::Transport(format!("GET {url} returned {status}")));
    }
    resp.json::<PublicConfig>()
        .await
        .map_err(|e| OidcError::Transport(format!("malformed auth public-config response: {e}")))
}

/// Fetch and validate the realm's OpenID discovery document.
///
/// The document's self-reported `issuer` must match `issuer` (both
/// canonicalised). Skipping that check would let a redirect or a
/// copy-pasted wrong realm silently mint tokens the TraceVault server will
/// reject, with no clue why.
pub async fn discover(client: &reqwest::Client, issuer: &str) -> Result<Discovery, OidcError> {
    let expected = canonical_issuer(issuer);
    let url = format!("{expected}/.well-known/openid-configuration");
    let resp = client.get(&url).send().await.map_err(transport)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(OidcError::Transport(format!("GET {url} returned {status}")));
    }
    let doc: DiscoveryDoc = resp
        .json()
        .await
        .map_err(|e| OidcError::Transport(format!("malformed discovery document: {e}")))?;

    let found = canonical_issuer(&doc.issuer);
    if found != expected {
        return Err(OidcError::IssuerMismatch { expected, found });
    }

    Ok(Discovery {
        device_authorization_endpoint: doc.device_authorization_endpoint,
        token_endpoint: doc.token_endpoint,
        revocation_endpoint: doc.revocation_endpoint,
    })
}

/// Start a device authorization (RFC 8628 §3.1). `tracevault-cli` is a
/// PUBLIC client, so the form carries `client_id` and no client secret.
pub async fn device_start(
    client: &reqwest::Client,
    discovery: &Discovery,
    client_id: &str,
) -> Result<DeviceAuth, OidcError> {
    let endpoint = discovery.device_authorization_endpoint.as_deref().ok_or(
        OidcError::EndpointUnavailable {
            what: "device authorization",
        },
    )?;
    let resp = post_form(
        client,
        endpoint,
        &[("client_id", client_id), ("scope", LOGIN_SCOPE)],
    )
    .send()
    .await
    .map_err(transport)?;
    let status = resp.status();
    if !status.is_success() {
        // Read the body only to extract an OAuth error code; the body of a
        // FAILED device-authorization response holds no secret.
        let body = resp.text().await.unwrap_or_default();
        return Err(
            oauth_error_from_body(&body).unwrap_or(OidcError::Transport(format!(
                "device authorization request returned {status}"
            ))),
        );
    }
    resp.json::<DeviceAuth>()
        .await
        .map_err(|e| OidcError::Transport(format!("malformed device authorization response: {e}")))
}

/// Poll the token endpoint until the user approves (or the attempt dies),
/// sleeping with `tokio::time::sleep` between attempts.
pub async fn poll_token(
    client: &reqwest::Client,
    discovery: &Discovery,
    client_id: &str,
    device: &DeviceAuth,
) -> Result<TokenSet, OidcError> {
    poll_token_with(client, discovery, client_id, device, tokio::time::sleep).await
}

/// [`poll_token`] with the sleep injected.
///
/// Two reasons this exists: tests must not burn wall-clock seconds, and the
/// elapsed budget is accumulated from the sleep durations rather than read
/// off a clock — so "give up after `expires_in`" is exercised deterministically
/// by a no-op sleep, and a `slow_down` genuinely shortens the number of
/// remaining attempts.
pub async fn poll_token_with<S, F>(
    client: &reqwest::Client,
    discovery: &Discovery,
    client_id: &str,
    device: &DeviceAuth,
    mut sleep: S,
) -> Result<TokenSet, OidcError>
where
    S: FnMut(Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    let mut interval = if device.interval == 0 {
        DEFAULT_POLL_INTERVAL_SECS
    } else {
        device.interval
    };
    let mut waited: u64 = 0;

    loop {
        sleep(Duration::from_secs(interval)).await;
        waited = waited.saturating_add(interval);

        let attempt = token_request(
            client,
            &discovery.token_endpoint,
            &[
                ("grant_type", DEVICE_CODE_GRANT),
                ("client_id", client_id),
                ("device_code", &device.device_code),
            ],
        )
        .await;

        match attempt {
            Ok(tokens) => return Ok(tokens),
            Err(OidcError::Oauth { error, description }) => match error.as_str() {
                // Still waiting on the human.
                "authorization_pending" => {}
                // RFC 8628 §3.5: back off permanently, by exactly 5s.
                "slow_down" => interval = interval.saturating_add(SLOW_DOWN_INCREMENT_SECS),
                "access_denied" => return Err(OidcError::AccessDenied),
                "expired_token" => return Err(OidcError::DeviceCodeExpired),
                _ => return Err(OidcError::Oauth { error, description }),
            },
            // Anything else (transport, 5xx, unparseable) is terminal: the
            // loop must not be able to spin forever on a broken endpoint.
            Err(e) => return Err(e),
        }

        // Budget check AFTER handling the attempt, so a login approved on
        // the very last allowed poll still succeeds.
        if waited >= device.expires_in {
            return Err(OidcError::PollTimeout {
                waited_secs: waited,
            });
        }
    }
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(
    client: &reqwest::Client,
    discovery: &Discovery,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet, OidcError> {
    let result = token_request(
        client,
        &discovery.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ],
    )
    .await;

    match result {
        // `invalid_grant` on a refresh means the session is gone for good
        // (expired, revoked, or the user's account was disabled) — a retry
        // will never help, so it gets its own "run login again" message.
        Err(OidcError::Oauth { error, .. }) if error == "invalid_grant" => {
            Err(OidcError::SessionExpired)
        }
        other => other,
    }
}

/// Best-effort revocation of a refresh token (RFC 7009). Called by
/// `tv logout`; a failure there is a warning, not an error, because the
/// local credential is removed regardless.
pub async fn revoke(
    client: &reqwest::Client,
    discovery: &Discovery,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), OidcError> {
    let endpoint = discovery
        .revocation_endpoint
        .as_deref()
        .ok_or(OidcError::EndpointUnavailable { what: "revocation" })?;
    let resp = post_form(
        client,
        endpoint,
        &[
            ("client_id", client_id),
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
        ],
    )
    .send()
    .await
    .map_err(transport)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(OidcError::Transport(format!(
            "revocation endpoint returned {status}"
        )));
    }
    Ok(())
}

/// POST a form to the token endpoint and interpret the answer.
///
/// OAuth error codes are read from the BODY, not inferred from the status:
/// RFC 8628's `authorization_pending`/`slow_down` both arrive as HTTP 400,
/// so a status-only reading of the response would turn "keep waiting" into
/// a hard failure.
async fn token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet, OidcError> {
    let resp = post_form(client, token_endpoint, form)
        .send()
        .await
        .map_err(transport)?;
    let status = resp.status();

    if status.is_success() {
        // NOTE: on a deserialize failure the body is deliberately NOT
        // included in the error — a 200 from the token endpoint contains
        // live tokens.
        return resp
            .json::<TokenSet>()
            .await
            .map_err(|e| OidcError::Transport(format!("malformed token response: {e}")));
    }

    let body = resp.text().await.unwrap_or_default();
    Err(
        oauth_error_from_body(&body).unwrap_or(OidcError::Transport(format!(
            "token endpoint returned {status}"
        ))),
    )
}

/// Parse an RFC 6749 §5.2 error body into [`OidcError::Oauth`], or `None`
/// when the body isn't one.
fn oauth_error_from_body(body: &str) -> Option<OidcError> {
    serde_json::from_str::<OauthErrorBody>(body)
        .ok()
        .map(|b| OidcError::Oauth {
            error: b.error,
            description: b.error_description,
        })
}

fn transport(e: reqwest::Error) -> OidcError {
    OidcError::Transport(format!("network error: {e}"))
}

/// Build an `application/x-www-form-urlencoded` POST.
///
/// Hand-encoded via `url::form_urlencoded` rather than `RequestBuilder::form`
/// because reqwest 0.13 puts `form()` behind its `form` feature, which would
/// pull `serde_urlencoded` into the dependency tree. `url` is already a
/// direct dependency and does exactly the same percent-encoding, so this
/// keeps the lockfile untouched.
fn post_form(
    client: &reqwest::Client,
    url: &str,
    pairs: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    client
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{http_json, spawn_seq, RECV_TIMEOUT};
    use std::sync::{Arc, Mutex};

    /// A `Discovery` whose token endpoint points at `base`.
    fn discovery_at(base: &str) -> Discovery {
        Discovery {
            device_authorization_endpoint: Some(format!("{base}/device")),
            token_endpoint: format!("{base}/token"),
            revocation_endpoint: Some(format!("{base}/revoke")),
        }
    }

    fn device_auth(interval: u64, expires_in: u64) -> DeviceAuth {
        DeviceAuth {
            device_code: "dev-code".into(),
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://idp.test/device".into(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    /// A sleep that records what it was asked to wait and returns instantly,
    /// so poll-loop timing is asserted without any wall-clock cost.
    fn recording_sleep(
        log: Arc<Mutex<Vec<u64>>>,
    ) -> impl FnMut(Duration) -> std::future::Ready<()> {
        move |d| {
            log.lock().unwrap().push(d.as_secs());
            std::future::ready(())
        }
    }

    #[tokio::test]
    async fn poll_retries_on_authorization_pending_then_succeeds() {
        let (base, rx) = spawn_seq(vec![
            http_json("400 Bad Request", r#"{"error":"authorization_pending"}"#),
            http_json(
                "200 OK",
                r#"{"access_token":"at","refresh_token":"rt","expires_in":300}"#,
            ),
        ]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = reqwest::Client::new();
        let tokens = poll_token_with(
            &client,
            &discovery_at(&base),
            "tracevault-cli",
            &device_auth(5, 600),
            recording_sleep(log.clone()),
        )
        .await
        .expect("a pending poll followed by success must succeed");

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        // Polled exactly twice, and both polls used the device_code grant.
        let first = rx.recv_timeout(RECV_TIMEOUT).expect("no first poll");
        let second = rx.recv_timeout(RECV_TIMEOUT).expect("no second poll");
        for req in [&first, &second] {
            assert!(req.contains("POST /token"), "unexpected request: {req}");
            assert!(
                req.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
                "poll must use the device_code grant: {req}"
            );
            assert!(req.contains("device_code=dev-code"), "missing device_code");
            assert!(
                req.contains("client_id=tracevault-cli"),
                "missing client_id"
            );
            assert!(
                !req.contains("client_secret"),
                "tracevault-cli is a PUBLIC client; no secret may be sent: {req}"
            );
        }
        assert_eq!(*log.lock().unwrap(), vec![5, 5]);
    }

    #[tokio::test]
    async fn slow_down_raises_interval_by_five_seconds() {
        let (base, _rx) = spawn_seq(vec![
            http_json("400 Bad Request", r#"{"error":"slow_down"}"#),
            http_json("400 Bad Request", r#"{"error":"authorization_pending"}"#),
            http_json("200 OK", r#"{"access_token":"at","expires_in":300}"#),
        ]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = reqwest::Client::new();
        poll_token_with(
            &client,
            &discovery_at(&base),
            "tracevault-cli",
            &device_auth(5, 600),
            recording_sleep(log.clone()),
        )
        .await
        .expect("slow_down must not be fatal");

        // First wait uses the advertised interval; every wait after the
        // slow_down is permanently +5s.
        assert_eq!(*log.lock().unwrap(), vec![5, 10, 10]);
    }

    #[tokio::test]
    async fn expired_token_is_terminal_with_its_own_message() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "400 Bad Request",
            r#"{"error":"expired_token"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = poll_token_with(
            &client,
            &discovery_at(&base),
            "tracevault-cli",
            &device_auth(5, 600),
            recording_sleep(Arc::new(Mutex::new(Vec::new()))),
        )
        .await
        .expect_err("expired_token must be terminal");

        assert!(matches!(err, OidcError::DeviceCodeExpired), "got {err:?}");
        assert!(
            err.to_string().contains("expired"),
            "message must say the code expired: {err}"
        );
        assert!(
            !err.to_string().contains("denied"),
            "expired must not read like a denial: {err}"
        );
    }

    #[tokio::test]
    async fn access_denied_is_terminal_with_its_own_message() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "400 Bad Request",
            r#"{"error":"access_denied"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = poll_token_with(
            &client,
            &discovery_at(&base),
            "tracevault-cli",
            &device_auth(5, 600),
            recording_sleep(Arc::new(Mutex::new(Vec::new()))),
        )
        .await
        .expect_err("access_denied must be terminal");

        assert!(matches!(err, OidcError::AccessDenied), "got {err:?}");
        assert!(
            err.to_string().contains("denied"),
            "message must say the login was denied: {err}"
        );
        assert!(
            !err.to_string().contains("expired"),
            "denial must not read like an expiry: {err}"
        );
    }

    #[tokio::test]
    async fn poll_gives_up_after_the_device_codes_own_expiry() {
        // expires_in=10 with interval=5 allows exactly two polls; a third
        // response is provided to prove it is never requested.
        let pending = || http_json("400 Bad Request", r#"{"error":"authorization_pending"}"#);
        let (base, rx) = spawn_seq(vec![pending(), pending(), pending()]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let client = reqwest::Client::new();
        let err = poll_token_with(
            &client,
            &discovery_at(&base),
            "tracevault-cli",
            &device_auth(5, 10),
            recording_sleep(log.clone()),
        )
        .await
        .expect_err("must give up once the device code's own lifetime is spent");

        assert!(
            matches!(err, OidcError::PollTimeout { waited_secs: 10 }),
            "got {err:?}"
        );
        assert_eq!(*log.lock().unwrap(), vec![5, 5]);
        assert!(rx.recv_timeout(RECV_TIMEOUT).is_ok());
        assert!(rx.recv_timeout(RECV_TIMEOUT).is_ok());
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the budget must cap polling; a third poll went out"
        );
    }

    #[tokio::test]
    async fn discovery_rejects_mismatched_issuer_naming_both_values() {
        // The document self-reports a DIFFERENT realm than the one we asked
        // for; the mismatch must be refused rather than silently followed.
        let (base, _rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"issuer":"https://idp.test/realms/other","token_endpoint":"https://idp.test/token"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = discover(&client, &base)
            .await
            .expect_err("a self-reported issuer mismatch must be rejected");

        let msg = err.to_string();
        assert!(
            msg.contains(&base),
            "error must name the expected issuer ({base}): {msg}"
        );
        assert!(
            msg.contains("https://idp.test/realms/other"),
            "error must name the issuer found in the document: {msg}"
        );
    }

    #[tokio::test]
    async fn discovery_accepts_issuer_differing_only_by_trailing_slash() {
        // `spawn_seq_with` because the document must self-report the URL it is
        // served from, which is only known once the port is bound.
        let (base, _rx) = crate::test_helpers::spawn_seq_with(|base| {
            vec![http_json(
                "200 OK",
                // Same issuer as requested, plus a trailing slash.
                &format!(
                    r#"{{"issuer":"{base}/","token_endpoint":"{base}/token","device_authorization_endpoint":"{base}/device","revocation_endpoint":"{base}/revoke"}}"#
                ),
            )]
        });
        let client = reqwest::Client::new();
        let disc = discover(&client, &format!("{base}/"))
            .await
            .expect("trailing slashes must be canonicalised away, not treated as a mismatch");
        assert_eq!(disc.token_endpoint, format!("{base}/token"));
        assert_eq!(disc.revocation_endpoint, Some(format!("{base}/revoke")));
    }

    /// The ONLY 503 that means "this instance has no Keycloak" is the one
    /// carrying `code: "oidc_not_configured"`.
    #[tokio::test]
    async fn public_config_503_with_oidc_not_configured_code_maps_to_no_keycloak() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "503 Service Unavailable",
            r#"{"error":"Keycloak is not configured on this TraceVault instance","code":"oidc_not_configured"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = fetch_public_config(&client, &base)
            .await
            .expect_err("503 must not be reported as a generic failure");
        assert!(matches!(err, OidcError::NoKeycloak), "got {err:?}");
    }

    /// The same endpoint answers 503 for a transient JWKS/IdP outage. That
    /// is retryable and must NOT be reported as "no Keycloak configured" —
    /// telling a user to go get an API key during a 30-second Keycloak
    /// restart is misleading and unactionable.
    #[tokio::test]
    async fn public_config_503_with_another_code_is_transient_not_no_keycloak() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "503 Service Unavailable",
            r#"{"error":"could not fetch JWKS","code":"idp_unreachable"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = fetch_public_config(&client, &base).await.unwrap_err();
        assert!(
            matches!(err, OidcError::IdpUnavailable { .. }),
            "a non-oidc_not_configured 503 must be the retryable variant; got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("temporarily unavailable") && msg.contains("retry"),
            "message must read as retryable: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("api key"),
            "must not send the user off to get an API key: {msg}"
        );
    }

    /// A 503 whose body isn't the expected envelope (empty, HTML from a
    /// proxy, truncated JSON) must fall into the retryable branch: "no
    /// Keycloak configured" is the stronger claim and needs positive
    /// evidence from the `code` field.
    #[tokio::test]
    async fn public_config_503_with_unparseable_body_is_transient() {
        for body in ["", "not json at all", r#"{"error":"boom"}"#] {
            let (base, _rx) = spawn_seq(vec![http_json("503 Service Unavailable", body)]);
            let client = reqwest::Client::new();
            let err = fetch_public_config(&client, &base).await.unwrap_err();
            assert!(
                matches!(err, OidcError::IdpUnavailable { .. }),
                "body {body:?} must yield the retryable variant, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn public_config_200_parses_issuer_and_client_id() {
        let (base, rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"issuer":"https://idp.test/realms/visdom","cli_client_id":"tracevault-cli"}"#,
        )]);
        let client = reqwest::Client::new();
        let cfg = fetch_public_config(&client, &format!("{base}/"))
            .await
            .unwrap();
        assert_eq!(cfg.issuer, "https://idp.test/realms/visdom");
        assert_eq!(cfg.cli_client_id, "tracevault-cli");
        let req = rx.recv_timeout(RECV_TIMEOUT).unwrap();
        assert!(
            req.contains("GET /api/v1/auth/public-config"),
            "unexpected request: {req}"
        );
    }

    #[tokio::test]
    async fn device_start_requests_offline_access_scope() {
        let (base, rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://idp.test/device","expires_in":600,"interval":5}"#,
        )]);
        let client = reqwest::Client::new();
        let device = device_start(&client, &discovery_at(&base), "tracevault-cli")
            .await
            .unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        let req = rx.recv_timeout(RECV_TIMEOUT).unwrap();
        assert!(
            req.contains("offline_access"),
            "offline_access is what yields a refresh token: {req}"
        );
        assert!(req.contains("client_id=tracevault-cli"));
    }

    #[tokio::test]
    async fn refresh_invalid_grant_maps_to_session_expired() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"Token is not active"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = refresh(&client, &discovery_at(&base), "tracevault-cli", "rt")
            .await
            .expect_err("invalid_grant must be a distinct 'session expired' error");
        assert!(matches!(err, OidcError::SessionExpired), "got {err:?}");
        assert!(
            err.to_string().contains("tracevault login"),
            "message must tell the user how to recover: {err}"
        );
    }

    #[tokio::test]
    async fn revoke_sends_refresh_token_hint() {
        let (base, rx) = spawn_seq(vec![http_json("200 OK", "{}")]);
        let client = reqwest::Client::new();
        revoke(&client, &discovery_at(&base), "tracevault-cli", "rt")
            .await
            .unwrap();
        let req = rx.recv_timeout(RECV_TIMEOUT).unwrap();
        assert!(req.contains("POST /revoke"), "unexpected request: {req}");
        assert!(req.contains("token_type_hint=refresh_token"), "{req}");
    }

    #[tokio::test]
    async fn revoke_without_endpoint_is_a_named_error() {
        let disc = Discovery {
            device_authorization_endpoint: None,
            token_endpoint: "https://idp.test/token".into(),
            revocation_endpoint: None,
        };
        let client = reqwest::Client::new();
        let err = revoke(&client, &disc, "tracevault-cli", "rt")
            .await
            .expect_err("no revocation endpoint must be an explicit error");
        assert!(err.to_string().contains("revocation"), "{err}");
    }

    /// Secrets must not be reachable through `{:?}`, which is how they would
    /// most plausibly end up in a log or a panic message.
    #[test]
    fn debug_impls_redact_secrets() {
        let device = device_auth(5, 600);
        let dbg = format!("{device:?}");
        assert!(!dbg.contains("dev-code"), "device_code leaked: {dbg}");

        let tokens = TokenSet {
            access_token: "super-secret-access".into(),
            refresh_token: Some("super-secret-refresh".into()),
            expires_in: 300,
        };
        let dbg = format!("{tokens:?}");
        assert!(!dbg.contains("super-secret-access"), "access leaked: {dbg}");
        assert!(
            !dbg.contains("super-secret-refresh"),
            "refresh leaked: {dbg}"
        );
    }

    #[test]
    fn canonical_issuer_trims_whitespace_and_trailing_slashes() {
        assert_eq!(
            canonical_issuer("  https://idp.test/realms/v//  "),
            "https://idp.test/realms/v"
        );
    }
}
