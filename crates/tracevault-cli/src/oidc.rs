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
use url::Url;

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
///
/// This type exists only for the CONFIGURED case: "no Keycloak here" is an
/// error ([`OidcError::NoKeycloak`]), not a `PublicConfig` with empty fields,
/// so no caller can accidentally proceed with an empty issuer.
#[derive(Debug, Clone)]
pub struct PublicConfig {
    pub issuer: String,
    pub cli_client_id: String,
    /// The audience the server expects in an access token's `aud`. Optional so
    /// the CLI keeps working against a server that does not publish it; used
    /// only to sharpen the diagnosis when `/auth/me` rejects a fresh token.
    pub audience: Option<String>,
}

/// Wire shape of `GET /api/v1/auth/public-config`.
///
/// Every field except `oidc_enabled` is optional on the wire because the
/// not-configured response omits them; requiring them here would turn a
/// well-formed "no Keycloak" answer into a parse error.
#[derive(Deserialize)]
struct PublicConfigWire {
    #[serde(default)]
    oidc_enabled: bool,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    cli_client_id: Option<String>,
    #[serde(default)]
    audience: Option<String>,
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

/// Failures of the login/refresh machinery. Every variant that a caller has
/// to *behave* differently about is its own variant; anything else collapses
/// into [`OidcError::Oauth`] (the IdP said no, with a code) or
/// [`OidcError::Transport`] (we never got a usable answer).
#[derive(Debug)]
pub enum OidcError {
    /// `/auth/public-config` answered `{"oidc_enabled": false}`: this TraceVault
    /// instance has no Keycloak at all, so interactive login is impossible and
    /// an API key must be used instead. Permanent until an operator changes the
    /// deployment. See [`fetch_public_config`].
    NoKeycloak,
    /// `/auth/public-config` answered a 5xx — the server, or an intermediary in
    /// front of it, is unhealthy right now. Retryable, and explicitly NOT a
    /// reason to go get an API key.
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
    /// The issuer is not `https` (and not loopback), so the refresh token would
    /// travel in clear text.
    InsecureIssuer { issuer: String },
    /// A discovery document named an endpoint on a different origin than the
    /// issuer's — a misconfiguration, or an attempt to collect tokens elsewhere.
    EndpointOriginMismatch {
        what: &'static str,
        endpoint: String,
        issuer: String,
    },
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
            Self::InsecureIssuer { issuer } => write!(
                f,
                "refusing to use the insecure issuer '{issuer}': signing in would send a \
                 long-lived refresh token over an unencrypted connection. Use an https issuer \
                 (http is allowed only for a loopback address during local development)."
            ),
            Self::EndpointOriginMismatch {
                what,
                endpoint,
                issuer,
            } => write!(
                f,
                "the identity provider's discovery document points its {what} endpoint at \
                 '{endpoint}', which is not the issuer's origin ('{issuer}'). Refusing, rather \
                 than sending credentials to an unrelated host."
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
/// "Is this instance configured for SSO, and against which realm?"
///
/// Two outcomes must stay distinguishable, because the advice differs and one
/// is permanent while the other is not:
///
/// * `200 {"oidc_enabled": false}` → the instance has no Keycloak at all.
///   PERMANENT; the user needs a `tvk_` API key ([`OidcError::NoKeycloak`]).
/// * any `5xx` → the server (or an intermediary/web proxy in front of it) is
///   unhealthy right now. RETRYABLE ([`OidcError::IdpUnavailable`]), and
///   deliberately never phrased as "go get an API key": telling a user in the
///   middle of a 30-second restart to change credentials is unactionable.
///
/// The not-configured answer is a 200 rather than a 503 because a 503 from a
/// healthy server makes load balancers fail the pod out and 5xx SLO alerts
/// fire. The 5xx branch remains regardless, since an intermediary can still
/// produce one when the backend is down.
///
/// `oidc_enabled: true` with `issuer` or `cli_client_id` missing is a hard
/// error rather than an empty-string default: the alternative is a device
/// authorization POSTed to `"/.well-known/..."`, failing with something
/// unrelated to the actual misconfiguration.
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

    if status.is_server_error() {
        return Err(OidcError::IdpUnavailable {
            detail: format!("server returned {status}"),
        });
    }
    if !status.is_success() {
        return Err(OidcError::Transport(format!("GET {url} returned {status}")));
    }

    let wire: PublicConfigWire = resp
        .json()
        .await
        .map_err(|e| OidcError::Transport(format!("malformed auth public-config response: {e}")))?;

    if !wire.oidc_enabled {
        return Err(OidcError::NoKeycloak);
    }
    let missing = |field: &'static str| {
        OidcError::Transport(format!(
            "the server reports SSO is enabled but its auth public-config omits `{field}` — the \
         server is misconfigured; report this to whoever operates it"
        ))
    };
    Ok(PublicConfig {
        issuer: wire
            .issuer
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| missing("issuer"))?,
        cli_client_id: wire
            .cli_client_id
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| missing("cli_client_id"))?,
        audience: wire.audience.filter(|s| !s.trim().is_empty()),
    })
}

