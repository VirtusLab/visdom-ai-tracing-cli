//! `tracevault agent-policies` — fetch agent-readable Markdown instructions
//! rendered server-side from the active policies for the current repo.

use crate::api_client::{resolve_credentials, ApiClient};
use std::path::Path;
use std::process::Command;

fn git_repo_name(project_root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .as_deref()
        .and_then(|p| p.rsplit('/').next())
        .map(String::from)
        .unwrap_or_else(|| "unknown".into())
}

pub async fn run(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (server_url, token) = resolve_credentials(project_root);

    let server_url = server_url.ok_or("No server URL configured. Run 'tracevault login' first.")?;
    let token = token.ok_or("Not logged in. Run 'tracevault login' first.")?;

    let client = ApiClient::new(&server_url, Some(&token));

    let repo_name = git_repo_name(project_root);
    let repos = client.list_repos().await?;
    let repo = repos.iter().find(|r| r.name == repo_name).ok_or_else(|| {
        format!("Repo '{repo_name}' not found on server. Run 'tracevault sync' first.")
    })?;

    let resp = client.get_agent_instructions(&repo.id).await?;
    print!("{}", resp.content);
    Ok(())
}
