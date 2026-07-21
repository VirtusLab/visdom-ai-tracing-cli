use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub server_url: String,
    pub token: String,
    pub email: String,
}

impl Credentials {
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

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(&path, json)
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
        assert_eq!(creds.token, "tok");
        assert_eq!(creds.email, "a@b.com");
    }
}