/// Fetch and validate the realm's OpenID discovery document.
///
/// Three checks, because everything downstream trusts what this returns with a
/// refresh token attached:
///
/// 1. The issuer must be `https`, unless it is loopback. A plaintext issuer
///    would POST the long-lived `offline_access` refresh token in clear text.
///    Loopback is exempt so local Keycloak development works.
/// 2. The document's self-reported `issuer` must match the one we asked for
///    (both canonicalised). Otherwise a redirect or a copy-pasted wrong realm
///    silently mints tokens the TraceVault server will reject, with no clue why.
/// 3. Every endpoint must share the issuer's ORIGIN. A misconfigured or tampered
///    document could otherwise name an unrelated host as the token endpoint and
///    collect the refresh token there.
pub async fn discover(client: &reqwest::Client, issuer: &str) -> Result<Discovery, OidcError> {
    let expected = canonical_issuer(issuer);
    let issuer_url = Url::parse(&expected).map_err(|e| {
        OidcError::Transport(format!("'{expected}' is not a valid issuer URL: {e}"))
    })?;
    require_secure(&issuer_url)?;

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

    let origin = issuer_url.origin();
    let checked =
        |endpoint: Option<String>, what: &'static str| -> Result<Option<String>, OidcError> {
            let Some(endpoint) = endpoint else {
                return Ok(None);
            };
            let parsed = Url::parse(&endpoint).map_err(|e| {
                OidcError::Transport(format!(
                    "the discovery document's {what} endpoint '{endpoint}' is not a valid URL: {e}"
                ))
            })?;
            if parsed.origin() != origin {
                return Err(OidcError::EndpointOriginMismatch {
                    what,
                    endpoint,
                    issuer: expected.clone(),
                });
            }
            Ok(Some(endpoint))
        };

    let token_endpoint = checked(Some(doc.token_endpoint), "token")?
        .expect("checked() returns Some for a Some input");
    Ok(Discovery {
        device_authorization_endpoint: checked(
            doc.device_authorization_endpoint,
            "device authorization",
        )?,
        token_endpoint,
        revocation_endpoint: checked(doc.revocation_endpoint, "revocation")?,
    })
}

/// Reject a non-TLS issuer, except on loopback.
///
/// The refresh token is long-lived and `offline_access`-scoped; handing it to an
/// `http://` endpoint puts it on the wire in clear text. Loopback is exempt
/// because a local Keycloak (`http://localhost:8080/realms/...`) is a normal
/// development setup and never leaves the machine.
fn require_secure(issuer: &Url) -> Result<(), OidcError> {
    if issuer.scheme() == "https" || is_loopback_host(issuer) {
        return Ok(());
    }
    Err(OidcError::InsecureIssuer {
        issuer: issuer.to_string(),
    })
}

/// Whether a URL's host is a loopback address or a `localhost` name.
fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
        None => false,
    }
}

/// Whether `url` is safe to hand to the desktop's URL handler.
///
/// `open::that` launches whatever handler is registered for the scheme, so a
/// broken or hostile realm response could otherwise get `file:///...` — or any
/// registered application scheme — launched on the user's desktop. Only the two
/// schemes a verification URI is ever legitimately expressed in are allowed.
pub fn is_browser_safe(url: &str) -> bool {
    Url::parse(url)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false)
}

/// Start a device authorization (RFC 8628 §3.1). `tracing-cli` is a
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

