use std::fs;
use std::path::{Path, PathBuf};

use tracevault_protocol::streaming::{StreamEventRequest, StreamEventType};

use crate::config::TracevaultConfig;

/// Send a VerificationPhaseStart event to the server, recording the current
/// timestamp as the start of the verification phase for this session.
///
/// Only the most recent call per session matters — calling this again simply
/// moves the phase cursor forward, discarding events from the previous phase.
///
/// `project_root` — the git-resolved PRIMARY worktree root (from
///   `paths::resolve_project_root`). Used to locate `.tracevault/`.
/// `cwd` — the ACTUAL working directory where the CLI was invoked. Used to
///   determine the current worktree toplevel for session disambiguation. In a
///   sibling linked worktree `cwd` differs from `project_root`, so both are
///   needed.
/// `explicit_session_id` — when Some, targets that session directly.
///   When None, the most recently modified session directory is used (suitable
///   for single-agent setups; pass `--session-id` in multi-agent setups).
pub async fn open_verification_phase(
    project_root: &Path,
    cwd: &Path,
    explicit_session_id: Option<&str>,
) -> Result<(), String> {
    let config = TracevaultConfig::load(project_root)
        .ok_or("TraceVault not initialized. Run `tracevault init` first.")?;

    let repo_id = config
        .repo_id
        .as_deref()
        .ok_or("No repo_id configured. Run `tracevault init`.")?;

    let sessions_dir = project_root.join(".tracevault").join("sessions");

    let session_id = if let Some(id) = explicit_session_id {
        // Verify the session directory exists when an explicit ID is given.
        let dir = sessions_dir.join(id);
        if !dir.is_dir() {
            return Err(format!(
                "Session directory not found: {}. Check the session ID.",
                dir.display()
            ));
        }
        let worktree_top = crate::paths::worktree_toplevel(cwd);
        if origin_match(&dir, &worktree_top) == OriginMatch::Mismatch {
            return Err(format!(
                "Session {id} belongs to a different worktree (origin != {worktree_top}). \
                 Run verify-start from that worktree, or omit --session-id."
            ));
        }
        id.to_string()
    } else {
        // Auto-detect with worktree disambiguation.
        //
        // Since all worktrees share the primary `.tracevault/sessions/` dir, the
        // most-recently-modified heuristic can silently open the wrong session.
        // Instead:
        //   1. Compute the current worktree toplevel.
        //   2. Look for sessions whose `origin` marker matches it.
        //   3. If exactly one matches → use it.
        //      If multiple match → error asking for --session-id.
        //      If none match (legacy sessions without marker) → fall back to
        //      most-recently-modified + print a warning.
        // Use `cwd` (the actual invocation directory), not `project_root`
        // (the primary root), so that in a sibling worktree we correctly
        // compare against the sibling's toplevel, not the primary's.
        let worktree_top = crate::paths::worktree_toplevel(cwd);
        let matching = find_sessions_by_origin(&sessions_dir, &worktree_top);

        match matching.len() {
            1 => matching[0]
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("Could not determine session ID from origin-matched session")?
                .to_string(),
            n if n > 1 => {
                let ids: Vec<String> = matching
                    .iter()
                    .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
                    .collect();
                return Err(format!(
                    "Multiple sessions belong to this worktree ({worktree_top}): {}. \
                     Pass --session-id to select one.",
                    ids.join(", ")
                ));
            }
            _ => {
                let session_dir = find_latest_session(&sessions_dir).ok_or(
                    "No active session found. Start a session by running an AI coding agent \
                     first, or pass --session-id to target a specific session.",
                )?;
                let chosen = session_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("Could not determine session ID")?
                    .to_string();
                eprintln!(
                    "Warning: no session matched this worktree by origin marker — \
                     falling back to most recently modified session ({chosen})."
                );
                chosen
            }
        }
    };

    let event = StreamEventRequest {
        protocol_version: 2,
        // Carry the tool like the hook stream path does — the server's
        // `sessions.tool` column is NOT NULL, so sending None makes the
        // session upsert fail. (The server also defends against this, but
        // there's no reason to send a null here.)
        tool: Some("claude-code".to_string()),
        event_type: StreamEventType::VerificationPhaseStart,
        session_id: session_id.clone(),
        timestamp: chrono::Utc::now(),
        hook_event_name: None,
        tool_name: None,
        tool_use_id: None,
        tool_input: None,
        tool_response: None,
        tool_is_error: None,
        // The server's VerificationPhaseStart handler only records a timestamp;
        // it stores no event row, so neither an index nor a uuid is needed.
        event_index: None,
        event_uuid: None,
        transcript_lines: None,
        transcript_offset: None,
        model: None,
        // Record the ACTUAL invocation directory (the worktree where
        // verify-start ran), matching the stream hook's `cwd` field. Using
        // `project_root` here would record the primary checkout in a sibling
        // worktree and misattribute the verification-phase metadata.
        cwd: Some(cwd.to_string_lossy().into_owned()),
        final_stats: None,
        flow_id: None,
        labels: None,
        params: None,
    };

    // Through `resolve_client`, like every other command — NOT by pairing
    // `Credentials::load()` with `config.server_url` directly.
    //
    // This is a security fix, not tidiness. `.tracevault/config.toml` is
    // TRACKED in user repos (the `.gitignore` this CLI writes covers only
    // `sessions/`, `cache/` and `*.local.toml`), so a pull request that edited
    // `server_url` made any reviewer who ran `tracevault verify-start` in that
    // checkout POST their Keycloak access token to an arbitrary host. The
    // refresh token is protected by the credential/URL scoping in
    // `resolve_credentials`, but the access token went out on the very first
    // request, ahead of every guard. Going through `resolve_client` applies
    // that scoping here too, and picks up TRACEVAULT_API_KEY /
    // TRACEVAULT_SERVER_URL, which this path ignored entirely.
    //
    // Two deliberate precedence changes come with it: the credentials file's
    // URL now beats `config.server_url`, and having no URL configured is an
    // error instead of a stale hardcoded default (which pointed at a
    // softwaremill host this project has since migrated away from).
    let client = crate::api_client::resolve_client(project_root).map_err(|e| e.to_string())?;

    client
        .stream_event(repo_id, &event)
        .await
        .map_err(|e| format!("Failed to send verification phase event: {e}"))?;

    println!("✓ Verification phase opened for session {session_id}");
    println!("  Tool calls from this point are evaluated by verification_phase-scoped policies.");
    println!("  Run `tracevault verify-start` again to reset the phase if needed.");

    Ok(())
}

