use crate::api_client::{resolve_credentials, ApiClient, CiVerifyRequest};
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

fn expand_range(
    project_root: &Path,
    range: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["rev-list", "--reverse", range])
        .current_dir(project_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git rev-list failed: {stderr}").into());
    }

    let shas = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(shas)
}

pub async fn verify(
    project_root: &Path,
    cwd: &Path,
    commits: Option<&str>,
    range: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let commit_list = if let Some(range) = range {
        expand_range(cwd, range)?
    } else if let Some(commits) = commits {
        commits
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        return Err("Provide either --commits or --range".into());
    };

    if commit_list.is_empty() {
        println!("No commits to verify.");
        return Ok(());
    }

    let (server_url, token) = resolve_credentials(project_root);

    let server_url =
        match server_url {
            Some(url) => url,
            None => return Err(
                "No server URL configured. Set TRACEVAULT_SERVER_URL or run 'tracevault login'."
                    .into(),
            ),
        };

    if token.is_none() {
        return Err("No auth token. Set TRACEVAULT_API_KEY or run 'tracevault login'.".into());
    }

    let client = ApiClient::new(&server_url, token.as_deref());

    // Resolve repo_id by name
    let repo_name = git_repo_name(project_root);
    let repos = client.list_repos().await?;
    let repo = repos.iter().find(|r| r.name == repo_name).ok_or_else(|| {
        format!(
            "Repo '{}' not found on server. Run 'tracevault sync' first.",
            repo_name
        )
    })?;

    println!(
        "Verifying {} commit(s) for repo '{}'...",
        commit_list.len(),
        repo_name
    );

    let result = client
        .verify_commits(
            &repo.id,
            CiVerifyRequest {
                commits: commit_list,
            },
        )
        .await?;

    // Print results
    println!();
    for r in &result.results {
        let icon = match r.status.as_str() {
            "pass" => "\x1b[32m✓\x1b[0m",
            "unregistered" => "\x1b[33m?\x1b[0m",
            "unsealed" => "\x1b[33m○\x1b[0m",
            _ => "\x1b[31m✗\x1b[0m",
        };
        let sha_short = if r.commit_sha.len() > 7 {
            &r.commit_sha[..7]
        } else {
            &r.commit_sha
        };
        println!("  {} {} — {}", icon, sha_short, r.status);

        if r.registered && r.sealed {
            let sig_icon = if r.signature_valid {
                "\x1b[32m✓\x1b[0m"
            } else {
                "\x1b[31m✗\x1b[0m"
            };
            let chain_icon = if r.chain_valid {
                "\x1b[32m✓\x1b[0m"
            } else {
                "\x1b[31m✗\x1b[0m"
            };
            println!("      signature: {}  chain: {}", sig_icon, chain_icon);
        }

        for p in &r.policy_results {
            let p_icon = match p.result.as_str() {
                "pass" => "\x1b[32m✓\x1b[0m",
                "fail" if p.action == "block_push" => "\x1b[31m✗\x1b[0m",
                "fail" => "\x1b[33m!\x1b[0m",
                _ => " ",
            };
            println!(
                "      {} [{}] {} — {}",
                p_icon, p.severity, p.rule_name, p.details
            );
        }
    }

    // Summary
    println!();
    println!(
        "Total: {}  Registered: {}  Sealed: {}  Policy passed: {}",
        result.total_commits,
        result.registered_commits,
        result.sealed_commits,
        result.policy_passed_commits
    );

    if result.status == "pass" {
        println!("\n\x1b[32mVerification passed.\x1b[0m");
        Ok(())
    } else {
        eprintln!("\n\x1b[31mVerification failed.\x1b[0m");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_worktree, init_git_repo};

    #[test]
    fn expand_range_uses_cwd_worktree_history() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        // One extra commit in the sibling worktree.
        std::fs::write(wt.join("g.txt"), "x").unwrap();
        for args in [
            vec!["-C", wt.to_str().unwrap(), "add", "."],
            vec!["-C", wt.to_str().unwrap(), "commit", "-m", "wt commit"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .status()
                .unwrap();
        }

        let shas = expand_range(&wt, "HEAD~1..HEAD").unwrap();
        assert_eq!(
            shas.len(),
            1,
            "range must resolve against the sibling worktree's branch"
        );
    }
}
