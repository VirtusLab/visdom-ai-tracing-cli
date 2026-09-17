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
use std::path::{Path, PathBuf};

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

// ── Credential resolution ────────────────────────────────────────────────────
//
// Which secret to trust for a given project root, and the guard that keeps one
// instance's credential from reaching another. This is policy about the on-disk
// format and the environment — no HTTP — so it lives here, next to the file it
// defends, rather than in the HTTP client module every command would otherwise
// import it from. `api_client::resolve_client` is the thin wrapper that turns
// the result into a client.

/// Resolve server URL and credential from multiple sources.
/// Priority: env var > credentials file > project config.toml
/// Returns (server_url, credential).
///
/// Only the credentials file can yield a [`Credential::Keycloak`]: an env var
/// or a `config.toml` `api_key` is always a `tvk_` API key, and an API key is
/// never treated as refreshable. That asymmetry is the whole contract — CI
/// and automation stay on the unchanging key path.
///
/// # Errors
///
/// Fails when the resolved server URL and a credential read from the
/// credentials file are for different TraceVault instances (see
/// [`server_mismatch_error`]). This covers EITHER kind of file-sourced
/// credential — the distinction that matters is not Keycloak-vs-key but
/// named-on-this-invocation vs. read-from-the-file. Failing is deliberate
/// rather than silently dropping the credential: every "Not logged in, run
/// `tracevault login`" message downstream would be actively WRONG advice — the
/// user IS logged in, and logging in again to the same instance would not
/// change anything.
#[allow(clippy::type_complexity)]
pub fn resolve_credentials(
    project_root: &Path,
) -> Result<(Option<String>, Option<Credential>), Box<dyn std::error::Error>> {
    resolve_credentials_inner(project_root, true)
}

/// [`resolve_credentials`] without the config-conflict warning.
///
/// For the two callers where that warning is wrong or unbearable:
///
/// - `init`, which has just REWRITTEN the very pin the warning compares
///   against, and whose `--server-url` then wins over the resolved URL
///   (`effective_url` in `init_in_directory`). The warning claims the login's
///   URL is being used, which on that path is simply false, and advises the
///   user to do what they are already doing.
/// - `stream`, the hook path: one process per captured event, so a repo with a
///   losing pin would print the line on every single tool call, for the whole
///   session. `Once` cannot help — each event is a fresh process.
#[allow(clippy::type_complexity)]
pub fn resolve_credentials_quiet(
    project_root: &Path,
) -> Result<(Option<String>, Option<Credential>), Box<dyn std::error::Error>> {
    resolve_credentials_inner(project_root, false)
}

