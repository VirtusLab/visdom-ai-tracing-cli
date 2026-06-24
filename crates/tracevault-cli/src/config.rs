use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct TracevaultConfig {
    pub agent: String,
    pub server_url: Option<String>,
    pub api_key: Option<String>,
    pub org_slug: Option<String>,
    pub repo_id: Option<String>,
}

impl Default for TracevaultConfig {
    fn default() -> Self {
        Self {
            agent: "claude-code".to_string(),
            server_url: None,
            api_key: None,
            org_slug: None,
            repo_id: None,
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
        let mut out = format!("# TraceVault configuration\nagent = \"{}\"\n", self.agent);
        if let Some(url) = &self.server_url {
            out.push_str(&format!("server_url = \"{url}\"\n"));
        }
        if let Some(slug) = &self.org_slug {
            out.push_str(&format!("org_slug = \"{slug}\"\n"));
        }
        if let Some(rid) = &self.repo_id {
            out.push_str(&format!("repo_id = \"{rid}\"\n"));
        }
        out
    }

    /// Parse config from the TOML file using simple line-based parsing
    /// (consistent with the existing resolve_credentials approach).
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = Self::config_path(project_root);
        let content = std::fs::read_to_string(path).ok()?;

        let parse_field = |key: &str| -> Option<String> {
            content
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split('=').nth(1))
                .map(|s| s.trim().trim_matches('"').to_string())
        };

        Some(Self {
            agent: parse_field("agent").unwrap_or_else(|| "claude-code".to_string()),
            server_url: parse_field("server_url"),
            api_key: parse_field("api_key"),
            org_slug: parse_field("org_slug"),
            repo_id: parse_field("repo_id"),
        })
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
        };
        let toml = cfg.to_toml();
        assert!(toml.contains("agent = \"claude-code\""));
        assert!(toml.contains("server_url = \"https://example.com\""));
        assert!(toml.contains("org_slug = \"my-org\""));
        assert!(toml.contains("repo_id = \"repo-1\""));
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
}