/// Poll the token endpoint until the user approves, or the attempt dies.
///
/// The sleep is a parameter (production passes `tokio::time::sleep`) for two
/// reasons: tests must not burn wall-clock seconds, and the elapsed budget is
/// accumulated from the sleep durations rather than read off a clock — so "give
/// up after `expires_in`" is exercised deterministically by a no-op sleep, and a
/// `slow_down` genuinely shortens the number of remaining attempts.
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

/// The `aud` claim of a JWT, read WITHOUT verifying the signature.
///
/// NOT authentication or validation of any kind, and must never be used as
/// such: this is for one diagnostic sentence, deciding whether a rejected
/// token was minted for this server at all. The CLI is the token's holder, not
/// its verifier — the server does that — so reading the payload it just
/// received from the IdP over TLS is only ever used to explain a failure.
///
/// Returns `None` on anything unexpected (not a JWT, un-decodable payload, no
/// `aud`). Callers must treat `None` as "cannot tell" and fall back to the
/// generic message rather than guessing.
pub fn unverified_audiences(access_token: &str) -> Option<Vec<String>> {
    let payload = access_token.split('.').nth(1)?;
    let json = base64url_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&json).ok()?;
    match claims.get("aud")? {
        serde_json::Value::String(one) => Some(vec![one.clone()]),
        serde_json::Value::Array(many) => Some(
            many.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
        ),
        _ => None,
    }
}