#[allow(clippy::type_complexity)]
fn resolve_credentials_inner(
    project_root: &Path,
    warn_on_conflict: bool,
) -> Result<(Option<String>, Option<Credential>), Box<dyn std::error::Error>> {
    // 1. Env var API key
    let env_key = std::env::var("TRACEVAULT_API_KEY").ok();

    // 2. Credentials file
    let creds = Credentials::load();

    // 3. Project config — parsed as TOML by its own loader, not by hand.
    //
    // The previous line-scanning version split on `=` and took field 1, so a
    // perfectly legal `api_key = "tvk_abc=="` resolved to `tvk_abc`: a silently
    // truncated key, surfacing later as an unexplained 401 that (by design) is
    // never retried. It also matched any line merely STARTING with the field
    // name (`server_url_backup`) and read keys nested under a `[table]` as
    // though they were top-level. `TracevaultConfig` already declares both
    // fields, so the hand-parse only existed to duplicate it — badly. Using
    // `load` also means a malformed config now warns instead of silently
    // yielding nothing.
    let config = crate::config::TracevaultConfig::load(project_root);
    let config_server_url = config.as_ref().and_then(|c| c.server_url.clone());
    let config_api_key = config.as_ref().and_then(|c| c.api_key.clone());

    // Surface the pairing the guard below structurally cannot see. That guard's
    // own comment notes it is "only reachable when TRACEVAULT_SERVER_URL is set:
    // with it unset the URL comes from this same file, so the two match by
    // construction" — true only because `config.toml` has already lost the
    // precedence race on the next line, silently.
    if warn_on_conflict {
        if let Some((file_url, config_url)) = config_url_conflict(
            config_server_url.as_deref(),
            creds.as_ref().map(|c| c.server_url.as_str()),
            std::env::var("TRACEVAULT_SERVER_URL").is_ok(),
        ) {
            // Once per process: `repo status` resolves twice in one command
            // (`resolve_repo_flag` and the codebase line), and printing the
            // identical line back to back reads like a bug.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!("{}", config_mismatch_warning(file_url, config_url));
            });
        }
    }

    // Resolve server URL: env > creds > config
    let server_url = std::env::var("TRACEVAULT_SERVER_URL")
        .ok()
        .or_else(|| creds.as_ref().map(|c| c.server_url.clone()))
        .or(config_server_url);

    // Resolve the credential: env api key > credentials file > config api key
    let credential = match env_key {
        // Named explicitly on this invocation: the operator's choice of
        // key/URL pairing is deliberate and is not second-guessed.
        Some(key) => Some(Credential::ApiKey(key)),
        None => {
            let from_file = creds.as_ref().and_then(Credentials::credential);
            if from_file.is_some() {
                // The two precedence chains above disagree: the URL can come
                // from the env while the credential comes from the file. Every
                // credential in that file — a Keycloak access token or a `tvk_`
                // API key alike — was issued by ONE TraceVault instance, so
                // handing it to a client pointed elsewhere transmits a secret to
                // a host it was never issued for, which then has it in its logs.
                //
                // This is the FIRST request, before any refresh, so neither
                // server-scoping guard in `refresh_locked` would see it.
                //
                // Only reachable when TRACEVAULT_SERVER_URL is set: with it
                // unset the URL comes from this same file, so the two match by
                // construction.
                let file_url = creds.as_ref().map_or("", |c| c.server_url.as_str());
                if let Some(target) = server_url.as_deref() {
                    if !same_server(file_url, target) {
                        return Err(server_mismatch_error(file_url, target));
                    }
                }
            }
            // A `config.toml` `api_key` is NOT guarded: committing a key to a
            // project config, next to that config's own `server_url`, is as
            // deliberate as naming it in the environment. It is also only
            // reachable when the file yielded nothing.
            from_file.or_else(|| config_api_key.map(Credential::ApiKey))
        }
    };

    Ok((server_url, credential))
}

/// Whether a repo-local `config.toml` `server_url` disagrees with the
/// credentials file's. Returns `(file_url, config_url)` when they conflict.
///
/// Deliberately a WARNING rather than a refusal, which is the whole difference
/// from [`server_mismatch_error`]. There, the env-supplied URL is the one
/// actually contacted, so refusing genuinely stops a token reaching a host it
/// was not issued for. Here the config's URL has ALREADY lost the precedence
/// race below and is never contacted, so refusing would prevent nothing — while
/// handing anyone who can land a `.tracevault/config.toml` in a repo you clone a
/// way to break every command (see
/// `verification_phase::tests::a_committed_config_url_cannot_redirect_the_token`,
/// which pins that a committed config URL is inert). The defect this addresses
/// was never that the wrong URL got used; it was that the disagreement was
/// SILENT — `tracevault sync` printed "Repo synced with server" while
/// registering the repo on the logged-in instance instead of the pinned one.
///
/// `env_url_set` suppresses it: `TRACEVAULT_SERVER_URL` outranks both, so the
/// pin is moot and the mismatch it can cause is already guarded below. Taken as
/// a bool rather than read here so this stays pure and directly testable.
pub(crate) fn config_url_conflict<'a>(
    config_url: Option<&'a str>,
    file_url: Option<&'a str>,
    env_url_set: bool,
) -> Option<(&'a str, &'a str)> {
    if env_url_set {
        return None;
    }
    let (config_url, file_url) = (config_url?, file_url?);
    (!same_server(file_url, config_url)).then_some((file_url, config_url))
}

/// The warning text for [`config_url_conflict`].
///
/// Names BOTH URLs and says which one wins. `tracevault status` reports the
/// credentials file's URL and does not read `config.toml` at all, so without
/// naming them the user can see neither side of the disagreement — which is
/// exactly how this went unnoticed long enough to register a repo on the wrong
/// instance.
fn config_mismatch_warning(file_url: &str, config_url: &str) -> String {
    format!(
        "tracevault: warning: this repo's .tracevault/config.toml pins '{config_url}', but you \
         are logged in to '{file_url}' — using '{file_url}'. Run `tracevault login --server-url \
         {config_url}` to use the repo's instance, or remove `server_url` from \
         .tracevault/config.toml to silence this."
    )
}

