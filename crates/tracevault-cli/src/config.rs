use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct TracevaultConfig {
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    // Never persisted; may still be parsed if present in a hand-authored file.
    // No production code reads this field today (credentials are resolved
    // independently in api_client.rs::resolve_credentials); kept on the
    // struct for forward-compat / round-trip parsing of existing files.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<UserContext>,
    /// The remote (codebase)'s server-side id, resolved by `init` (best-effort)
    /// via `resolve_remote`. Display-only today — `repo_id` stays authoritative
    /// for ingest — but recorded so a future caller doesn't have to re-resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// The codebase's registered name, resolved by `init` alongside `remote_id`.
    /// Display-only — lets bound-mode `status` print the codebase without an
    /// extra network round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codebase_name: Option<String>,
    /// Client-side default project **name** for this repo (no server counterpart).
    /// Resolution precedence rung 3; resolved to an id at use time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project: Option<String>,
}

fn default_agent() -> String {
    "claude-code".to_string()
}

impl Default for TracevaultConfig {
    fn default() -> Self {
        Self {
            agent: "claude-code".to_string(),
            server_url: None,
            api_key: None,
            org_slug: None,
            repo_id: None,
            user_context: None,
            remote_id: None,
            codebase_name: None,
            default_project: None,
        }
    }
}

/// Cross-repo user context source. Cargo-`dependency`-style shorthand:
/// bool toggles enable at the default path; a string is enable + that path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContext {
    Toggle(bool),
    Path(String),
    Full {
        #[serde(default = "crate::config::enable_default")]
        enable: bool,
        #[serde(default)]
        path: Option<String>,
    },
}

impl Default for UserContext {
    fn default() -> Self {
        UserContext::Toggle(false)
    }
}

pub(crate) fn enable_default() -> bool {
    true
}

/// Root of the user-level TraceVault config dir: `~/.config/tracevault/`
/// (beside `credentials.json` / `context.json`). Falls back to `./tracevault`.
pub fn tv_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tracevault")
}

pub fn user_config_path_in(config_root: &Path) -> PathBuf {
    config_root.join("config.toml")
}
pub fn user_config_path() -> PathBuf {
    user_config_path_in(&tv_config_root())
}
pub fn default_user_context_path_in(config_root: &Path) -> PathBuf {
    config_root.join("context.json")
}

/// `~/.config/tracevault/context.json`, alongside credentials.json.
/// Public path accessor (used by tests / lib consumers as the canonical default);
/// production resolves via `UserContext::path`/`path_in`, so the bin has no direct
/// caller.
#[allow(dead_code)]
pub fn default_user_context_path() -> PathBuf {
    default_user_context_path_in(&tv_config_root())
}

/// Load the user-level `config.toml`, distinguishing a **missing** file
/// (`Ok(None)`) from a malformed/unreadable one (`Err`). Mirrors
/// `TracevaultConfig::try_load` but for a direct file path (the user-level
/// config is NOT inside a `.tracevault/` dir).
pub fn try_load_user_config_in(config_root: &Path) -> Result<Option<TracevaultConfig>, String> {
    let path = user_config_path_in(config_root);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Lenient: missing OR malformed → `TracevaultConfig::default()` (warn on malformed).
pub fn load_user_config_in(config_root: &Path) -> TracevaultConfig {
    match try_load_user_config_in(config_root) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => TracevaultConfig::default(),
        Err(e) => {
            eprintln!("tracevault: warning: malformed user config.toml: {e}");
            TracevaultConfig::default()
        }
    }
}

/// Resolve the effective user-context source. A repo that *configured* it
/// (`Some`) wins — including an explicit `Some(Toggle(false))` (hard-off, no
/// fallback). Only when the repo did NOT configure it (`None`) does the
/// user-level `config.toml` under `config_root` supply it.
pub fn resolve_user_context_in(
    repo_configured: Option<UserContext>,
    config_root: &Path,
) -> UserContext {
    repo_configured.unwrap_or_else(|| {
        load_user_config_in(config_root)
            .user_context
            .unwrap_or_default()
    })
}

