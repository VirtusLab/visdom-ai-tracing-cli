use crate::api_client::ApiClient;
use crate::config::TracevaultConfig;
use crate::credentials::resolve_credentials;
use crate::resolution::{git_remote_url, git_repo_name, plan_registration, RegistrationPlan};
use std::path::Path;

pub async fn sync_repo(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (server_url, credential) = resolve_credentials(project_root)?;

    let server_url = match server_url {
        Some(url) => url,
        None => {
            eprintln!("No server_url configured. Skipping sync.");
            return Ok(());
        }
    };

    if credential.is_none() {
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

    let client = ApiClient::with_credential(&server_url, credential);

    let repo_name = git_repo_name(project_root);

    // Same guard as `init`: a remote whose codebase already has a repo record
    // must not get a second one under this checkout's directory name.
    let preferred = TracevaultConfig::load(project_root)
        .and_then(|c| c.repo_id)
        .and_then(|id| id.parse::<uuid::Uuid>().ok());
    match plan_registration(&client, &remote, &repo_name, preferred).await {
        Ok(RegistrationPlan::Register) => {}
        Ok(RegistrationPlan::Existing(codebase, repo_id)) => {
            println!(
                "Repo already registered (id: {repo_id}, codebase: {}); nothing to sync",
                codebase.display_name()
            );
            return Ok(());
        }
        Ok(RegistrationPlan::Ambiguous(codebase)) => {
            eprintln!(
                "Warning: codebase {} already has {} repo records ({}) and none is named \
                 '{repo_name}'; not registering another.",
                codebase.display_name(),
                codebase.repos.len(),
                codebase.repo_list()
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!(
                "Warning: could not check the server for an existing repo of this remote ({e}); \
                 not registering."
            );
            return Ok(());
        }
    }

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
