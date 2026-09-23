use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracevault_protocol::hooks::{parse_hook_event, HookResponse};
use tracevault_protocol::streaming::{
    extract_is_error_from_transcript, StreamEventRequest, StreamEventType,
};

/// Convert a resolved [`crate::context::EffectiveContext`] into the three
/// optional fields that are stamped onto a [`StreamEventRequest`].
///
/// - `flow_id`  — taken directly from `ctx.flow_id`
/// - `labels`   — `None` when the vec is empty, `Some(vec)` otherwise
/// - `params`   — `None` when the map is empty, `Some(HashMap)` otherwise
///   (BTreeMap → HashMap conversion)
///
/// This is a pure function so it can be unit-tested without I/O.
#[allow(clippy::type_complexity)]
pub fn apply_context(
    ctx: crate::context::EffectiveContext,
) -> (
    Option<String>,
    Option<Vec<String>>,
    Option<HashMap<String, String>>,
) {
    let flow_id = ctx.flow_id;
    let labels = if ctx.labels.is_empty() {
        None
    } else {
        Some(ctx.labels)
    };
    let params = if ctx.params.is_empty() {
        None
    } else {
        Some(ctx.params.into_iter().collect())
    };
    (flow_id, labels, params)
}

/// Returns `(lines, start_offset, end_offset)`: `start_offset` is the seek
/// position read from `offset_path` (0 if absent), and `end_offset` is the
/// byte offset after the last line read (the old `bytes_read`).
///
/// Callers should send `start_offset` as `StreamEventRequest::transcript_offset`
/// (so the server's `chunk_index = transcript_offset + line_index` is stable
/// per physical line across overlapping/retried reads) and persist
/// `end_offset` to `.stream_offset` as the next read's seek position.
pub fn read_new_transcript_lines(
    transcript_path: &Path,
    offset_path: &Path,
) -> Result<(Vec<serde_json::Value>, i64, i64), io::Error> {
    // An empty path means "no transcript yet" — Codex documents `transcript_path`
    // as nullable (e.g. on SessionStart before a rollout exists), and the hook
    // payload deserializes null/absent to "". Guard explicitly so we never hand
    // `Path::new("")` to `exists()`/`File::open` (whose behavior on an empty path
    // is platform-dependent); treat it as a clean no-op.
    if transcript_path.as_os_str().is_empty() || !transcript_path.exists() {
        return Ok((vec![], 0, 0));
    }

    let offset: i64 = if offset_path.exists() {
        let content = fs::read_to_string(offset_path)?;
        content
            .trim()
            .parse::<i64>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        0
    };

    let mut file = fs::File::open(transcript_path)?;
    file.seek(SeekFrom::Start(offset as u64))?;

    let reader = io::BufReader::new(file);
    let mut lines = Vec::new();
    let mut bytes_read = offset;

    for line_result in reader.lines() {
        let line = line_result?;
        // +1 for the newline character
        bytes_read += line.len() as i64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            lines.push(value);
        }
    }

    Ok((lines, offset, bytes_read))
}

pub fn append_pending(pending_path: &Path, json: &str) -> Result<(), io::Error> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_path)?;
    writeln!(file, "{json}")?;
    Ok(())
}

pub fn drain_pending(pending_path: &Path) -> Result<Vec<String>, io::Error> {
    if !pending_path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(pending_path)?;
    let lines: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    fs::remove_file(pending_path)?;
    Ok(lines)
}

/// How a stream event is attributed on the wire, resolved from the local repo
/// and project bindings before anything is sent.
///
/// The rule this type encodes: an event ships when EITHER a repo or a project
/// resolves. Before this existed the hook required a repo binding and dropped
/// the event otherwise, which silently discarded every event of a repo-less
/// session even when its project was bound — despite the server supporting
/// exactly that shape (`ProjectStreamQuery::repo_id` is `Option<Uuid>`:
/// "repo-less (0-repo) projects are supported").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// A usable repo binding resolved. `project` is the capture-time project
    /// overlay; when `None` the server deduces the project from the repo.
    Repo {
        repo_id: String,
        project: Option<uuid::Uuid>,
    },
    /// No usable repo binding, but a project did — repo-less project-scoped
    /// ingest.
    ProjectOnly { project_id: uuid::Uuid },
}

impl Attribution {
    /// Offline-queue filename for this attribution. Keyed by whatever
    /// identifies the target so a mid-session rebind (workspace mode) can
    /// never flush one target's queued events to another.
    ///
    /// The `project-` infix is load-bearing for back-compat: an older CLI's
    /// `repo_id_from_pending_filename` extracts `project-<uuid>` from this
    /// name, fails its UUID check, and skips the file — so a downgrade leaves
    /// these queues alone instead of trying to POST them at a repo endpoint.
    pub(crate) fn pending_file_name(&self) -> String {
        use crate::commands::flush::{PROJECT_QUEUE_PREFIX, QUEUE_PREFIX, QUEUE_SUFFIX};
        match self {
            Self::Repo { repo_id, .. } => format!("{QUEUE_PREFIX}{repo_id}{QUEUE_SUFFIX}"),
            Self::ProjectOnly { project_id } => {
                format!("{PROJECT_QUEUE_PREFIX}{project_id}{QUEUE_SUFFIX}")
            }
        }
    }
}

/// Pure attribution resolver: fold the repo binding and the capture-time
/// project into the target this event will be sent to. `None` means neither
/// resolved and the hook must no-op.
///
/// A binding whose `repo_id` is not a UUID degrades to `ProjectOnly` rather
/// than dropping the event — a corrupted session-state file should cost the
/// repo attribution, not the whole trace, when a project is available.
///
/// `project` here is whatever `capture_project` resolved, and its lowest tier
/// is the MACHINE-GLOBAL `project switch --user` default — not just
/// session-scoped bindings. That tier is therefore now a sufficient condition
/// for an event to ship, where previously it could only redirect an event that
/// was already shipping because a repo bound. This is deliberate: it is the
/// only binding a container can establish before the agent session exists and
/// before the repo is cloned, which is the case this whole path serves.
/// `resolve_stream_binding` has the same machine-global tier for repos, so the
/// two sides stay symmetric. Consequence worth knowing: a session started in an
/// unrelated directory, with no repo and no `.tracevault/`, will attribute to
/// that default project.
pub(crate) fn attribution_for(
    binding: Option<&crate::session_state::RepoBinding>,
    project: Option<uuid::Uuid>,
) -> Option<Attribution> {
    match binding.filter(|b| binding_repo_id_is_valid(&b.repo_id)) {
        Some(b) => Some(Attribution::Repo {
            repo_id: b.repo_id.clone(),
            project,
        }),
        None => project.map(|project_id| Attribution::ProjectOnly { project_id }),
    }
}

/// Offline-queue path for an attribution target.
fn pending_path_for(session_dir: &Path, attribution: &Attribution) -> std::path::PathBuf {
    session_dir.join(attribution.pending_file_name())
}

/// Resolve the project root and session directory for a stream hook invocation.
///
/// This is the pure, testable core of `run_stream`'s path resolution. It uses
/// [`crate::paths::resolve_project_root`] which queries `git rev-parse
/// --git-common-dir` first, so it correctly resolves to the **primary**
/// `.tracevault/` directory from any worktree — including sibling linked
/// worktrees where the primary `.tracevault/` is not an ancestor of `hook_cwd`.
///
/// Returns `(resolved, session_dir)` — the resolved
/// [`crate::paths::ProjectRoot`] (so callers can inspect `.source`, e.g. to warn
/// on a Fallback that would create a stray `.tracevault/`) and `session_dir`
/// under `<resolved.root>/.tracevault/sessions/<session_id>/`.
pub fn resolve_session_paths(
    hook_cwd: &Path,
    session_id: &str,
) -> (crate::paths::ProjectRoot, std::path::PathBuf) {
    let resolved = crate::paths::resolve_project_root(hook_cwd);
    let session_dir = resolved
        .root
        .join(".tracevault")
        .join("sessions")
        .join(session_id);
    (resolved, session_dir)
}

/// A resolved binding is usable for hook attribution only if its repo_id is a
/// real UUID — guards against a corrupted/edited session-state file injecting
/// path separators into the pending-<repo_id>.jsonl filename.
///
/// `pub(crate)`: also called by `commands::status`, so a binding with a
/// malformed `repo_id` is never displayed as bound (`Check::ok`) when this
/// same check means the hook will not actually honor it.
pub(crate) fn binding_repo_id_is_valid(repo_id: &str) -> bool {
    uuid::Uuid::parse_str(repo_id).is_ok()
}

/// The effective repo binding for a stream event, given the loaded session
/// state, the event's worktree, and the bound-config fallback. Pure — the
/// hook path has no repo-override flag, so `repo_flag` is always `None` here.
pub(crate) fn resolve_stream_binding(
    session: &crate::session_state::SessionState,
    worktree: &str,
    bound: Option<crate::session_state::RepoBinding>,
    user_default: Option<crate::session_state::RepoBinding>,
) -> Option<crate::session_state::RepoBinding> {
    crate::resolution::effective_binding(crate::resolution::ResolveInputs {
        repo_flag: None,
        session,
        worktree_path: Some(worktree),
        bound,
        user_default,
    })
    .map(|(b, _)| b)
}

/// `TRACEVAULT_PROJECT` as a binding, UUID form only.
///
/// A project NAME would require a `list_projects` round trip, and the capture
/// path fires per event in a short-lived process — the same reason
/// `capture_project` already ignores the repo config's `default_project`. The
/// name form is honoured by the interactive commands, which already have a
/// client in hand.
pub(crate) fn env_project_binding() -> Option<crate::session_state::ProjectBinding> {
    let raw = std::env::var("TRACEVAULT_PROJECT").ok()?;
    let raw = raw.trim();
    let parsed = raw.parse::<uuid::Uuid>().ok()?;
    Some(crate::session_state::ProjectBinding {
        project_id: parsed.to_string(),
        project_name: String::new(),
        updated_at: String::new(),
        forced_until: None,
    })
}

/// Which attribution mode THIS PARTICULAR winning binding declares.
///
/// `explicit` comes either from `TRACEVAULT_PROJECT_ATTRIBUTION` (per-process,
/// re-asserted at every launch, no expiry — genuinely global, by design) OR
/// from a LIVE `forced_until` carried by `effective` — the exact
/// [`crate::session_state::ProjectBinding`] that [`capture_project`] resolved
/// as the one this event is being attributed to.
///
/// `forced_until` is stored PER-BINDING (see that field's own doc comment:
/// "the FORCE lapses (the binding itself survives)"), so this must read the
/// force off THAT binding and no other. An earlier version of this function
/// instead did a process-global disk read
/// (`user_project_default::load_with_force()`), which had it backwards two
/// ways at once: a session-scoped force (`project switch` without `--user`,
/// which is the ordinary case whenever a session id is set) is written into
/// session state and was never read there at all, so it silently never took
/// effect while still telling the user membership was not being checked; and
/// a live force sitting on the user-level default leaked onto ANY
/// higher-precedence project that happened to resolve instead (subagent
/// override, `TRACEVAULT_PROJECT`, session active), stamping `explicit` for a
/// project nobody forced. Taking `effective` as a parameter — the actual
/// winning binding — makes both mistakes impossible: a force can only apply
/// when it is attached to the binding actually being used.
///
/// Whether that force is live is [`crate::session_state::force_status`]'s
/// answer, not this function's: `commands::project`'s status line reads the
/// same field and must never disagree with the header this fills, so there is
/// exactly one parse and one `Utc::now()` comparison. A lapsed persisted
/// force reads as `derived` — not sending an `explicit` header IS the
/// fallback, so ingest never starts failing because a force was forgotten —
/// and so does an unparseable one, for the same fail-safe reason.
///
/// An unrecognised (or absent) `TRACEVAULT_PROJECT_ATTRIBUTION` also reads as
/// `derived`, deliberately asymmetric with the server, which 400s an unknown
/// value: that strictness is for callers that bypass the CLI entirely, and
/// failing a hook over a typo'd env var helps nobody — silently falling back
/// to the always-checked default is the safe direction to fail in.
///
/// This always returns one of the two strings and the caller always sends it
/// as the header — including the `"derived"` case. The server treats an
/// absent header and an explicit `derived` value identically, but nothing
/// here omits the header; "always emits one of two values" is simpler to
/// reason about than "sometimes sends a header, sometimes doesn't."
pub(crate) fn attribution_mode(
    effective: Option<&crate::session_state::ProjectBinding>,
) -> &'static str {
    let env_forced = std::env::var("TRACEVAULT_PROJECT_ATTRIBUTION")
        .map(|v| v.trim().eq_ignore_ascii_case("explicit"))
        .unwrap_or(false);
    let binding_forced = effective
        .and_then(|b| b.forced_until.as_deref())
        .is_some_and(|s| {
            matches!(
                crate::session_state::force_status(s),
                crate::session_state::ForceStatus::Live(_)
            )
        });
    if env_forced || binding_forced {
        "explicit"
    } else {
        "derived"
    }
}