/// [`resolve_user_context_in`] rooted at [`tv_config_root`].
pub fn resolve_user_context(repo_configured: Option<UserContext>) -> UserContext {
    resolve_user_context_in(repo_configured, &tv_config_root())
}

impl UserContext {
    /// The file this source points at (configured path or default), regardless
    /// of whether it is enabled. Used by `--user` editing.
    pub fn path(&self) -> PathBuf {
        self.path_in(&tv_config_root())
    }

    /// Like [`path`](Self::path), but resolves the "no explicit path" default
    /// relative to `config_root` instead of the process-global
    /// [`tv_config_root`]. This matters for the user-level `config.toml` (and
    /// for tests with an injected root): its unconfigured default is the
    /// `context.json` colocated with it under `config_root`, which need not be
    /// the real `tv_config_root()`.
    pub fn path_in(&self, config_root: &Path) -> PathBuf {
        match self {
            UserContext::Path(p) => PathBuf::from(p),
            UserContext::Full { path: Some(p), .. } => PathBuf::from(p),
            _ => default_user_context_path_in(config_root),
        }
    }

    /// `Some(path)` when enabled (consulted by the hook); `None` when disabled.
    pub fn resolve(&self) -> Option<PathBuf> {
        self.resolve_in(&tv_config_root())
    }

    /// Like [`resolve`](Self::resolve), but resolves the "no explicit path"
    /// default relative to `config_root` (via [`path_in`](Self::path_in))
    /// instead of the process-global [`tv_config_root`]. Callers that took an
    /// injected `config_root` MUST use this — `resolve()` would leak back to
    /// the real `~/.config` for the default-path case.
    pub fn resolve_in(&self, config_root: &Path) -> Option<PathBuf> {
        let enabled = match self {
            UserContext::Toggle(b) => *b,
            UserContext::Path(_) => true,
            UserContext::Full { enable, .. } => *enable,
        };
        enabled.then(|| self.path_in(config_root))
    }

    /// Map `init`'s `--no-user-context` / `--user-context <path>` flags to a
    /// *requested* user-context override. `Ok(None)` means neither flag was given
    /// (the caller decides the default/preserve behavior). Rejects an empty/blank
    /// `--user-context` value.
    pub fn from_init_flags(
        no_user_context: bool,
        user_context: Option<String>,
    ) -> Result<Option<UserContext>, String> {
        match (no_user_context, user_context) {
            (true, _) => Ok(Some(UserContext::Toggle(false))),
            (false, Some(p)) if p.trim().is_empty() => {
                Err("--user-context path must not be empty".to_string())
            }
            (false, Some(p)) => Ok(Some(UserContext::Path(p))),
            (false, None) => Ok(None),
        }
    }
}

impl TracevaultConfig {
    pub fn config_dir(project_root: &Path) -> PathBuf {
        project_root.join(".tracevault")
    }

    pub fn config_path(project_root: &Path) -> PathBuf {
        Self::config_dir(project_root).join("config.toml")
    }

    /// Runtime artifacts inside a `.tracevault/` directory that must never be
    /// committed (`config.toml` and the `.gitignore` itself stay tracked).
    const GITIGNORE_CONTENTS: &'static str = "sessions/\ncache/\n*.local.toml\n";