/// Minimal unpadded base64url decoder, so reading a JWT payload needs no new
/// dependency. `None` on any character outside the alphabet.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            // Padding is optional in base64url and always trailing.
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
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
            "tracing-cli",
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
            assert!(req.contains("client_id=tracing-cli"), "missing client_id");
            assert!(
                !req.contains("client_secret"),
                "tracing-cli is a PUBLIC client; no secret may be sent: {req}"
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
            "tracing-cli",
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
            "tracing-cli",
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
            "tracing-cli",
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
            "tracing-cli",
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

    /// "No Keycloak here" is a 200 with `oidc_enabled: false`, and it is
    /// PERMANENT: the user must switch to an API key. (The server answers 200
    /// rather than 503 because a 503 from a healthy server fails the pod out of
    /// its load balancer and trips 5xx SLO alerts.)
    /// An `http://` issuer would put the long-lived `offline_access` refresh
    /// token on the wire in clear text, so discovery refuses before it even
    /// fetches the document. (Nothing is spawned here: reaching the assertion
    /// proves no request was attempted.)
    #[tokio::test]
    async fn discovery_refuses_a_plaintext_issuer() {
        let client = reqwest::Client::new();
        let err = discover(&client, "http://idp.example.com/realms/v")
            .await
            .expect_err("a plaintext issuer must be refused");
        assert!(
            matches!(err, OidcError::InsecureIssuer { .. }),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("refresh token") && msg.contains("https"),
            "{msg}"
        );
    }

    /// ...but a loopback issuer over http is a normal local-development setup
    /// and must keep working: the token never leaves the machine.
    #[tokio::test]
    async fn discovery_allows_a_loopback_issuer_over_http() {
        let (base, _rx) = crate::test_helpers::spawn_seq_with(|base| {
            vec![http_json(
                "200 OK",
                &format!(r#"{{"issuer":"{base}","token_endpoint":"{base}/token"}}"#),
            )]
        });
        // `spawn_seq` binds 127.0.0.1, so this exercises the loopback exemption.
        assert!(base.starts_with("http://127.0.0.1"));
        let client = reqwest::Client::new();
        discover(&client, &base)
            .await
            .expect("loopback http must stay usable for local development");
    }

    /// A document that names another host as its token endpoint would collect
    /// the refresh token there. Refused, naming the endpoint and the issuer.
    #[tokio::test]
    async fn discovery_rejects_an_endpoint_on_another_origin() {
        let (base, _rx) = crate::test_helpers::spawn_seq_with(|base| {
            vec![http_json(
                "200 OK",
                &format!(
                    r#"{{"issuer":"{base}","token_endpoint":"https://evil.example.com/token"}}"#
                ),
            )]
        });
        let client = reqwest::Client::new();
        let err = discover(&client, &base)
            .await
            .expect_err("a token endpoint on another origin must be refused");
        assert!(
            matches!(err, OidcError::EndpointOriginMismatch { what: "token", .. }),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("evil.example.com"),
            "must name the endpoint: {msg}"
        );
        assert!(msg.contains(&base), "must name the issuer: {msg}");
    }

    /// The same check covers the endpoints that are optional — the revocation
    /// endpoint also receives the refresh token.
    #[tokio::test]
    async fn discovery_rejects_an_offsite_revocation_endpoint() {
        let (base, _rx) = crate::test_helpers::spawn_seq_with(|base| {
            vec![http_json(
                "200 OK",
                &format!(
                    r#"{{"issuer":"{base}","token_endpoint":"{base}/token","revocation_endpoint":"https://evil.example.com/revoke"}}"#
                ),
            )]
        });
        let client = reqwest::Client::new();
        let err = discover(&client, &base).await.unwrap_err();
        assert!(
            matches!(
                err,
                OidcError::EndpointOriginMismatch {
                    what: "revocation",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A port or scheme difference is a different origin too — `https://host` and
    /// `https://host:8443` are not interchangeable for credential delivery.
    #[tokio::test]
    async fn discovery_treats_a_different_port_as_a_different_origin() {
        let (base, _rx) = crate::test_helpers::spawn_seq_with(|base| {
            // Same scheme and host as the issuer, different port. A fixed port
            // rather than one derived from `base`: prefixing a digit onto an
            // ephemeral port can exceed 65535, which would fail URL parsing and
            // pass this test for the wrong reason.
            vec![http_json(
                "200 OK",
                &format!(r#"{{"issuer":"{base}","token_endpoint":"http://127.0.0.1:9/token"}}"#),
            )]
        });
        let client = reqwest::Client::new();
        let err = discover(&client, &base).await.unwrap_err();
        assert!(
            matches!(err, OidcError::EndpointOriginMismatch { .. }),
            "got {err:?}"
        );
    }

    /// The loopback exemption by name as well as by address: the
    /// server-backed test above only exercises `127.0.0.1`.
    #[test]
    fn require_secure_exempts_loopback_by_name_and_address() {
        for allowed in [
            "https://idp.example.com/realms/v",
            "http://localhost:8080/realms/v",
            "http://keycloak.localhost/realms/v",
            "http://127.0.0.1:8080/realms/v",
            "http://[::1]:8080/realms/v",
        ] {
            require_secure(&Url::parse(allowed).unwrap())
                .unwrap_or_else(|e| panic!("{allowed} must be allowed: {e}"));
        }
        for refused in [
            "http://idp.example.com/realms/v",
            // Not loopback despite the name resembling it.
            "http://localhost.evil.example.com/realms/v",
            "http://10.0.0.5:8080/realms/v",
        ] {
            let err = require_secure(&Url::parse(refused).unwrap())
                .expect_err("{refused} must be refused");
            assert!(matches!(err, OidcError::InsecureIssuer { .. }), "{refused}");
        }
    }

    /// `open::that` hands the URL to the desktop's registered handler, so only
    /// the two schemes a verification URI is legitimately expressed in may pass.
    #[test]
    fn is_browser_safe_allows_only_http_and_https() {
        assert!(is_browser_safe("https://idp.example.com/device?code=X"));
        assert!(is_browser_safe("http://localhost:8080/device"));

        for hostile in [
            "file:///etc/passwd",
            "file:///Users/me/.ssh/id_rsa",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vscode://file/etc/passwd",
            "smb://attacker.example.com/share",
            "not a url at all",
            "",
        ] {
            assert!(
                !is_browser_safe(hostile),
                "{hostile} must not be handed to the desktop URL handler"
            );
        }
    }

    #[tokio::test]
    async fn public_config_oidc_disabled_is_permanent_no_keycloak() {
        let (base, _rx) = spawn_seq(vec![http_json("200 OK", r#"{"oidc_enabled":false}"#)]);
        let client = reqwest::Client::new();
        let err = fetch_public_config(&client, &base)
            .await
            .expect_err("oidc_enabled:false must not parse as a usable config");
        assert!(matches!(err, OidcError::NoKeycloak), "got {err:?}");
    }

    /// A 5xx is the server (or an intermediary in front of it) being unhealthy:
    /// retryable, and never phrased as "go get an API key". This branch stays
    /// even though the not-configured case moved to 200, because a web proxy
    /// still answers 503 when the backend is down.
    #[tokio::test]
    async fn public_config_5xx_is_transient_not_no_keycloak() {
        for status in [
            "503 Service Unavailable",
            "502 Bad Gateway",
            "500 Internal Server Error",
        ] {
            let (base, _rx) = spawn_seq(vec![http_json(status, r#"{"error":"backend down"}"#)]);
            let client = reqwest::Client::new();
            let err = fetch_public_config(&client, &base).await.unwrap_err();
            assert!(
                matches!(err, OidcError::IdpUnavailable { .. }),
                "{status} must be the retryable variant; got {err:?}"
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
    }

    /// A 5xx body is not parsed at all now, so a proxy's HTML error page or an
    /// empty body must still land in the retryable branch — never the permanent
    /// one, which is the stronger claim.
    #[tokio::test]
    async fn public_config_5xx_with_unparseable_body_is_transient() {
        for body in ["", "not json at all", "<html>502 Bad Gateway</html>"] {
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
    async fn public_config_200_parses_issuer_client_id_and_audience() {
        let (base, rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/visdom","audience":"tracevault","cli_client_id":"tracing-cli"}"#,
        )]);
        let client = reqwest::Client::new();
        let cfg = fetch_public_config(&client, &format!("{base}/"))
            .await
            .unwrap();
        assert_eq!(cfg.issuer, "https://idp.test/realms/visdom");
        assert_eq!(cfg.cli_client_id, "tracing-cli");
        assert_eq!(cfg.audience.as_deref(), Some("tracevault"));
        let req = rx.recv_timeout(RECV_TIMEOUT).unwrap();
        assert!(
            req.contains("GET /api/v1/auth/public-config"),
            "unexpected request: {req}"
        );
    }

    /// A server that omits `audience` is still usable — it only costs the
    /// sharper wrong-audience diagnosis.
    #[tokio::test]
    async fn public_config_without_audience_still_resolves() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v","cli_client_id":"tracing-cli"}"#,
        )]);
        let client = reqwest::Client::new();
        let cfg = fetch_public_config(&client, &base).await.unwrap();
        assert_eq!(cfg.audience, None);
    }

    /// `oidc_enabled: true` with a field missing is a misconfigured server.
    /// Defaulting to an empty string would send the device authorization to a
    /// nonsense URL and fail with something unrelated to the real cause.
    #[tokio::test]
    async fn public_config_enabled_but_incomplete_is_a_named_error() {
        for (body, field) in [
            (
                r#"{"oidc_enabled":true,"cli_client_id":"tracing-cli"}"#,
                "issuer",
            ),
            (
                r#"{"oidc_enabled":true,"issuer":"https://idp.test/realms/v"}"#,
                "cli_client_id",
            ),
            (
                r#"{"oidc_enabled":true,"issuer":"  ","cli_client_id":"tracing-cli"}"#,
                "issuer",
            ),
        ] {
            let (base, _rx) = spawn_seq(vec![http_json("200 OK", body)]);
            let client = reqwest::Client::new();
            let err = fetch_public_config(&client, &base).await.unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(field) && msg.contains("misconfigured"),
                "must name the missing field `{field}`: {msg}"
            );
            assert!(
                !matches!(err, OidcError::NoKeycloak),
                "an incomplete config is not the same as no Keycloak: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn device_start_requests_offline_access_scope() {
        let (base, rx) = spawn_seq(vec![http_json(
            "200 OK",
            r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://idp.test/device","expires_in":600,"interval":5}"#,
        )]);
        let client = reqwest::Client::new();
        let device = device_start(&client, &discovery_at(&base), "tracing-cli")
            .await
            .unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        let req = rx.recv_timeout(RECV_TIMEOUT).unwrap();
        assert!(
            req.contains("offline_access"),
            "offline_access is what yields a refresh token: {req}"
        );
        assert!(req.contains("client_id=tracing-cli"));
    }

    #[tokio::test]
    async fn refresh_invalid_grant_maps_to_session_expired() {
        let (base, _rx) = spawn_seq(vec![http_json(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"Token is not active"}"#,
        )]);
        let client = reqwest::Client::new();
        let err = refresh(&client, &discovery_at(&base), "tracing-cli", "rt")
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
        revoke(&client, &discovery_at(&base), "tracing-cli", "rt")
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
        let err = revoke(&client, &disc, "tracing-cli", "rt")
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

    /// Unpadded base64url, so a fixture JWT can be assembled from a readable
    /// payload instead of a pasted blob. Test-only; production only ever decodes.
    fn base64url_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            // Unpadded: emit only the characters backed by real input bytes —
            // 1 byte → 2 chars, 2 → 3, 3 → 4.
            for shift in [18, 12, 6, 0].iter().take(chunk.len() + 1) {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            }
        }
        out
    }

    /// Build an unsigned JWT for the fixtures below from a readable payload.
    ///
    /// Two reasons not to paste a pre-encoded literal. A base64 blob hides what
    /// the fixture asserts — you cannot see which `aud` it claims without decoding
    /// it by hand. And a JWT's encoded header is a short fixed prefix that every
    /// secret scanner matches on: these fixtures carry nothing secret (the
    /// signature is a placeholder `unverified_audiences` never checks), but a
    /// scanner cannot know that, and three HIGH findings on one test file is how a
    /// repo trains people to wave through "it's only a fixture" — after which a
    /// real leak looks the same. Aikido flagged exactly this on `f9ee250`.
    fn unsigned_jwt(payload_json: &str) -> String {
        format!(
            "{}.{}.signature-not-checked",
            base64url_encode(br#"{"alg":"none"}"#),
            base64url_encode(payload_json.as_bytes())
        )
    }

    /// Anchors the test-only encoder against a value that did not come from it:
    /// `bm90LWpzb24` is the payload the "decodes but is not JSON" case below has
    /// always used. Without this, a mirrored bug in encode and decode could let
    /// every fixture-driven assertion pass while agreeing on the wrong bytes.
    #[test]
    fn base64url_encode_matches_a_known_value() {
        assert_eq!(base64url_encode(b"not-json"), "bm90LWpzb24");
        assert_eq!(
            base64url_decode(&base64url_encode(b"abc")),
            Some(b"abc".to_vec())
        );
        // Byte counts that exercise every remainder: 1, 2 and 0 mod 3.
        for raw in [&b"a"[..], &b"ab"[..], &b"abc"[..], &b"abcd"[..]] {
            assert_eq!(
                base64url_decode(&base64url_encode(raw)),
                Some(raw.to_vec()),
                "round trip failed for {raw:?}"
            );
        }
    }

    /// Reading `aud` out of a JWT payload: diagnosis only, never validation.
    #[test]
    fn unverified_audiences_reads_string_and_array_forms() {
        assert_eq!(
            unverified_audiences(&unsigned_jwt(r#"{"aud":"tracevault","sub":"u"}"#)),
            Some(vec!["tracevault".to_string()])
        );
        assert_eq!(
            unverified_audiences(&unsigned_jwt(
                r#"{"aud":["account","tracevault"],"sub":"u"}"#
            )),
            Some(vec!["account".to_string(), "tracevault".to_string()])
        );
        assert_eq!(
            unverified_audiences(&unsigned_jwt(r#"{"aud":"some-other-app"}"#)),
            Some(vec!["some-other-app".to_string()])
        );
    }

    /// Anything unexpected must be `None` — "cannot tell" — so the caller falls
    /// back to the generic message instead of guessing at a cause.
    #[test]
    fn unverified_audiences_is_none_when_it_cannot_tell() {
        // No `aud` claim.
        assert_eq!(unverified_audiences(&unsigned_jwt(r#"{"sub":"u"}"#)), None);
        // Not a JWT at all (an opaque token, as some IdPs issue).
        assert_eq!(unverified_audiences("opaque-token"), None);
        assert_eq!(unverified_audiences(""), None);
        // Payload is not base64url.
        assert_eq!(unverified_audiences("a.!!!not-base64!!!.c"), None);
        // Payload decodes but is not JSON.
        assert_eq!(unverified_audiences("a.bm90LWpzb24.c"), None);
    }

    #[test]
    fn base64url_decode_handles_padding_and_rejects_junk() {
        assert_eq!(base64url_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64url_decode("aGVsbG8="), Some(b"hello".to_vec()));
        // `-` and `_` are the url-safe substitutions for `+` and `/`:
        // 111110 111110 111111 111111 -> 0xFB 0xEF 0xFF.
        assert_eq!(base64url_decode("--__"), Some(vec![0xfb, 0xef, 0xff]));
        assert!(base64url_decode("not valid!").is_none());
    }

    #[test]
    fn canonical_issuer_trims_whitespace_and_trailing_slashes() {
        assert_eq!(
            canonical_issuer("  https://idp.test/realms/v//  "),
            "https://idp.test/realms/v"
        );
    }
}
