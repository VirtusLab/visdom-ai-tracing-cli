//! `tracevault login` — interactive human sign-in via Keycloak's RFC 8628
//! device authorization grant.
//!
//! The TraceVault server is not part of the token exchange: it only tells the
//! CLI which realm and client id to use (`/api/v1/auth/public-config`). The
//! CLI then talks to Keycloak directly, and the refresh token it gets back
//! (via `offline_access`) is what lets unattended git hooks keep working
//! without ever prompting.
//!
//! Automation does NOT use this command — CI keeps setting
//! `TRACEVAULT_API_KEY`.

use crate::api_client::{ApiClient, GetMeError};
use crate::credentials::{Credential, Credentials, KeycloakSession};
use crate::oidc::{self, OidcError};

/// Decide whether we're probably in a headless environment where opening a
/// browser will fail. Errs on the side of *not* opening when we're unsure,
/// since printing the URL is always safe and opening a browser in a
/// Docker/CI/SSH session usually fails noisily.
fn is_headless() -> bool {
    // Explicit opt-out always wins.
    if std::env::var_os("TRACEVAULT_NO_BROWSER").is_some() {
        return true;
    }
    // Common CI indicators.
    if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() {
        return true;
    }
    // Typical "running inside a container" hint. Not bulletproof — some
    // desktop containers do have a browser — but a strong signal.
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }
    // On Linux/BSD, a graphical session needs one of these env vars. macOS
    // and Windows don't use them, so only apply this check on Unix-like
    // platforms that aren't macOS.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return true;
        }
    }
    false
}

/// What `login` should do with the verification URL.
///
/// A separate decision from the printing so it can be tested without driving a
/// whole login: the `Refuse` case in particular is a security property, and the
/// only way to observe it otherwise would be a launched application.
#[derive(Debug, PartialEq, Eq)]
enum BrowserAction {
    /// Headless, or `--no-browser`.
    Skip,
    /// The IdP handed us something that must not reach a desktop handler.
    Refuse,
    Open,
}

/// `open::that` launches whatever handler is registered for the URL's scheme, so
/// a broken or hostile realm response could otherwise get `file:///...` — or any
/// registered application scheme — launched on the user's desktop. The URL and
/// code are printed regardless, so refusing costs the user nothing.
fn browser_action(url: &str, no_browser: bool, headless: bool) -> BrowserAction {
    if no_browser || headless {
        BrowserAction::Skip
    } else if !oidc::is_browser_safe(url) {
        BrowserAction::Refuse
    } else {
        BrowserAction::Open
    }
}

fn print_url_banner(url: &str) {
    println!();
    println!("  Open this URL in a browser to finish logging in:");
    println!();
    println!("  {url}");
    println!();
}

/// Show the user code as prominently as a terminal allows.
///
/// A device flow is unusable if the user can't find the code, and this is the
/// only value in the whole flow that is *meant* to be displayed — the device
/// code, access token and refresh token are never printed anywhere.
fn print_user_code(user_code: &str) {
    let rule = "─".repeat(user_code.chars().count() + 8);
    println!("  Then enter this one-time code:");
    println!();
    println!("  ┌{rule}┐");
    println!("  │    {user_code}    │");
    println!("  └{rule}┘");
    println!();
}

pub async fn login(server_url: &str, no_browser: bool) -> Result<(), Box<dyn std::error::Error>> {
    login_with(server_url, no_browser, tokio::time::sleep).await
}

