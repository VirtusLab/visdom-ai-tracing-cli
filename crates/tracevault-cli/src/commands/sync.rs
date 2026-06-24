use crate::api_client::{resolve_credentials, ApiClient};
use crate::commands::init::git_remote_url;
use crate::config::TracevaultConfig;
use std::path::Path;

pub async fn sync_repo(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (server_url, token) = resolve_credentials(project_root);

    let server_url = match server_url {
        Some(url) => url,
        None => {
            eprintln!("No server_url configured. Skipping sync.");
            return Ok(());
        }
    };

    if token.is_none() {
        eprintln!("Not logged in. Run 'tracevault login' to sync.");
        return Ok(());
    }

    let org_slug = TracevaultConfig::load(project_root)
        .and_then(|c| c.org_slug)
        .ok_or("No org_slug in config. Run 'tracevault init' first.")?;

    let remote = match git_remote_url(project_root) {
        Some(url) => url,
        None => {
            eprintln!("No git remote 'origin' configured. Skipping sync.");
            return Ok(());
        }
    };

    let client = ApiClient::new(&server_url, token.as_deref());

    let repo_name = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(String::from)
        .unwrap_or_else(|| "unknown".into());

    match client
        .register_repo(
            &org_slug,
            crate::api_client::RegisterRepoRequest {
                repo_name,
                github_url: Some(remote.clone()),
            },
        )
        .await
    {
        Ok(resp) => {
            println!(
                "Repo synced with server (id: {}, remote: {})",
                resp.repo_id, remote
            );
        }
        Err(e) => {
            eprintln!("Warning: could not sync repo with server: {e}");
        }
    }

    Ok(())
}
