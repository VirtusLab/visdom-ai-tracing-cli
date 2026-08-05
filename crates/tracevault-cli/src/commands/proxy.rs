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

    let server_url = creds.server_url.trim_end_matches('/');
    let proxy_url = format!("{server_url}/proxy/anthropic");
    let creds_path = Credentials::path();

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
    // credential. A Keycloak access token is refreshed every few minutes and
    // would silently stop working — say so instead of pointing at a "token"
    // field that no longer exists in that file.
    match creds.credential() {
        Some(Credential::Keycloak(_)) => {
            println!(
                "     {ANSI_DIM}This machine is signed in with a Keycloak session, whose access \
                 token expires every few minutes — it cannot be pasted here as a static value. \
                 Use a TraceVault API key (tvk_...) for the proxy instead.{ANSI_RESET}"
            );
        }
        _ => {
            println!(
                "     {ANSI_DIM}Your TraceVault token lives in {} as the \"token\" field.{ANSI_RESET}",
                creds_path.display()
            );
        }
    }
    println!();
    println!("  3. Run your AI tool as usual. Requests go through TraceVault and are");
    println!("     forwarded to api.anthropic.com using the Anthropic key you stored");
    println!("     in step 1.");

    0
}
