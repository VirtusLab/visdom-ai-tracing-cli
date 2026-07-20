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
