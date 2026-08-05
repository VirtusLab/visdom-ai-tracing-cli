//! The on-disk credential (`~/.config/tracevault/credentials.json`) and the
//! two kinds of authentication the CLI supports.
//!
//! There are exactly two, and they are not interchangeable:
//!
//! * **`tvk_` API key** — what automation and CI use, forever. Never
//!   expires, never refreshes, comes from `TRACEVAULT_API_KEY`, a project
//!   `config.toml`, or the `token` field of this file.
//! * **Keycloak session** — what a human gets from `tracevault login`
//!   (RFC 8628 device flow). Short-lived access token plus an
//!   `offline_access` refresh token, stored under the `auth` object, and
//!   refreshed transparently so unattended git hooks never prompt.
//!
//! The file shape is additive: a pre-Keycloak file
//! (`{"server_url","token","email"}`, possibly still carrying a pre
//! single-tenant `org_slug`) keeps working untouched.

use crate::oidc::TokenSet;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// How long before expiry an access token is proactively refreshed. Sized to
/// comfortably cover a slow request that is issued just under the wire: a
/// token that would expire mid-flight is refreshed first instead.
pub const REFRESH_WINDOW_SECS: i64 = 60;

/// A live Keycloak session for the human who ran `tracevault login`.
///
/// `issuer` and `client_id` are stored alongside the tokens because a
/// refresh needs them and must not depend on the TraceVault server being
/// reachable to re-read its public config.
#[derive(Clone, Serialize, Deserialize)]
pub struct KeycloakSession {
    pub issuer: String,
    pub client_id: String,
    /// SECRET. Long-lived (`offline_access`); this is the credential that
    /// makes unattended hooks work.
    pub refresh_token: String,
    /// SECRET. Short-lived bearer presented to the TraceVault server.
    pub access_token: String,
    /// Absolute unix seconds at which `access_token` stops being valid.
    pub access_expires_at: i64,
}

impl KeycloakSession {
    /// Whether `access_token` is expired or close enough to expiry
    /// ([`REFRESH_WINDOW_SECS`]) that it should be refreshed before use.
    ///
    /// `now` is passed in rather than read from the clock so callers — and
    /// tests of the window boundary — are deterministic.
    pub fn needs_refresh(&self, now: i64) -> bool {
        self.access_expires_at.saturating_sub(now) < REFRESH_WINDOW_SECS
    }

    /// Fold a fresh token response into this session.
    ///
    /// A token endpoint may omit `refresh_token` when it doesn't rotate
    /// refresh tokens; the previous one is kept in that case, since dropping
    /// it would silently downgrade the session to "expires in 5 minutes".
    pub fn apply(&mut self, tokens: &TokenSet, now: i64) {
        self.access_token = tokens.access_token.clone();
        if let Some(rt) = &tokens.refresh_token {
            self.refresh_token = rt.clone();
        }
        self.access_expires_at = tokens.access_expires_at(now);
    }
}

/// Hand-written so no `{:?}` can leak either token.
impl fmt::Debug for KeycloakSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeycloakSession")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("refresh_token", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .finish()
    }
}

/// How a request authenticates itself.
#[derive(Clone)]
pub enum Credential {
    /// A `tvk_` API key: presented verbatim, never refreshed.
    ApiKey(String),
    /// A Keycloak session: refreshed on demand.
    Keycloak(KeycloakSession),
}

/// Hand-written so no `{:?}` can leak the key or the tokens.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("Credential::ApiKey(<redacted>)"),
            Self::Keycloak(s) => write!(f, "Credential::Keycloak({s:?})"),
        }
    }
}

