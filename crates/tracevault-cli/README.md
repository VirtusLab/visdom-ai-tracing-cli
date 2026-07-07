# tracevault

CLI tool for Visdom Trace — AI code tracing and attribution.

## Install

```sh
cargo install tracevault-cli
```

## Usage

```sh
tracevault init        # Initialize in a repo
tracevault status      # Show tracing status
tracevault check       # Evaluate policies before push
tracevault flush       # Retry any events that failed to stream live
```

### Workspace / detached mode

For headless or autonomous workers that clone and work across multiple repositories (rather
than a single pinned repo), use workspace mode to bind a session to different repos on the fly.

**When to use:** Claude Code sessions not bound to one repo, headless/autonomous agents,
multi-repo workspaces, or a user-level (global) installation — versus the normal `tracevault init`
single-repo flow.

**Global installation** — `tracevault init --global` installs TraceVault hooks once into
`~/.claude/settings.json` (deep-merged; appends to existing hooks and does not clobber) for use
across ALL Claude Code sessions without per-repo setup. Does not create `.tracevault/config.toml`.
Adds two session-level hooks: `SessionStart` exports the session ID and injects the bound repo's
policies, and `UserPromptSubmit` re-injects policies when the session's effective repo changes.

**Commands**

- `tracevault repo switch <path>` — bind the current session's tracing to the repo at `<path>`
  (must be pre-registered with TraceVault) and print its policies.
- `tracevault repo status [--path <path>]` — show the session's effective repo binding and which
  precedence tier it came from.
- `tracevault repo reset` — clear the session's workspace binding.

**Session identity** — these commands resolve the session via `--session-id` or the
`TRACEVAULT_SESSION_ID` environment variable (set by `tracevault init --global`).

**Precedence** (high → low): `--path` override → subagent worktree override → session binding
(`repo switch`) → bound `.tracevault/config.toml`.

**Note:** Repos must be pre-registered with TraceVault. Workspace mode resolves a repo by its
git remote URL; it does not create new registrations.

## License

Apache-2.0
