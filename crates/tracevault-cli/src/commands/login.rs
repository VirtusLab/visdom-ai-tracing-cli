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

    if no_browser || is_headless() {
        println!("Not attempting to auto-open a browser (headless environment detected or --no-browser set).");
    } else {
        println!("Attempting to open the URL in your default browser...");
        if let Err(e) = open::that(browser_url) {
            // Non-fatal: the URL and code are already visible above, the user
            // can just copy them.
            eprintln!("Could not open browser automatically: {e}");
            eprintln!("Copy the URL above into a browser manually.");
        }
    }

    // 4. Wait for the user to finish in the browser.
    println!("Waiting for you to approve the sign-in...");
    let tokens = oidc::poll_token(&http, &discovery, &config.cli_client_id, &device).await?;

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
            println!("Credentials saved to {}", Credentials::path().display());
            Ok(())
        }
        Err(GetMeError::Forbidden(_)) => {
            // The expected state for a brand-new Keycloak account: the token
            // is valid, the authorization isn't. This message is the main
            // diagnostic the user will see, so it names the exact roles and
            // says who has to act. The response body is deliberately not
            // echoed — it adds nothing over this explanation.
            println!();
            println!("Credentials saved to {}", Credentials::path().display());
            eprintln!();
            eprintln!(
                "Signed in successfully, but this account is NOT authorized to use Visdom Trace."
            );
            eprintln!(
                "It lacks the `tracing` Keycloak realm role. An administrator must grant your \
                 account the `tracing` realm role (or `tracing-admin` for admin access) in \
                 Keycloak; after that, re-run any TraceVault command — no new login is needed."
            );
            Err("account lacks the `tracing` Keycloak realm role".into())
        }
        Err(GetMeError::Unauthorized) => {
            println!();
            println!("Credentials saved to {}", Credentials::path().display());
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
            println!("Credentials saved to {}", Credentials::path().display());
            eprintln!("Warning: could not confirm your identity with the server: {e}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_headless, print_user_code};

    #[test]
    fn tracevault_no_browser_env_forces_headless() {
        // SAFETY: test-scoped env mutation. serial_test is not available, but
        // this test only reads a variable name nothing else touches.
        unsafe {
            std::env::set_var("TRACEVAULT_NO_BROWSER", "1");
        }
        assert!(is_headless());
        unsafe {
            std::env::remove_var("TRACEVAULT_NO_BROWSER");
        }
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
