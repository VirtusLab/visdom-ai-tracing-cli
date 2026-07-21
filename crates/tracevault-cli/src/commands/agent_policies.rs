//! `tracevault agent-policies` — fetch agent-readable Markdown instructions
//! rendered server-side from the active policies for the current repo.

use crate::api_client::{resolve_credentials, ApiClient};
use crate::resolution::{resolve_repo_by_name, ResolveRepoByNameError};
use std::path::Path;

pub async fn run(project_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (server_url, token) = resolve_credentials(project_root);

    let server_url = server_url.ok_or("No server URL configured. Run 'tracevault login' first.")?;
    let token = token.ok_or("Not logged in. Run 'tracevault login' first.")?;

    let client = ApiClient::new(&server_url, Some(&token));

    let repo = resolve_repo_by_name(&client, project_root)
        .await
        .map_err(|e| match e {
            ResolveRepoByNameError::Network(err) => err,
            ResolveRepoByNameError::NotFound { repo_name } => {
                format!("Repo '{repo_name}' not found on server. Run 'tracevault sync' first.")
                    .into()
            }
        })?;

    let resp = client.get_agent_instructions(&repo.id).await?;
    print!("{}", resp.content);
    Ok(())
}
