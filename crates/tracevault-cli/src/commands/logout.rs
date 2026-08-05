//! `tracevault logout` — drop the local credential, and for a Keycloak
//! session also revoke the refresh token at the IdP.
//!
//! Revocation is best-effort by design: the local file is what makes this
//! machine authenticated, so removing it must succeed even when the IdP is
//! unreachable. A user who couldn't log out because a network call failed
//! would be left holding a live credential they believe is gone.

use crate::credentials::{Credential, Credentials, KeycloakSession};
use crate::oidc;

pub async fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let creds = Credentials::load().ok_or("Not logged in. No credentials file found.")?;

    // An API key is not revocable from here (it is managed server-side), so
    // only a Keycloak session has anything to revoke.
    if let Some(Credential::Keycloak(session)) = creds.credential() {
        match revoke_session(&session).await {
            Ok(()) => println!("Keycloak session revoked."),
            Err(e) => eprintln!("Warning: could not revoke the Keycloak session: {e}"),
        }
    }

    Credentials::delete()?;
    println!("Logged out. Credentials removed.");
    Ok(())
}

/// Discover the realm's revocation endpoint and revoke the refresh token.
///
/// Split out so the "revocation failed" path is a plain `Err` the caller
/// downgrades to a warning, instead of control flow tangled into `logout`.
async fn revoke_session(session: &KeycloakSession) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let discovery = oidc::discover(&http, &session.issuer).await?;
    oidc::revoke(
        &http,
        &discovery,
        &session.client_id,
        &session.refresh_token,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Losing the credentials file is the ONLY thing logout must guarantee.
    /// The issuer here points at a port nothing listens on, so discovery
    /// (and therefore revocation) fails — the file must still be gone.
    #[tokio::test]
    async fn logout_removes_credentials_even_when_revocation_fails() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(
            creds_dir.join("credentials.json"),
            r#"{"server_url":"https://example.com","email":"a@b.com","auth":{
                "issuer":"http://127.0.0.1:1","client_id":"tracevault-cli",
                "refresh_token":"rt","access_token":"at","access_expires_at":9999999999}}"#,
        )
        .unwrap();

        logout()
            .await
            .expect("a failed revocation must not fail logout");
        assert!(
            !Credentials::path().exists(),
            "the credentials file must be removed even when revocation fails"
        );
    }

    /// An API-key credential has nothing to revoke; logout is a local delete.
    #[tokio::test]
    async fn logout_with_api_key_credential_just_deletes_the_file() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(
            creds_dir.join("credentials.json"),
            r#"{"server_url":"https://example.com","token":"tvk_abc","email":"a@b.com"}"#,
        )
        .unwrap();

        logout().await.expect("api-key logout must succeed");
        assert!(!Credentials::path().exists());
    }

    #[tokio::test]
    async fn logout_without_credentials_reports_not_logged_in() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let err = logout().await.expect_err("no credentials must be an error");
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }
}
