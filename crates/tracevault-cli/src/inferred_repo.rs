//! Repo inference for project-only sessions (VIS-1258).
//!
//! A session bound to a project but to no repo (an umbrella directory such
//! as `~/git/visdom`, VIS-1255) sends its events project-only, so per-repo
//! views and commit ↔ session linkage miss them. Such a session usually works
//! in child checkouts. When an event's checkout — the git toplevel of a file
//! tool's `file_path`, else of the event's `cwd` — has an origin remote whose
//! codebase has a repo in the bound project, the event is sent with that
//! repo; otherwise it stays project-only.
//!
//! Rules:
//! - Only when no repo binding resolves from any tier (subagent override,
//!   `repo switch`, bound config, user default). Inference never overrides
//!   one, and is never stored as one: it is per event.
//! - Never a repo outside the bound project
//!   ([`crate::resolution::resolve_path_to_project_binding`]).
//! - No network per event: a checkout is resolved once per session and
//!   remembered in [`crate::session_state::SessionState::inferred_repos`]. A
//!   miss is remembered for [`NEGATIVE_TTL`], a failed lookup for
//!   [`FAILURE_TTL`], so a checkout registered later is still picked up.
//!
//! `status` reads the same cache through [`cached_repo`], the lookup the hook
//! uses before it asks the server.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::api_client::ApiClient;
use crate::session_state::{InferredRepo, RepoBinding, SessionState};

/// How long a checkout that resolved to no repo of the project is not asked
/// about again.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(15 * 60);

/// How long a checkout whose lookup failed (network, server error, timeout)
/// is not asked about again. Short: the failure says nothing about the
/// checkout, but asking on every event would spend each hook's delivery
/// budget on a server that is not answering.
pub const FAILURE_TTL: Duration = Duration::from_secs(2 * 60);

/// Cap on one checkout's lookup, inside the hook's own budget.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// The keys of `tool_input` that name the file a tool works on.
const FILE_KEYS: [&str; 2] = ["file_path", "notebook_path"];

/// The checkout an event works in: the git toplevel of the file a file tool
/// names in `tool_input` (relative paths are taken from `cwd`), else
/// `worktree_top`, the toplevel of the event's `cwd`. `None` when the named
/// file is in no git checkout: the event touches no repo — even if the shell
/// sits in one — and stays project-only.
pub(crate) fn event_checkout(
    cwd: &Path,
    tool_input: Option<&serde_json::Value>,
    worktree_top: &str,
) -> Option<String> {
    let file = tool_input
        .and_then(|input| FILE_KEYS.iter().find_map(|k| input.get(*k)?.as_str()))
        .filter(|p| !p.is_empty());
    match file {
        Some(p) => git_toplevel_of(&cwd.join(p)),
        None => Some(worktree_top.to_string()),
    }
}

/// The canonical git toplevel of the checkout holding `path` (a file or
/// directory, which need not exist yet: a `Write` creates its file), or
/// `None` outside git. Canonicalized like [`crate::paths::worktree_toplevel`],
/// so both name a checkout the same way.
fn git_toplevel_of(path: &Path) -> Option<String> {
    let dir: PathBuf = path.ancestors().find(|p| p.is_dir())?.to_path_buf();
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let top = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    Some(
        top.canonicalize()
            .unwrap_or(top)
            .to_string_lossy()
            .into_owned(),
    )
}