/// `credentials.json`.
///
/// `token` and `auth` are both optional and mutually exclusive in practice:
/// `token` is written by an API-key login, `auth` by `tracevault login`.
/// Neither is written when absent, so an API-key file stays byte-identical
/// in shape to what earlier CLI versions wrote.
#[derive(Serialize, Deserialize)]
pub struct Credentials {
    pub server_url: String,
    /// SECRET. A `tvk_` API key, when this file holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The signed-in identity, for display. Empty when a login saved
    /// credentials before `/auth/me` could resolve it (e.g. the account
    /// lacks the `tracing` realm role).
    #[serde(default)]
    pub email: String,
    /// The Keycloak session, when this file holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<KeycloakSession>,
}

/// Hand-written so no `{:?}` can leak the API key.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("server_url", &self.server_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("email", &self.email)
            .field("auth", &self.auth)
            .finish()
    }
}

impl Credentials {
    /// A Keycloak session credential file (what `tracevault login` writes).
    ///
    /// There is deliberately no `api_key` counterpart: nothing in the CLI
    /// writes an API-key credentials file (a key is supplied via
    /// `TRACEVAULT_API_KEY` or `config.toml`), it is only ever READ from a
    /// file a user or an older CLI version created.
    pub fn keycloak(server_url: &str, email: String, session: KeycloakSession) -> Self {
        Self {
            server_url: server_url.to_string(),
            token: None,
            email,
            auth: Some(session),
        }
    }

