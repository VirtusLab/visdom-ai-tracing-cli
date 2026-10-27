//! Binding an unbound session to a repo from where it runs (VIS-1048): the
//! hook's cwd → its checkout's `origin` remote → the server's repo for that
//! remote, stored as the session's normal `repo switch` binding. This is what
//! makes `init --global` + `login` enough to record a session started in a
//! registered repo's checkout.
//!
//! Runs only when nothing else binds the session (`effective_binding` is
//! `None`), from the `SessionStart`/`UserPromptSubmit` hooks and, for agents
//! or sessions that missed those, the stream hook. Each origin URL is looked
//! up at most once per session: a hit becomes `SessionState::active`, a miss
//! is remembered in `SessionState::auto_bind_checked`. A failed lookup
//! (network, server error, timeout) records nothing, so a later hook retries.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::api_client::ApiClient;
use crate::resolution::{
    checkout_config_repo_id, git_remote_url, git_repo_name, resolve_url_to_codebase, RepoChoice,
};
use crate::session_state::{RepoBinding, SessionState};

/// Cap on the server lookup, which runs inside a hook with a 10 s timeout;
/// the caller's deadline can shorten it further.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// What the server says about a checkout's origin remote.
#[derive(Debug)]
enum Lookup {
    Bound(RepoBinding),
    /// No codebase for the remote, or one with no live repo. Carries the
    /// message to show the user.
    Unregistered(String),
    /// Several repo records and nothing says which one this checkout is.
    Ambiguous(String),
}

/// Look up `git_url` (the origin of the checkout at `path`) on the server.
///
/// Several repos of the codebase → the one the checkout's own
/// `.tracevault/config.toml` names, else the one named like the checkout's
/// directory (as `init` would pick it), else no guess: a session bound to the
/// wrong record of a codebase is split from its other sessions (VIS-1018).
async fn look_up(
    path: &Path,
    git_url: &str,
    client: &ApiClient,
) -> Result<Lookup, Box<dyn std::error::Error>> {
    let checkout = crate::paths::worktree_toplevel(path);
    let unregistered = || {
        Lookup::Unregistered(format!(
            "TraceVault: this session is not recorded: the origin remote of {checkout} is not \
             registered with the TraceVault server. Register it by running `tracevault sync` in \
             the checkout, then bind this session with `tracevault repo switch {checkout}`."
        ))
    };
    let Some(codebase) = resolve_url_to_codebase(git_url, client).await? else {
        return Ok(unregistered());
    };
    if codebase.repos.is_empty() {
        return Ok(unregistered());
    }
    let repo_name = git_repo_name(path);
    let named = codebase
        .repos
        .iter()
        .find(|r| r.name == repo_name)
        .map(|r| r.id);
    let preferred: Vec<uuid::Uuid> = checkout_config_repo_id(path)
        .into_iter()
        .chain(named)
        .collect();
    Ok(match codebase.choose(&preferred) {
        RepoChoice::Only(id) | RepoChoice::Preferred(id, _) => Lookup::Bound(codebase.binding(id)),
        RepoChoice::Ambiguous => Lookup::Ambiguous(format!(
            "TraceVault: this session is not recorded: codebase {} has {} repo records ({}) and \
             nothing says which one {checkout} is. Bind one with `tracevault repo switch \
             {checkout} --repo-id <id>`.",
            codebase.display_name(),
            codebase.repos.len(),
            codebase.repo_list()
        )),
    })
}

/// The result of [`bind_session`]: the binding it stored (if any), and a
/// message for the user the first time a remote cannot be bound.
#[derive(Debug, Default)]
pub(crate) struct AutoBind {
    pub binding: Option<RepoBinding>,
    pub notice: Option<String>,
}

/// Bind session `session_id` (stored under `sessions_dir`) to the repo of the
/// checkout at `cwd`, if it has an origin remote the server knows. Callers run
/// this only when the session resolves to no repo. The lookup gives up at
/// `deadline` (the hook's own budget). Never fails: anything that goes wrong
/// leaves the session unbound.
pub(crate) async fn bind_session_in(
    sessions_dir: &Path,
    session_id: &str,
    cwd: &Path,
    client: &ApiClient,
    deadline: Instant,
) -> AutoBind {
    let Some(git_url) = git_remote_url(cwd) else {
        return AutoBind::default();
    };
    if !wants_lookup(
        &crate::session_state::load_from(sessions_dir, session_id),
        &git_url,
    ) {
        return AutoBind::default();
    }
    let budget = LOOKUP_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
    let lookup = match tokio::time::timeout(budget, look_up(cwd, &git_url, client)).await {
        Ok(Ok(lookup)) => lookup,
        Ok(Err(_)) | Err(_) => return AutoBind::default(),
    };
    // Reload after the lookup: a `repo switch` or `repo reset` that ran
    // meanwhile, or a concurrent hook that already recorded this remote,
    // wins over this lookup's result.
    let mut state = crate::session_state::load_from(sessions_dir, session_id);
    if !wants_lookup(&state, &git_url) {
        return AutoBind {
            binding: state.active,
            notice: None,
        };
    }
    let result = match lookup {
        Lookup::Bound(binding) => {
            state.active = Some(binding.clone());
            AutoBind {
                binding: Some(binding),
                notice: None,
            }
        }
        Lookup::Unregistered(notice) | Lookup::Ambiguous(notice) => {
            state.auto_bind_checked.push(git_url);
            AutoBind {
                binding: None,
                notice: Some(notice),
            }
        }
    };
    // Best-effort: an unsaved binding still attributes this event, and the
    // next hook simply looks it up again.
    let _ = crate::session_state::save_to(sessions_dir, session_id, &state);
    result
}