/// Return the most recently modified session directory under `sessions_dir`.
fn find_latest_session(sessions_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(sessions_dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .max_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|e| e.path())
}

#[derive(Debug, PartialEq, Eq)]
enum OriginMatch {
    Match,
    Mismatch,
    NoMarker,
}

/// Classify a session's `origin` marker against the current worktree toplevel.
fn origin_match(session_dir: &Path, worktree_top: &str) -> OriginMatch {
    match fs::read_to_string(session_dir.join("origin")) {
        Ok(s) if s.trim() == worktree_top => OriginMatch::Match,
        Ok(_) => OriginMatch::Mismatch,
        // Only a genuinely-absent marker is "legacy/no marker" (allowed).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OriginMatch::NoMarker,
        // Marker present but unreadable: don't let it bypass the cross-worktree
        // guard — treat it as a mismatch.
        Err(_) => OriginMatch::Mismatch,
    }
}

/// Return all session directories under `sessions_dir` whose `origin` marker
/// file matches `worktree_top` (exact string match after trimming).
fn find_sessions_by_origin(sessions_dir: &Path, worktree_top: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return vec![];
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let origin_file = e.path().join("origin");
            fs::read_to_string(&origin_file)
                .map(|s| s.trim() == worktree_top)
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{add_worktree, init_git_repo};

    fn make_session_dir(sessions_dir: &Path, session_id: &str, origin: Option<&str>) -> PathBuf {
        let dir = sessions_dir.join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(o) = origin {
            std::fs::write(dir.join("origin"), o).unwrap();
        }
        dir
    }

    // ── find_sessions_by_origin unit tests ───────────────────────────────────

    #[test]
    fn find_by_origin_matches_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        make_session_dir(&sessions, "sess-a", Some("/repo/primary"));
        make_session_dir(&sessions, "sess-b", Some("/repo/sibling"));
        make_session_dir(&sessions, "sess-c", None); // legacy — no marker

        let matched = find_sessions_by_origin(&sessions, "/repo/primary");
        assert_eq!(matched.len(), 1);
        assert!(matched[0].ends_with("sess-a"));
    }

    #[test]
    fn find_by_origin_no_match_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        make_session_dir(&sessions, "sess-a", Some("/repo/other"));
        make_session_dir(&sessions, "sess-b", None);

        let matched = find_sessions_by_origin(&sessions, "/repo/primary");
        assert!(matched.is_empty());
    }

    #[test]
    fn find_by_origin_multiple_match() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        make_session_dir(&sessions, "sess-a", Some("/repo/primary"));
        make_session_dir(&sessions, "sess-b", Some("/repo/primary"));
        make_session_dir(&sessions, "sess-c", Some("/other"));

        let matched = find_sessions_by_origin(&sessions, "/repo/primary");
        assert_eq!(matched.len(), 2);
    }

    // ── worktree-aware selection with real git worktrees ──────────────────────

    /// check/verify-start from a sibling worktree resolves sessions_dir to the
    /// primary `.tracevault/` directory, not the worktree dir.
    #[test]
    fn sessions_dir_from_sibling_worktree_is_under_primary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        // When open_verification_phase is called with project_root from
        // resolve_project_root(sibling_wt), the sessions_dir is primary.
        let resolved = crate::paths::resolve_project_root(&wt);
        assert_eq!(
            resolved.root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "sibling worktree must resolve to primary repo root"
        );
        let sessions_dir = resolved.root.join(".tracevault").join("sessions");
        assert!(
            sessions_dir.starts_with(&repo),
            "sessions_dir must be under primary repo root"
        );
    }

    /// Two sessions seeded under the shared sessions dir with different origins.
    /// find_sessions_by_origin must return only the one matching the current
    /// worktree toplevel.
    #[test]
    fn disambiguation_selects_current_worktree_session() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        let primary_top = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        let wt_top = wt.canonicalize().unwrap().to_string_lossy().into_owned();

        let sessions_dir = repo.join(".tracevault").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        make_session_dir(&sessions_dir, "session-primary", Some(&primary_top));
        make_session_dir(&sessions_dir, "session-sibling", Some(&wt_top));

        // From the sibling worktree, only session-sibling matches.
        let matched = find_sessions_by_origin(&sessions_dir, &wt_top);
        assert_eq!(matched.len(), 1);
        assert!(
            matched[0].ends_with("session-sibling"),
            "expected session-sibling, got {:?}",
            matched[0]
        );

        // From the primary, only session-primary matches.
        let matched_primary = find_sessions_by_origin(&sessions_dir, &primary_top);
        assert_eq!(matched_primary.len(), 1);
        assert!(matched_primary[0].ends_with("session-primary"));
    }

    /// Two sessions with the same origin → caller gets 2 results and should
    /// emit an error requesting --session-id.
    #[test]
    fn disambiguation_errors_on_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        make_session_dir(&sessions, "sess-a", Some("/worktree/top"));
        make_session_dir(&sessions, "sess-b", Some("/worktree/top"));

        let matched = find_sessions_by_origin(&sessions, "/worktree/top");
        assert_eq!(
            matched.len(),
            2,
            "both sessions must be returned for disambiguation"
        );
    }

    /// Legacy sessions with no origin markers → find_sessions_by_origin returns
    /// empty; find_latest_session (fallback) still returns a session.
    #[test]
    fn disambiguation_fallback_when_no_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        make_session_dir(&sessions, "sess-legacy-1", None);
        make_session_dir(&sessions, "sess-legacy-2", None);

        let matched = find_sessions_by_origin(&sessions, "/any/worktree");
        assert!(matched.is_empty(), "no markers means no matches");

        // Fallback must still find a session.
        let latest = find_latest_session(&sessions);
        assert!(latest.is_some(), "fallback must find a session");
    }

    /// Verify that `paths::worktree_toplevel(sibling_wt)` returns the sibling
    /// worktree's path, NOT the primary repo path. This confirms that the `cwd`
    /// parameter to `open_verification_phase` correctly identifies the invoking
    /// worktree for disambiguation purposes.
    #[test]
    fn current_worktree_toplevel_returns_sibling_top_not_primary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("sibling-wt");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        add_worktree(&repo, &wt);

        let wt_top = crate::paths::worktree_toplevel(&wt);
        let wt_canonical = wt.canonicalize().unwrap().to_string_lossy().into_owned();
        let repo_canonical = repo.canonicalize().unwrap().to_string_lossy().into_owned();

        assert_eq!(
            wt_top, wt_canonical,
            "current_worktree_toplevel from sibling must return sibling path"
        );
        assert_ne!(
            wt_top, repo_canonical,
            "current_worktree_toplevel from sibling must NOT return primary repo path"
        );
    }

    #[test]
    fn origin_match_classifies_marker_states() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let m = make_session_dir(&sessions, "match", Some("/wt/a"));
        let x = make_session_dir(&sessions, "mismatch", Some("/wt/b"));
        let n = make_session_dir(&sessions, "none", None);

        assert_eq!(origin_match(&m, "/wt/a"), OriginMatch::Match);
        assert_eq!(origin_match(&x, "/wt/a"), OriginMatch::Mismatch);
        assert_eq!(origin_match(&n, "/wt/a"), OriginMatch::NoMarker);
    }

    #[test]
    fn origin_match_unreadable_marker_is_mismatch_not_nomarker() {
        // A present-but-unreadable marker must NOT bypass the cross-worktree
        // guard. Make `origin` a directory so read_to_string fails with a
        // non-NotFound error.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-bad");
        std::fs::create_dir_all(dir.join("origin")).unwrap();

        assert_eq!(origin_match(&dir, "/wt/a"), OriginMatch::Mismatch);
    }

    /// `verify-start` used to pair `Credentials::load()` with
    /// `config.server_url` directly, bypassing everything `resolve_credentials`
    /// enforces. Since `.tracevault/config.toml` is TRACKED in user repos, a PR
    /// that edited `server_url` made any reviewer running this command POST
    /// their Keycloak access token to an arbitrary host.
    ///
    /// Going through `resolve_client` closes it by PRECEDENCE, which is stronger
    /// than refusing: the credentials file's own URL outranks
    /// `config.server_url`, so a committed config can no longer redirect the
    /// token anywhere. Proven by pointing the saved login at a local one-shot
    /// server and the config at a host that must never be contacted — if the
    /// old code path were still in place, no request would arrive here at all.
    #[tokio::test]
    async fn a_committed_config_url_cannot_redirect_the_token() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", home.path());
        guard.remove("TRACEVAULT_API_KEY");
        guard.remove("TRACEVAULT_SERVER_URL");

        let (login_server, rx) = crate::test_helpers::spawn_seq(vec![
            crate::test_helpers::http_json(
                "200 OK",
                r#"{"session_db_id":"22222222-2222-4222-8222-222222222222","event_db_id":null,"status":"ok"}"#,
            ),
        ]);

        // The saved login is for `login_server`.
        let creds_dir = home.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            format!(
                r#"{{"server_url":"{login_server}","email":"a@b.com","auth":{{
                "issuer":"https://idp.example.com/realms/v","client_id":"tracevault-cli",
                "refresh_token":"rt","access_token":"at","access_expires_at":9999999999}}}}"#
            ),
        )
        .unwrap();

        // The repo's committed config points somewhere else — the malicious PR.
        let repo = tempfile::tempdir().unwrap();
        let tv = repo.path().join(".tracevault");
        std::fs::create_dir_all(tv.join("sessions").join("sess-1")).unwrap();
        std::fs::write(
            tv.join("config.toml"),
            "repo_id = \"11111111-1111-4111-8111-111111111111\"\n\
             server_url = \"https://attacker.invalid\"\n",
        )
        .unwrap();

        open_verification_phase(repo.path(), repo.path(), Some("sess-1"))
            .await
            .expect("the event must go to the login's server");

        let request = rx
            .recv_timeout(crate::test_helpers::RECV_TIMEOUT)
            .expect("no request reached the login's server — the config URL was used instead");
        assert!(
            request.contains("/api/v1/repos/11111111-1111-4111-8111-111111111111/stream"),
            "unexpected request: {request}"
        );
        assert!(
            request.contains("Bearer at"),
            "must present the saved credential: {request}"
        );
    }

    /// And it consults the credential/URL guard: a `TRACEVAULT_SERVER_URL`
    /// pointing at another instance is refused before any request goes out,
    /// rather than sending the saved token there.
    #[tokio::test]
    async fn a_mismatched_env_server_url_refuses_before_sending() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let home = tempfile::tempdir().unwrap();
        let mut guard = crate::test_helpers::EnvVarGuard::new();
        guard.set("XDG_CONFIG_HOME", home.path());
        guard.remove("TRACEVAULT_API_KEY");
        guard.set("TRACEVAULT_SERVER_URL", "https://instance-a.example.com");

        let creds_dir = home.path().join("tracevault");
        std::fs::create_dir_all(&creds_dir).unwrap();
        std::fs::write(
            creds_dir.join("credentials.json"),
            r#"{"server_url":"https://instance-b.example.com","email":"a@b.com","auth":{
                "issuer":"https://idp.example.com/realms/v","client_id":"tracevault-cli",
                "refresh_token":"rt","access_token":"at","access_expires_at":9999999999}}"#,
        )
        .unwrap();

        let repo = tempfile::tempdir().unwrap();
        let tv = repo.path().join(".tracevault");
        std::fs::create_dir_all(tv.join("sessions").join("sess-1")).unwrap();
        std::fs::write(
            tv.join("config.toml"),
            "repo_id = \"11111111-1111-4111-8111-111111111111\"\n",
        )
        .unwrap();

        let err = open_verification_phase(repo.path(), repo.path(), Some("sess-1"))
            .await
            .expect_err("a mismatched server URL must be refused");
        assert!(
            err.contains("instance-a.example.com") && err.contains("instance-b.example.com"),
            "the refusal must name both URLs: {err}"
        );
        assert!(
            err.contains("refusing to use the saved credentials"),
            "must be the credential-scoping refusal, not a connection error: {err}"
        );
    }
}
