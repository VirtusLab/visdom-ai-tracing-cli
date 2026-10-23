use crate::api_client::ApiClient;
use crate::commands::init::persist_repo_binding;
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
            persist_repo_binding(project_root, repo_id, Some(&codebase.remote));
            println!(
                "Repo already registered (id: {repo_id}, codebase: {}); bound this checkout to it",
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
            persist_repo_binding(project_root, resp.repo_id, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::http_json;

    /// `sync` on a checkout whose remote already has a repo record (e.g. after
    /// an `init` that ran logged out) binds that record in config.toml instead
    /// of registering a second one (VIS-1018).
    #[tokio::test]
    async fn sync_binds_the_existing_repo_of_a_known_remote() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::test_helpers::init_git_repo(root);
        let ok = std::process::Command::new("git")
            .args(["-C", &root.to_string_lossy()])
            .args(["remote", "add", "origin", "git@github.com:org/repo.git"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let repo_id = "44000761-0000-4000-8000-000000000001";
        let (base, rx) = crate::test_helpers::spawn_seq(vec![
            http_json(
                "200 OK",
                r#"{"remote_id":"2280a1c9-0000-4000-8000-000000000003","name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready"}"#,
            ),
            http_json(
                "200 OK",
                &format!(
                    r#"{{"name":"tracing","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{{"id":"{repo_id}","name":"tracevault"}}]}}"#
                ),
            ),
        ]);
        std::fs::create_dir_all(root.join(".tracevault")).unwrap();
        std::fs::write(
            TracevaultConfig::config_path(root),
            format!("server_url = \"{base}\"\n"),
        )
        .unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", root);
        guard.set("TRACEVAULT_API_KEY", "tok");
        guard.remove("TRACEVAULT_SERVER_URL");

        sync_repo(root).await.unwrap();

        let cfg = TracevaultConfig::load(root).unwrap();
        assert_eq!(cfg.repo_id.as_deref(), Some(repo_id));
        assert_eq!(cfg.codebase_name.as_deref(), Some("tracing"));
        let requests: Vec<String> = rx.try_iter().collect();
        assert!(
            !requests.iter().any(|r| r.starts_with("POST /api/v1/repos")),
            "requests: {requests:?}"
        );
    }
}