/// [`bind_session_in`] for the session state in its usual place.
pub(crate) async fn bind_session(
    session_id: &str,
    cwd: &Path,
    client: &ApiClient,
    deadline: Instant,
) -> AutoBind {
    match crate::session_state::sessions_dir() {
        Some(dir) => bind_session_in(&dir, session_id, cwd, client, deadline).await,
        None => AutoBind::default(),
    }
}

/// Whether a session in `state` should look up `git_url`. Pure.
fn wants_lookup(state: &SessionState, git_url: &str) -> bool {
    state.active.is_none()
        && !state.auto_bind_off
        && !state.auto_bind_checked.iter().any(|u| u == git_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{http_json, init_git_repo, spawn_seq};

    const REMOTE_ID: &str = "44000761-8d22-4256-bd2c-27a0ba278c6f";
    const LOW: &str = "10000000-0000-4000-8000-000000000001";
    const HIGH: &str = "20000000-0000-4000-8000-000000000002";
    const SESSION: &str = "auto-bind-test-session";

    fn checkout(url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        let ok = std::process::Command::new("git")
            .args([
                "-C",
                &dir.path().to_string_lossy(),
                "remote",
                "add",
                "origin",
                url,
            ])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        dir
    }

    fn resolved() -> String {
        http_json(
            "200 OK",
            &format!(
                r#"{{"remote_id":"{REMOTE_ID}","name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready"}}"#
            ),
        )
    }

    fn detail(repos: &[(&str, &str)]) -> String {
        let repos: Vec<String> = repos
            .iter()
            .map(|(id, name)| format!(r#"{{"id":"{id}","name":"{name}"}}"#))
            .collect();
        http_json(
            "200 OK",
            &format!(
                r#"{{"name":"repo","normalized_url":"github.com/org/repo","clone_status":"ready","repos":[{}]}}"#,
                repos.join(",")
            ),
        )
    }

    fn not_found() -> String {
        http_json("404 Not Found", r#"{"error":"No remote for that URL"}"#)
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn state(sessions: &Path) -> SessionState {
        crate::session_state::load_from(sessions, SESSION)
    }

    #[tokio::test]
    async fn binds_a_registered_remote_as_the_session_binding() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![resolved(), detail(&[(LOW, "repo")])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        let binding = got.binding.expect("bound");
        assert_eq!(binding.repo_id, LOW);
        assert_eq!(
            binding.git_url.as_deref(),
            Some("git@github.com:org/repo.git")
        );
        assert!(got.notice.is_none());
        assert_eq!(state(sessions.path()).active, Some(binding));
    }

    #[tokio::test]
    async fn a_bound_session_is_not_looked_up_again() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![resolved(), detail(&[(LOW, "repo")])]);
        let client = ApiClient::new(&base, Some("tok"));

        bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;
        let again = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(again.binding.is_none() && again.notice.is_none());
        assert_eq!(rx.try_iter().count(), 2, "one lookup: resolve + detail");
    }

    #[tokio::test]
    async fn an_unregistered_remote_stays_unbound_and_is_reported_once() {
        let repo = checkout("git@github.com:org/unregistered.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![not_found()]);
        let client = ApiClient::new(&base, Some("tok"));

        let first = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;
        let second = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(first.binding.is_none());
        let notice = first.notice.expect("said once");
        assert!(notice.contains("not registered"), "{notice}");
        assert!(second.binding.is_none() && second.notice.is_none());
        assert_eq!(rx.try_iter().count(), 1, "the miss is not looked up again");
        let st = state(sessions.path());
        assert!(st.active.is_none());
        assert_eq!(
            st.auto_bind_checked,
            vec!["git@github.com:org/unregistered.git"]
        );
    }

    #[tokio::test]
    async fn a_codebase_without_live_repos_counts_as_unregistered() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![resolved(), detail(&[])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(got.binding.is_none());
        assert!(got.notice.unwrap().contains("not registered"));
    }

    #[tokio::test]
    async fn several_repo_records_bind_the_one_named_like_the_checkout() {
        let repo = checkout("git@github.com:org/repo.git");
        let dir_name = repo
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![
            resolved(),
            detail(&[(LOW, "other"), (HIGH, &dir_name)]),
        ]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert_eq!(got.binding.unwrap().repo_id, HIGH);
    }

    #[tokio::test]
    async fn several_repo_records_and_no_hint_stay_unbound_without_guessing() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, _rx) = spawn_seq(vec![resolved(), detail(&[(LOW, "a"), (HIGH, "b")])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(got.binding.is_none());
        let notice = got.notice.unwrap();
        assert!(notice.contains("--repo-id"), "{notice}");
        assert!(state(sessions.path()).active.is_none());
    }

    #[tokio::test]
    async fn a_failed_lookup_records_nothing_so_a_later_hook_retries() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let client = ApiClient::new("http://127.0.0.1:1", Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(got.binding.is_none() && got.notice.is_none());
        assert_eq!(state(sessions.path()), SessionState::default());
    }

    #[tokio::test]
    async fn a_spent_deadline_skips_the_lookup_and_records_nothing() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let (base, rx) = spawn_seq(vec![resolved(), detail(&[(LOW, "repo")])]);
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(
            sessions.path(),
            SESSION,
            repo.path(),
            &client,
            Instant::now(),
        )
        .await;

        assert!(got.binding.is_none() && got.notice.is_none());
        assert_eq!(state(sessions.path()), SessionState::default());
        assert_eq!(rx.try_iter().count(), 0);
    }

    /// Serve `responses` in order, running `during` (another writer of the
    /// session state) before answering the first request — i.e. while the
    /// lookup is in flight.
    fn spawn_with_concurrent_write(
        responses: Vec<String>,
        during: impl FnOnce() + Send + 'static,
    ) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut during = Some(during);
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                    line.clear();
                }
                if let Some(f) = during.take() {
                    f();
                }
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.as_bytes());
            }
        });
        base
    }

    #[tokio::test]
    async fn a_repo_reset_during_the_lookup_keeps_the_session_unbound() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let dir = sessions.path().to_path_buf();
        let base = spawn_with_concurrent_write(vec![resolved(), detail(&[(LOW, "repo")])], {
            let dir = dir.clone();
            move || {
                let reset = SessionState {
                    auto_bind_off: true,
                    ..Default::default()
                };
                crate::session_state::save_to(&dir, SESSION, &reset).unwrap();
            }
        });
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(&dir, SESSION, repo.path(), &client, later()).await;

        assert!(got.binding.is_none());
        let st = state(&dir);
        assert!(st.active.is_none() && st.auto_bind_off);
    }

    #[tokio::test]
    async fn a_repo_switch_during_the_lookup_wins() {
        let repo = checkout("git@github.com:org/repo.git");
        let sessions = tempfile::tempdir().unwrap();
        let dir = sessions.path().to_path_buf();
        let switched = RepoBinding {
            repo_id: HIGH.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: String::new(),
        };
        let base = spawn_with_concurrent_write(vec![resolved(), detail(&[(LOW, "repo")])], {
            let dir = dir.clone();
            let switched = switched.clone();
            move || {
                let state = SessionState {
                    active: Some(switched),
                    ..Default::default()
                };
                crate::session_state::save_to(&dir, SESSION, &state).unwrap();
            }
        });
        let client = ApiClient::new(&base, Some("tok"));

        let got = bind_session_in(&dir, SESSION, repo.path(), &client, later()).await;

        assert_eq!(got.binding.as_ref(), Some(&switched));
        assert_eq!(state(&dir).active, Some(switched));
    }

    #[tokio::test]
    async fn no_origin_remote_means_no_lookup() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let sessions = tempfile::tempdir().unwrap();
        // Nothing listens here: a lookup would fail, not hang, but must not happen.
        let client = ApiClient::new("http://127.0.0.1:1", Some("tok"));

        let got = bind_session_in(sessions.path(), SESSION, repo.path(), &client, later()).await;

        assert!(got.binding.is_none() && got.notice.is_none());
        assert_eq!(state(sessions.path()), SessionState::default());
    }

    #[test]
    fn wants_lookup_only_for_an_unbound_session_and_an_unchecked_remote() {
        let url = "git@github.com:org/repo.git";
        assert!(wants_lookup(&SessionState::default(), url));
        let switched = SessionState {
            active: Some(RepoBinding {
                repo_id: LOW.into(),
                git_url: None,
                remote_id: None,
                codebase_name: None,
                updated_at: String::new(),
            }),
            ..Default::default()
        };
        assert!(!wants_lookup(&switched, url));
        let reset = SessionState {
            auto_bind_off: true,
            ..Default::default()
        };
        assert!(!wants_lookup(&reset, url));
        let checked = SessionState {
            auto_bind_checked: vec![url.into()],
            ..Default::default()
        };
        assert!(!wants_lookup(&checked, url));
        // Another remote (the session moved to another checkout) is looked up.
        assert!(wants_lookup(&checked, "git@github.com:org/other.git"));
    }
}