/// What the session's cache says about `checkout` within `project`:
/// `Some(Some(repo))` resolved, `Some(None)` a remembered miss, `None` not
/// known (never looked up, looked up for another project, or a miss whose
/// retry time has come). Pure.
fn cached(
    state: &SessionState,
    checkout: &str,
    project: uuid::Uuid,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Option<RepoBinding>> {
    let entry = state.inferred_repos.get(checkout)?;
    if entry.project_id != project.to_string() {
        return None;
    }
    if entry.repo.is_none() {
        let retry = entry
            .retry_after
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?;
        if retry <= now {
            return None;
        }
    }
    Some(entry.repo.clone())
}

/// The repo the session's events in `checkout` are inferred to belong to
/// within `project`, from the session's cache only — what the hook sends
/// under, without asking the server. `None` when not inferred (or not yet).
pub(crate) fn cached_repo(
    state: &SessionState,
    checkout: &str,
    project: uuid::Uuid,
) -> Option<RepoBinding> {
    cached(state, checkout, project, chrono::Utc::now()).flatten()
}

/// The repo the hook inferred for the session's events in `checkout`, in
/// whichever project it inferred it — where those events went, for `check`
/// and `verify-start`. Unlike [`cached_repo`] it does not re-resolve the
/// session's project: that needs the hook's own workspace config
/// (VIS-1256), which a command run in the checkout does not see.
pub(crate) fn recorded_repo(state: &SessionState, checkout: &str) -> Option<RepoBinding> {
    state.inferred_repos.get(checkout)?.repo.clone()
}

/// The repo the session's events in `checkout` belong to within `project`:
/// from the session's cache, else looked up on the server once and
/// remembered (see the module docs). Gives up at `deadline`. Never fails:
/// anything that goes wrong leaves the event project-only.
pub(crate) async fn infer_in(
    sessions_dir: &Path,
    session_id: &str,
    checkout: &str,
    project: uuid::Uuid,
    client: &ApiClient,
    deadline: Instant,
) -> Option<RepoBinding> {
    let state = crate::session_state::load_from(sessions_dir, session_id);
    if let Some(hit) = cached(&state, checkout, project, chrono::Utc::now()) {
        return hit;
    }
    let budget = LOOKUP_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
    if budget.is_zero() {
        return None;
    }
    let lookup = tokio::time::timeout(
        budget,
        crate::resolution::resolve_path_to_project_binding(Path::new(checkout), project, client),
    )
    .await;
    let (repo, ttl) = match lookup {
        Ok(Ok(Some(repo))) => (Some(repo), None),
        Ok(Ok(None)) => (None, Some(NEGATIVE_TTL)),
        Ok(Err(_)) | Err(_) => (None, Some(FAILURE_TTL)),
    };
    let retry_after = ttl.map(|ttl| {
        (chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default()).to_rfc3339()
    });
    // Under the state lock: the session's parallel hooks record other
    // checkouts in the same file. Best-effort: an unsaved entry only means
    // the next event asks again.
    let entry = InferredRepo {
        project_id: project.to_string(),
        retry_after,
        repo: repo.clone(),
    };
    let _ = crate::session_state::update_in(sessions_dir, session_id, |state| {
        state.inferred_repos.insert(checkout.to_string(), entry);
    });
    repo
}

/// [`infer_in`] for the session state in its usual place.
pub(crate) async fn infer(
    session_id: &str,
    checkout: &str,
    project: uuid::Uuid,
    client: &ApiClient,
    deadline: Instant,
) -> Option<RepoBinding> {
    let dir = crate::session_state::sessions_dir()?;
    infer_in(&dir, session_id, checkout, project, client, deadline).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{http_json, init_git_repo, spawn_seq};

    const PROJECT: &str = "a0000000-0000-4000-8000-00000000000a";
    const OTHER_PROJECT: &str = "b0000000-0000-4000-8000-00000000000b";
    const REMOTE_ID: &str = "44000761-8d22-4256-bd2c-27a0ba278c6f";
    const REPO: &str = "10000000-0000-4000-8000-000000000001";
    const REPO2: &str = "20000000-0000-4000-8000-000000000002";
    const SESSION: &str = "inferred-repo-test-session";

    fn pid(s: &str) -> uuid::Uuid {
        s.parse().unwrap()
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn checkout(url: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        assert!(Command::new("git")
            .args([
                "-C",
                &dir.path().to_string_lossy(),
                "remote",
                "add",
                "origin",
                url
            ])
            .status()
            .unwrap()
            .success());
        let top = crate::paths::worktree_toplevel(dir.path());
        (dir, top)
    }

    fn resolved() -> String {
        http_json(
            "200 OK",
            &format!(
                r#"{{"remote_id":"{REMOTE_ID}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
            ),
        )
    }

    fn detail(repos: &[&str]) -> String {
        let repos: Vec<String> = repos
            .iter()
            .map(|id| format!(r#"{{"id":"{id}","name":"repo"}}"#))
            .collect();
        http_json(
            "200 OK",
            &format!(
                r#"{{"name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{}]}}"#,
                repos.join(",")
            ),
        )
    }

    fn project(repos: &[&str]) -> String {
        let repos: Vec<String> = repos
            .iter()
            .map(|id| format!(r#"{{"id":"{id}"}}"#))
            .collect();
        http_json("200 OK", &format!(r#"{{"repos":[{}]}}"#, repos.join(",")))
    }

    fn not_found() -> String {
        http_json("404 Not Found", r#"{"error":"No remote for that URL"}"#)
    }

    #[tokio::test]
    async fn infers_a_repo_of_the_project_once_per_checkout() {
        let (_repo, top) = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![resolved(), detail(&[REPO]), project(&[REPO])]);
        let client = ApiClient::new(&base, Some("tok"));

        let first = infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later(),
        )
        .await;
        let second = infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later(),
        )
        .await;

        assert_eq!(first.as_ref().map(|b| b.repo_id.as_str()), Some(REPO));
        assert_eq!(second, first, "served from the session's cache");
        assert_eq!(rx.try_iter().count(), 3, "one lookup, three requests");
        let state = crate::session_state::load_from(sessions.path(), SESSION);
        assert!(state.active.is_none(), "inference never binds the session");
        assert_eq!(cached_repo(&state, &top, pid(PROJECT)), first);
    }

    #[tokio::test]
    async fn never_infers_a_repo_outside_the_project() {
        let (_repo, top) = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![resolved(), detail(&[REPO]), project(&[REPO2])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later(),
        )
        .await;
        let again = infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later(),
        )
        .await;

        assert!(got.is_none() && again.is_none());
        assert_eq!(rx.try_iter().count(), 3, "the miss is remembered");
    }

    #[tokio::test]
    async fn picks_the_codebase_repo_that_is_in_the_project() {
        let (_repo, top) = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![resolved(), detail(&[REPO, REPO2]), project(&[REPO2])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later(),
        )
        .await;

        assert_eq!(got.map(|b| b.repo_id), Some(REPO2.to_string()));
    }

    #[tokio::test]
    async fn an_unregistered_remote_is_remembered_as_a_miss() {
        let (_repo, top) = checkout("git@github.com:org/unknown.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![not_found()]);
        let client = ApiClient::new(&base, Some("tok"));

        assert!(infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later()
        )
        .await
        .is_none());
        assert!(infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later()
        )
        .await
        .is_none());
        assert_eq!(rx.try_iter().count(), 1);
        let state = crate::session_state::load_from(sessions.path(), SESSION);
        let entry = &state.inferred_repos[&top];
        assert!(entry.repo.is_none() && entry.retry_after.is_some());
    }

    #[tokio::test]
    async fn a_failed_lookup_is_retried_after_a_short_while() {
        let (_repo, top) = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![http_json("500 Internal Server Error", "{}")]);
        let client = ApiClient::new(&base, Some("tok"));

        assert!(infer_in(
            sessions.path(),
            SESSION,
            &top,
            pid(PROJECT),
            &client,
            later()
        )
        .await
        .is_none());
        let state = crate::session_state::load_from(sessions.path(), SESSION);
        let retry = chrono::DateTime::parse_from_rfc3339(
            state.inferred_repos[&top].retry_after.as_deref().unwrap(),
        )
        .unwrap();
        let in_secs = (retry.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
        assert!(
            in_secs <= FAILURE_TTL.as_secs() as i64 && in_secs > 0,
            "{in_secs}"
        );
    }

    #[test]
    fn cache_entries_are_per_project_and_misses_expire() {
        let now = chrono::Utc::now();
        let mut state = SessionState::default();
        let found = RepoBinding {
            repo_id: REPO.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        };
        state.inferred_repos.insert(
            "/found".into(),
            InferredRepo {
                project_id: PROJECT.into(),
                retry_after: None,
                repo: Some(found.clone()),
            },
        );
        let miss = |at: chrono::DateTime<chrono::Utc>| InferredRepo {
            project_id: PROJECT.into(),
            retry_after: Some(at.to_rfc3339()),
            repo: None,
        };
        state.inferred_repos.insert(
            "/live-miss".into(),
            miss(now + chrono::Duration::minutes(1)),
        );
        state
            .inferred_repos
            .insert("/old-miss".into(), miss(now - chrono::Duration::minutes(1)));

        assert_eq!(
            cached(&state, "/found", pid(PROJECT), now),
            Some(Some(found))
        );
        assert_eq!(cached(&state, "/found", pid(OTHER_PROJECT), now), None);
        assert_eq!(cached(&state, "/live-miss", pid(PROJECT), now), Some(None));
        assert_eq!(cached(&state, "/old-miss", pid(PROJECT), now), None);
        assert_eq!(cached(&state, "/never", pid(PROJECT), now), None);
    }

    #[test]
    fn the_checkout_is_the_touched_file_s_then_the_cwd_s() {
        let umbrella = tempfile::tempdir().unwrap();
        let child = umbrella.path().join("child");
        std::fs::create_dir_all(child.join("src")).unwrap();
        init_git_repo(&child);
        let child_top = crate::paths::worktree_toplevel(&child);
        let cwd_top = "/the/cwd/checkout";

        let input = |k: &str, v: &str| serde_json::json!({ k: v });
        // Absolute, relative to cwd, and a file that does not exist yet.
        for (key, path) in [
            (
                "file_path",
                child.join("src/main.rs").to_string_lossy().into_owned(),
            ),
            ("file_path", "child/src/new.rs".to_string()),
            ("notebook_path", "child/nb.ipynb".to_string()),
        ] {
            assert_eq!(
                event_checkout(umbrella.path(), Some(&input(key, &path)), cwd_top).as_deref(),
                Some(child_top.as_str()),
                "{key}={path}"
            );
        }
        // No file named: the cwd's checkout.
        assert_eq!(
            event_checkout(umbrella.path(), None, cwd_top).as_deref(),
            Some(cwd_top)
        );
        assert_eq!(
            event_checkout(
                umbrella.path(),
                Some(&serde_json::json!({"command": "ls"})),
                cwd_top
            )
            .as_deref(),
            Some(cwd_top)
        );
        // A named file outside git touches no checkout — not the cwd's either.
        assert_eq!(
            event_checkout(
                umbrella.path(),
                Some(&input("file_path", "notes.txt")),
                cwd_top
            ),
            None
        );
    }

    /// Parallel hooks recording different checkouts keep each other's entry.
    #[test]
    fn concurrent_cache_writes_keep_every_entry() {
        let sessions = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let dir = sessions.path().to_path_buf();
                std::thread::spawn(move || {
                    crate::session_state::update_in(&dir, SESSION, |state| {
                        state.inferred_repos.insert(
                            format!("/checkout/{i}"),
                            InferredRepo {
                                project_id: PROJECT.into(),
                                retry_after: Some("2999-01-01T00:00:00Z".into()),
                                repo: None,
                            },
                        );
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let state = crate::session_state::load_from(sessions.path(), SESSION);
        assert_eq!(state.inferred_repos.len(), 16);
        let stray = std::fs::read_dir(sessions.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray, "no temp file left behind");
    }
}
