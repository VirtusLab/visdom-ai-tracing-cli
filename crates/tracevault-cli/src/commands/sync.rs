use crate::api_client::ApiClient;
use crate::commands::init::persist_repo_binding;
use crate::commands::migrate::{migrate_default_project, migration_report, ResolveNameError};
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

    let client = ApiClient::with_credential(&server_url, credential);

    // Before the remote check: an umbrella workspace directory has no remote
    // but may still carry a legacy `default_project` to migrate (VIS-1256).
    migrate_config_default_project(project_root, &client).await?;

    let remote = match git_remote_url(project_root) {
        Some(url) => url,
        None => {
            eprintln!("No git remote 'origin' configured. Skipping sync.");
            return Ok(());
        }
    };

    let repo_name = git_repo_name(project_root);

    // Same guard as `init`: a remote whose codebase already has a repo record
    // must not get a second one under this checkout's directory name.
    let preferred = TracevaultConfig::load(project_root)
        .and_then(|c| c.repo_id)
        .and_then(|id| id.parse::<uuid::Uuid>().ok());
    match plan_registration(&client, &remote, &repo_name, preferred).await {
        Ok(RegistrationPlan::Register) => {}
        Ok(RegistrationPlan::Existing(codebase, repo_id)) => {
            let bound = persist_repo_binding(project_root, repo_id, Some(&codebase.remote));
            println!(
                "Repo already registered (id: {repo_id}, codebase: {}); {}",
                codebase.display_name(),
                if bound {
                    "bound .tracevault/config.toml to it"
                } else {
                    "no .tracevault/config.toml here to bind (run `tracevault init` to create one)"
                }
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

/// Migrate `project_root`'s `.tracevault/config.toml` legacy
/// `default_project` NAME to `project_id` + `project_name` (VIS-1256). An
/// unknown or ambiguous name fails `sync` with the config unchanged; an
/// unreachable server is a warning (the rest of `sync` treats it the same
/// way), also with the config unchanged. A missing or malformed config is
/// left alone.
async fn migrate_config_default_project(
    project_root: &Path,
    client: &ApiClient,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(mut config) = TracevaultConfig::load(project_root) else {
        return Ok(());
    };
    match migrate_default_project(&mut config, client).await {
        Ok(migration) => {
            if let Some(line) = migration_report(&migration) {
                config.save(project_root)?;
                println!("{line}");
            }
            Ok(())
        }
        Err(ResolveNameError::Unreachable(e)) => {
            eprintln!(
                "Warning: could not migrate default_project (could not list projects: {e}); \
                 it stays unused for capture until `tracevault sync` succeeds."
            );
            Ok(())
        }
        Err(e @ ResolveNameError::Name(_)) => Err(e.to_string().into()),
    }
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

    /// VIS-1256: `sync` migrates a legacy `default_project` NAME to
    /// `project_id` + `project_name` — also in a non-git umbrella directory,
    /// which has no remote to sync otherwise — and keeps the other keys.
    #[tokio::test]
    async fn sync_migrates_default_project_in_a_non_git_dir() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let pid = "00000000-0000-4000-8000-0000000000aa";
        let (base, rx) = crate::test_helpers::spawn_seq(vec![http_json(
            "200 OK",
            &format!(r#"[{{"id":"{pid}","name":"visdom"}}]"#),
        )]);
        std::fs::create_dir_all(root.join(".tracevault")).unwrap();
        std::fs::write(
            TracevaultConfig::config_path(root),
            format!("server_url = \"{base}\"\ndefault_project = \"visdom\"\nfuture_key = 1\n"),
        )
        .unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", root);
        guard.set("TRACEVAULT_API_KEY", "tok");
        guard.remove("TRACEVAULT_SERVER_URL");

        sync_repo(root).await.unwrap();

        let raw = std::fs::read_to_string(TracevaultConfig::config_path(root)).unwrap();
        assert!(!raw.contains("default_project"), "{raw}");
        assert!(raw.contains("future_key = 1"), "{raw}");
        let cfg = TracevaultConfig::load(root).unwrap();
        assert_eq!(cfg.project_id.as_deref(), Some(pid));
        assert_eq!(cfg.project_name.as_deref(), Some("visdom"));
        assert_eq!(cfg.repo_id, None);
        let requests: Vec<String> = rx.try_iter().collect();
        assert!(
            requests[0].starts_with("GET /api/v1/projects"),
            "{requests:?}"
        );
    }

    /// An unknown name fails `sync`, naming the value, with the config
    /// unchanged.
    #[tokio::test]
    async fn sync_unknown_default_project_errors_and_keeps_config() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (base, _rx) = crate::test_helpers::spawn_seq(vec![http_json(
            "200 OK",
            r#"[{"id":"00000000-0000-4000-8000-0000000000aa","name":"visdom"}]"#,
        )]);
        std::fs::create_dir_all(root.join(".tracevault")).unwrap();
        let content = format!("server_url = \"{base}\"\ndefault_project = \"typo\"\n");
        std::fs::write(TracevaultConfig::config_path(root), &content).unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", root);
        guard.set("TRACEVAULT_API_KEY", "tok");
        guard.remove("TRACEVAULT_SERVER_URL");

        let err = sync_repo(root).await.unwrap_err().to_string();
        assert!(err.contains("'typo'"), "{err}");
        assert_eq!(
            std::fs::read_to_string(TracevaultConfig::config_path(root)).unwrap(),
            content
        );
    }
}
