# Visdom Trace CLI

The public CLI for Visdom Trace — AI code tracing and attribution. Installs the
`tracevault` binary.

## Crates

- **`tracevault-cli`** — the `tracevault` command-line tool (hooks, session capture, verification).
- **`visdom-ai-tracing-protocol`** — the wire-protocol types (stream + hook events) shared between the
  CLI and the Visdom Trace server. Published so both sides depend on one source of truth.

## Install

```bash
brew install VirtusLab/visdom-ai-tracing/tracevault
```

or from crates.io:

```bash
cargo install tracevault-cli
```

## Usage

### `tracevault login` — sign in (humans)

`tracevault login --server-url <URL>` signs you in through your organisation's Keycloak using
the OAuth 2.0 **device authorization grant** (RFC 8628) — the same flow as `gh auth login` or
a smart TV. The CLI asks the server which realm it trusts
(`GET /api/v1/auth/public-config`) and then talks to Keycloak directly; your password never
touches TraceVault.

```sh
tracevault login --server-url https://your-tracevault-server.example.com
```

It prints a verification URL and a one-time code, tries to open your browser (skip that with
`--no-browser`, which is also implied in CI/containers/headless sessions), and waits while you
approve. On success it writes `~/.config/tracevault/credentials.json` (mode `0600`) holding a
short-lived access token plus a long-lived `offline_access` **refresh token**, and prints the
account and role the server resolved for you.

That refresh token is what makes unattended use work: every command refreshes the access token
by itself shortly before it expires, so git hooks and background captures never stop to
prompt. Nothing has to be re-run periodically.

`tracevault logout` revokes the refresh token at Keycloak (best-effort — the local file is
removed either way) and deletes the credentials file.

If sign-in succeeds but the server answers "not authorized", the Keycloak account is missing
the `tracing` realm role. The credentials are still saved; an administrator has to grant
`tracing` (or `tracing-admin`), after which any TraceVault command works — no new login
needed.

**CI and automation do not use `tracevault login`.** They keep using a long-lived API key,
which never expires and needs no browser:

```sh
export TRACEVAULT_SERVER_URL=https://your-tracevault-server.example.com
export TRACEVAULT_API_KEY=tvk_...
```

`TRACEVAULT_API_KEY` takes precedence over the credentials file, so a key set in the
environment always wins on a machine that also has an interactive login. A server deployed
without Keycloak supports only this API-key path, and `tracevault login` says exactly that
instead of starting a flow that cannot complete.

### Project attribution — `TRACEVAULT_PROJECT` and `TRACEVAULT_PROJECT_ATTRIBUTION`

Two environment variables control which project captured events are attributed to, and who
owns that decision. They are separate on purpose: one says *which* project, the other says
*who vouches for it*.

```sh
# WHICH project: a launcher asserting attribution for everything in this process.
export TRACEVAULT_PROJECT=22222222-2222-4222-8222-222222222222

# WHO owns the decision: this caller does, so do not check repo/project membership.
export TRACEVAULT_PROJECT_ATTRIBUTION=explicit
```

**`TRACEVAULT_PROJECT`** sits above every remembered binding (session, repo config,
deduction, user default) and below only a `--project` flag and a subagent's per-worktree
override, so a `.tracevault/config.toml` baked into a pod image cannot outrank the launcher.

> **Only the UUID form is honoured.** Resolving a name needs a `list_projects` round trip,
> and the capture hook runs per event in a short-lived process, so it never makes one: a
> name is ignored and attribution falls through to the next tier. `tracevault project status`
> reports exactly what the capture path does, so it does not resolve a name either — it
> warns that the value is unused. `tracevault status` does resolve it, for display only, and
> flags the tier as one the wire ignores. **Export the UUID.**

**`TRACEVAULT_PROJECT_ATTRIBUTION=explicit`** declares that the caller owns the attribution,
so the server stamps the named project without checking that the repo belongs to it. Any
other value (or none) means `derived`, today's always-checked behaviour. It is process-wide
and **never expires** — it is re-asserted at every launch, which is the point for a pod
launcher.

The persisted equivalent is a flag on a switch:

```sh
tracevault project switch payments --project-attribution explicit
```

That stamps the force onto the binding it writes, and **it lapses after about one working
day (~12h)**, after which the CLI quietly stops sending the header and attribution is
checked again — so a force nobody remembers granting cannot outlive its reason. Switching
again without the flag clears the force immediately. The env-var form has no such lifetime.

Forcing is a trust claim and the server enforces it at ingest, not at `switch` time: it
requires a Control Plane identity with `Operator` on the target project. A long-lived
`tvk_` API key can never force. A refused force comes back as a `403`; the CLI prints an
error naming the force as a possible cause and the event is **queued for retry, never
re-attributed to some other project** — fix the grant (or drop the force) and the next
drain delivers it.

Use `tracevault project status` to see which tier won and which mode is in effect, and
`tracevault status` for the same thing as part of the full diagnostic.

### `tracevault init` — set up tracing in a repo

`tracevault init` wires TraceVault into a repository: it installs the AI-agent hooks that
capture sessions, adds git hooks (a pre-push policy check and a post-commit metadata push),
creates `.tracevault/`, and registers the repo with the server. Run it once per repo, from
the primary checkout (not a linked worktree).

**Choosing the agent — `--agent`**

TraceVault captures sessions from more than one coding agent. `--agent` selects which one to
install hooks for (default `claude-code`):

| Command | Installs | For |
|---|---|---|
| `tracevault init` | `.claude/settings.json` hooks | Claude Code (default) |
| `tracevault init --agent codex` | `.codex/hooks.json` hooks | OpenAI Codex CLI |

