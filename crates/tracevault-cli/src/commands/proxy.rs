//! `tracevault proxy info` — print the TraceVault LLM proxy configuration
//! a user needs to point their AI tool (Claude Code, GSD2, Cursor, etc.) at
//! the proxy.
//!
//! Read-only and purely local: never calls the network. Output is intended
//! to be copy-pasted directly into a shell or tool config.

use crate::credentials::{Credential, Credentials};

const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Print the proxy configuration. Returns process exit code: 0 on success,
/// 1 when no credentials are available (user has not logged in).
pub fn run_proxy_info() -> i32 {
    let creds = match Credentials::load() {
        Some(c) => c,
        None => {
            eprintln!(
                "Not logged in. Run `tracevault login --server-url <url>` first \
                 to obtain a TraceVault session token, then try again."
            );
            eprintln!(
                "Credentials file expected at: {}",
                Credentials::path().display()
            );
            return 1;
        }
    };

    let creds_path = Credentials::path();

    // Resolved BEFORE any instructions are printed: the setup script below
    // cannot be followed without a credential, so bailing here avoids
    // sandwiching the error between steps the user is being told to perform.
    let credential = match creds.credential() {
        Some(c) => c,
        None => {
            eprintln!(
                "The credentials file at {} holds no usable credential (no API key and no \
                 Keycloak session).",
                creds_path.display()
            );
            eprintln!(
                "Run `tracevault login --server-url <url>`, or set TRACEVAULT_API_KEY to a \
                 TraceVault API key (tvk_...)."
            );
            return 1;
        }
    };

    let server_url = creds.server_url.trim_end_matches('/');
    let proxy_url = format!("{server_url}/proxy/anthropic");

    println!("{ANSI_BOLD}TraceVault LLM Proxy{ANSI_RESET}");
    println!();
    println!("  Server:           {server_url}");
    println!("  Proxy base URL:   {ANSI_BOLD}{proxy_url}{ANSI_RESET}");
    println!("  Credentials file: {}", creds_path.display());
    println!();
    println!("{ANSI_BOLD}Setup{ANSI_RESET}");
    println!();
    println!("  1. Configure your Anthropic API key once at:");
    println!("       {server_url}/me/proxy");
    println!();
    println!("  2. Set these environment variables for your AI tool:");
    println!();
    println!("       {ANSI_BOLD}export ANTHROPIC_BASE_URL=\"{proxy_url}\"{ANSI_RESET}");
    println!(
        "       {ANSI_BOLD}export ANTHROPIC_API_KEY=\"<your TraceVault session token>\"{ANSI_RESET}"
    );
    println!();
    // `ANTHROPIC_API_KEY` is static tool configuration, so it needs a static
    // credential. Both arms are spelled out rather than using a catch-all, so a
    // future `Credential` variant cannot silently inherit the API-key wording.
    match credential {
        Credential::ApiKey(_) => {
            println!(
                "     {ANSI_DIM}Your TraceVault token lives in {} as the \"token\" field.{ANSI_RESET}",
                creds_path.display()
            );
        }
        // A Keycloak access token is refreshed every few minutes and would
        // silently stop working if pasted into a static env var.
        Credential::Keycloak(_) => {
            println!(
                "     {ANSI_DIM}This machine is signed in with a Keycloak session, whose access \
                 token expires every few minutes — it cannot be pasted here as a static value. \
                 Use a TraceVault API key (tvk_...) for the proxy instead.{ANSI_RESET}"
            );
        }
    }
    println!();
    println!("  3. Run your AI tool as usual. Requests go through TraceVault and are");
    println!("     forwarded to api.anthropic.com using the Anthropic key you stored");
    println!("     in step 1.");

    0
}

#[cfg(test)]
mod tests {
    use super::run_proxy_info;
    use std::fs;

    /// Redirect `XDG_CONFIG_HOME` at a tempdir containing `body` as the
    /// credentials file (or no file at all when `body` is `None`), run
    /// `run_proxy_info`, and return its exit code.
    fn exit_code_for(body: Option<&str>) -> i32 {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        if let Some(body) = body {
            let creds_dir = dir.path().join("tracevault");
            fs::create_dir_all(&creds_dir).unwrap();
            fs::write(creds_dir.join("credentials.json"), body).unwrap();
        }
        run_proxy_info()
    }

    /// A credentials file with neither a `token` nor an `auth` object holds
    /// nothing the user can paste. Reporting success and pointing at a "token"
    /// field that isn't in the file sends them looking for it.
    #[test]
    fn a_file_with_no_usable_credential_exits_non_zero() {
        let code = exit_code_for(Some(
            r#"{"server_url":"https://example.com","email":"a@b.com"}"#,
        ));
        assert_eq!(
            code, 1,
            "a file with no usable credential must not report success"
        );
    }

    #[test]
    fn an_api_key_file_still_succeeds() {
        let code = exit_code_for(Some(
            r#"{"server_url":"https://example.com","token":"tvk_abc","email":"a@b.com"}"#,
        ));
        assert_eq!(code, 0);
    }

    #[test]
    fn a_keycloak_file_still_succeeds() {
        let code = exit_code_for(Some(
            r#"{"server_url":"https://example.com","email":"a@b.com","auth":{"issuer":"i","client_id":"c","refresh_token":"rt","access_token":"at","access_expires_at":1}}"#,
        ));
        assert_eq!(code, 0);
    }

    /// The pre-existing contract: no credentials file at all is exit 1.
    #[test]
    fn a_missing_credentials_file_exits_non_zero() {
        assert_eq!(exit_code_for(None), 1);
    }
}