/// The capture-time project BINDING for a stream event: a thin wrapper over
/// the shared chain [`crate::resolution::capture_project_binding`] (subagent
/// worktree override -> `TRACEVAULT_PROJECT` -> session `active_project` ->
/// user-level default; no `--project` flag on the hook path), with a binding
/// whose stored id is not a UUID dropped exactly as
/// [`crate::resolution::capture_project_id`] drops it. Local only — no
/// network (the hook fires per event in a short-lived process), which is why
/// repo config `default_project` (a name) is not a tier and why
/// `TRACEVAULT_PROJECT` is honoured in its UUID form only.
/// `commands::project::status` calls the same shared chain, so it reports
/// exactly what this returns.
///
/// Returns the full [`crate::session_state::ProjectBinding`], not just its
/// id: `forced_until` travels with whichever binding actually wins this
/// precedence chain, so a caller can ask THAT binding — via
/// [`attribution_mode`] — whether it is currently forced, rather than
/// consulting some other, unrelated binding (see `attribution_mode`'s doc
/// comment for why that distinction matters). Callers that only need the id
/// use [`capture_project`].
pub(crate) fn capture_binding(
    session: &crate::session_state::SessionState,
    worktree_path: Option<&str>,
) -> Option<crate::session_state::ProjectBinding> {
    use crate::resolution::{capture_project_binding, capture_project_id, CaptureProjectInputs};
    let (binding, _) = capture_project_binding(&CaptureProjectInputs {
        project_flag: None,
        env_project: env_project_binding(),
        session,
        worktree_path,
        user_default: crate::user_project_default::load(),
    })?;
    // Defensive: a corrupted/hand-edited binding whose id isn't a real UUID
    // must not be usable for attribution at all (mirrors
    // `binding_repo_id_is_valid` for the repo side) — checked here, once, so
    // every caller gets the same guarantee rather than re-deriving it.
    capture_project_id(&binding)?;
    Some(binding)
}

/// The capture-time project ID for a stream event: [`capture_binding`]
/// projected down to its id. `None` -> fall back to the repo-scoped stream
/// (server deduces).
///
/// `pub(crate)`: also called by `commands::status`, which reuses this
/// function (plus `resolve_stream_binding`/`attribution_for`) as the
/// authoritative "will this session record anything" gate, so the status
/// verdict can never drift from what this hook actually does. `commands::flush`
/// and `commands::project::status` call it for the same reason.
pub(crate) fn capture_project(
    session: &crate::session_state::SessionState,
    worktree_path: Option<&str>,
) -> Option<uuid::Uuid> {
    capture_binding(session, worktree_path)
        .as_ref()
        .and_then(crate::resolution::capture_project_id)
}

/// The one-line warning printed when a repo-less event is dropped as
/// permanently undeliverable. Pure, so the wording is asserted directly.
fn undeliverable_warning(pid: uuid::Uuid) -> String {
    format!(
        "tracevault: warning: project {pid} does not resolve (400/404/409); this session has no \
         repo binding, so there is nothing to attribute these events to and they are being \
         DROPPED rather than queued. Run `tracevault project switch <name>` to bind a project \
         that exists."
    )
}

/// True when a stream-send error's rendered message carries a `(4xx ...)`
/// status marker — i.e. the server deterministically rejected the request in
/// a way that will never succeed on blind retry (bad binding, missing
/// permission, ambiguous repo, ...). Deliberately excludes 5xx and
/// transport/network/timeout failures (which have no `(NNN ` marker at all,
/// or a 5xx one) — those remain transient and must keep propagating so the
/// existing buffer/retry logic in `run_stream` still applies to them.
///
/// Takes `&dyn Error` rather than `&Box<dyn Error>` to avoid the clippy
/// `borrowed_box` lint at call sites that already hold a `Box<dyn Error>`.
///
/// COUPLING: the `"(NNN "` markers below must byte-for-byte match what
/// `ApiClient::authed_send_json`'s `err_prefix` closures render (e.g.
/// `stream_event`'s `"Stream failed ({status})"`, where `{status}` Displays
/// as `"404 Not Found"`, yielding `"Stream failed (404 Not Found): ..."` —
/// hence the `"(404 "` marker below matching on the open-paren + code + the
/// space before the reason phrase). If that error-formatting ever changes,
/// update this list (and its tests) to match.
/// Which deterministic client error this is, so the refusal error can name a
/// cause that is actually possible instead of assuming one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientErrorKind {
    /// 403, which now has TWO plausible causes — see [`send_stream_event`].
    Forbidden,
    /// 400/404/409: a binding/scoping problem, and only that.
    Scoping,
}

/// The one-line error printed when the server refuses a project-scoped send.
/// Pure, so the wording is asserted directly rather than by capturing stderr.
///
/// `mode` is the attribution mode this send actually declared — the same
/// value [`attribution_mode`] put in the `x-tracevault-project-attribution`
/// header, so the message and the wire can never disagree. It matters because
/// the force gate also refuses with a 403: "not Operator on the project", or
/// "`explicit` from a `tvk_` key". Without naming it, an operator who asked
/// for `explicit` got an error about membership and realm roles and was never
/// told the force itself was what got refused. Failing closed on trust is
/// right; failing closed silently is the defect.
///
/// The extra clause is attached to `Forbidden` ONLY. A `Scoping` 4xx
/// (400/404/409) means the project id itself does not resolve — the force
/// gate never ran — so blaming the force there would be a new wrong
/// explanation of exactly the kind this fixes.
pub(crate) fn refused_error(pid: uuid::Uuid, kind: &ClientErrorKind, mode: &str) -> String {
    let base = match kind {
        // Since Keycloak, a 403 has a SECOND and now more common cause: the
        // account has no `tracing` realm role at all, in which case nothing this
        // hook does will work and `project switch` is confidently wrong advice —
        // on the code path users hit most. The server's 403 envelope is not
        // distinguishable from here (see `ClientErrorKind`), so name both.
        ClientErrorKind::Forbidden => format!(
            "tracevault: error: the server refused to attribute this event to active project \
             {pid} (403). Either that project does not apply to this repo (not a member, or \
             missing TracePush) — run `tracevault project switch <name>` (add `--user` if the \
             binding is the machine-wide default in `user_project.toml`) — or this account lacks \
             the `tracing` Keycloak realm role entirely, which an administrator must grant. The \
             event was NOT re-attributed elsewhere; it has been queued and will be retried once \
             the binding or the role is fixed."
        ),
        ClientErrorKind::Scoping => format!(
            "tracevault: error: active project {pid} does not apply to this repo (not a member, \
             missing permission, or the project no longer exists). The event was NOT \
             re-attributed elsewhere; it has been queued and will be retried once the binding is \
             fixed. Run `tracevault project switch <name>` (add `--user` if the binding is the \
             machine-wide default in `user_project.toml`) to update it."
        ),
    };
    if mode == "explicit" && matches!(kind, ClientErrorKind::Forbidden) {
        return format!(
            "{base} This send declared `explicit` attribution, so the refusal may be of the \
             FORCE itself: forcing needs `Operator` on that project AND a Control Plane \
             identity (a `tvk_` API key can never force). Drop the force with `tracevault \
             project switch <name>` (no `--project-attribution explicit`) if membership \
             attribution is what you want."
        );
    }
    base
}

pub(crate) fn deterministic_client_error_kind(
    e: &dyn std::error::Error,
) -> Option<ClientErrorKind> {
    let s = e.to_string();
    // 401 is deliberately excluded: it's an authentication failure (bad/expired
    // token), not a project-scoping problem, and the buffer/retry path already
    // handles it correctly without a misleading project-scoping message. 403
    // stays in: it can mean the token is valid but lacks TracePush on the bound
    // project (or the account lacks the realm role entirely), and the refusal
    // line can name both possible causes.
    if s.contains("(403 ") {
        return Some(ClientErrorKind::Forbidden);
    }
    if ["(400 ", "(404 ", "(409 "]
        .iter()
        .any(|marker| s.contains(marker))
    {
        return Some(ClientErrorKind::Scoping);
    }
    None
}

/// Send a single stream event, routing to the project-scoped endpoint when a
/// local project binding resolved (`capture_pid = Some(_)`), else the
/// repo-scoped endpoint (server-side deduction/refusal). Factored out so both
/// the pending-flush loop and the live send share one branch.
///
/// Routing is decided by the [`Attribution`] resolved before the send:
/// `Repo` with no project goes to the repo-scoped endpoint (the server
/// deduces the project); `Repo` with a project goes to the project-scoped
/// endpoint carrying `repo_id`; `ProjectOnly` goes to the project-scoped
/// endpoint with no `repo_id` at all. Having no fallback endpoint, it decides
/// instead between buffering and dropping: a `Scoping` 4xx (the project does
/// not resolve) can never succeed on retry — the offline queue is keyed by
/// that same project id — so it warns and returns `Ok(None)`, while a 403 or
/// any transient error propagates to be buffered. See the arm itself for why
/// the two differ.
///
/// `Ok(None)` therefore means "deliberately dropped, do not queue": callers
/// must treat it like a success for offset/queue purposes, not like an error.
///
/// When the attribution carries BOTH a repo and a project, the project was
/// explicitly declared (by binding or by `project switch`), so the server's
/// refusal of it is an error, never re-derived elsewhere: a deterministic
/// client error (the bound project doesn't apply to this repo: not a member,
/// or the caller lacks `TracePush` on it) prints the refusal line to stderr
/// and returns `Err(e)` with the original error, unchanged, so the caller's
/// existing buffer/retry logic queues it in the REPO's pending file
/// (`pending-<repo_id>.jsonl`, see [`Attribution::pending_file_name`]); the
/// next drain (the hook's or `tracevault flush`'s) re-applies whatever capture
/// project is in force at that time (mirrors the server-side rule from VIS-305: a declared project the server
/// refuses is not silently re-attributed). A transient error (5xx,
/// network/transport, timeout) also propagates as `Err`, and is handled
/// identically by that same buffer/retry path.
///
/// `warned` is shared across every call made for a single hook
/// invocation (both the pending-flush loop and the live send). The
/// pending-flush loop only `break`s on `Err`, so without this flag a stale,
/// refused binding would print the refusal line once per buffered event
/// before the loop breaks. Gating on `*warned` caps stderr at one line per
/// invocation. The repo-less drop warning shares the flag for the same
/// reason: draining a queue of undeliverable events would otherwise print one
/// line per buffered event.
///
/// `mode` is the invocation's attribution mode — [`attribution_mode`] applied
/// to the capture binding — sent verbatim as the
/// `x-tracevault-project-attribution` header on every project-scoped send.
/// Taken as a parameter rather than derived here because the binding is
/// invariant across a whole hook invocation while this function runs once per
/// buffered event, and the mode costs a `user_project.toml` read plus an
/// RFC3339 parse. `commands::flush` resolves it once per queue for the same
/// reason.
async fn send_stream_event(
    client: &crate::api_client::ApiClient,
    attribution: &Attribution,
    mode: &str,
    req: &StreamEventRequest,
    warned: &mut bool,
) -> Result<Option<tracevault_protocol::streaming::StreamEventResponse>, Box<dyn std::error::Error>>
{
    let (repo_id, capture_pid) = match attribution {
        // Repo-less: there is no repo-scoped endpoint to fall back TO, so the
        // only choice is buffer-for-retry vs. drop, and that turns on whether
        // a retry could EVER succeed.
        //
        // `Scoping` (400/404/409) means the project itself does not resolve.
        // The offline queue is keyed by that same project id
        // (`pending-project-<uuid>.jsonl`), so re-binding to a project that
        // does exist writes to a DIFFERENT file and these events are never
        // retried under the corrected binding — they would accumulate forever,
        // one per tool call, each hook fire re-reading and re-appending the
        // whole queue. Warn and drop instead: undeliverable is undeliverable,
        // and a growing queue only hides it.
        //
        // `Forbidden` (403) is deliberately NOT dropped. It can mean the
        // account lacks the realm role entirely, which an administrator can
        // grant — after which the buffered events do deliver. Same for any
        // transient error. Both propagate so the caller queues them.
        Attribution::ProjectOnly { project_id } => {
            let attempt = client
                .stream_event_for_project(*project_id, None, mode, req)
                .await;
            return match attempt {
                Ok(r) => Ok(Some(r)),
                Err(e) => match deterministic_client_error_kind(e.as_ref()) {
                    Some(ClientErrorKind::Scoping) => {
                        if !*warned {
                            eprintln!("{}", undeliverable_warning(*project_id));
                            *warned = true;
                        }
                        Ok(None)
                    }
                    _ => Err(e),
                },
            };
        }
        Attribution::Repo { repo_id, project } => (repo_id.as_str(), *project),
    };
    match capture_pid {
        None => client.stream_event(repo_id, req).await.map(Some),
        Some(pid) => {
            let attempt = client
                .stream_event_for_project(pid, Some(repo_id), mode, req)
                .await;
            let kind = match &attempt {
                Ok(_) => None,
                Err(e) => deterministic_client_error_kind(e.as_ref()),
            };
            match (attempt, kind) {
                (Ok(r), _) => Ok(Some(r)),
                (Err(e), Some(kind)) => {
                    if !*warned {
                        // Since Keycloak, a 403 has a SECOND and now more common
                        // cause: the account has no `tracing` realm role at all,
                        // in which case nothing this hook does will work and
                        // `project switch` is confidently wrong advice — on the
                        // code path users hit most. The server's 403 envelope
                        // isn't distinguishable from here (see
                        // `ClientErrorKind`), so the wording names both.
                        eprintln!("{}", refused_error(pid, &kind, mode));
                        *warned = true;
                    }
                    // The caller declared this project; the server refused it.
                    // Do NOT re-attribute to another project by falling back to
                    // the repo-scoped endpoint — propagate so the caller queues
                    // this event for retry under the same (or a corrected)
                    // binding.
                    Err(e)
                }
                // transient — propagate for buffer/retry
                (Err(e), None) => Err(e),
            }
        }
    }
}

