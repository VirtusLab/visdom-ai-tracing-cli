use crate::api_client::{resolve_credentials, ApiClient};
use crate::resolution::{git_remote_url, git_repo_name};
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

    let remote = match git_remote_url(project_root) {
        Some(url) => url,
        None => {
            eprintln!("No git remote 'origin' configured. Skipping sync.");
            return Ok(());
        }
    };

    let client = ApiClient::new(&server_url, token.as_deref());

    let repo_name = git_repo_name(project_root);

    match client
        .register_repo(crate::api_client::RegisterRepoRequest {
            repo_name,
            github_url: Some(remote.clone()),
        })
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