    /// Ensure a `.gitignore` exists inside `config_dir` (a `.tracevault/` dir)
    /// so runtime artifacts are ignored no matter where the directory was
    /// created — including the per-subproject `.tracevault/` dirs the stream
    /// hook may create from a nested working directory. Idempotent: an existing
    /// `.gitignore` is never overwritten. Best-effort; callers may ignore the
    /// result (a failure must never break the hook).
    pub fn ensure_gitignore(config_dir: &Path) -> std::io::Result<()> {
        let path = config_dir.join(".gitignore");
        if path.exists() {
            return Ok(());
        }
        std::fs::write(path, Self::GITIGNORE_CONTENTS)
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).unwrap_or_default();
        format!("# TraceVault configuration\n{body}")
    }

    /// Load `config.toml`, distinguishing a **missing** file (`Ok(None)`) from a
    /// present-but-**malformed** one (`Err(message)`). Callers that need to react
    /// differently to those two cases (e.g. to avoid silently writing to a
    /// default path when a configured one failed to parse) should use this.
    ///
    /// Only a genuine "not found" counts as missing; any other IO error (e.g.
    /// permission denied) is surfaced as `Err` with the path, rather than being
    /// masked as an absent config.
    pub fn try_load(project_root: &Path) -> Result<Option<Self>, String> {
        let path = Self::config_path(project_root);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        toml::from_str(&content)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// Lenient load: a missing **or** malformed `config.toml` yields `None`, with
    /// a warning printed for the malformed case. Kept for callers that treat "no
    /// usable config" uniformly; prefer [`try_load`](Self::try_load) when the
    /// missing/malformed distinction matters.
    pub fn load(project_root: &Path) -> Option<Self> {
        match Self::try_load(project_root) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("tracevault: warning: malformed config.toml: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn to_toml_all_fields() {
        let cfg = TracevaultConfig {
            agent: "claude-code".into(),
            server_url: Some("https://example.com".into()),
            api_key: None, // api_key not included in to_toml
            org_slug: Some("my-org".into()),
            repo_id: Some("repo-1".into()),
            user_context: None,
            remote_id: None,
            codebase_name: None,
            default_project: None,
        };
        let toml = cfg.to_toml();
        assert!(toml.contains("agent = \"claude-code\""));
        assert!(toml.contains("server_url = \"https://example.com\""));
        assert!(toml.contains("org_slug = \"my-org\""));
        assert!(toml.contains("repo_id = \"repo-1\""));
        assert!(
            !toml.contains("user_context"),
            "None user_context must be omitted"
        );
    }

    #[test]
    fn to_toml_minimal() {
        let cfg = TracevaultConfig::default();
        let toml = cfg.to_toml();
        assert!(toml.contains("agent = \"claude-code\""));
        assert!(!toml.contains("server_url"));
    }

    #[test]
    fn load_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let tv_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&tv_dir).unwrap();
        fs::write(
            tv_dir.join("config.toml"),
            "agent = \"claude-code\"\nserver_url = \"https://example.com\"\norg_slug = \"myorg\"\n",
        )
        .unwrap();
        let cfg = TracevaultConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.agent, "claude-code");
        assert_eq!(cfg.server_url.unwrap(), "https://example.com");
        assert_eq!(cfg.org_slug.unwrap(), "myorg");
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(TracevaultConfig::load(dir.path()).is_none());
    }

    #[test]
    fn try_load_distinguishes_missing_valid_and_malformed() {
        // Missing file → Ok(None), not an error.
        let missing = tempfile::tempdir().unwrap();
        assert!(matches!(
            TracevaultConfig::try_load(missing.path()),
            Ok(None)
        ));

        // Valid file → Ok(Some(cfg)).
        let valid = tempfile::tempdir().unwrap();
        let tv_dir = valid.path().join(".tracevault");
        fs::create_dir_all(&tv_dir).unwrap();
        fs::write(tv_dir.join("config.toml"), "agent = \"claude-code\"\n").unwrap();
        assert!(matches!(
            TracevaultConfig::try_load(valid.path()),
            Ok(Some(_))
        ));

        // Present but unparseable → Err (NOT silently treated as missing).
        let bad = tempfile::tempdir().unwrap();
        let bad_dir = bad.path().join(".tracevault");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("config.toml"), "this = = not valid toml").unwrap();
        assert!(TracevaultConfig::try_load(bad.path()).is_err());

        // Present but unreadable (a non-NotFound IO error — here config.toml is a
        // directory) → Err, NOT masked as missing.
        let io = tempfile::tempdir().unwrap();
        fs::create_dir_all(io.path().join(".tracevault").join("config.toml")).unwrap();
        assert!(TracevaultConfig::try_load(io.path()).is_err());
    }

    #[test]
    fn ensure_gitignore_creates_runtime_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();

        TracevaultConfig::ensure_gitignore(&config_dir).unwrap();

        let gitignore = config_dir.join(".gitignore");
        assert!(gitignore.exists(), ".gitignore must be created");
        let content = fs::read_to_string(&gitignore).unwrap();
        assert!(content.contains("sessions/"), "must ignore sessions/");
        assert!(content.contains("cache/"), "must ignore cache/");
        assert!(content.contains("*.local.toml"), "must ignore *.local.toml");
    }

    #[test]
    fn toml_round_trip_omits_api_key_and_none_fields() {
        let cfg = TracevaultConfig {
            agent: "claude-code".into(),
            server_url: Some("https://example.com".into()),
            api_key: Some("secret".into()),
            org_slug: Some("my-org".into()),
            repo_id: None,
            user_context: None,
            remote_id: None,
            codebase_name: None,
            default_project: None,
        };
        let toml = cfg.to_toml();
        assert!(toml.contains("agent = \"claude-code\""));
        assert!(toml.contains("server_url = \"https://example.com\""));
        assert!(toml.contains("org_slug = \"my-org\""));
        assert!(!toml.contains("api_key"), "api_key must never be written");
        assert!(!toml.contains("repo_id"), "None fields must be omitted");

        // Re-parse (api_key absent from disk) round-trips the rest.
        let parsed: TracevaultConfig = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.agent, "claude-code");
        assert_eq!(parsed.server_url.as_deref(), Some("https://example.com"));
        assert_eq!(parsed.org_slug.as_deref(), Some("my-org"));
        assert_eq!(parsed.api_key, None);
    }

    #[test]
    fn user_context_forms_resolve_correctly() {
        // absent ⇒ unset (never configured)
        let none_cfg: TracevaultConfig = toml::from_str("agent = \"claude-code\"").unwrap();
        assert!(none_cfg.user_context.is_none());

        // false ⇒ disabled
        let f: TracevaultConfig =
            toml::from_str("agent=\"claude-code\"\nuser_context = false").unwrap();
        assert!(matches!(f.user_context, Some(UserContext::Toggle(false))));
        assert!(f.user_context.unwrap().resolve().is_none());

        // true ⇒ enabled at default path
        let t: TracevaultConfig =
            toml::from_str("agent=\"claude-code\"\nuser_context = true").unwrap();
        assert_eq!(
            t.user_context.unwrap().resolve(),
            Some(default_user_context_path())
        );

        // "path" ⇒ enabled at that path
        let p: TracevaultConfig =
            toml::from_str("agent=\"claude-code\"\nuser_context = \"/tmp/ctx.json\"").unwrap();
        assert_eq!(
            p.user_context.unwrap().resolve(),
            Some(PathBuf::from("/tmp/ctx.json"))
        );

        // { enable = false, path = ... } ⇒ disabled but path() remembers it
        let obj: TracevaultConfig = toml::from_str(
            "agent=\"claude-code\"\n[user_context]\nenable = false\npath = \"/tmp/x.json\"",
        )
        .unwrap();
        let obj_ctx = obj.user_context.unwrap();
        assert!(obj_ctx.resolve().is_none());
        assert_eq!(obj_ctx.path(), PathBuf::from("/tmp/x.json"));
    }

    #[test]
    fn explicit_disable_is_persisted_not_skipped() {
        let cfg = TracevaultConfig {
            user_context: Some(UserContext::Toggle(false)),
            ..Default::default()
        };
        let toml = cfg.to_toml();
        assert!(
            toml.contains("user_context"),
            "explicit off must serialize, got:\n{toml}"
        );
        let back: TracevaultConfig = toml::from_str(&toml).unwrap();
        assert!(matches!(
            back.user_context,
            Some(UserContext::Toggle(false))
        ));
    }

    #[test]
    fn user_config_paths_are_under_config_root() {
        let root = std::path::Path::new("/x/tracevault");
        assert_eq!(
            user_config_path_in(root),
            std::path::Path::new("/x/tracevault/config.toml")
        );
        assert_eq!(
            default_user_context_path_in(root),
            std::path::Path::new("/x/tracevault/context.json")
        );
    }

    #[test]
    fn try_load_user_config_missing_is_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(try_load_user_config_in(dir.path()), Ok(None)));
    }

    #[test]
    fn try_load_user_config_reads_user_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            user_config_path_in(dir.path()),
            "agent=\"claude-code\"\nuser_context = true\n",
        )
        .unwrap();
        let cfg = try_load_user_config_in(dir.path()).unwrap().unwrap();
        // Validate against the INJECTED root (resolve_in), not the real
        // tv_config_root(): `user_context = true` resolves to the default
        // `context.json` colocated under `dir`.
        assert_eq!(
            cfg.user_context.unwrap().resolve_in(dir.path()),
            Some(default_user_context_path_in(dir.path()))
        );
    }

    #[test]
    fn try_load_user_config_malformed_is_err() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(user_config_path_in(dir.path()), "this is = = not toml").unwrap();
        assert!(try_load_user_config_in(dir.path()).is_err());
    }

    #[test]
    fn load_user_config_lenient_defaults_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_user_config_in(dir.path()).user_context.is_none());
    }

    #[test]
    fn load_user_config_lenient_defaults_on_malformed() {
        // The hook depends on this fail-safe: a malformed user config.toml must
        // degrade to a default (no user context), never propagate an error.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(user_config_path_in(dir.path()), "this is = = not toml").unwrap();
        assert!(load_user_config_in(dir.path()).user_context.is_none());
    }

    #[test]
    fn resolve_user_context_prefers_repo_over_global() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(user_config_path_in(dir.path()), "user_context = true\n").unwrap();
        // repo explicitly set (even explicit-off) → repo wins, NO global fallback
        let uc = resolve_user_context_in(Some(UserContext::Toggle(false)), dir.path());
        assert!(
            uc.resolve().is_none(),
            "explicit repo-off must hard-disable"
        );
    }

    #[test]
    fn resolve_user_context_falls_back_to_global_when_repo_unset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(user_config_path_in(dir.path()), "user_context = true\n").unwrap();
        let uc = resolve_user_context_in(None, dir.path());
        // Resolve against the injected root, not the real tv_config_root().
        assert_eq!(
            uc.resolve_in(dir.path()),
            Some(default_user_context_path_in(dir.path()))
        );
    }

    #[test]
    fn resolve_user_context_off_when_neither_configured() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_user_context_in(None, dir.path())
            .resolve()
            .is_none());
    }

    #[test]
    fn from_init_flags_maps_variants() {
        assert!(matches!(
            UserContext::from_init_flags(true, None),
            Ok(Some(UserContext::Toggle(false)))
        ));
        assert!(matches!(
            UserContext::from_init_flags(false, None),
            Ok(None)
        ));
        assert!(matches!(
            UserContext::from_init_flags(false, Some("/p".into())),
            Ok(Some(UserContext::Path(_)))
        ));
        assert!(UserContext::from_init_flags(false, Some("  ".into())).is_err());
    }

    #[test]
    fn ensure_gitignore_preserves_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join(".tracevault");
        fs::create_dir_all(&config_dir).unwrap();
        let gitignore = config_dir.join(".gitignore");
        fs::write(&gitignore, "custom-user-content\n").unwrap();

        TracevaultConfig::ensure_gitignore(&config_dir).unwrap();

        // Idempotent: an existing .gitignore is never overwritten.
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            "custom-user-content\n"
        );
    }

    #[test]
    fn default_project_roundtrips_through_toml() {
        let cfg = TracevaultConfig {
            default_project: Some("payments-platform".into()),
            ..Default::default()
        };
        let toml = cfg.to_toml();
        assert!(toml.contains("default_project = \"payments-platform\""));
        let parsed: TracevaultConfig = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.default_project.as_deref(), Some("payments-platform"));
    }

    #[test]
    fn default_project_absent_is_omitted_from_toml() {
        let toml = TracevaultConfig::default().to_toml();
        assert!(!toml.contains("default_project"));
    }
}