/// [`login`] with the poll loop's sleep injected.
///
/// Exists so tests can drive the whole flow — the "save the credentials before
/// identifying, and keep them whatever `/auth/me` says" invariant is the point
/// of this command — without spending the device code's poll interval in real
/// seconds on every case.
async fn login_with<S, F>(
    server_url: &str,
    no_browser: bool,
    sleep: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: FnMut(std::time::Duration) -> F,
    F: std::future::Future<Output = ()>,
{
    // A dedicated client for the IdP conversation: these requests carry no
    // TraceVault credential, and there isn't one yet anyway.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // 1. Which realm does this server trust?
    let config = match oidc::fetch_public_config(&http, server_url).await {
        Ok(c) => c,
        Err(OidcError::NoKeycloak) => {
            // Permanent: no amount of retrying helps, so point at the only
            // credential that works on such an instance.
            eprintln!(
                "This TraceVault server has no Keycloak/SSO configured, so `tracevault login` \
                 cannot run the browser sign-in flow."
            );
            eprintln!();
            eprintln!("Use an API key instead:");
            eprintln!("  export TRACEVAULT_SERVER_URL=\"{server_url}\"");
            eprintln!("  export TRACEVAULT_API_KEY=\"tvk_...\"");
            // Deliberately no credentials written on this path.
            return Err("server has no Keycloak configured".into());
        }
        // Transient: propagating the error keeps the "retry in a moment"
        // wording and, crucially, does NOT tell the user to switch to an API
        // key — the deployment is fine, the IdP is just blipping.
        Err(e) => return Err(e.into()),
    };

    // 2. Where are the realm's endpoints?
    let discovery = oidc::discover(&http, &config.issuer).await?;

    // 3. Start the device authorization.
    let device = oidc::device_start(&http, &discovery, &config.cli_client_id).await?;

    // The complete URI has the code pre-filled, which is what we hand to the
    // browser; the plain URI plus the printed code is what a human retypes on
    // another device.
    let browser_url = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device.verification_uri);

    print_url_banner(&device.verification_uri);
    print_user_code(&device.user_code);

    match browser_action(browser_url, no_browser, is_headless()) {
        BrowserAction::Skip => {
            println!("Not attempting to auto-open a browser (headless environment detected or --no-browser set).");
        }
        BrowserAction::Refuse => {
            eprintln!(
                "Not opening the verification URL automatically: the identity provider returned a \
                 non-web URL. Use the URL and code printed above."
            );
        }
        BrowserAction::Open => {
            println!("Attempting to open the URL in your default browser...");
            if let Err(e) = open::that(browser_url) {
                // Non-fatal: the URL and code are already visible above, the
                // user can just copy them.
                eprintln!("Could not open browser automatically: {e}");
                eprintln!("Copy the URL above into a browser manually.");
            }
        }
    }

    // 4. Wait for the user to finish in the browser.
    println!("Waiting for you to approve the sign-in...");
    let tokens =
        oidc::poll_token_with(&http, &discovery, &config.cli_client_id, &device, sleep).await?;

    let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
        // Without a refresh token the credential would die in minutes, which
        // defeats the whole point for git hooks — fail loudly rather than
        // save something that stops working after one coffee break.
        format!(
            "the identity provider did not return a refresh token — the `offline_access` scope is \
             likely not enabled for the `{}` client in Keycloak",
            config.cli_client_id
        )
    })?;

    let now = chrono::Utc::now().timestamp();
    // Kept for the wrong-audience diagnosis below, after the session takes
    // ownership of the token.
    let access_token = tokens.access_token.clone();
    let session = KeycloakSession {
        issuer: oidc::canonical_issuer(&config.issuer),
        client_id: config.cli_client_id.clone(),
        refresh_token,
        access_token: tokens.access_token.clone(),
        access_expires_at: tokens.access_expires_at(now),
    };

    // 5. Save FIRST, identify second. The token exchange already succeeded,
    // so the login is real: a failure (or a 403) while asking the server who
    // we are must not throw the credential away. `email` is filled in below
    // once the server tells us.
    let mut creds = Credentials::keycloak(server_url, String::new(), session.clone());
    creds.save()?;

    // 6. Confirm with TraceVault who this token resolves to. This is also
    // where a missing realm role surfaces.
    let client = ApiClient::with_credential(server_url, Some(Credential::Keycloak(session)));
    match client.get_me().await {
        Ok(me) => {
            creds.email = me.email.clone();
            creds.save()?;
            println!();
            match &me.role {
                Some(role) => println!("Logged in as {} (role: {role})", me.email),
                None => println!("Logged in as {}", me.email),
            }
            println!("Credentials saved to {}", Credentials::path_for_display());
            Ok(())
        }
        Err(GetMeError::Forbidden(_)) => {
            // The expected state for a brand-new Keycloak account: the token
            // is valid, the authorization isn't. This message is the main
            // diagnostic the user will see, so it names the exact roles and
            // says who has to act. The response body is deliberately not
            // echoed — it adds nothing over this explanation.
            println!();
            println!("Credentials saved to {}", Credentials::path_for_display());
            eprintln!();
            eprintln!(
                "Signed in successfully, but this account is NOT authorized to use Visdom Trace."
            );
            eprintln!(
                "It lacks the `tracing` Keycloak realm role. An administrator must grant your \
                 account the `tracing` realm role (or `tracing-admin` for admin access) in \
                 Keycloak; after that, re-run any TraceVault command — no new login is needed."
            );
            // A second, less common cause of the same 403: the realm's client
            // has no audience mapper, so the token is not FOR this server at
            // all. Only mentioned when the token provably lacks the audience
            // the server advertises — a wrong guess here would send an admin
            // looking at the wrong setting, so "cannot tell" stays silent.
            if let Some(expected) = &config.audience {
                if let Some(found) = oidc::unverified_audiences(&access_token) {
                    if !found.iter().any(|a| a == expected) {
                        eprintln!();
                        eprintln!(
                            "Note: the issued token's audience is [{}], but this server expects \
                             '{expected}'. If granting the role does not help, the realm's \
                             `{}` client is missing its audience mapper for '{expected}'.",
                            found.join(", "),
                            config.cli_client_id
                        );
                    }
                }
            }
            Err("account lacks the `tracing` Keycloak realm role".into())
        }
        Err(GetMeError::Unauthorized) => {
            println!();
            println!("Credentials saved to {}", Credentials::path_for_display());
            Err(
                "the server rejected the freshly issued token — check that the server and the \
                 CLI point at the same Keycloak realm and audience"
                    .into(),
            )
        }
        Err(e) => {
            // Network/server hiccup only: the credential itself is good, so
            // this is a warning and the command still succeeds.
            println!();
            println!("Credentials saved to {}", Credentials::path_for_display());
            eprintln!("Warning: could not confirm your identity with the server: {e}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{browser_action, is_headless, print_user_code, BrowserAction};

    use crate::credentials::Credentials;
    use crate::test_helpers::{http_json, spawn_seq_with};

    /// The four responses a successful login consumes, in order: public-config,
    /// discovery, device authorization, token. `me` is appended by the caller,
    /// which is where the interesting variation lives.
    ///
    /// `interval: 1` keeps the device response realistic while the injected
    /// no-op sleep means no test actually waits.
    fn login_responses(base: &str, me: String) -> Vec<String> {
        vec![
            http_json(
                "200 OK",
                &format!(
                    r#"{{"oidc_enabled":true,"issuer":"{base}","audience":"tracevault","cli_client_id":"tracing-cli"}}"#
                ),
            ),
            http_json(
                "200 OK",
                &format!(
                    r#"{{"issuer":"{base}","device_authorization_endpoint":"{base}/device","token_endpoint":"{base}/token","revocation_endpoint":"{base}/revoke"}}"#
                ),
            ),
            http_json(
                "200 OK",
                &format!(
                    r#"{{"device_code":"dc","user_code":"WDJB-MJHT","verification_uri":"{base}/device/verify","expires_in":600,"interval":1}}"#
                ),
            ),
            http_json(
                "200 OK",
                r#"{"access_token":"the-at","refresh_token":"the-rt","expires_in":300}"#,
            ),
            me,
        ]
    }

    /// Drive a whole login against a sequenced fake server whose `/auth/me`
    /// answers `me`. Returns login's result plus the credentials file afterwards.
    async fn run_login(me: String) -> (Result<(), String>, Option<Credentials>) {
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (base, _rx) = spawn_seq_with(|base| login_responses(base, me));
        // No-op sleep: the poll interval must not cost real seconds.
        let result = super::login_with(&base, true, |_| std::future::ready(()))
            .await
            .map_err(|e| e.to_string());
        let saved = Credentials::load();
        (result, saved)
    }

    /// The happy path: credentials saved AND `email` backfilled from `/auth/me`.
    #[tokio::test]
    async fn a_successful_login_saves_the_credentials_and_backfills_the_email() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let (result, saved) = run_login(http_json(
            "200 OK",
            r#"{"user_id":"11111111-1111-4111-8111-111111111111","email":"alice@example.com","name":"Alice","role":"tracing-admin"}"#,
        ))
        .await;

        result.expect("a complete flow must succeed");
        let saved = saved.expect("credentials must be on disk");
        assert_eq!(saved.email, "alice@example.com", "email must be backfilled");
        let auth = saved.auth.expect("the session must be saved");
        assert_eq!(auth.refresh_token, "the-rt");
        assert_eq!(auth.access_token, "the-at");
        assert_eq!(auth.client_id, "tracing-cli");
    }

    /// The central invariant, and the state every new Keycloak account starts
    /// in: the token is valid, the authorization is not. The credentials MUST
    /// survive — re-running login would change nothing, an admin has to grant a
    /// role — and the command must still exit non-zero.
    #[tokio::test]
    async fn a_403_keeps_the_credentials_and_still_fails() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let (result, saved) = run_login(http_json(
            "403 Forbidden",
            r#"{"error":"missing required role"}"#,
        ))
        .await;

        let err = result.expect_err("a 403 must not report success");
        assert!(
            err.contains("tracing") && err.contains("role"),
            "the error must name the missing realm role: {err}"
        );
        let saved = saved.expect("a 403 must NOT throw the credentials away");
        assert!(saved.auth.is_some(), "the session must still be saved");
        assert_eq!(
            saved.email, "",
            "email is only backfilled on success, and must not be invented"
        );
    }

    /// A 401 on a token the IdP just issued means the server and the CLI
    /// disagree about the realm/audience.
    ///
    /// Note what the client does first: a 401 with a Keycloak credential
    /// force-refreshes and retries once (`send_authed`), so a DEFINITIVE 401
    /// needs the refresh round trip and the retry served too — otherwise the
    /// failed refresh is reported as a network error and login (correctly)
    /// treats it as "cannot confirm" and succeeds with a warning. Either way the
    /// credential is kept; this covers the definitive branch.
    #[tokio::test]
    async fn a_401_keeps_the_credentials_and_still_fails() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let unauthorized = || http_json("401 Unauthorized", r#"{"error":"invalid token"}"#);
        let (base, _rx) = spawn_seq_with(|base| {
            let mut responses = login_responses(base, unauthorized());
            // The forced refresh: discovery, then a token grant.
            responses.push(http_json(
                "200 OK",
                &format!(
                    r#"{{"issuer":"{base}","token_endpoint":"{base}/token","revocation_endpoint":"{base}/revoke"}}"#
                ),
            ));
            responses.push(http_json(
                "200 OK",
                r#"{"access_token":"at2","refresh_token":"rt2","expires_in":300}"#,
            ));
            // ...and the retried `/auth/me`, still rejected.
            responses.push(unauthorized());
            responses
        });

        let err = super::login_with(&base, true, |_| std::future::ready(()))
            .await
            .expect_err("a definitive 401 must not report success");
        let err = err.to_string();
        assert!(
            err.contains("realm") || err.contains("audience"),
            "the error should point at the realm/audience mismatch: {err}"
        );
        assert!(
            Credentials::load().and_then(|c| c.auth).is_some(),
            "a 401 must not throw away a credential the IdP just issued"
        );
    }

    /// A network failure while confirming identity must not lose the login: the
    /// token exchange already succeeded, so the credential is good and the
    /// command reports success with a warning.
    #[tokio::test]
    async fn a_network_failure_confirming_identity_keeps_the_credentials() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        // Only four responses are served, so the fifth request — `/auth/me` —
        // hits a closed listener.
        let (base, _rx) = spawn_seq_with(|base| {
            let mut r = login_responses(base, String::new());
            r.truncate(4);
            r
        });

        super::login_with(&base, true, |_| std::future::ready(()))
            .await
            .expect("an unreachable server must not fail a completed login");

        let saved = Credentials::load().expect("credentials must survive a network failure");
        assert!(saved.auth.is_some());
        assert_eq!(saved.email, "");
    }

    /// `oidc_enabled: false` must write NOTHING: there is no session to save,
    /// and a stale credentials file would be worse than none.
    #[tokio::test]
    async fn a_server_without_keycloak_writes_no_credentials() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let (base, _rx) =
            spawn_seq_with(|_| vec![http_json("200 OK", r#"{"oidc_enabled":false}"#)]);

        let err = super::login_with(&base, true, |_| std::future::ready(()))
            .await
            .expect_err("login cannot succeed against a server with no Keycloak");
        assert!(err.to_string().contains("no Keycloak"), "{err}");
        assert!(
            Credentials::load().is_none(),
            "no credentials may be written when there was never a session"
        );
    }

    /// A `verification_uri_complete` the IdP hands us is attacker-influenced
    /// input if the realm is compromised or simply broken. It must never reach
    /// `open::that`, which would launch the handler registered for its scheme.
    #[test]
    fn a_non_web_verification_url_is_never_opened() {
        for hostile in [
            "file:///etc/passwd",
            "file:///Users/me/.ssh/id_rsa",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vscode://file/etc/passwd",
            "not a url at all",
            "",
        ] {
            assert_eq!(
                browser_action(hostile, false, false),
                BrowserAction::Refuse,
                "{hostile} must not be handed to the desktop URL handler"
            );
        }
    }

    #[test]
    fn a_web_verification_url_is_opened_unless_suppressed() {
        let url = "https://idp.example.com/device?user_code=WDJB-MJHT";
        assert_eq!(browser_action(url, false, false), BrowserAction::Open);
        assert_eq!(
            browser_action(url, true, false),
            BrowserAction::Skip,
            "--no-browser must suppress the open"
        );
        assert_eq!(
            browser_action(url, false, true),
            BrowserAction::Skip,
            "a headless environment must suppress the open"
        );
        // Plain http is legitimate for a local Keycloak.
        assert_eq!(
            browser_action("http://localhost:8080/device", false, false),
            BrowserAction::Open
        );
    }

    /// Holds the shared env lock and restores through `EnvVarGuard` rather than
    /// setting the variable raw. The previous version asserted in a SAFETY
    /// comment that nothing else touched `TRACEVAULT_NO_BROWSER`; once the login
    /// flow tests existed that was no longer true, leaving a window in which they
    /// could clear it mid-assertion on a desktop with `DISPLAY` set (where
    /// `is_headless()` has no other reason to be true).
    #[test]
    fn tracevault_no_browser_env_forces_headless() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("TRACEVAULT_NO_BROWSER", "1");
        assert!(is_headless());
    }

    /// The box drawn around the user code must line up for any code length —
    /// Keycloak's default is `XXXX-XXXX`, but the format is not guaranteed.
    #[test]
    fn user_code_banner_does_not_panic_on_any_code_length() {
        for code in ["", "A", "WDJB-MJHT", "VERYLONGDEVICECODE-123456"] {
            print_user_code(code);
        }
    }
}
