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

/// Whether two TraceVault base URLs identify the same instance.
///
/// Needed because the two sides reach us differently formatted from the same
/// user-supplied string: `ApiClient` trims trailing slashes from its
/// `base_url`, while `credentials.json` stores `server_url` exactly as it was
/// passed to `tracevault login`. So `https://tv.example.com/` and
/// `https://tv.example.com` must compare equal, or a legitimate session would
/// never be adopted.
///
/// Comparison is otherwise byte-exact (no host lowercasing, no default-port
/// folding). That is deliberate: both values originate from the same string in
/// every real flow, and the two failure directions are not symmetric. A false
/// negative costs one extra token refresh; a false positive would send an
/// access token to a host it was not minted for. When in doubt, don't match.
pub fn same_server(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('/').to_string();
    norm(a) == norm(b)
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
    /// # The single policy both writers follow
    ///
    /// **A write never destroys a credential it did not come to replace.** This
    /// constructor and [`Self::persist_session`] are the only two writers, and
    /// they now agree: `persist_session` replaces `auth` and keeps everything
    /// else, and this preserves any pre-existing `token` (an API key) instead of
    /// dropping it. Logging in supersedes a key without deleting it, which is
    /// safe because [`Self::credential`] already prefers a Keycloak session over
    /// a leftover `token` — so the key is inert while the session lives, and
    /// still there if the session is later removed.
    ///
    /// The token is only carried over when the existing file is for the SAME
    /// server ([`same_server`]): moving another instance's key into this
    /// instance's file would be the same cross-instance mixing the resolution
    /// guards exist to prevent.
    ///
    /// There is deliberately no `api_key` counterpart constructor: nothing in
    /// the CLI creates an API-key credentials file (a key comes from
    /// `TRACEVAULT_API_KEY` or `config.toml`); such a file is only ever READ,
    /// from one a user or an older CLI version wrote.
    pub fn keycloak(server_url: &str, email: String, session: KeycloakSession) -> Self {
        let preserved_token = Self::load()
            .filter(|existing| same_server(&existing.server_url, server_url))
            .and_then(|existing| existing.token);
        Self {
            server_url: server_url.to_string(),
            token: preserved_token,
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

    /// Where the credential file lives, or an error naming what is missing.
    ///
    /// Returns `Err` rather than inventing a path. The previous fallback,
    /// `PathBuf::from("~/.config")`, is a RELATIVE path — a shell expands `~`,
    /// `Path` does not — so with no resolvable config directory `login` created
    /// `./~/.config/tracevault/` in the current working directory and wrote an
    /// offline refresh token into it, plausibly inside the user's repo. There is
    /// no defensible guess for "where is this user's config", so failing loudly
    /// is the only correct answer.
    pub fn path() -> Result<PathBuf, std::io::Error> {
        Self::path_in(dirs::config_dir())
    }

    /// [`Self::path`] with the config directory injected, so the
    /// no-config-directory branch is testable.
    ///
    /// It cannot be reached through the environment on glibc Linux — with `HOME`
    /// unset `dirs::config_dir()` falls back to the passwd database — so in the
    /// wild it needs something more unusual (a container with no passwd entry
    /// for the uid, some static builds). That makes it rarer than "HOME unset",
    /// but no less wrong to guess at.
    fn path_in(config_dir: Option<PathBuf>) -> Result<PathBuf, std::io::Error> {
        let dir = config_dir.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot determine the user config directory: set XDG_CONFIG_HOME (or HOME) so \
                 credentials can be stored under $XDG_CONFIG_HOME/tracevault/",
            )
        })?;
        Ok(dir.join("tracevault").join("credentials.json"))
    }

    /// The credential file's path for DISPLAY, when there is nothing useful to
    /// do about a missing config directory (a message is being printed either
    /// way). Never used to read or write.
    pub fn path_for_display() -> String {
        match Self::path() {
            Ok(p) => p.display().to_string(),
            Err(_) => "<no user config directory: set XDG_CONFIG_HOME or HOME>".to_string(),
        }
    }

    pub fn load() -> Option<Self> {
        let path = Self::path().ok()?;
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
    ///
    /// The temp file is created with `create_new(true)`, which is what makes
    /// the 0600 promise real: `OpenOptions::mode()` applies ONLY when the file
    /// is created, so opening a path that already exists inherits whatever mode
    /// (or symlink target) is already there, and the following `rename` would
    /// then publish that mode as the credential file's. A stale
    /// `.credentials.json.tmp*` left by an earlier crash under a looser umask
    /// is enough to defeat it. `create_new` turns that case into
    /// `AlreadyExists`, which we resolve by picking a fresh name rather than
    /// failing the save.
    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;

        let mut opts = fs::OpenOptions::new();
        // `create_new` also refuses to follow a symlink planted at the temp
        // path, so tokens cannot be written outside this directory.
        opts.write(true).create_new(true);
        // Mode is set at creation, not after: a chmod afterwards would leave
        // the tokens world-readable for the interval in between.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        // A few attempts, since each `AlreadyExists` means a genuinely
        // different name is tried next (the counter is process-local and
        // monotonic, and the nanos component differs per attempt). Bounded so a
        // pathological directory can't spin here forever.
        const ATTEMPTS: usize = 8;
        let mut last_err = None;
        for _ in 0..ATTEMPTS {
            let tmp = path.with_file_name(Self::temp_file_name());
            let file = match opts.open(&tmp) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };

            let write_result = (|| {
                use std::io::Write;
                let mut file = file;
                file.write_all(json.as_bytes())?;
                file.sync_all()
            })();
            if let Err(e) = write_result {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            if let Err(e) = fs::rename(&tmp, &path) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            // Bound the accumulation of abandoned temp files, each of which
            // holds a full credential including the refresh token.
            Self::sweep_stale_temp_files();
            return Ok(());
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not create a unique temporary credentials file",
            )
        }))
    }

    /// Filename prefix of the atomic-write temp files. Anything matching this
    /// contains a full credential JSON — including the offline refresh token —
    /// so leftovers are credential material, not scratch data.
    const TEMP_PREFIX: &'static str = ".credentials.json.tmp";

    /// A temp file this old cannot still be in flight: a save is a couple of
    /// syscalls. Only files older than this are swept, so a concurrent save in
    /// another process never has its temp file deleted out from under it (which
    /// would make its `rename` fail and turn a benign race into an error).
    const TEMP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

    /// Best-effort removal of abandoned temp files, oldest-first semantics by
    /// age threshold. Never reports failure: this is hygiene, and a stray file
    /// we cannot delete must not fail the save or the logout that triggered it.
    fn sweep_stale_temp_files() {
        Self::remove_temp_files(Some(Self::TEMP_STALE_AFTER));
    }

    /// Remove atomic-write temp files in the credential directory. With
    /// `min_age`, only files at least that old; without it, all of them (used
    /// by `delete`, where the user is logging out and nothing should survive).
    fn remove_temp_files(min_age: Option<std::time::Duration>) {
        let Some(dir) = Self::path()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
        else {
            return;
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(Self::TEMP_PREFIX) {
                continue;
            }
            if let Some(min_age) = min_age {
                let recent = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .and_then(|t| t.elapsed().map_err(std::io::Error::other))
                    .map(|age| age < min_age)
                    // A clock skew or unreadable mtime means "can't prove it's
                    // stale" — leave it rather than risk deleting a live temp.
                    .unwrap_or(true);
                if recent {
                    continue;
                }
            }
            let _ = fs::remove_file(entry.path());
        }
    }

    /// A temp file name unique per process AND per call within a process.
    ///
    /// The pid alone is not enough: two saves in one process (a refresh racing
    /// the second `save()` of a login) would collide on the same path, and with
    /// `create_new` that turns into a spurious failure. The counter makes it
    /// unique within the process; the nanos make a name unlikely to collide
    /// with a stale file left by a previous process that had the same pid.
    fn temp_file_name() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!(
            "{}{}.{}.{}",
            Self::TEMP_PREFIX,
            std::process::id(),
            n,
            nanos
        )
    }

    /// Persist refreshed Keycloak tokens for `server_url`, keeping every other
    /// field of the file as it is on disk — including a `token` (API key),
    /// per the single write policy documented on [`Self::keycloak`].
    ///
    /// Re-reads the file rather than rewriting a cached copy so a
    /// concurrently changed `server_url`/`email` isn't reverted. A missing
    /// file is not an error: there is nothing to keep in sync (the session
    /// may be held by a process whose file was removed by `tv logout`).
    ///
    /// Writes NOTHING when the file on disk belongs to a different server (see
    /// [`same_server`]). There is one credentials file, so a long-running
    /// process holding instance A's session would otherwise overwrite the
    /// credentials of instance B that the user logged into meanwhile —
    /// silently signing them out of B. Skipping is not an error: A's refreshed
    /// token still works for the rest of A's process lifetime.
    ///
    /// The skip warns, because it is otherwise undiagnosable from the outside:
    /// if the two URLs differ only by formatting the mismatch is PERMANENT, so
    /// persistence is disabled for every future invocation and the CLI silently
    /// re-refreshes forever.
    pub fn persist_session(
        server_url: &str,
        session: &KeycloakSession,
    ) -> Result<(), std::io::Error> {
        let Some(mut creds) = Self::load() else {
            return Ok(());
        };
        if !same_server(&creds.server_url, server_url) {
            eprintln!(
                "Warning: not saving the refreshed token — the credentials file is for \
                 '{}', but this session is for '{}'. The refreshed token applies to this \
                 process only; every later command will refresh again.",
                creds.server_url, server_url
            );
            return Ok(());
        }
        creds.auth = Some(session.clone());
        creds.save()
    }

    /// Remove the credential file — and any atomic-write temp files left
    /// behind.
    ///
    /// The sweep is the difference between "logout" and "logout, except for the
    /// refresh tokens still on disk": every `.credentials.json.tmp*` holds a
    /// full credential JSON, so a temp file abandoned by an earlier crash
    /// between `open` and `rename` would keep a working offline refresh token
    /// in the config directory after the user asked to be logged out. No age
    /// threshold here (unlike [`Self::sweep_stale_temp_files`]): the user is
    /// logging out, so a save racing this is already a lost race — and its
    /// failed `rename` cannot resurrect the credential file, since the very
    /// next thing that happens is this delete.
    ///
    /// The sweep is best-effort: a stray file that cannot be removed must not
    /// fail the logout.
    pub fn delete() -> Result<(), std::io::Error> {
        let path = Self::path()?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Self::remove_temp_files(None);
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

        let raw = fs::read_to_string(Credentials::path().unwrap()).unwrap();
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
            let mode = fs::metadata(Credentials::path().unwrap())
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

    /// A stale temp file must not donate its mode to the credential file.
    ///
    /// `OpenOptions::mode()` only applies at CREATION, so opening an existing
    /// path keeps that path's mode — and the `rename` would then publish it as
    /// `credentials.json`'s. A crash under a looser umask is enough to leave
    /// such a file behind. This test pre-creates the exact temp path the old
    /// pid-only scheme used, with mode 0644; it fails (0644) without
    /// `create_new` + a unique suffix, and passes (0600) with them.
    #[cfg(unix)]
    #[test]
    fn a_stale_world_readable_temp_file_cannot_relax_the_credential_mode() {
        use std::os::unix::fs::PermissionsExt;

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        let stale = creds_dir.join(format!(".credentials.json.tmp{}", std::process::id()));
        fs::write(&stale, "leftover from a crashed run").unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();

        Credentials::keycloak(
            "https://example.com",
            "a@b.com".into(),
            KeycloakSession {
                issuer: "i".into(),
                client_id: "c".into(),
                refresh_token: "rt".into(),
                access_token: "at".into(),
                access_expires_at: 1,
            },
        )
        .save()
        .expect("a stale temp file must not fail the save");

        let mode = fs::metadata(Credentials::path().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "credentials.json inherited the stale temp file's mode"
        );
        // The tokens must not have been written into the stale file either.
        let stale_contents = fs::read_to_string(&stale).unwrap();
        assert!(
            !stale_contents.contains("rt"),
            "secrets were written into the pre-existing temp file: {stale_contents}"
        );
    }

    /// Two saves in one process must both succeed: with `create_new` and a
    /// pid-only temp name they would collide on the same path, so the name also
    /// carries a process-local counter.
    #[test]
    fn repeated_saves_in_one_process_do_not_collide() {
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
        for i in 0..5 {
            Credentials::keycloak(
                "https://example.com",
                format!("a{i}@b.com"),
                session.clone(),
            )
            .save()
            .unwrap_or_else(|e| panic!("save #{i} failed: {e}"));
        }
        assert_eq!(Credentials::load().unwrap().email, "a4@b.com");

        // No temp files left behind by any of the attempts.
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

    /// Logout must leave no credential material behind. Every
    /// `.credentials.json.tmp*` holds a full credential JSON including the
    /// offline refresh token, so a leftover from a crashed save would otherwise
    /// keep a working token on disk after the user logged out.
    #[test]
    fn delete_also_removes_leftover_temp_files() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(creds_dir.join("credentials.json"), "{}").unwrap();
        // Two leftovers, in both the old pid-only and the current shapes.
        let old_shape = creds_dir.join(".credentials.json.tmp4242");
        let new_shape = creds_dir.join(".credentials.json.tmp4242.0.123");
        fs::write(&old_shape, r#"{"auth":{"refresh_token":"still-live-rt"}}"#).unwrap();
        fs::write(&new_shape, r#"{"auth":{"refresh_token":"still-live-rt"}}"#).unwrap();

        Credentials::delete().unwrap();

        assert!(!Credentials::path().unwrap().exists());
        let remaining: Vec<_> = fs::read_dir(&creds_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            remaining.is_empty(),
            "logout left credential material on disk: {remaining:?}"
        );
    }

    /// The `save()` sweep must not delete a temp file that another process
    /// could still be writing — that would break its `rename`. Only files past
    /// the staleness threshold go.
    #[test]
    fn save_sweeps_only_old_temp_files() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", dir.path());

        let creds_dir = dir.path().join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        let fresh = creds_dir.join(".credentials.json.tmp999.0.1");
        fs::write(&fresh, "in flight in another process").unwrap();

        Credentials::keycloak(
            "https://example.com",
            "a@b.com".into(),
            KeycloakSession {
                issuer: "i".into(),
                client_id: "c".into(),
                refresh_token: "rt".into(),
                access_token: "at".into(),
                access_expires_at: 1,
            },
        )
        .save()
        .unwrap();

        assert!(
            fresh.exists(),
            "a just-created temp file may still be in flight elsewhere; sweeping it would break \
             that process's rename"
        );
    }

    /// With no way to locate the user's config directory, `path()` must FAIL
    /// rather than invent one. The old fallback was `PathBuf::from("~/.config")`
    /// — a relative path, since only a shell expands `~` — so `login` created
    /// `./~/.config/tracevault/` in the current working directory and wrote an
    /// offline refresh token into it, plausibly inside the user's repo.
    #[test]
    fn path_fails_instead_of_inventing_a_relative_home() {
        let err = Credentials::path_in(None)
            .expect_err("no config directory must not yield a guessed path");
        let msg = err.to_string();
        assert!(
            msg.contains("XDG_CONFIG_HOME") && msg.contains("HOME"),
            "the error must name the variables to set: {msg}"
        );
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

        // The display helper degrades to an explanation, never to a path that
        // would be wrong to write to.
        let shown = Credentials::path_for_display();
        assert!(
            !shown.starts_with('~'),
            "a `~`-prefixed path must never be presented as a location: {shown}"
        );

        // And with a real directory it still composes the same layout.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Credentials::path_in(Some(dir.path().to_path_buf())).unwrap(),
            dir.path().join("tracevault").join("credentials.json")
        );
    }

    /// Logging in must not destroy a `tvk_` API key already in the file: the two
    /// writers had opposite policies, with `persist_session` carefully
    /// preserving the token that `keycloak` silently dropped. The key stays
    /// inert while the session lives (`credential()` prefers the session) and is
    /// still there if the session is later removed.
    #[test]
    fn keycloak_preserves_an_existing_api_key_for_the_same_server() {
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{"server_url":"https://example.com","token":"tvk_precious","email":"old@b.com"}"#,
        );

        Credentials::keycloak(
            "https://example.com",
            "new@b.com".into(),
            KeycloakSession {
                issuer: "i".into(),
                client_id: "c".into(),
                refresh_token: "rt".into(),
                access_token: "at".into(),
                access_expires_at: 1,
            },
        )
        .save()
        .unwrap();

        let reloaded = Credentials::load().unwrap();
        assert_eq!(
            reloaded.token.as_deref(),
            Some("tvk_precious"),
            "login destroyed an API key it did not come to replace"
        );
        assert_eq!(reloaded.email, "new@b.com");
        // The session still wins for actual use.
        assert!(matches!(
            reloaded.credential(),
            Some(Credential::Keycloak(_))
        ));
    }

    /// ...but a key belonging to a DIFFERENT instance is not carried into this
    /// instance's file: that is the same cross-instance mixing the resolution
    /// guards exist to prevent.
    #[test]
    fn keycloak_does_not_carry_over_another_servers_api_key() {
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{"server_url":"https://instance-b.example.com","token":"tvk_for_b","email":"b@b.com"}"#,
        );

        Credentials::keycloak(
            "https://instance-a.example.com",
            "a@b.com".into(),
            KeycloakSession {
                issuer: "i".into(),
                client_id: "c".into(),
                refresh_token: "rt".into(),
                access_token: "at".into(),
                access_expires_at: 1,
            },
        )
        .save()
        .unwrap();

        let reloaded = Credentials::load().unwrap();
        assert_eq!(
            reloaded.token, None,
            "another instance's key must not be moved into this instance's file"
        );
        assert_eq!(reloaded.server_url, "https://instance-a.example.com");
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
        Credentials::persist_session("https://example.com", &session).unwrap();

        let reloaded = Credentials::load().unwrap();
        assert_eq!(reloaded.email, "a@b.com");
        assert_eq!(reloaded.server_url, "https://example.com");
        let auth = reloaded.auth.unwrap();
        assert_eq!(auth.access_token, "at2");
        assert_eq!(auth.refresh_token, "rt2");
        assert_eq!(auth.access_expires_at, 1_300);
    }

    /// One credentials file, several possible TraceVault instances: persisting
    /// a refresh for instance A must not clobber a file that now belongs to
    /// instance B, which would silently sign the user out of B.
    #[test]
    fn persist_session_leaves_another_servers_file_alone() {
        let original = r#"{"server_url":"https://instance-b.example.com","email":"b@b.com","auth":{"issuer":"i","client_id":"c","refresh_token":"b-rt","access_token":"b-at","access_expires_at":99}}"#;
        let (_dir, _lock, _guard) = with_credentials_file(original);

        let a_session = KeycloakSession {
            issuer: "i".into(),
            client_id: "c".into(),
            refresh_token: "a-rt".into(),
            access_token: "a-at".into(),
            access_expires_at: 1_234,
        };
        Credentials::persist_session("https://instance-a.example.com", &a_session)
            .expect("a foreign file is a skip, not an error");

        let after = fs::read_to_string(Credentials::path().unwrap()).unwrap();
        assert_eq!(after, original, "another server's file was overwritten");
    }

    /// The single owner of the normalisation property: the two sides of the
    /// comparison are formatted differently by construction (`ApiClient` trims
    /// trailing slashes, the file keeps whatever login was given), so a trailing
    /// slash must count as a match. Call sites only prove that they consult
    /// `same_server`, via their mismatch tests.
    #[test]
    fn same_server_normalises_trailing_slashes_and_whitespace() {
        assert!(same_server(
            "https://x.example.com",
            "https://x.example.com/"
        ));
        assert!(same_server(
            " https://x.example.com/ ",
            "https://x.example.com"
        ));
        assert!(!same_server(
            "https://x.example.com",
            "https://y.example.com"
        ));
        // A different path is a different instance, not a formatting variant.
        assert!(!same_server(
            "https://x.example.com",
            "https://x.example.com/tv"
        ));
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
        Credentials::persist_session("https://example.com", &session)
            .expect("a logged-out user must not turn a refresh into a hard error");
        assert!(
            !Credentials::path().unwrap().exists(),
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