/// Record-count offset arithmetic for the inline (fileless) path, mirroring the
/// byte-offset semantics of `read_new_transcript_lines`: returns
/// `(send_offset, next_offset)` where `send_offset` is stamped on the request as
/// `transcript_offset` (base for the server's per-line `chunk_index`) and
/// `next_offset` is persisted to `.stream_offset`.
pub fn inline_offset_bump(start_offset: i64, records_len: i64) -> (i64, i64) {
    (start_offset, start_offset + records_len)
}

/// Choose the transcript source for a stream event. Inline records (from a
/// plugin-based agent like OpenCode) win over the file-read result whenever the
/// hook event carries them — INCLUDING an empty vec, so an event with no new
/// records (e.g. OpenCode `stop`) preserves the prior record-count offset
/// instead of resetting it to the file-path's 0.
pub fn resolve_transcript_source(
    inline_records: Option<Vec<serde_json::Value>>,
    prior_offset: i64,
    file_result: (Vec<serde_json::Value>, i64, i64),
) -> (Vec<serde_json::Value>, i64, i64) {
    match inline_records {
        Some(records) => {
            let (send, next) = inline_offset_bump(prior_offset, records.len() as i64);
            (records, send, next)
        }
        None => file_result,
    }
}

/// Derive the event-level tool-error flag from inline transcript records.
/// OpenCode tool events carry no `tool_use_id`, so `extract_is_error_from_transcript`
/// can't run; instead the plugin forwards a `toolResult` record whose `isError`
/// signals the outcome. Returns `Some(true|false)` for the first `toolResult`
/// record found, or `None` when there is none (non-tool events).
pub fn inline_tool_is_error(lines: &[serde_json::Value]) -> Option<bool> {
    lines.iter().find_map(|line| {
        let msg = line.get("message")?;
        if msg.get("role").and_then(|v| v.as_str()) == Some("toolResult") {
            Some(
                msg.get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            )
        } else {
            None
        }
    })
}

/// Stamp the agent identity onto a request's `tool` + `protocol_version`.
/// Factored out so the mapping is testable without the network/FS-bound
/// `run_stream`.
pub fn stamp_agent(req: &mut StreamEventRequest, agent: crate::agent::Agent) {
    req.tool = Some(agent.tool_name().to_string());
    req.protocol_version = agent.protocol_version() as u32;
}