/// The error for "the saved credential is for a different TraceVault instance".
///
/// Applies to any FILE-SOURCED credential, Keycloak session or `tvk_` key
/// alike: both are secrets issued by one instance, and a key would not even
/// work against another instance — so without this the user gets a confusing
/// 401 from the wrong host instead of an explanation.
///
/// Names BOTH URLs, because the whole failure is that two of them disagree and
/// the user can see neither: `tracevault status` reports the file's URL, and
/// the env var is often set in a shell profile or a CI job definition rather
/// than on the command line.
///
/// The third suggestion matters: pointing at the SAME instance through another
/// address (a `kubectl port-forward`, an internal vs. external hostname) is a
/// legitimate workflow that this guard cannot distinguish from a genuine
/// mismatch, so the message names the ways to proceed deliberately.
fn server_mismatch_error(file_url: &str, target: &str) -> Box<dyn std::error::Error> {
    format!(
        "refusing to use the saved credentials: the credentials file is for '{file_url}', but \
         this command is targeting '{target}'. A credential issued by one TraceVault instance \
         must not be sent to another.\n\
         Either unset TRACEVAULT_SERVER_URL to use '{file_url}', or run `tracevault login \
         --server-url {target}`. If '{target}' is the same instance reached through a different \
         address (e.g. a port-forward), set TRACEVAULT_API_KEY to a `tvk_` key for it instead."
    )
    .into()
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

    /// Fixed "now" for the resolution fixtures. A constant rather than the wall
    /// clock so nothing here depends on timing.
    const NOW: i64 = 1_800_000_000;

    /// Write a Keycloak credentials file for `server_url`/`issuer` into `dir`.
    fn write_keycloak_file(dir: &std::path::Path, server_url: &str, issuer: &str, expires_at: i64) {
        let creds_dir = dir.join("tracevault");
        fs::create_dir_all(&creds_dir).unwrap();
        fs::write(
            creds_dir.join("credentials.json"),
            format!(
                r#"{{"server_url":"{server_url}","email":"a@b.com","auth":{{"issuer":"{issuer}","client_id":"tracing-cli","refresh_token":"old-rt","access_token":"old-at","access_expires_at":{expires_at}}}}}"#
            ),
        )
        .unwrap();
    }

    // ---- resolve_credentials: a credential read from the FILE must not follow
    // ---- a TRACEVAULT_SERVER_URL override to another instance. Applies to a
    // ---- Keycloak session and a `tvk_` key alike; an env- or config-supplied
    // ---- key is a deliberate operator act and is left alone.

    type ResolveFixture = (
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
        crate::test_helpers::EnvVarGuard,
    );

    /// Set up an empty project root with `TRACEVAULT_SERVER_URL` set to
    /// `env_url` (unset when `None`) and `credentials.json` containing
    /// `file_body` (no file when `None`). Returns the tempdir (which doubles as
    /// the project root) plus the guards the caller must keep alive.
    fn resolve_fixture_with(file_body: Option<&str>, env_url: Option<&str>) -> ResolveFixture {
        let env_lock = crate::test_helpers::lock_env_mutation_sync();
        let dir = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", dir.path());
        // A stray key in the developer's environment would take precedence and
        // mask what these tests are checking.
        guard.remove("TRACEVAULT_API_KEY");
        match env_url {
            Some(u) => guard.set("TRACEVAULT_SERVER_URL", u),
            None => guard.remove("TRACEVAULT_SERVER_URL"),
        }
        if let Some(body) = file_body {
            let creds_dir = dir.path().join("tracevault");
            fs::create_dir_all(&creds_dir).unwrap();
            fs::write(creds_dir.join("credentials.json"), body).unwrap();
        }
        (dir, env_lock, guard)
    }

    /// [`resolve_fixture_with`] for a file holding a Keycloak session.
    fn resolve_fixture(file_server_url: &str, env_url: Option<&str>) -> ResolveFixture {
        let fixture = resolve_fixture_with(None, env_url);
        write_keycloak_file(
            fixture.0.path(),
            file_server_url,
            "https://idp.test/realms/v",
            NOW + 3600,
        );
        fixture
    }

    /// [`resolve_fixture_with`] for a file holding a `tvk_` API key.
    fn resolve_fixture_api_key(file_server_url: &str, env_url: Option<&str>) -> ResolveFixture {
        resolve_fixture_with(
            Some(&format!(
                r#"{{"server_url":"{file_server_url}","token":"tvk_from_file","email":"a@b.com"}}"#
            )),
            env_url,
        )
    }

    /// The precedence chains disagree: the URL comes from the env, the
    /// credential from the file. Handing either kind over would send a secret
    /// minted by instance B to instance A on the FIRST request, before any
    /// refresh, so neither guard in `refresh_locked` would ever see it.
    ///
    /// Both credential kinds are checked here because the distinction that
    /// matters is not Keycloak-vs-key but named-on-this-invocation
    /// vs. read-from-the-file — and because the two assert the same message.
    #[test]
    fn a_mismatched_server_url_refuses_either_kind_of_file_credential() {
        for kind in ["keycloak session", "api key"] {
            let (dir, _lock, _guard) = if kind == "keycloak session" {
                resolve_fixture(
                    "https://instance-b.example.com",
                    Some("https://instance-a.example.com"),
                )
            } else {
                resolve_fixture_api_key(
                    "https://instance-b.example.com",
                    Some("https://instance-a.example.com"),
                )
            };

            let err = resolve_credentials(dir.path())
                .expect_err("a credential for another instance must not be handed out");
            let msg = err.to_string();
            // Both URLs must be named: the user can see neither (status prints
            // the file's, and the env var is usually set in a profile or CI job).
            assert!(
                msg.contains("https://instance-b.example.com"),
                "{kind}: must name the file's server: {msg}"
            );
            assert!(
                msg.contains("https://instance-a.example.com"),
                "{kind}: must name the targeted server: {msg}"
            );
            // And it must point at the deliberate ways forward, including the
            // same-instance-different-address case this guard cannot
            // distinguish from a real mismatch.
            assert!(msg.contains("TRACEVAULT_SERVER_URL"), "{kind}: {msg}");
            assert!(msg.contains("tracevault login"), "{kind}: {msg}");
            assert!(msg.contains("TRACEVAULT_API_KEY"), "{kind}: {msg}");
            assert!(
                !msg.contains("tvk_from_file"),
                "{kind}: the error must not echo the credential: {msg}"
            );
        }
    }

    /// The two sides are formatted differently by construction (`ApiClient`
    /// trims trailing slashes, the file keeps what login was given), so a
    /// trailing slash must not be read as "a different instance". This guard
    /// hard-FAILS, so a false positive here would break every command — hence
    /// one positive test, while the other call sites need only prove that they
    /// consult `same_server` at all, which their mismatch tests do.
    #[test]
    fn a_matching_server_url_modulo_trailing_slash_still_resolves() {
        let (dir, _lock, _guard) =
            resolve_fixture("https://example.com/", Some("https://example.com"));

        let (url, credential) =
            resolve_credentials(dir.path()).expect("a trailing slash is not a different instance");
        assert_eq!(url.as_deref(), Some("https://example.com"));
        assert!(matches!(credential, Some(Credential::Keycloak(_))));
    }

    /// The normal case: no env override, so the URL comes from the same file as
    /// the credential and they match by construction.
    #[test]
    fn no_env_override_resolves_the_files_session_unchanged() {
        let (dir, _lock, _guard) = resolve_fixture("https://example.com", None);

        let (url, credential) = resolve_credentials(dir.path()).expect("the normal case must work");
        assert_eq!(url.as_deref(), Some("https://example.com"));
        assert!(matches!(credential, Some(Credential::Keycloak(_))));
    }

    /// The config is parsed as TOML, not scanned line by line. A base64-ish key
    /// containing `=` used to be truncated at the first `=` (`tvk_abc==` ->
    /// `tvk_abc`), producing an unexplained 401 that the `ApiKey` path
    /// deliberately never retries.
    #[test]
    fn a_config_api_key_containing_equals_signs_is_not_truncated() {
        let (dir, _lock, _guard) = resolve_fixture_with(None, None);
        let config_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://example.com\"\napi_key = \"tvk_YWJjZA==\"\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(
                k, "tvk_YWJjZA==",
                "the key must survive verbatim, `=` padding and all"
            ),
            other => panic!("expected the config API key, got {other:?}"),
        }
    }

    /// Line-scanning also matched any field whose name merely STARTED with the
    /// one being looked for, and read a key nested in a `[table]` as top-level.
    /// TOML parsing gets both right.
    #[test]
    fn a_config_lookalike_field_is_not_mistaken_for_the_real_one() {
        let (dir, _lock, _guard) = resolve_fixture_with(None, None);
        let config_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://real.example.com\"\n\
             [user_context]\n\
             enable = false\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://real.example.com"));
        assert!(
            credential.is_none(),
            "no api_key is configured anywhere, so there is no credential"
        );
    }

    /// A `config.toml` `api_key` is committed next to that config's own
    /// `server_url`: as deliberate as naming it in the environment, so a URL
    /// override must not break it. (It is also only consulted when the
    /// credentials file yielded nothing, as here.)
    #[test]
    fn a_config_api_key_is_unaffected_by_a_mismatched_server_url() {
        let (dir, _lock, _guard) =
            resolve_fixture_with(None, Some("https://instance-a.example.com"));
        let config_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "server_url = \"https://instance-b.example.com\"\napi_key = \"tvk_from_config\"\n",
        )
        .unwrap();

        let (url, credential) = resolve_credentials(dir.path())
            .expect("a config-supplied key must never be second-guessed");
        assert_eq!(url.as_deref(), Some("https://instance-a.example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_from_config"),
            other => panic!("expected the config API key, got {other:?}"),
        }
    }

    /// `TRACEVAULT_API_KEY` plus a URL pointing anywhere is a deliberate,
    /// explicit act by the operator (and the CI path). It must be unaffected.
    #[test]
    fn an_env_api_key_is_unaffected_by_a_mismatched_server_url() {
        let (dir, _lock, mut guard) = resolve_fixture(
            "https://instance-b.example.com",
            Some("https://instance-a.example.com"),
        );
        guard.set("TRACEVAULT_API_KEY", "tvk_abc");

        let (url, credential) = resolve_credentials(dir.path())
            .expect("an explicitly supplied API key must never be second-guessed");
        assert_eq!(url.as_deref(), Some("https://instance-a.example.com"));
        match credential {
            Some(Credential::ApiKey(k)) => assert_eq!(k, "tvk_abc"),
            other => panic!("expected the env API key, got {other:?}"),
        }
    }

    // ---- config_url_conflict: a repo-local `.tracevault/config.toml`
    // ---- `server_url` sits BELOW the credentials file, so it loses silently.
    // ---- It stays losing — it must only stop being silent.

    /// Write a project-level `.tracevault/config.toml` under `root`.
    fn write_project_config(root: &std::path::Path, body: &str) {
        let config_dir = root.join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn a_config_url_differing_from_the_files_is_a_conflict() {
        assert_eq!(
            config_url_conflict(
                Some("https://pinned.example.com"),
                Some("https://login.example.com"),
                false
            ),
            Some(("https://login.example.com", "https://pinned.example.com")),
            "the file's URL is returned first, as the one actually used"
        );
    }

    /// The config is hand-authored while the file records whatever `login` was
    /// given, so the two are formatted independently. A trailing slash must not
    /// read as a disagreement, or every correctly-configured repo warns forever
    /// and the warning stops being read.
    #[test]
    fn a_trailing_slash_is_not_a_conflict() {
        assert_eq!(
            config_url_conflict(
                Some("https://example.com"),
                Some("https://example.com/"),
                false
            ),
            None
        );
    }

    /// `TRACEVAULT_SERVER_URL` outranks both, so the pin is moot and the
    /// env-vs-file guard already covers that pairing.
    #[test]
    fn an_env_server_url_suppresses_the_conflict() {
        assert_eq!(
            config_url_conflict(
                Some("https://pinned.example.com"),
                Some("https://login.example.com"),
                true
            ),
            None
        );
    }

    /// Nothing to disagree with on either side.
    #[test]
    fn a_missing_side_is_never_a_conflict() {
        assert_eq!(
            config_url_conflict(None, Some("https://login.example.com"), false),
            None
        );
        assert_eq!(
            config_url_conflict(Some("https://pinned.example.com"), None, false),
            None
        );
    }

    /// The warning has to carry what neither `status` nor the success line ever
    /// showed: both URLs, and which one is actually being used.
    #[test]
    fn the_warning_names_both_urls_and_the_winner() {
        let msg = config_mismatch_warning("https://login.example.com", "http://localhost:8080");
        assert!(
            msg.contains("http://localhost:8080"),
            "must name the pin: {msg}"
        );
        assert!(
            msg.contains("https://login.example.com"),
            "must name the login: {msg}"
        );
        assert!(
            msg.contains("config.toml"),
            "must name where the pin lives: {msg}"
        );
        assert!(
            msg.contains("tracevault login"),
            "must name the way forward: {msg}"
        );
    }

    /// The behavioural contract, and the reason this is a warning and not a
    /// refusal: a `config.toml` `server_url` remains inert. A committed one is
    /// attacker-controlled (see
    /// `verification_phase::tests::a_committed_config_url_cannot_redirect_the_token`),
    /// so it must neither redirect the credential NOR break the command.
    #[test]
    fn a_conflicting_config_url_still_resolves_to_the_files_instance() {
        for kind in ["keycloak session", "api key"] {
            let (dir, _lock, _guard) = if kind == "keycloak session" {
                resolve_fixture("https://instance-b.example.com", None)
            } else {
                resolve_fixture_api_key("https://instance-b.example.com", None)
            };
            write_project_config(dir.path(), "server_url = \"https://attacker.invalid\"\n");

            let (url, credential) = resolve_credentials(dir.path())
                .expect("a conflicting config must warn, never break the command");
            assert_eq!(
                url.as_deref(),
                Some("https://instance-b.example.com"),
                "{kind}: the config URL must stay inert"
            );
            assert!(credential.is_some(), "{kind}: the credential must survive");
        }
    }

    /// A config carrying its own `api_key` changes nothing: the file still wins
    /// both the URL and the credential, so the config's key is never sent
    /// anywhere. It is only consulted when the file yields nothing.
    #[test]
    fn a_conflicting_config_with_its_own_key_still_resolves_to_the_file() {
        let (dir, _lock, _guard) = resolve_fixture("https://instance-b.example.com", None);
        write_project_config(
            dir.path(),
            "server_url = \"https://instance-a.example.com\"\napi_key = \"tvk_from_config\"\n",
        );

        let (url, credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://instance-b.example.com"));
        assert!(
            matches!(credential, Some(Credential::Keycloak(_))),
            "the file's session wins; the config key is never reached"
        );
    }

    /// With no credentials file there is no disagreement, so the config's
    /// `server_url` is used exactly as before.
    #[test]
    fn a_config_server_url_is_used_when_there_is_no_credentials_file() {
        let (dir, _lock, _guard) = resolve_fixture_with(None, None);
        write_project_config(
            dir.path(),
            "server_url = \"https://instance-a.example.com\"\n",
        );

        let (url, _credential) = resolve_credentials(dir.path()).unwrap();
        assert_eq!(url.as_deref(), Some("https://instance-a.example.com"));
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
                    "client_id":"tracing-cli",
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
                assert_eq!(s.client_id, "tracing-cli");
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
            client_id: "tracing-cli".into(),
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
            client_id: "tracing-cli".into(),
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
        // Stored WITH a trailing slash, while login is given it without: the
        // carry-over filter must normalise, not compare bytes.
        let (_dir, _lock, _guard) = with_credentials_file(
            r#"{"server_url":"https://example.com/","token":"tvk_precious","email":"old@b.com"}"#,
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

        // The other direction at this same call site: the file's URL differing
        // only by a trailing slash IS this server, so the write must happen.
        // Without this half, replacing `same_server` with `!=`/`==` here goes
        // unnoticed and quietly disables persistence — the state the skip
        // warning exists to explain.
        fs::write(
            Credentials::path().unwrap(),
            r#"{"server_url":"https://instance-a.example.com/","email":"a@b.com","auth":{"issuer":"i","client_id":"c","refresh_token":"old","access_token":"old","access_expires_at":1}}"#,
        )
        .unwrap();
        Credentials::persist_session("https://instance-a.example.com", &a_session).unwrap();
        let auth = Credentials::load().unwrap().auth.unwrap();
        assert_eq!(
            auth.access_token, "a-at",
            "a trailing slash must not block a legitimate persist"
        );
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