    /// Which credential this file carries. A Keycloak session wins over a
    /// leftover `token` so a re-login supersedes an old API key rather than
    /// silently continuing to use it.
    pub fn credential(&self) -> Option<Credential> {
        if let Some(session) = &self.auth {
            return Some(Credential::Keycloak(session.clone()));
        }
        self.token.clone().map(Credential::ApiKey)
    }

    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("tracevault")
            .join("credentials.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::path();
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Write the credential file atomically, readable only by its owner.
    ///
    /// Atomic because a token refresh rewrites this file while other
    /// processes (git hooks, a parallel `tracevault stream`) may be reading
    /// it: a plain truncate-then-write leaves a window in which a reader
    /// parses a half-written file and concludes the user is logged out. The
    /// temp file is created in the SAME directory so the `rename` stays
    /// within one filesystem and is therefore atomic.
    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;

        // Include the pid so two concurrent writers can't clobber each
        // other's temp file mid-write.
        let tmp = path.with_file_name(format!(".credentials.json.tmp{}", std::process::id()));

        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        // Mode is set at creation, not after: a chmod afterwards would leave
        // the tokens world-readable for the interval in between.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let write_result = opts.open(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(json.as_bytes())?;
            f.sync_all()
        });
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// Persist refreshed Keycloak tokens, keeping every other field of the
    /// file as it is on disk.
    ///
    /// Re-reads the file rather than rewriting a cached copy so a
    /// concurrently changed `server_url`/`email` isn't reverted. A missing
    /// file is not an error: there is nothing to keep in sync (the session
    /// may be held by a process whose file was removed by `tv logout`).
    pub fn persist_session(session: &KeycloakSession) -> Result<(), std::io::Error> {
        let Some(mut creds) = Self::load() else {
            return Ok(());
        };
        creds.auth = Some(session.clone());
        creds.save()
    }

    pub fn delete() -> Result<(), std::io::Error> {
        let path = Self::path();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redirect `XDG_CONFIG_HOME` at a tempdir and write `body` as the
    /// credentials file. Returns the tempdir plus both guards, which the
    /// caller must keep alive for the duration of the test (the env lock is
    /// released, and the real `~/.config` restored, when they drop).
    fn with_credentials_file(
        body: &str,
    ) -> (
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
        crate::test_helpers::EnvVarGuard,
    ) {
        let env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", dir.path());
        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(creds_dir.join("credentials.json"), body).unwrap();
        (dir, env_lock, guard)
    }

    /// Old on-disk `credentials.json` files may still contain `org_slug`
    /// from before the single-tenant org removal (mirrors config.rs's
    /// `load_old_config_with_org_slug_still_parses`). serde must silently
    /// ignore the unknown field rather than fail to parse (no
    /// `deny_unknown_fields` on `Credentials`), so `Credentials::load()` keeps
    /// working on an un-migrated credentials file.
    ///
    /// `XDG_CONFIG_HOME` is redirected to a tempdir so this reads a fixture
    /// file rather than the developer's real
    /// `~/.config/tracevault/credentials.json`; `_env_lock` serializes this
    /// against other tests in the crate that mutate the same env var (see
    /// `test_helpers::lock_env_mutation_sync`).
    #[test]
    fn load_old_credentials_with_org_slug_still_parses() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(
            creds_dir.join("credentials.json"),
            r#"{"server_url":"https://example.com","token":"tok","email":"a@b.com","org_slug":"x"}"#,
        )
        .unwrap();

        let creds =
            Credentials::load().expect("old credentials.json with org_slug must still parse");
        assert_eq!(creds.server_url, "https://example.com");
        assert_eq!(creds.token.as_deref(), Some("tok"));
        assert_eq!(creds.email, "a@b.com");
    }

    /// A pre-Keycloak flat file is an API key, not a broken Keycloak
    /// session: automation must keep working across this change.
    #[test]
    fn flat_token_file_loads_as_api_key() {
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{"server_url":"https://example.com","token":"tvk_abc","email":"a@b.com"}"#,
        );
        let creds = Credentials::load().expect("flat credentials file must load");
        match creds.credential() {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_abc"),
            other => panic!("expected an ApiKey credential, got {other:?}"),
        }
    }

    /// A file with an `auth` object is a refreshable Keycloak session.
    #[test]
    fn file_with_auth_object_loads_as_keycloak() {
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{
                "server_url":"https://example.com",
                "email":"a@b.com",
                "auth":{
                    "issuer":"https://idp.test/realms/visdom",
                    "client_id":"tracevault-cli",
                    "refresh_token":"rt",
                    "access_token":"at",
                    "access_expires_at":1893456000
                }
            }"#,
        );
        let creds = Credentials::load().expect("keycloak credentials file must load");
        match creds.credential() {
            Some(Credential::Keycloak(s)) => {
                assert_eq!(s.issuer, "https://idp.test/realms/visdom");
                assert_eq!(s.client_id, "tracevault-cli");
                assert_eq!(s.refresh_token, "rt");
                assert_eq!(s.access_token, "at");
                assert_eq!(s.access_expires_at, 1_893_456_000);
            }
            other => panic!("expected a Keycloak credential, got {other:?}"),
        }
    }

    #[test]
    fn no_token_and_no_auth_yields_no_credential() {
        let (_dir, _lock, _guard) =
            with_credentials_file(r#"{"server_url":"https://example.com","email":"a@b.com"}"#);
        let creds = Credentials::load().unwrap();
        assert!(creds.credential().is_none());
    }

    /// The refresh window is a `<` on the remaining lifetime: exactly 60s
    /// left is still usable, 59s is not.
    #[test]
    fn needs_refresh_only_inside_the_sixty_second_window() {
        let session = KeycloakSession {
            issuer: "https://idp.test/realms/visdom".into(),
            client_id: "tracevault-cli".into(),
            refresh_token: "rt".into(),
            access_token: "at".into(),
            access_expires_at: 1_000_000,
        };
        assert!(!session.needs_refresh(1_000_000 - 61));
        assert!(!session.needs_refresh(1_000_000 - 60));
        assert!(session.needs_refresh(1_000_000 - 59));
        assert!(session.needs_refresh(1_000_000));
        assert!(session.needs_refresh(1_000_001));
    }

    #[test]
    fn apply_keeps_the_previous_refresh_token_when_none_is_returned() {
        let mut session = KeycloakSession {
            issuer: "i".into(),
            client_id: "c".into(),
            refresh_token: "old-rt".into(),
            access_token: "old-at".into(),
            access_expires_at: 10,
        };
        session.apply(
            &TokenSet {
                access_token: "new-at".into(),
                refresh_token: None,
                expires_in: 300,
            },
            1_000,
        );
        assert_eq!(session.access_token, "new-at");
        assert_eq!(session.refresh_token, "old-rt");
        assert_eq!(session.access_expires_at, 1_300);

        session.apply(
            &TokenSet {
                access_token: "newer-at".into(),
                refresh_token: Some("new-rt".into()),
                expires_in: 60,
            },
            2_000,
        );
        assert_eq!(session.refresh_token, "new-rt");
        assert_eq!(session.access_expires_at, 2_060);
    }

    /// A saved Keycloak file must round-trip, must not invent a `token`
    /// field, and (on unix) must not be readable by anyone else.
    #[test]
    fn save_round_trips_keycloak_and_is_owner_only() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let session = KeycloakSession {
            issuer: "https://idp.test/realms/visdom".into(),
            client_id: "tracevault-cli".into(),
            refresh_token: "rt".into(),
            access_token: "at".into(),
            access_expires_at: 42,
        };
        Credentials::keycloak("https://example.com", "a@b.com".into(), session)
            .save()
            .unwrap();

        let raw = fs::read_to_string(Credentials::path()).unwrap();
        assert!(
            !raw.contains("\"token\""),
            "a Keycloak file must not carry an api-key `token` field: {raw}"
        );
        let reloaded = Credentials::load().unwrap();
        assert!(matches!(
            reloaded.credential(),
            Some(Credential::Keycloak(_))
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(Credentials::path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "credentials must be owner-only");
        }

        // The atomic-write temp file must not be left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("tracevault"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "credentials.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// A refresh must update only the `auth` block — the file's `email`
    /// (used by `tracevault status`) has to survive.
    #[test]
    fn persist_session_preserves_other_fields() {
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{
                "server_url":"https://example.com",
                "email":"a@b.com",
                "auth":{"issuer":"i","client_id":"c","refresh_token":"rt","access_token":"at","access_expires_at":1}
            }"#,
        );
        let mut session = match Credentials::load().unwrap().credential() {
            Some(Credential::Keycloak(s)) => s,
            other => panic!("expected Keycloak, got {other:?}"),
        };
        session.apply(
            &TokenSet {
                access_token: "at2".into(),
                refresh_token: Some("rt2".into()),
                expires_in: 300,
            },
            1_000,
        );
        Credentials::persist_session(&session).unwrap();

        let reloaded = Credentials::load().unwrap();
        assert_eq!(reloaded.email, "a@b.com");
        assert_eq!(reloaded.server_url, "https://example.com");
        let auth = reloaded.auth.unwrap();
        assert_eq!(auth.access_token, "at2");
        assert_eq!(auth.refresh_token, "rt2");
        assert_eq!(auth.access_expires_at, 1_300);
    }

    #[test]
    fn persist_session_on_a_missing_file_is_not_an_error() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let session = KeycloakSession {
            issuer: "i".into(),
            client_id: "c".into(),
            refresh_token: "rt".into(),
            access_token: "at".into(),
            access_expires_at: 1,
        };
        Credentials::persist_session(&session)
            .expect("a logged-out user must not turn a refresh into a hard error");
        assert!(
            !Credentials::path().exists(),
            "persist_session must not resurrect a deleted credentials file"
        );
    }

    #[test]
    fn debug_impls_redact_secrets() {
        let creds = Credentials {
            server_url: "https://example.com".into(),
            token: Some("tvk_secret".into()),
            email: "a@b".into(),
            auth: None,
        };
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("tvk_secret"), "api key leaked: {dbg}");

        let creds = Credentials::keycloak(
            "https://example.com",
            "a@b".into(),
            KeycloakSession {
                issuer: "i".into(),
                client_id: "c".into(),
                refresh_token: "secret-rt".into(),
                access_token: "secret-at".into(),
                access_expires_at: 1,
            },
        );
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("secret-rt"), "refresh token leaked: {dbg}");
        assert!(!dbg.contains("secret-at"), "access token leaked: {dbg}");
    }
}