pub async fn run_stream(
    event_type: &str,
    agent: crate::agent::Agent,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read HookEvent from stdin
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut hook_event = parse_hook_event(&input)?;

    // Resolve project_root and session_dir via the shared git-aware resolver.
    //
    // `resolve_session_paths` uses `git rev-parse --git-common-dir` to locate
    // the primary worktree root, so it works correctly from a primary checkout,
    // a nested worktree, AND a sibling linked worktree (where the primary
    // `.tracevault/` is NOT an ancestor of hook_cwd — the old ancestor-walk
    // would fall back to hook_cwd itself, fail to load config, and silently
    // drop the event).
    //
    // The hook must never hard-fail (a failing hook blocks the Claude Code tool
    // call).  Genuine resolution failure (no git, no `.tracevault/`) results in
    // a Fallback root (start dir).  Note: `fs::create_dir_all(&session_dir)?`
    // just below runs BEFORE the config/credentials check and can itself
    // `?`-exit early — that remains graceful because `main.rs` catches all
    // `Err` from `run_stream` and exits 0 without blocking the tool.
    let hook_cwd = Path::new(&hook_event.cwd);
    let (resolved, session_dir) = resolve_session_paths(hook_cwd, &hook_event.session_id);
    if resolved.source == crate::paths::ProjectRootSource::Fallback {
        // Neither git nor an ancestor `.tracevault/` resolved a project root, so
        // we are about to create a fresh `.tracevault/` at the hook's working
        // directory — which is how stray per-subdirectory `.tracevault/` dirs
        // appear. Surface it (best-effort; stderr never blocks the hook).
        eprintln!(
            "tracevault: warning: could not resolve a git project root from {}; \
             creating .tracevault/ there. Ensure `git` is on PATH for the hook and \
             that it runs inside the repository to keep sessions under the repo root.",
            hook_cwd.display()
        );
    }
    let project_root = resolved.root;

    // 2. Create session dir
    fs::create_dir_all(&session_dir)?;

    // Ensure runtime artifacts (sessions/, cache/, *.local.toml) are git-ignored
    // inside whatever `.tracevault/` we just created — including a per-subproject
    // one. `tracevault init` writes this for the root dir, but runtime-created
    // dirs would otherwise have no .gitignore and leak sessions into commits.
    // Best-effort: never fail the hook on this.
    let _ = crate::config::TracevaultConfig::ensure_gitignore(&project_root.join(".tracevault"));

    // Write origin marker so verify-start can disambiguate sessions by worktree.
    // Best-effort: never fail the hook on marker errors. Uses the shared
    // canonicalizing helper so the value matches what verify-start compares
    // against on the read side.
    let worktree_top = crate::paths::worktree_toplevel(hook_cwd);
    let _ = fs::write(session_dir.join("origin"), &worktree_top);

    // 3. Mint a time-ordered event id. UUIDv7 is stamped at hook-fire time, so
    //    it both orders events and is a stable idempotency key — no shared
    //    `.event_counter` file (which raced between concurrent parallel-tool
    //    hooks and could collide/drop events).
    let event_uuid = uuid::Uuid::now_v7();

    // 4. Read new transcript lines
    let transcript_path = Path::new(&hook_event.transcript_path);
    let offset_path = session_dir.join(".stream_offset");
    // transcript_offset carries the batch START byte offset so the server
    // derives a stable per-line chunk_index across overlapping/retried reads;
    // .stream_offset persists the END (next read position).
    let (transcript_lines, start_offset, new_offset) =
        read_new_transcript_lines(transcript_path, &offset_path)?;

    // Fileless inline path (OpenCode): a plugin-based agent with no single
    // tailable transcript supplies records directly on the hook event. When
    // present — INCLUDING an empty vec (OpenCode's `stop`/session.idle event
    // sends `transcript_records: []`) — they take priority over the file-read
    // result. `.stream_offset` is reused as a per-session RECORD counter so
    // the server's chunk_index stays stable/monotonic across retries, exactly
    // as with the byte-offset path. Treating an empty vec as "no new
    // records" rather than "fall back to the file" matters: OpenCode's
    // `transcript_path` is always "", so the file-read result is always
    // `(vec![], 0, 0)` — falling through to it would clobber the accumulated
    // offset back to 0 and collide with earlier turns' chunk_index on the
    // server.
    // Only inline (plugin-based) events override the file-read result, and only
    // they need `.stream_offset` re-read as a record counter. File-based agents
    // (claude/codex/gsd) skip this entirely — no extra hook-path I/O — and keep
    // the byte-offset values from `read_new_transcript_lines` above.
    let is_inline = hook_event.transcript_records.is_some();
    let (transcript_lines, start_offset, new_offset) = if is_inline {
        // Fail CLOSED, mirroring `read_new_transcript_lines`: a corrupt/unreadable
        // `.stream_offset` must NOT silently reset to 0 — that would restart the
        // record counter and collide with earlier turns' chunk_index on the
        // server (the offset-clobber failure mode). Propagate the error instead;
        // `main` treats a hook error as non-fatal (exits 0 without blocking the
        // tool call). Negatives are clamped to 0 defensively.
        let prior_offset: i64 = if offset_path.exists() {
            let content = fs::read_to_string(&offset_path)?;
            content
                .trim()
                .parse::<i64>()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                .max(0)
        } else {
            0
        };
        // `take()` moves the records out (nothing below reads
        // `transcript_records` again) — avoids deep-cloning a potentially
        // large payload (full bash stdout / file contents) on the hook path.
        resolve_transcript_source(
            hook_event.transcript_records.take(),
            prior_offset,
            (transcript_lines, start_offset, new_offset),
        )
    } else {
        (transcript_lines, start_offset, new_offset)
    };

    // 5. Build StreamEventRequest
    let stream_event_type = match event_type {
        "notification" => StreamEventType::SessionStart,
        "stop" => StreamEventType::SessionEnd,
        _ => StreamEventType::ToolUse,
    };

    // Extract is_error from transcript for this tool_use_id. Inline (OpenCode)
    // events have no tool_use_id, so fall back to a forwarded `toolResult`
    // record's `isError` — otherwise a failed OpenCode tool would render as
    // successful (the event-level flag file-based agents populate would be None).
    let tool_is_error = hook_event
        .tool_use_id
        .as_deref()
        .and_then(|uid| extract_is_error_from_transcript(uid, &transcript_lines))
        .or_else(|| inline_tool_is_error(&transcript_lines));

    // Load config once, up front, so it can back both the user-level context
    // layer resolution below and the repo_id lookup further down — avoids a
    // redundant second read of the same file. `try_load` keeps the
    // missing/malformed distinction so the required-config error further down
    // can report which one it is.
    // Best-effort here: a missing/unconfigured repo config does NOT mean "no
    // user context" — `resolve_user_context` below falls back to the user-level
    // config in that case. A malformed repo config is surfaced as an error
    // later (the required-config check), not silently dropped here.
    let config = crate::config::TracevaultConfig::try_load(&project_root);
    // Repo config's user_context wins when the repo configured it; otherwise
    // fall back to the user-level ~/.config/tracevault/config.toml. This is
    // what lets a detached session (no checkout) still carry user context.
    let repo_uc = config
        .as_ref()
        .ok()
        .and_then(|opt| opt.as_ref())
        .and_then(|c| c.user_context.clone());
    let user_layer = crate::config::resolve_user_context(repo_uc).resolve();

    // Load the EFFECTIVE merged context (user layer, if enabled, merged with
    // global and per-worktree) and extract fields before building the
    // request.  Using `effective` means parallel sessions in different linked
    // worktrees each stamp their own per-worktree context without
    // interfering with each other.
    let ctx = crate::context::Context::effective(hook_cwd, user_layer.as_deref());
    let (ctx_flow_id, ctx_labels, ctx_params) = apply_context(ctx);

    let mut req = StreamEventRequest {
        protocol_version: 1,
        tool: Some("claude-code".to_string()),
        event_type: stream_event_type,
        session_id: hook_event.session_id.clone(),
        timestamp: chrono::Utc::now(),
        hook_event_name: Some(hook_event.hook_event_name.clone()),
        tool_name: hook_event.tool_name.clone(),
        tool_use_id: hook_event.tool_use_id.clone(),
        tool_input: hook_event.tool_input.clone(),
        tool_response: hook_event.tool_response.clone(),
        tool_is_error,
        event_index: None,
        event_uuid: Some(event_uuid),
        transcript_lines: if transcript_lines.is_empty() {
            None
        } else {
            Some(transcript_lines)
        },
        transcript_offset: Some(start_offset),
        model: None,
        cwd: Some(hook_event.cwd.clone()),
        final_stats: None,
        flow_id: ctx_flow_id,
        labels: ctx_labels,
        params: ctx_params,
    };

    stamp_agent(&mut req, agent);

    req.truncate_large_fields();

    // 6. Resolve credentials
    let (server_url, credential) = crate::credentials::resolve_credentials_quiet(&project_root)?;

    // 7. Config (loaded above, alongside the user-level context layer
    // resolution) no longer directly gates repo_id — a missing config is not
    // fatal on its own, since the repo may instead resolve via the
    // workspace-mode precedence chain (session binding / subagent worktree
    // override) below. A malformed config is still surfaced here — that's
    // easy to miss in hook output otherwise. Only a bound `config.toml`'s
    // repo_id feeds the lowest-precedence tier (via `binding_from_config`
    // below).
    if let Err(e) = &config {
        return Err(format!("malformed .tracevault/config.toml: {e}").into());
    }

    // Resolve the effective repo binding (workspace/detached mode). Bound
    // mode (a pinned config.toml) still works — it's the lowest-precedence
    // tier, wired below via `binding_from_config`, fed from the `config`
    // already loaded above (no second read of config.toml on this hot path).
    let session = crate::session_state::load(&hook_event.session_id);
    let bound = config
        .as_ref()
        .ok()
        .and_then(|opt| opt.as_ref())
        .and_then(crate::resolution::binding_from_config);
    let user_default = crate::user_default::load();
    let binding = resolve_stream_binding(&session, &worktree_top, bound, user_default);
    // Graceful no-op, exactly like the hook's normal success path. Do NOT
    // error: a failing hook would block the tool.
    let no_op_allow = || -> Result<(), Box<dyn std::error::Error>> {
        let response = HookResponse::allow();
        println!("{}", serde_json::to_string(&response)?);
        Ok(())
    };
    // Explicit local project binding (if any) overrides server-side repo
    // deduction. Computed BEFORE the attribution gate below, reusing the
    // already-loaded session + worktree used just above for the repo-binding
    // resolution: a repo-less session is attributable by project alone, so the
    // project must be known before deciding whether this event can be sent.
    // The full binding (not just its id) is resolved: `attribution_mode`
    // reads its `forced_until`, and this is the binding that actually decided
    // the target project, so it's the only one whose force is allowed to
    // apply (see `attribution_mode`'s doc comment).
    let capture_binding = capture_binding(&session, Some(worktree_top.as_str()));
    let capture_pid = capture_binding
        .as_ref()
        .and_then(crate::resolution::capture_project_id);
    // Resolved once per invocation, not per event — the binding is invariant
    // across the pending-flush loop and the live send below, while the mode
    // read costs a `user_project.toml` read and an RFC3339 parse.
    // `commands::flush` resolves it once per queue for the same reason.
    let mode = attribution_mode(capture_binding.as_ref());

    // Ship if EITHER a repo or a project resolved; no-op only when neither
    // did. (`binding_repo_id_is_valid` guards a corrupted/hand-edited
    // session-state file whose repo_id could carry path separators into the
    // pending filename — such a binding degrades to project-only inside
    // `attribution_for` rather than dropping the event.)
    let Some(attribution) = attribution_for(binding.as_ref(), capture_pid) else {
        no_op_allow()?;
        return Ok(());
    };

    // 8. Create ApiClient
    let server_url = server_url.ok_or("server_url not configured")?;
    let client = crate::api_client::ApiClient::with_credential(&server_url, credential);

    // 9. Try drain pending queue and send
    let pending_path = pending_path_for(&session_dir, &attribution);
    let pending_events = drain_pending(&pending_path)?;

    let mut send_failed = false;
    // Spans the whole invocation (pending-flush loop + live send below) so a
    // refused project binding prints its error at most once per invocation,
    // not once per buffered event.
    let mut warned = false;

    // Send pending events first
    for (i, pending_json) in pending_events.iter().enumerate() {
        if let Ok(pending_req) = serde_json::from_str::<StreamEventRequest>(pending_json) {
            if send_stream_event(&client, &attribution, mode, &pending_req, &mut warned)
                .await
                .is_err()
            {
                // Re-queue only the failed event and the ones after it that
                // were never attempted, in order. Events before `i` already
                // got a server response and must NOT be re-queued here — a
                // prior iteration already appended them if IT failed, and if
                // it succeeded, re-adding them would deliver them twice on
                // the next drain.
                for evt in &pending_events[i..] {
                    append_pending(&pending_path, evt)?;
                }
                send_failed = true;
                break;
            }
        }
    }

    // Send current event
    let req_json = serde_json::to_string(&req)?;
    // The inline (OpenCode) path CONSUMES its records from the hook event — they
    // are not re-readable like a transcript file — so its `.stream_offset` record
    // counter must advance whenever the event is accepted for delivery, INCLUDING
    // when it is queued after a failure. Otherwise the next inline event reuses
    // these chunk_indexes and the server drops the overlap via ON CONFLICT. The
    // queued request already carries this event's `transcript_offset`, so the
    // records keep their indices on drain. (File-based agents must NOT advance on
    // failure: their next read re-covers these lines cumulatively/idempotently.)
    if send_failed {
        append_pending(&pending_path, &req_json)?;
        if is_inline {
            fs::write(&offset_path, new_offset.to_string())?;
        }
    } else {
        match send_stream_event(&client, &attribution, mode, &req, &mut warned).await {
            Ok(_) => {
                // 10. On success update .stream_offset
                fs::write(&offset_path, new_offset.to_string())?;
            }
            Err(_) => {
                // 11. On failure append to pending.jsonl (advance the inline
                // record counter regardless — see the note above).
                append_pending(&pending_path, &req_json)?;
                if is_inline {
                    fs::write(&offset_path, new_offset.to_string())?;
                }
            }
        }
    }

    // 12. Always print HookResponse::allow() to stdout
    let response = HookResponse::allow();
    println!("{}", serde_json::to_string(&response)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserContext;
    use crate::context::{Context, EffectiveContext};
    use std::collections::BTreeMap;

    // ── user layer resolution: disabled config → no user layer ────────────────

    #[test]
    fn disabled_user_context_resolves_to_none() {
        // Default/disabled `user_context` (the `Toggle(false)` variant) must
        // resolve to `None`, matching the pre-existing hook behavior of
        // stamping without a user layer when it isn't configured.
        assert!(UserContext::Toggle(false).resolve().is_none());
    }

    #[test]
    fn effective_with_no_user_layer_on_empty_repo_is_empty() {
        // With no user layer and no global/worktree context files present,
        // `Context::effective` must yield a default (empty) `EffectiveContext`
        // — i.e. passing `None` for `user_layer` is behavior-preserving.
        let dir = tempfile::tempdir().unwrap();
        let effective = Context::effective(dir.path(), None);
        assert_eq!(effective, EffectiveContext::default());
    }

    // ── apply_context: all three fields populated ─────────────────────────────

    #[test]
    fn apply_context_stamps_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut params = BTreeMap::new();
        params.insert("env".to_string(), "prod".to_string());
        params.insert("region".to_string(), "eu-west-1".to_string());

        let ctx = EffectiveContext {
            flow_id: Some("flow-xyz".to_string()),
            labels: vec!["backend".to_string(), "urgent".to_string()],
            params,
        };

        let (flow_id, labels, params_out) = apply_context(ctx);

        assert_eq!(flow_id, Some("flow-xyz".to_string()));
        assert_eq!(
            labels,
            Some(vec!["backend".to_string(), "urgent".to_string()])
        );
        let p = params_out.expect("params should be Some");
        assert_eq!(p.get("env").map(String::as_str), Some("prod"));
        assert_eq!(p.get("region").map(String::as_str), Some("eu-west-1"));

        // Verify the stored context round-trips through save_to → load_from →
        // merge_layers → apply_context (stored params are now `Option<String>`).
        let written = Context {
            flow_id: Some("flow-xyz".to_string()),
            labels: vec!["backend".to_string(), "urgent".to_string()],
            params: {
                let mut m = BTreeMap::new();
                m.insert("env".to_string(), Some("prod".to_string()));
                m.insert("region".to_string(), Some("eu-west-1".to_string()));
                m
            },
        };
        let ctx_path = dir.path().join(".tracevault").join("context.json");
        written.save_to(&ctx_path).unwrap();
        let loaded = Context::load_from(&ctx_path);
        let (flow_id2, labels2, params2) = apply_context(Context::merge_layers(&[&loaded]));
        assert_eq!(flow_id2, Some("flow-xyz".to_string()));
        assert!(labels2.is_some());
        assert!(params2.is_some());
    }

    // ── apply_context: missing context file → all None ────────────────────────

    #[test]
    fn apply_context_missing_file_all_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join(".tracevault").join("context.json");
        let ctx = Context::load_from(&missing_path); // no context.json → default
        let (flow_id, labels, params) = apply_context(Context::merge_layers(&[&ctx]));
        assert!(flow_id.is_none(), "flow_id should be None");
        assert!(labels.is_none(), "labels should be None");
        assert!(params.is_none(), "params should be None");
    }

    // ── apply_context: empty labels vec / empty params map → None ─────────────

    #[test]
    fn apply_context_empty_collections_are_none() {
        let ctx = EffectiveContext {
            flow_id: None,
            labels: vec![],
            params: BTreeMap::new(),
        };
        let (flow_id, labels, params) = apply_context(ctx);
        assert!(flow_id.is_none());
        assert!(
            labels.is_none(),
            "empty labels should be None, not Some([])"
        );
        assert!(
            params.is_none(),
            "empty params should be None, not Some({{}})"
        );
    }

    // ── apply_context: flow_id only, collections empty ────────────────────────

    #[test]
    fn apply_context_flow_id_only() {
        let ctx = EffectiveContext {
            flow_id: Some("my-flow".to_string()),
            labels: vec![],
            params: BTreeMap::new(),
        };
        let (flow_id, labels, params) = apply_context(ctx);
        assert_eq!(flow_id, Some("my-flow".to_string()));
        assert!(labels.is_none());
        assert!(params.is_none());
    }

    // ── resolve_session_paths tests ───────────────────────────────────────────
    //
    // These tests verify that `resolve_session_paths` routes through the
    // git-aware resolver so that sibling linked worktrees capture events under
    // the PRIMARY `.tracevault/sessions/`, not the worktree directory.

    use crate::test_helpers::{add_worktree, init_git_repo};

    /// Primary checkout: project_root resolves to the repo root; session_dir is
    /// under `<repo>/.tracevault/sessions/<id>/`.
    #[test]
    fn resolve_session_paths_primary_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        // Create a .tracevault/ in the primary root to match a real init.
        std::fs::create_dir_all(repo.join(".tracevault")).unwrap();

        let (resolved, session_dir) = resolve_session_paths(&repo, "sess-primary-123");
        let project_root = resolved.root;

        assert_eq!(
            project_root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "primary checkout: project_root must be repo root"
        );
        assert_eq!(
            session_dir.canonicalize().unwrap_or(session_dir.clone()),
            repo.join(".tracevault")
                .join("sessions")
                .join("sess-primary-123"),
            "primary checkout: session_dir must be <repo>/.tracevault/sessions/<id>/"
        );
        // session_dir must be INSIDE the primary repo, not elsewhere.
        assert!(
            session_dir.starts_with(&repo),
            "session_dir must be under the primary repo root"
        );
    }

    /// Sibling linked worktree: `hook_cwd` is OUTSIDE the primary repo tree.
    /// The old ancestor-walk would fall back to `hook_cwd` itself; the new
    /// git-aware resolver must return the PRIMARY root.
    #[test]
    fn resolve_session_paths_sibling_worktree_uses_primary_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Sibling worktree lives outside the primary repo directory.
        let wt = tmp.path().join("sibling-wt");

        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        // Place .tracevault/ ONLY in the primary repo — not in the worktree.
        std::fs::create_dir_all(repo.join(".tracevault")).unwrap();
        add_worktree(&repo, &wt);

        let (resolved, session_dir) = resolve_session_paths(&wt, "sess-sibling-456");
        let project_root = resolved.root;

        // Must resolve to the PRIMARY repo root, not the sibling worktree dir.
        assert_eq!(
            project_root.canonicalize().unwrap(),
            repo.canonicalize().unwrap(),
            "sibling worktree: project_root must be primary repo root (not worktree dir)"
        );
        // session_dir must be inside the PRIMARY .tracevault/, not the worktree.
        assert!(
            session_dir.starts_with(&repo),
            "sibling worktree: session_dir must be under the PRIMARY repo root"
        );
        assert!(
            !session_dir.starts_with(&wt),
            "sibling worktree: session_dir must NOT be under the sibling worktree dir"
        );
        assert!(
            session_dir.ends_with(
                std::path::Path::new(".tracevault")
                    .join("sessions")
                    .join("sess-sibling-456")
            ),
            "session_dir must end with .tracevault/sessions/<session_id>"
        );
    }

    /// Non-git directory with no `.tracevault/` ancestor: project_root falls
    /// back to `hook_cwd` itself (Fallback source); the function must not panic.
    #[test]
    fn resolve_session_paths_non_git_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        // No git repo, no .tracevault/ anywhere.
        let (resolved, session_dir) = resolve_session_paths(tmp.path(), "sess-fallback-789");
        let project_root = resolved.root;

        assert_eq!(
            project_root,
            tmp.path(),
            "non-git: project_root must fall back to hook_cwd"
        );
        assert_eq!(
            session_dir,
            tmp.path()
                .join(".tracevault")
                .join("sessions")
                .join("sess-fallback-789"),
            "non-git: session_dir must be relative to the fallback root"
        );
    }

    /// The origin marker written by run_stream contains the worktree toplevel.
    /// This test exercises the same `paths::worktree_toplevel` helper the hook
    /// uses (can't call run_stream end-to-end without a real server) so the
    /// marker content is computed and canonicalized exactly as in production.
    #[test]
    fn origin_marker_written_in_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);

        let session_dir = repo
            .join(".tracevault")
            .join("sessions")
            .join("test-sess-001");
        std::fs::create_dir_all(&session_dir).unwrap();

        let hook_cwd = &repo;
        let worktree_top = crate::paths::worktree_toplevel(hook_cwd);
        let _ = std::fs::write(session_dir.join("origin"), &worktree_top);

        let origin_content = std::fs::read_to_string(session_dir.join("origin")).unwrap();
        let expected = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            origin_content.trim(),
            expected.as_str(),
            "origin marker must contain the canonicalized worktree toplevel"
        );
    }

    // ── resolve_stream_binding: workspace-mode precedence for the stream hook ──

    use crate::session_state::{RepoBinding, SessionState};

    fn binding(id: &str) -> RepoBinding {
        RepoBinding {
            repo_id: id.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: "t".into(),
        }
    }

    /// Workspace mode: no pinned config, but the session is bound (e.g. via
    /// `tracevault repo switch`) — the stream event attributes to that repo.
    #[test]
    fn resolve_stream_binding_uses_session_active_when_unbound() {
        let session = SessionState {
            active: Some(binding("session-repo")),
            subagents: HashMap::new(),
            ..Default::default()
        };
        let got = resolve_stream_binding(&session, "/wt/top", None, None);
        assert_eq!(got.unwrap().repo_id, "session-repo");
    }

    /// Bound-mode regression: an empty session (no `repo switch` ever run)
    /// with a bound config.toml still resolves via the config.
    #[test]
    fn resolve_stream_binding_falls_back_to_bound_config() {
        let session = SessionState::default();
        let got = resolve_stream_binding(&session, "/wt/top", Some(binding("bound-repo")), None);
        assert_eq!(got.unwrap().repo_id, "bound-repo");
    }

    /// No session binding and no bound config: nothing resolves, which is
    /// exactly what should trigger `run_stream`'s graceful no-op.
    #[test]
    fn resolve_stream_binding_none_when_nothing_resolves() {
        let session = SessionState::default();
        let got = resolve_stream_binding(&session, "/wt/top", None, None);
        assert!(got.is_none());
    }

    /// Subagent override precedence: a per-worktree override for the current
    /// worktree wins over the session's `active` binding.
    #[test]
    fn resolve_stream_binding_prefers_subagent_override_for_worktree() {
        let session = SessionState {
            active: Some(binding("session-repo")),
            subagents: HashMap::from([("/wt/x".to_string(), binding("subagent-repo"))]),
            ..Default::default()
        };
        let got = resolve_stream_binding(&session, "/wt/x", Some(binding("bound-repo")), None);
        assert_eq!(got.unwrap().repo_id, "subagent-repo");
    }

    /// Workspace mode: no bound config, but a user-level default is set (e.g.
    /// via `tracevault repo switch --user`) — the stream event attributes to
    /// that repo when nothing more specific resolves.
    #[test]
    fn resolve_stream_binding_falls_back_to_user_default() {
        let session = SessionState::default();
        let got = resolve_stream_binding(&session, "/wt/x", None, Some(binding("userdef")));
        assert_eq!(got.unwrap().repo_id, "userdef");
    }

    /// The user-level default is the LOWEST-precedence tier: a bound config
    /// still wins over it.
    #[test]
    fn resolve_stream_binding_prefers_bound_over_user_default() {
        let session = SessionState::default();
        let got = resolve_stream_binding(
            &session,
            "/wt/x",
            Some(binding("bound")),
            Some(binding("userdef")),
        );
        assert_eq!(got.unwrap().repo_id, "bound");
    }

    // ── attribution_for: repo-or-project gate ────────────────────────────────

    fn binding_with(repo_id: &str) -> crate::session_state::RepoBinding {
        crate::session_state::RepoBinding {
            repo_id: repo_id.into(),
            git_url: None,
            remote_id: None,
            codebase_name: None,
            updated_at: String::new(),
        }
    }

    /// The regression this whole change exists for: a session with a project
    /// binding and NO repo binding must still ship, repo-less, instead of
    /// being dropped by the hook.
    #[test]
    fn attribution_falls_back_to_project_when_no_repo_binds() {
        let pid = uuid::Uuid::new_v4();
        assert_eq!(
            attribution_for(None, Some(pid)),
            Some(Attribution::ProjectOnly { project_id: pid })
        );
    }

    /// Neither binding resolves — the only case that may still no-op.
    #[test]
    fn attribution_is_none_when_neither_repo_nor_project_binds() {
        assert_eq!(attribution_for(None, None), None);
    }

    /// A repo alone keeps the pre-existing repo-scoped behaviour, with the
    /// server left to deduce the project.
    #[test]
    fn attribution_uses_repo_with_no_project_overlay() {
        let repo = uuid::Uuid::new_v4().to_string();
        let b = binding_with(&repo);
        assert_eq!(
            attribution_for(Some(&b), None),
            Some(Attribution::Repo {
                repo_id: repo,
                project: None
            })
        );
    }

    /// Both bound: the project overlays the repo (project-scoped ingest
    /// carrying repo_id), unchanged from before.
    #[test]
    fn attribution_overlays_project_on_repo() {
        let repo = uuid::Uuid::new_v4().to_string();
        let pid = uuid::Uuid::new_v4();
        let b = binding_with(&repo);
        assert_eq!(
            attribution_for(Some(&b), Some(pid)),
            Some(Attribution::Repo {
                repo_id: repo,
                project: Some(pid)
            })
        );
    }

    /// A corrupted repo_id costs the repo attribution, not the trace: it
    /// degrades to project-only rather than dropping the event.
    #[test]
    fn attribution_degrades_a_corrupt_repo_id_to_project_only() {
        let pid = uuid::Uuid::new_v4();
        let b = binding_with("../../escape");
        assert_eq!(
            attribution_for(Some(&b), Some(pid)),
            Some(Attribution::ProjectOnly { project_id: pid })
        );
    }

    /// ...but with no project to fall back to, a corrupt repo_id still drops,
    /// so it can never reach the pending filename.
    #[test]
    fn attribution_drops_a_corrupt_repo_id_with_no_project() {
        let b = binding_with("../../escape");
        assert_eq!(attribution_for(Some(&b), None), None);
    }

    // ── pending queue filenames ──────────────────────────────────────────────

    /// The repo filename is unchanged, so queues buffered by earlier releases
    /// are still found and drained after this upgrade.
    #[test]
    fn pending_file_name_for_a_repo_is_unchanged() {
        let repo = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            Attribution::Repo {
                repo_id: repo.clone(),
                project: Some(uuid::Uuid::new_v4())
            }
            .pending_file_name(),
            format!("pending-{repo}.jsonl")
        );
    }

    /// Repo-less queues carry the `project-` infix, which an older CLI reads
    /// as a non-UUID repo id and skips (see the doc comment).
    #[test]
    fn pending_file_name_for_a_project_is_distinct() {
        let pid = uuid::Uuid::new_v4();
        assert_eq!(
            Attribution::ProjectOnly { project_id: pid }.pending_file_name(),
            format!("pending-project-{pid}.jsonl")
        );
    }

    // ── binding_repo_id_is_valid: hook-attribution UUID guard ────────────────

    #[test]
    fn binding_repo_id_is_valid_accepts_real_uuid() {
        assert!(binding_repo_id_is_valid(
            "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn binding_repo_id_is_valid_rejects_path_traversal() {
        assert!(!binding_repo_id_is_valid("../evil"));
    }

    #[test]
    fn binding_repo_id_is_valid_rejects_empty() {
        assert!(!binding_repo_id_is_valid(""));
    }

    #[test]
    fn binding_repo_id_is_valid_rejects_non_uuid() {
        assert!(!binding_repo_id_is_valid("not-a-uuid"));
    }

    // ── stamp_agent: agent → tool/protocol_version mapping ───────────────────

    #[test]
    fn stamp_agent_sets_tool_and_version() {
        use crate::agent::Agent;
        let mut req = StreamEventRequest {
            protocol_version: 1,
            tool: Some("claude-code".to_string()),
            event_type: StreamEventType::ToolUse,
            session_id: "s".into(),
            timestamp: chrono::Utc::now(),
            hook_event_name: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            tool_response: None,
            tool_is_error: None,
            event_index: None,
            event_uuid: None,
            transcript_lines: None,
            transcript_offset: None,
            model: None,
            cwd: None,
            final_stats: None,
            flow_id: None,
            labels: None,
            params: None,
        };
        stamp_agent(&mut req, Agent::Codex);
        assert_eq!(req.tool.as_deref(), Some("codex"));
        assert_eq!(req.protocol_version, 2);

        stamp_agent(&mut req, Agent::ClaudeCode);
        assert_eq!(req.tool.as_deref(), Some("claude-code"));
        assert_eq!(req.protocol_version, 1);
    }

    // ── pending_path_for: per-target offline queue ───────────────────────────

    fn repo_attr(repo_id: &str) -> Attribution {
        Attribution::Repo {
            repo_id: repo_id.into(),
            project: None,
        }
    }

    #[test]
    fn pending_path_is_repo_scoped() {
        let dir = std::path::Path::new("/tmp/sess");
        assert_eq!(
            pending_path_for(dir, &repo_attr("repo-a")),
            std::path::Path::new("/tmp/sess/pending-repo-a.jsonl")
        );
        assert_ne!(
            pending_path_for(dir, &repo_attr("repo-a")),
            pending_path_for(dir, &repo_attr("repo-b")),
        );
    }

    /// A repo-less session must not share a queue file with any repo, so its
    /// buffered events can never be flushed to a repo endpoint.
    #[test]
    fn pending_path_for_a_project_never_collides_with_a_repo() {
        let dir = std::path::Path::new("/tmp/sess");
        let pid = uuid::Uuid::from_u128(9);
        assert_eq!(
            pending_path_for(dir, &Attribution::ProjectOnly { project_id: pid }),
            std::path::Path::new(&format!("/tmp/sess/pending-project-{pid}.jsonl"))
        );
        assert_ne!(
            pending_path_for(dir, &Attribution::ProjectOnly { project_id: pid }),
            pending_path_for(dir, &repo_attr(&pid.to_string())),
        );
    }

    // ── read_new_transcript_lines: empty transcript_path is a clean no-op ──────

    #[test]
    fn read_new_transcript_lines_empty_path_is_noop() {
        // Codex's nullable transcript_path deserializes to "" (see HookEvent).
        // An empty path must yield no lines and offset 0, never an error from
        // handing `Path::new("")` to exists()/File::open.
        let dir = tempfile::tempdir().unwrap();
        let offset_path = dir.path().join(".stream_offset");
        let (lines, start_offset, end_offset) =
            read_new_transcript_lines(std::path::Path::new(""), &offset_path).unwrap();
        assert!(
            lines.is_empty(),
            "empty path must yield no transcript lines"
        );
        assert_eq!(start_offset, 0);
        assert_eq!(end_offset, 0);
    }

    // ── read_new_transcript_lines: start_offset is stable across a persisted
    //    end_offset, and equals the previous read's end_offset ─────────────────

    #[test]
    fn read_new_transcript_lines_second_read_starts_at_previous_end() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("transcript.jsonl");
        let offset_path = dir.path().join(".stream_offset");

        fs::write(
            &transcript_path,
            "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
        )
        .unwrap();

        let (lines, start_offset, end_offset) =
            read_new_transcript_lines(&transcript_path, &offset_path).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(start_offset, 0, "first read starts at byte 0");
        assert!(end_offset > 0);

        fs::write(&offset_path, end_offset.to_string()).unwrap();

        let mut f = OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        writeln!(f, "{{\"type\":\"user\",\"message\":\"more\"}}").unwrap();
        drop(f);

        let (lines2, start_offset2, end_offset2) =
            read_new_transcript_lines(&transcript_path, &offset_path).unwrap();
        assert_eq!(lines2.len(), 1);
        assert_eq!(
            start_offset2, end_offset,
            "second read's start_offset must equal the first read's persisted end_offset"
        );
        assert!(end_offset2 > start_offset2);
    }

    // ── capture_project: local-only project resolver ──────────────────────────

    /// Holds the env lock and points the config dir (`XDG_CONFIG_HOME` on
    /// Linux, `HOME` for macOS's `dirs::config_dir()`) at a tempdir: the last
    /// tier reads `user_project.toml` from there, so without isolation this
    /// test fails on any machine with a real user default and races tests
    /// that write one.
    ///
    /// `TRACEVAULT_PROJECT` is pinned UNSET for the same reason:
    /// `capture_project` reads it (rung 3, above `session.active_project`),
    /// and other tests in this same binary SET it. The crate lock only
    /// serializes mutators against each other, so a non-locking reader still
    /// races them — and `std::env::set_var` concurrent with `std::env::var`
    /// is exactly the UB the `unsafe` blocks in `EnvVarGuard` are annotated
    /// against. Holding the lock AND declaring the value is what makes the
    /// precedence assertions below describe the tiers they name rather than
    /// whatever the ambient shell happens to export.
    #[tokio::test]
    async fn capture_project_precedence_local_only() {
        use crate::session_state::{ProjectBinding, SessionState};
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.set("HOME", tmp.path());
        _guard.remove("TRACEVAULT_PROJECT");

        let pb = |id: &str| ProjectBinding {
            project_id: id.into(),
            project_name: "n".into(),
            updated_at: "".into(),
            forced_until: None,
        };
        let u = uuid::Uuid::from_u128;
        // session active only
        let s = SessionState {
            active_project: Some(pb(&u(2).to_string())),
            ..Default::default()
        };
        assert_eq!(capture_project(&s, Some("/wt")), Some(u(2)));
        // subagent worktree beats session
        let mut s = s;
        s.subagent_projects
            .insert("/wt".into(), pb(&u(1).to_string()));
        assert_eq!(capture_project(&s, Some("/wt")), Some(u(1)));
        // no worktree match -> session active
        assert_eq!(capture_project(&s, Some("/other")), Some(u(2)));
        // empty session, no user_project.toml in the isolated config dir -> None
        assert_eq!(capture_project(&SessionState::default(), None), None);
        // malformed stored id -> None (defensive)
        let bad = SessionState {
            active_project: Some(pb("not-a-uuid")),
            ..Default::default()
        };
        assert_eq!(capture_project(&bad, None), None);
        // a valid user_project.toml in the isolated config dir is the last tier
        let path = crate::user_project_default::default_project_path().unwrap();
        assert!(path.starts_with(tmp.path()), "not isolated: {path:?}");
        crate::user_project_default::save(&pb(&u(3).to_string())).unwrap();
        assert_eq!(capture_project(&SessionState::default(), None), Some(u(3)));
    }

    /// The full binding — not just its id — survives `capture_binding`: this
    /// is what lets `attribution_mode` see a session-scoped `forced_until`
    /// (Important finding 1 in the VIS-305 Part C review: the old
    /// process-global disk read never saw a force written into SESSION
    /// state, which is where a plain `project switch --project-attribution
    /// explicit` — no `--user` — writes it).
    /// ENV ISOLATION: see `capture_project_precedence_local_only` — this
    /// test also goes through the capture chain, which reads
    /// `TRACEVAULT_PROJECT` at a rung ABOVE the session-active binding it is
    /// asserting on.
    #[test]
    fn capture_binding_preserves_forced_until_from_the_winning_tier() {
        use crate::session_state::{ProjectBinding, SessionState};
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT");
        let pid = uuid::Uuid::from_u128(2);
        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let s = SessionState {
            active_project: Some(ProjectBinding {
                project_id: pid.to_string(),
                project_name: "n".into(),
                updated_at: "".into(),
                forced_until: Some(future.clone()),
            }),
            ..Default::default()
        };
        let got = capture_binding(&s, Some("/wt")).expect("session-active binding resolves");
        assert_eq!(got.forced_until, Some(future));
    }

    /// `_env_lock` serializes this against other tests in the crate that
    /// mutate `TRACEVAULT_PROJECT` (see `test_helpers::lock_env_mutation_sync`),
    /// including any that call `commands::project::status`, which reads the
    /// same var.
    #[test]
    fn env_project_binding_takes_a_uuid_and_ignores_a_name() {
        // A name would need a `list_projects` round trip, and this path runs
        // per-event in a short-lived hook process. Same rule already excludes
        // `config_default` here.
        let uuid = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();

        _guard.set("TRACEVAULT_PROJECT", uuid);
        let got = env_project_binding().expect("uuid form is honoured");
        assert_eq!(got.project_id, uuid);

        _guard.set("TRACEVAULT_PROJECT", "my-project");
        assert!(
            env_project_binding().is_none(),
            "a name is ignored on the network-free capture path"
        );

        _guard.remove("TRACEVAULT_PROJECT");
        assert!(env_project_binding().is_none());
    }

    // ── attribution_mode ───────────────────────────────────────────────────
    //
    // `attribution_mode` no longer touches disk at all: it takes the WINNING
    // binding (as resolved by `capture_project`) as a parameter, so these
    // tests only need to isolate `TRACEVAULT_PROJECT_ATTRIBUTION` via the
    // crate's env-mutation lock/guard, not `XDG_CONFIG_HOME`.

    fn forced_binding(forced_until: Option<&str>) -> crate::session_state::ProjectBinding {
        crate::session_state::ProjectBinding {
            project_id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
            project_name: "p".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            forced_until: forced_until.map(str::to_string),
        }
    }

    #[test]
    fn env_force_is_explicit_and_needs_no_expiry() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        assert_eq!(attribution_mode(None), "explicit");
    }

    #[test]
    fn absent_or_unrecognised_env_is_derived() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();

        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        assert_eq!(attribution_mode(None), "derived");

        // The CLI does not forward a value it does not recognise: the server
        // would 400 it, and failing a hook over a typo'd env var helps
        // nobody. The server's strictness is for callers that bypass the
        // CLI.
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "yes-please");
        assert_eq!(attribution_mode(None), "derived");
    }

    /// The binding-persisted half of `attribution_mode`: a LIVE `forced_until`
    /// on the WINNING binding (no env var involved) must also read as
    /// `explicit` — this is what `project switch --project-attribution
    /// explicit` ultimately drives, whether that binding lives in session
    /// state or the user-level default.
    #[test]
    fn live_binding_force_is_explicit_without_env() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let binding = forced_binding(Some(&future));

        assert_eq!(attribution_mode(Some(&binding)), "explicit");
    }

    /// This is the test that would fail if a lapsed persisted force still
    /// made it onto the wire: with no env override, a `forced_until` in the
    /// past on the winning binding must make `attribution_mode` — the exact
    /// function that supplies the header — report `derived`, not `explicit`.
    /// Not sending an `explicit` header IS the fallback to derived
    /// attribution.
    #[test]
    fn lapsed_binding_force_is_derived_without_env() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let binding = forced_binding(Some(&past));

        assert_eq!(attribution_mode(Some(&binding)), "derived");
    }

    /// A `forced_until` that doesn't even parse as RFC3339 (a hand-edited or
    /// corrupted `user_project.toml`/session-state file) must fail SAFE —
    /// read as `derived`, never panic or, worse, read as `explicit`. Pins the
    /// fail-safe direction against a future refactor to `.expect(...)`.
    #[test]
    fn unparseable_forced_until_is_derived() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let binding = forced_binding(Some("not-a-timestamp"));

        assert_eq!(attribution_mode(Some(&binding)), "derived");
    }

    /// Env-provided force must be exempt from expiry in fact, not just by
    /// coincidence of the OR: pairing it with a binding force that has
    /// ALREADY LAPSED still yields `explicit`. If `attribution_mode` were
    /// refactored to gate the env branch on any timestamp, this is the test
    /// that would catch it — `lapsed_binding_force_is_derived_without_env`
    /// alone couldn't, since it never sets the env var.
    #[test]
    fn env_force_is_explicit_even_with_a_lapsed_binding_force() {
        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("TRACEVAULT_PROJECT_ATTRIBUTION", "explicit");

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let binding = forced_binding(Some(&past));

        assert_eq!(attribution_mode(Some(&binding)), "explicit");
    }

    /// VIS-305 Part C review, Important finding 1: a SESSION-scoped force
    /// (`project switch --project-attribution explicit` WITHOUT `--user`,
    /// which is the default whenever a session id is set — i.e. the ordinary
    /// case inside every instrumented agent session) must actually reach the
    /// wire as `explicit`. The old process-global `attribution_mode` only
    /// ever consulted the user-level default store and never saw a force
    /// written into session state at all, so this would have silently sent
    /// `derived` while `project switch`'s own success message claimed
    /// membership was not being checked. Exercised end-to-end through
    /// `send_stream_event` and a real captured request header, not just the
    /// pure `attribution_mode` function, so a regression anywhere in the
    /// plumbing between `capture_project` and the wire is caught.
    #[tokio::test]
    async fn session_scoped_force_is_honoured_on_the_wire() {
        use crate::session_state::{ProjectBinding, SessionState};

        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        // No user-level default at all: the force under test lives ONLY in
        // session state, so a pass here can't be explained by an ambient or
        // accidentally-written user_project.toml.
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");
        // `capture_project` reads `TRACEVAULT_PROJECT` ABOVE the
        // session-active tier this test pins the force on; unset it so an
        // ambient export can't redirect the capture at another project.
        _guard.remove("TRACEVAULT_PROJECT");

        let pid = uuid::Uuid::from_u128(321);
        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        let session = SessionState {
            active_project: Some(ProjectBinding {
                project_id: pid.to_string(),
                project_name: "proj".into(),
                updated_at: "".into(),
                forced_until: Some(future),
            }),
            ..Default::default()
        };
        let capture_binding = capture_binding(&session, None);

        let resp = ok_stream_response();
        let (base, rx) = spawn_once_capturing_request(Box::leak(resp.into_boxed_str()));
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let mut warned = false;

        let got = send_stream_event(
            &client,
            &Attribution::ProjectOnly { project_id: pid },
            attribution_mode(capture_binding.as_ref()),
            &req,
            &mut warned,
        )
        .await
        .expect("send_stream_event must succeed");
        assert_eq!(
            got.expect("a successful send returns a response").status,
            "accepted"
        );

        let captured = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            captured
                .to_lowercase()
                .contains("x-tracevault-project-attribution: explicit"),
            "a session-scoped (no --user) force must reach the wire as `explicit`, got: {captured}"
        );
    }

    /// VIS-305 Part C review, Important finding 2: a LIVE force sitting on
    /// the machine-global user-level default must NOT leak onto a different,
    /// higher-precedence project that resolves instead. The old
    /// process-global `attribution_mode` couldn't tell the two apart — it
    /// declared `explicit` for ANY project as long as SOME force existed
    /// anywhere on disk. Here the user-default's force belongs to project A,
    /// but `TRACEVAULT_PROJECT` (a higher-precedence local tier) points this
    /// capture at project B, which was never forced.
    #[test]
    fn user_default_force_does_not_leak_onto_a_higher_precedence_project() {
        use crate::session_state::{ProjectBinding, SessionState};

        let _env_lock = crate::test_helpers::lock_env_mutation_sync();
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let project_a = uuid::Uuid::from_u128(1);
        let future = (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339();
        crate::user_project_default::save(&ProjectBinding {
            project_id: project_a.to_string(),
            project_name: "a".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            forced_until: Some(future),
        })
        .unwrap();

        let project_b = uuid::Uuid::from_u128(2);
        _guard.set("TRACEVAULT_PROJECT", project_b.to_string());

        let capture_binding = capture_binding(&SessionState::default(), None);
        assert_eq!(
            capture_binding.as_ref().map(|b| b.project_id.as_str()),
            Some(project_b.to_string()).as_deref(),
            "TRACEVAULT_PROJECT must outrank the user-level default"
        );

        assert_eq!(
            attribution_mode(capture_binding.as_ref()),
            "derived",
            "a force on the user-default project must not leak onto a DIFFERENT, \
             higher-precedence project that never asked to be forced"
        );
    }

    // ── send_stream_event: endpoint routing based on capture_pid ──────────────

    use std::io::BufReader;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// How long an accept loop waits for a connection that never arrives
    /// (e.g. a regression that stops the client from making an expected
    /// request) before giving up, so the server thread exits cleanly
    /// instead of blocking in `accept()` forever.
    const ACCEPT_DEADLINE: Duration = Duration::from_secs(5);

    /// Poll a non-blocking `listener` for a connection until one arrives or
    /// `ACCEPT_DEADLINE` elapses. Returns `None` on timeout (or any non-
    /// `WouldBlock` accept error) so callers can stop cleanly rather than
    /// block forever.
    fn accept_with_deadline(listener: &TcpListener) -> Option<std::net::TcpStream> {
        let deadline = Instant::now() + ACCEPT_DEADLINE;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // The accepted stream's blocking mode isn't guaranteed to
                    // be inherited from the (non-blocking) listener across
                    // platforms — make it explicitly blocking so the
                    // subsequent read/write calls behave as before.
                    let _ = stream.set_nonblocking(false);
                    return Some(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return None,
            }
        }
    }

    /// How long a test waits for the captured request before failing (rather
    /// than hanging forever if the client never connects). Mirrors the
    /// harness in `tests/stream_event_project_test.rs`.
    const SEND_STREAM_EVENT_RECV_TIMEOUT: Duration = Duration::from_secs(5);

    /// Read one request's line and headers off `stream`, hand the pair to
    /// `tx` as a single string, then write `response` back.
    ///
    /// Shared by [`spawn_once_capturing_request`] and
    /// [`spawn_n_capturing_requests`], which grew this same capture
    /// independently and disagreed only on `trim()` vs `trim_end()` for the
    /// blank-line terminator (equivalent, since a bare CRLF trims empty
    /// either way).
    ///
    /// Deliberately NOT `test_helpers::read_request`: that one is private to
    /// its module and blocks in `accept()`, where these two use
    /// `set_nonblocking` plus a deadline.
    ///
    /// Headers are captured, not just the request line, so a test can assert
    /// on `x-tracevault-project-attribution` — without them a regression that
    /// hardcoded `"derived"` at a `stream_event_for_project` call site would
    /// pass the whole suite, since nothing in-crate ever looked at the header
    /// actually sent. They are appended AFTER the request line, leaving
    /// `starts_with`/`contains` assertions on the line itself unaffected.
    fn capture_request_and_respond(
        stream: std::net::TcpStream,
        tx: &mpsc::Sender<String>,
        response: &str,
    ) {
        let mut reader = BufReader::new(stream);
        let mut captured = String::new();
        let _ = reader.read_line(&mut captured);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                break;
            }
            if header.trim().is_empty() {
                break;
            }
            captured.push_str(&header);
        }
        let _ = tx.send(captured);
        let mut stream = reader.into_inner();
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// Spawn a one-shot server that returns `response` (a full HTTP response)
    /// to the first request, capturing it via
    /// [`capture_request_and_respond`]. Mirrors the harness in
    /// `tests/stream_event_project_test.rs` / `tests/resolve_remote_test.rs`.
    fn spawn_once_capturing_request(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Some(stream) = accept_with_deadline(&listener) {
                capture_request_and_respond(stream, &tx, response);
            }
        });
        (format!("http://{addr}"), rx)
    }

    fn ok_stream_response() -> String {
        let body = serde_json::to_string(&tracevault_protocol::streaming::StreamEventResponse {
            session_db_id: uuid::Uuid::nil(),
            event_db_id: Some(uuid::Uuid::nil()),
            status: "accepted".to_string(),
        })
        .unwrap();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn sample_stream_event_request() -> StreamEventRequest {
        StreamEventRequest {
            protocol_version: 2,
            tool: Some("claude-code".to_string()),
            event_type: StreamEventType::ToolUse,
            session_id: "sess-1".into(),
            timestamp: chrono::Utc::now(),
            hook_event_name: Some("PostToolUse".into()),
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            tool_response: None,
            tool_is_error: None,
            event_index: None,
            event_uuid: None,
            transcript_lines: None,
            transcript_offset: None,
            model: None,
            cwd: None,
            final_stats: None,
            flow_id: None,
            labels: None,
            params: None,
        }
    }

    /// With a `SessionState` whose `active_project` is set, `capture_project`
    /// resolves a `capture_pid`, and `send_stream_event` must route to the
    /// project-scoped endpoint (`/projects/{pid}/stream?repo_id=...`) rather
    /// than the repo-scoped one.
    #[tokio::test]
    async fn send_stream_event_routes_to_project_endpoint_when_binding_active() {
        use crate::session_state::{ProjectBinding, SessionState};

        // Isolate from any ambient user-level project default so this test's
        // `capture_project` call depends only on the SessionState below —
        // mirrors the isolation pattern in `commands::project`'s tests.
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        // `capture_project` reads `TRACEVAULT_PROJECT` above the
        // session-active tier asserted below — pin it unset. Ditto
        // `TRACEVAULT_PROJECT_ATTRIBUTION`, which the `derived` header
        // assertion at the end of this test depends on.
        _guard.remove("TRACEVAULT_PROJECT");
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let pid = uuid::Uuid::from_u128(99);
        let session = SessionState {
            active_project: Some(ProjectBinding {
                project_id: pid.to_string(),
                project_name: "proj".into(),
                updated_at: "".into(),
                forced_until: None,
            }),
            ..Default::default()
        };
        let capture_binding = capture_binding(&session, None);
        let capture_pid = capture_binding
            .as_ref()
            .and_then(crate::resolution::capture_project_id);
        assert_eq!(capture_pid, Some(pid));

        let resp = ok_stream_response();
        let (base, rx) = spawn_once_capturing_request(Box::leak(resp.into_boxed_str()));
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();

        let mut warned = false;
        let got = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: capture_pid,
            },
            attribution_mode(capture_binding.as_ref()),
            &req,
            &mut warned,
        )
        .await
        .expect("send_stream_event must succeed");
        assert_eq!(
            got.expect("a successful send returns a response").status,
            "accepted"
        );

        let line = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            line.starts_with(&format!(
                "POST /api/v1/projects/{pid}/stream?repo_id=11111111-1111-1111-1111-111111111111 "
            )),
            "an active project binding must route to the project-scoped endpoint, got: {line}"
        );
        // Covers the `Attribution::Repo { project: Some(_) }` call site of
        // `stream_event_for_project` (the `ProjectOnly` call site is covered
        // by `session_scoped_force_is_honoured_on_the_wire` and
        // `send_stream_event_project_only_omits_repo_id`) — an unforced
        // binding must still send the header, declaring `derived`.
        assert!(
            line.to_lowercase()
                .contains("x-tracevault-project-attribution: derived"),
            "got: {line}"
        );
    }

    /// With an empty binding (default `SessionState`, no worktree override)
    /// and no user-level default present in the test env, `capture_project`
    /// resolves `None` and `send_stream_event` must fall back to the
    /// repo-scoped endpoint.
    #[tokio::test]
    async fn send_stream_event_routes_to_repo_endpoint_when_no_binding() {
        use crate::session_state::SessionState;

        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        // No user_project.toml under this isolated config dir, so there is no
        // ambient user-level default to leak in — and no `TRACEVAULT_PROJECT`
        // either, which `capture_project` would otherwise honour and turn
        // this "nothing resolves" case into a project-scoped send.
        _guard.set("XDG_CONFIG_HOME", tmp.path());
        _guard.remove("TRACEVAULT_PROJECT");

        let capture_binding = capture_binding(&SessionState::default(), None);
        assert_eq!(capture_binding, None);

        let resp = ok_stream_response();
        let (base, rx) = spawn_once_capturing_request(Box::leak(resp.into_boxed_str()));
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();

        let mut warned = false;
        let got = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: None,
            },
            attribution_mode(capture_binding.as_ref()),
            &req,
            &mut warned,
        )
        .await
        .expect("send_stream_event must succeed");
        assert_eq!(
            got.expect("a successful send returns a response").status,
            "accepted"
        );

        let line = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            line.starts_with("POST /api/v1/repos/11111111-1111-1111-1111-111111111111/stream "),
            "no binding must fall back to the repo-scoped endpoint, got: {line}"
        );
    }

    // ── I1/I2: refused project-scoped send is an error + transient no-fallback ───

    /// Spawn a server that replies to up to `responses.len()` sequential
    /// connections with the given full HTTP responses, in order, capturing
    /// each request (line + headers, via [`capture_request_and_respond`])
    /// over the channel. The listening socket itself is dropped (closing it)
    /// once all responses have been served, so any further connection attempt
    /// fails fast (connection refused) rather than hanging for the client's
    /// request timeout — this lets a test assert "no further request was
    /// sent" cheaply.
    fn spawn_n_capturing_requests(
        responses: Vec<&'static str>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                // If a regression stops the client from making this request
                // (e.g. the expected Nth call never happens), don't block
                // forever in `accept()` — give up after `ACCEPT_DEADLINE`
                // and let the thread end so the test process can't hang.
                let Some(stream) = accept_with_deadline(&listener) else {
                    break;
                };
                capture_request_and_respond(stream, &tx, response);
            }
            // `listener` (and `tx`) drop here, closing the socket and the
            // channel — a stray extra request gets ECONNREFUSED immediately
            // instead of hanging, and a stray extra `rx.recv` returns
            // Disconnected immediately instead of blocking.
        });
        (format!("http://{addr}"), rx)
    }

    /// I1: a deterministic 400 ("is not a member of project ...") from the
    /// project-scoped endpoint must be an error, not a silent second send to
    /// the repo-scoped endpoint. TWO responses (400 then 200) are staged so
    /// the "no second request" check is non-vacuous — see the comment in
    /// `send_stream_event_project_only_drops_on_scoping_4xx` for why a single
    /// staged response would make that assertion pass vacuously.
    #[tokio::test]
    async fn send_stream_event_errors_on_deterministic_400_without_fallback() {
        let body_400 = "repo 11111111-1111-1111-1111-111111111111 is not a member of project 22222222-2222-2222-2222-222222222222";
        let resp_400 = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_400.len(),
            body_400
        );
        let resp_200 = ok_stream_response();

        let (base, rx) = spawn_n_capturing_requests(vec![
            Box::leak(resp_400.into_boxed_str()),
            Box::leak(resp_200.into_boxed_str()),
        ]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(42);

        let mut warned = false;
        let err = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: Some(pid),
            },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect_err("a refused declared project must be an error, not a silent fallback");
        assert!(
            err.to_string().contains("400"),
            "propagated error must reflect the status, got: {err}"
        );
        assert!(
            warned,
            "the refusal must be printed (gated on `warned`) so it is visible"
        );

        let first = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no first request captured");
        assert!(
            first.contains(&format!("/projects/{pid}/stream")),
            "first request must hit the project-scoped endpoint, got: {first}"
        );
        assert!(
            first.contains("repo_id="),
            "first request must carry repo_id, got: {first}"
        );

        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "no second (repo-scoped fallback) request must be sent"
        );
    }

    /// A 403 from the project-scoped endpoint must likewise be an error, not a
    /// silent second send to the repo-scoped endpoint.
    #[tokio::test]
    async fn send_stream_event_errors_on_403_without_fallback() {
        let body_403 = "forbidden";
        let resp_403 = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_403.len(),
            body_403
        );
        let resp_200 = ok_stream_response();

        let (base, rx) = spawn_n_capturing_requests(vec![
            Box::leak(resp_403.into_boxed_str()),
            Box::leak(resp_200.into_boxed_str()),
        ]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(43);

        let mut warned = false;
        let err = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: Some(pid),
            },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect_err("a refused declared project must be an error, not a silent fallback");
        assert!(
            err.to_string().contains("403"),
            "propagated error must reflect the status, got: {err}"
        );
        assert!(
            warned,
            "the refusal must be printed (gated on `warned`) so it is visible"
        );

        let first = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no first request captured");
        assert!(
            first.contains(&format!("/projects/{pid}/stream")),
            "first request must hit the project-scoped endpoint, got: {first}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "no second (repo-scoped fallback) request must be sent"
        );
    }

    /// The refusal error must be printed at most once per hook invocation
    /// (i.e. per shared `warned` flag), even across multiple calls sharing
    /// it — mirroring how the pending-flush loop and the live send share one
    /// `warned` for a single invocation.
    #[tokio::test]
    async fn refused_error_is_printed_once_per_invocation() {
        let body_400 = "not a member";
        let resp_400 = || {
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_400.len(),
                body_400
            )
        };
        let (base, _rx) = spawn_n_capturing_requests(vec![
            Box::leak(resp_400().into_boxed_str()),
            Box::leak(resp_400().into_boxed_str()),
        ]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(44);
        let attribution = Attribution::Repo {
            repo_id: "11111111-1111-1111-1111-111111111111".into(),
            project: Some(pid),
        };

        let mut warned = false;
        assert!(
            send_stream_event(
                &client,
                &attribution,
                attribution_mode(None),
                &req,
                &mut warned
            )
            .await
            .is_err(),
            "a refused declared project must be an error"
        );
        assert!(warned, "the flag must be set after the first refusal");

        assert!(
            send_stream_event(
                &client,
                &attribution,
                attribution_mode(None),
                &req,
                &mut warned
            )
            .await
            .is_err(),
            "a second refusal must also be an error"
        );
        assert!(warned, "the flag stays set across the shared invocation");
    }

    /// Finding 5's scenario end to end, on the repo-bound path, as VIS-316
    /// leaves it: a live persisted force makes this send declare `explicit`,
    /// the force gate refuses it with a 403 — and the event is now an ERROR
    /// queued for retry, never re-sent to the repo-scoped endpoint where the
    /// server would DEDUCE a project that is precisely not the one the
    /// operator forced.
    ///
    /// Pins the fact the refusal clause depends on: the mode that went on
    /// the wire really is `explicit`, and it is the same value
    /// `refused_error` is handed — the message and the wire cannot disagree.
    /// A spare 200 is staged so "no second request" is non-vacuous.
    #[tokio::test]
    async fn a_refused_force_declares_explicit_and_is_an_error_not_a_fallback() {
        let body_403 = "forbidden";
        let resp_403 = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_403.len(),
            body_403
        );
        let resp_200 = ok_stream_response();

        let (base, rx) = spawn_n_capturing_requests(vec![
            Box::leak(resp_403.into_boxed_str()),
            Box::leak(resp_200.into_boxed_str()),
        ]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(42);

        // A live persisted force on the winning binding — no env var needed,
        // so this test takes no env lock.
        let forced = crate::session_state::ProjectBinding {
            project_id: pid.to_string(),
            project_name: "forced".into(),
            updated_at: "".into(),
            forced_until: Some((chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339()),
        };

        let mut warned = false;
        let err = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: Some(pid),
            },
            attribution_mode(Some(&forced)),
            &req,
            &mut warned,
        )
        .await
        .expect_err("a refused force must be an error, not a silent fallback");
        assert!(
            err.to_string().contains("403"),
            "propagated error must reflect the status, got: {err}"
        );
        assert!(warned, "the refusal must be printed exactly once");

        let first = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no first request captured");
        assert!(
            first.contains(&format!("/projects/{pid}/stream")),
            "first request must hit the project-scoped endpoint, got: {first}"
        );
        assert!(
            first
                .to_lowercase()
                .contains("x-tracevault-project-attribution: explicit"),
            "the refused send must have DECLARED explicit — this is the mode \
             `refused_error` is handed: {first}"
        );

        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a refused force must NOT be re-attributed via the repo-scoped endpoint"
        );
    }

    /// The repo-less send: a `ProjectOnly` attribution must hit the bare
    /// project endpoint with NO `repo_id` query pair. This is the wire shape
    /// the server documents as "repo-less (0-repo) projects are supported".
    ///
    /// Takes the env lock and pins `TRACEVAULT_PROJECT_ATTRIBUTION` unset:
    /// this test also asserts the `derived` attribution header, and
    /// `attribution_mode` reads that variable even when the binding passed
    /// in is `None`.
    #[tokio::test]
    async fn send_stream_event_project_only_omits_repo_id() {
        let _env_lock = crate::test_helpers::lock_env_mutation().await;
        let mut _guard = crate::test_helpers::EnvVarGuard::new();
        _guard.remove("TRACEVAULT_PROJECT_ATTRIBUTION");

        let resp = ok_stream_response();
        let (base, rx) = spawn_once_capturing_request(Box::leak(resp.into_boxed_str()));
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(11);

        let mut warned = false;
        let got = send_stream_event(
            &client,
            &Attribution::ProjectOnly { project_id: pid },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect("a repo-less project send must succeed");
        assert_eq!(
            got.expect("a successful send returns a response").status,
            "accepted"
        );

        let line = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            line.starts_with(&format!("POST /api/v1/projects/{pid}/stream ")),
            "expected a bare project path with no query string, got: {line}"
        );
        assert!(
            line.to_lowercase()
                .contains("x-tracevault-project-attribution: derived"),
            "with no forced binding, the header must declare `derived`, got: {line}"
        );
        assert!(
            !warned,
            "a repo-less send has no fallback and must not warn about one"
        );
    }

    /// A `Scoping` 4xx is permanently undeliverable for a repo-less session:
    /// the queue is keyed by the very project id the server says does not
    /// resolve, so a corrected binding writes to a different file and these
    /// events would never be retried — they would just accumulate, one per
    /// tool call. They must be DROPPED (`Ok(None)`), loudly, and must not
    /// trigger a second, impossible request.
    #[tokio::test]
    async fn send_stream_event_project_only_drops_on_scoping_4xx() {
        let body = "no such project";
        let resp_404 = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        // TWO responses are staged although only one request is expected. With
        // a single response the helper drops its sender the moment the list is
        // exhausted, so the "no second request" assertion below would pass on
        // `Disconnected` whether or not a second request happened — a vacuous
        // check. Staging a spare keeps the server accepting, so a stray
        // request really is captured and really fails the test.
        let (base, rx) = spawn_n_capturing_requests(vec![
            Box::leak(resp_404.clone().into_boxed_str()),
            Box::leak(resp_404.into_boxed_str()),
        ]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(12);

        let mut warned = false;
        let got = send_stream_event(
            &client,
            &Attribution::ProjectOnly { project_id: pid },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect("an undeliverable repo-less event is dropped, not surfaced as Err");
        assert!(
            got.is_none(),
            "Ok(None) signals the deliberate drop; Some(_) would read as a real send"
        );
        assert!(
            warned,
            "dropping data silently is the failure mode to avoid"
        );

        let first = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            first.contains(&format!("/projects/{pid}/stream")),
            "must hit the project-scoped endpoint, got: {first}"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a repo-less send must make exactly one request — no fallback exists"
        );
    }

    /// A 403 is NOT dropped. It can mean the account lacks the realm role,
    /// which an administrator can grant — after which the buffered events do
    /// deliver. It must propagate so the caller queues them.
    #[tokio::test]
    async fn send_stream_event_project_only_buffers_on_403() {
        let body = "forbidden";
        let resp_403 = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (base, _rx) = spawn_n_capturing_requests(vec![Box::leak(resp_403.into_boxed_str())]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();

        let mut warned = false;
        let err = send_stream_event(
            &client,
            &Attribution::ProjectOnly {
                project_id: uuid::Uuid::from_u128(13),
            },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect_err("a 403 is recoverable by a role grant, so it must buffer, not drop");
        assert!(
            err.to_string().contains("403"),
            "propagated error must reflect the status, got: {err}"
        );
    }

    /// I2: a transient 503 from the project-scoped endpoint must propagate as
    /// an `Err` WITHOUT falling back to the repo-scoped endpoint, so the
    /// existing buffer/retry logic in `run_stream` still handles it. Only one
    /// request (to the project endpoint) may be observed.
    #[tokio::test]
    async fn send_stream_event_does_not_fall_back_on_transient_503() {
        let body_503 = "internal error";
        let resp_503 = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_503.len(),
            body_503
        );
        let (base, rx) = spawn_n_capturing_requests(vec![Box::leak(resp_503.into_boxed_str())]);
        let client = crate::api_client::ApiClient::new(&base, Some("tok"));
        let req = sample_stream_event_request();
        let pid = uuid::Uuid::from_u128(7);

        let mut warned = false;
        let err = send_stream_event(
            &client,
            &Attribution::Repo {
                repo_id: "11111111-1111-1111-1111-111111111111".into(),
                project: Some(pid),
            },
            attribution_mode(None),
            &req,
            &mut warned,
        )
        .await
        .expect_err("a transient 503 must propagate as Err, not be swallowed by a fallback");
        assert!(
            err.to_string().contains("503"),
            "propagated error must reflect the transient status, got: {err}"
        );
        assert!(
            deterministic_client_error_kind(err.as_ref()).is_none(),
            "a 503 must not be classified as a deterministic client error"
        );

        let first = rx
            .recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT)
            .expect("no request captured");
        assert!(
            first.contains(&format!("/projects/{pid}/stream")),
            "must hit the project-scoped endpoint, got: {first}"
        );

        // The mock server only serves ONE response and then drops its
        // listener/sender — a stray fallback request would either fail the
        // `expect_err` above (if it somehow succeeded) or, if it also failed,
        // would still mean a second request was attempted. Confirming the
        // channel is immediately disconnected (rather than blocking) proves
        // no second request was ever sent.
        assert!(
            rx.recv_timeout(SEND_STREAM_EVENT_RECV_TIMEOUT).is_err(),
            "no second (fallback) request should have been sent for a transient error"
        );
    }

    /// A 403 must be classified apart from the binding/scoping statuses: since
    /// Keycloak it has a second, now more common cause (no `tracing` realm role
    /// at all) whose advice is completely different.
    #[test]
    fn a_403_is_classified_apart_from_the_scoping_statuses() {
        let err = |s: &str| -> Box<dyn std::error::Error> { s.to_string().into() };
        assert_eq!(
            deterministic_client_error_kind(err("Stream failed (403 Forbidden): {}").as_ref()),
            Some(ClientErrorKind::Forbidden)
        );
        for scoping in [
            "Stream failed (400 Bad Request): {}",
            "Stream failed (404 Not Found): {}",
            "Stream failed (409 Conflict): {}",
        ] {
            assert_eq!(
                deterministic_client_error_kind(err(scoping).as_ref()),
                Some(ClientErrorKind::Scoping),
                "{scoping}"
            );
        }
        // Unchanged: 401 and transient failures are not deterministic client
        // errors and must keep propagating to buffer/retry.
        for transient in [
            "Stream failed (401 Unauthorized): {}",
            "Stream failed (503 Service Unavailable): {}",
            "error sending request for url (http://x/stream)",
        ] {
            assert_eq!(
                deterministic_client_error_kind(err(transient).as_ref()),
                None,
                "{transient}"
            );
        }
    }

    /// The 403 error must not tell a user whose account has no `tracing` role
    /// to switch projects, which cannot possibly help. It names both causes,
    /// and — since the CLI no longer re-attributes to another project — must
    /// never claim it did.
    #[test]
    fn refused_error_names_both_403_causes_and_never_claims_re_attribution() {
        let pid = uuid::Uuid::from_u128(7);

        let forbidden = refused_error(pid, &ClientErrorKind::Forbidden, "derived");
        assert!(forbidden.contains("403"), "{forbidden}");
        assert!(
            forbidden.contains("realm role"),
            "must offer the missing-role explanation: {forbidden}"
        );
        assert!(
            forbidden.contains("TracePush"),
            "must still offer the project explanation: {forbidden}"
        );
        assert!(
            forbidden.starts_with("tracevault: error:"),
            "a refusal is an error, not a warning: {forbidden}"
        );
        assert!(forbidden.contains(&pid.to_string()));

        // A 400/404/409 genuinely IS a binding problem, so that wording stays
        // unhedged — hedging everything would dilute the useful case.
        let scoping = refused_error(pid, &ClientErrorKind::Scoping, "derived");
        assert!(scoping.contains("project switch"), "{scoping}");
        assert!(
            !scoping.contains("realm role"),
            "the scoping case must not mention the realm role: {scoping}"
        );

        for s in [&forbidden, &scoping] {
            // A machine-wide default is rebound with `--user`; without the
            // hint, `project switch` in a session only shadows it.
            assert!(s.contains("--user"), "must name the --user fix: {s}");
            assert!(
                s.contains("user_project.toml"),
                "must name the machine-wide default's file: {s}"
            );
            assert!(
                !s.contains("repo deduction"),
                "must never describe re-attribution to another project: {s}"
            );
            assert!(
                !s.contains("Attributing via"),
                "must never claim the event was re-attributed: {s}"
            );
        }
    }

    /// VIS-305 whole-branch review, Important finding 5: a REFUSED FORCE
    /// said nothing about the force. The 403 the force gate returns for "not
    /// Operator on the project" or "`explicit` from a `tvk_` key" is
    /// indistinguishable from here (see `ClientErrorKind`) from a membership
    /// 403, so an operator who asked for `explicit` got a message about
    /// membership and realm roles and was never told the force itself was
    /// what got refused. Failing closed on trust is right; failing closed
    /// silently is the defect. (VIS-316 made the refusal an error rather
    /// than a fallback; the missing explanation is orthogonal to that and
    /// still has to be given.)
    #[test]
    fn the_403_error_says_the_force_was_refused_when_the_send_declared_explicit() {
        let pid = uuid::Uuid::from_u128(7);

        let explicit = refused_error(pid, &ClientErrorKind::Forbidden, "explicit");
        assert!(
            explicit.contains("FORCE"),
            "the refusal of the force itself must be named: {explicit}"
        );
        assert!(
            explicit.contains("Operator"),
            "must name the grant forcing requires: {explicit}"
        );
        assert!(
            explicit.contains("Control Plane"),
            "must name the identity forcing requires: {explicit}"
        );
        // `main`'s Forbidden wording is ADDED TO, not replaced: the
        // membership causes it already names must survive alongside the
        // force clause.
        assert!(
            explicit.contains("realm role") && explicit.contains("TracePush"),
            "the membership causes must still be named: {explicit}"
        );
        // The event is queued, not re-attributed — the force clause must not
        // reintroduce the claim VIS-316 removed.
        assert!(
            !explicit.contains("repo deduction") && !explicit.contains("Attributing via"),
            "must never claim the event was re-attributed: {explicit}"
        );

        // Same 403, `derived` mode: no force was asked for, so none was
        // refused and the clause must not appear. This is the discriminating
        // half — without it the test would pass on a message that blamed the
        // force unconditionally, which is just a different wrong story.
        let derived = refused_error(pid, &ClientErrorKind::Forbidden, "derived");
        assert!(
            !derived.contains("FORCE"),
            "a `derived` send never forced anything: {derived}"
        );
    }

    /// The force clause belongs to the 403 the force gate returns, and to
    /// nothing else: a 400/404/409 means the project id does not resolve, so
    /// the gate never ran. Blaming the force there would be a new wrong
    /// explanation of the same family as the one finding 5 reports.
    #[test]
    fn an_explicit_scoping_failure_does_not_blame_the_force() {
        let pid = uuid::Uuid::from_u128(7);
        let scoping = refused_error(pid, &ClientErrorKind::Scoping, "explicit");
        assert!(!scoping.contains("FORCE"), "{scoping}");
        assert_eq!(
            scoping,
            refused_error(pid, &ClientErrorKind::Scoping, "derived"),
            "the scoping wording does not depend on the mode"
        );
    }
}