Both wire the same capture pipeline: the agent's hooks invoke `tracevault`, which streams the
session (transcript, tokens, cost, file changes) to the server tagged with the agent, so
Claude Code and Codex sessions show up side by side, each with its own badge. Codex file
changes come from the `apply_patch` tool event on Codex >= 0.153 (which carries the patch in
its hook payload) and from the session rollout on older versions.

`--claude-settings shared|local` chooses between `.claude/settings.json` (committed) and
`.claude/settings.local.json` (git-ignored). It applies only to `--agent claude-code`; with
`--agent codex` it is rejected (Codex always writes `.codex/hooks.json`).

**Codex hook trust — required once, or nothing is captured**

Codex >= 0.153 gates hooks behind persisted *hook trust*. Until it is granted, a
non-interactive `codex exec` skips the hooks **silently**: `hooks.json` looks installed, and
no session ever reaches the server. Neither `codex doctor` nor `codex features` reports the
trust state, so there is nothing to check after the fact.

After `tracevault init --agent codex` (or `--global --agent codex`), run `codex` once
interactively in a repo and approve the hook-trust prompt. Trust then persists for
`codex exec` runs too. For unattended use (CI images), pass
`--dangerously-bypass-hook-trust` instead — it runs the hooks without a persisted trust
record, which is appropriate when the hook source is your own installed CLI.

**Global install — `--global`**

`--global` installs hooks once for every session on the machine instead of per-repo, paired
with workspace mode (bind a repo mid-session with `tracevault repo switch`):

```sh
tracevault init --global                 # ~/.claude/settings.json + ~/.claude/CLAUDE.md
tracevault init --global --agent codex   # ~/.codex/hooks.json  + ~/.codex/AGENTS.md
```

### `tracevault context` — tagging events with flow and metadata

`tracevault context` manages context (flow ID, labels, params) that the Claude Code hook
stamps on every captured event. This drives grouping and filtering in the Flows view and
the analytics UI.

**Three layers, low → high precedence**

```
user  →  repo (global)  →  worktree
```

- **User** — optional, cross-repo, opt-in. Lives outside the repo (e.g.
  `~/.config/tracevault/context.json`), so it follows *you* across every project instead of
  being scoped to one repo.
- **Repo (global)** — `.tracevault/context.json`, shared by all worktrees of the repo.
- **Worktree** — `.tracevault/worktrees/<key>/context.json`, present only in a linked git
  worktree.

More specific wins: for `flow_id` and each `params` key, the highest layer that sets a value
takes precedence (worktree > repo > user). `labels` are a union across every present layer —
there is no removal of a label across layers, only `--remove-label` on the file you're
editing removes it from that file. `params` support a `null` tombstone: `context update
--remove-param KEY` records `KEY = null` in that layer's file rather than deleting the key,
so the removal propagates through the merge and drops an inherited value from a
lower-precedence layer (`context show`'s per-value provenance shows exactly which layer each
value in the effective context came from; values dropped by a tombstone are simply absent
from the output).

**Enabling the user layer — `user_context` in `config.toml`**

Off by default for compatibility: a `config.toml` without the field, or with
`user_context = false`, never consults a user layer. `tracevault init` enables it by default
for newly initialized projects (`--no-user-context` to opt out, `--user-context <path>` to
point it at an explicit file up front). The field accepts four forms:

| `config.toml` | Meaning |
|---|---|
| `user_context = false` (or field absent) | disabled — no user layer is consulted |
| `user_context = true` | enabled, reading `~/.config/tracevault/context.json` |
| `user_context = "/custom/path.json"` | enabled, reading from that file |
| `[user_context]` with `enable = false` / `path = "..."` | disabled, but remembers a path for later re-enabling |

Change it after the fact with `tracevault context source` (one of `--enable`, `--disable`,
`--path <file>`, or `--default` is required):

```sh
tracevault context source --enable                 # turn on at the default path
tracevault context source --path ~/team-ctx.json    # turn on, reading a custom file
tracevault context source --default                 # turn on and reset to the default path
tracevault context source --disable                 # turn off
```

**Editing each layer**

`context set` / `context update` / `context clear` operate on the repo/worktree file by
scope (the per-worktree file by default in a linked worktree, `--global` to force the
repo-wide file). Pass `--user` on any of them to target the resolved user-context file
instead, regardless of worktree scope:

```sh
tracevault context set --user --flow personal-defaults --label solo-dev --param editor=nvim
tracevault context update --user --remove-param editor
```

`tracevault context show` prints every layer that's present (User, Global, This worktree)
plus an Effective section that annotates each flow/label/param with the layer it resolved
from — useful for debugging why a value did or didn't win.

**Examples**

```sh
# One-time: point your personal context at a file you reuse across every repo
tracevault context source --path ~/.config/tracevault/context.json
tracevault context set --user --label solo-dev --param editor=nvim

# Per-repo, as before
tracevault context set --flow add-payment-retry --label payments --label backend
tracevault context update --param env=staging --remove-label backend
tracevault context show
tracevault context clear
```

## GitHub Action

This repo ships a composite action that verifies commits in a PR or push have corresponding
traces sealed on the server. It installs the CLI, detects the commit range from the event,
runs `tracevault verify --range`, and writes a pass/fail summary to the Actions step summary.

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0   # the action verifies a commit range, so it needs full history
- uses: VirtusLab/visdom-ai-tracing-cli/action@main
  with:
    server-url: https://your-tracevault-server.example.com
    api-key: ${{ secrets.TRACEVAULT_API_KEY }}
    # version: v0.20.1   # optional; defaults to the latest release
```

## License

Apache-2.0.
