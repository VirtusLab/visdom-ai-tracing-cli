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

## Quick start

There are two ways to set up capture. Start with the per-repo setup unless you know you need
the machine-wide one.

### One repository

Run these from the root of the repository you want traced — the primary checkout (not a
linked worktree), with an `origin` remote:

```sh
tracevault login --server-url https://your-tracevault-server.example.com   # once per machine
tracevault init                                                            # once per repo
```

Then start a new Claude Code session in the repo. Its sessions are captured and streamed to
the server as they happen.

**Log in before `init`.** `init` registers the repo with the server and writes the repo's
server id (`repo_id`) into `.tracevault/config.toml`; the hooks send events under that id, and
without it — and without any of the workspace-mode bindings described
[below](#every-session-on-the-machine----global) — they send nothing. So per-repo `init` refuses
to run without credentials (a login, or an API key in `TRACEVAULT_API_KEY`): it exits 1 before
installing anything and prints the commands to run instead:

```sh
tracevault login --server-url https://your-tracevault-server.example.com
tracevault init
```

(With `--server-url`, or `TRACEVAULT_SERVER_URL` set, the printed `login` command carries that
URL.) A repo set up by an older CLI, which installed the hooks without a login and exited 0, has
hooks but no `repo_id`: log in and run `tracevault sync` in the repo — it registers the repo and
writes the missing `repo_id`. Events from before that were not recorded and cannot be recovered.

### Every session on the machine — `--global`

```sh
tracevault login --server-url https://your-tracevault-server.example.com
tracevault init --global
```

This installs the hooks once, into `~/.claude/settings.json`, so they run in every Claude Code
session on the machine. **A session started in a checkout of a repo the server knows is
recorded and attributed to that repo with nothing more to do:** on its first hook (normally
`SessionStart`) the CLI takes the checkout's `origin` remote, asks the server which repo it
belongs to — once per session — and binds the session to it exactly as `tracevault repo
switch` would. The repo must already be registered, by someone's `tracevault init` or
`tracevault sync` in a checkout of it. If it is not, the session stays unrecorded and Claude
Code shows a message saying so, once; run `tracevault sync` in the checkout to register it,
then `tracevault repo switch <path>` to bind the running session. A remote whose codebase has
several repo records on the server binds the one the checkout's `.tracevault/config.toml` names
or the one named like the checkout's directory; with neither, it stays unbound and the message
lists the records to pick from with `repo switch --repo-id`.

That automatic binding applies only to a session nothing else binds. In order of precedence,
a session is bound to a repo by:

1. **`tracevault repo switch <path>` inside the session** — also how a session moves to work in
   another repo. `init --global` adds a note to `~/.claude/CLAUDE.md` telling the agent to run
   it when it does. The command reads the session id from `$TRACEVAULT_SESSION_ID`, which the
   session-start hook exports into Claude Code's shell (through Claude Code's
   `CLAUDE_ENV_FILE`; other agents do not get it, so pass `--session-id <id>` there). Run
   without a session id, it sets the user-level default (point 3) instead. The automatic
   binding is stored the same way, so a later `repo switch` replaces it, and `repo reset`
   clears it for good (the session is not bound automatically again).
2. **The repo has its own `tracevault init`** (a `.tracevault/config.toml` with a `repo_id`):
   sessions started in it are recorded as with the per-repo setup.
3. **A user-level default: `tracevault repo switch --user <path>`.** Every session not bound
   by points 1–2 is then attributed to that one repo, whatever directory it runs in — the
   automatic binding never applies while a user-level default is set.
4. **The automatic binding** from the session's checkout, described above.

A session with no repo is still recorded, repo-less, when it resolves to a project:
`TRACEVAULT_PROJECT` set to a project UUID, a `tracevault project switch` in the session, or a
user-level `tracevault project switch --user` default (which then catches every unbound session
on the machine) — see [Project attribution](#project-attribution--tracevault_project-and-tracevault_project_attribution).
A session that resolves to neither — one started outside any checkout, or in one whose remote
the server does not know — records nothing. If something else regenerates
`~/.claude/CLAUDE.md`, the note from point 1 is lost; re-run `tracevault init --global` to
restore it (it is idempotent). Without it, sessions are still bound automatically to the repo
they start in, but the agent is not told to `repo switch` when it moves to another repo.

### Check the result

The definitive check is end to end: start a Claude Code session in the repo, have it do
something (any tool call), and look for the session in the dashboard under **Traces →
Sessions**. The checks below are quicker but only show that the setup is right, not that
events arrived:

- In a per-repo setup, `.tracevault/config.toml` contains a `repo_id = "…"` line. Without it,
  sessions in the repo are captured only if bound some other way (workspace mode, a user-level
  default, a project) — and then not necessarily to this repo.
- `tracevault status` shows `✓ Logged in`, `✓ Recording` and `✓ Repo registered on server`.
  The server repo is looked up by the local `repo_id`, so a `config.toml` without one is an
  error (`✗ Repo registered on server — … has no repo_id`), not a match by directory name.
- Outside an agent session, `Recording` answers "will a new session in this checkout record?",
  using the same resolution as the capture hook: `✗` when `config.toml` has no usable
  `repo_id` and nothing else binds the checkout, `!` when nothing binds it at all (normal in
  workspace mode, where each session is bound by its hooks from the checkout's `origin`
  remote, or with `repo switch`).
- Inside an agent session (or with `--session-id <id>`), `Recording` checks that session's
  binding instead: `✗ Recording — nothing will be recorded` means the session is bound to
  neither a repo nor a project.

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

It prints a verification URL and a one-time code, tries to open your browser, and waits while
you approve. It skips the browser with `--no-browser`, or when `TRACEVAULT_NO_BROWSER` or `CI`
/ `GITHUB_ACTIONS` is set, inside a container (`/.dockerenv`), or — on Linux and other
non-macOS Unix — when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set. On success it writes
`credentials.json` (mode `0600`) into the TraceVault config directory — `~/.config/tracevault/`
on Linux (`$XDG_CONFIG_HOME/tracevault/` if set), `~/Library/Application Support/tracevault/`
on macOS — holding a short-lived access token plus a long-lived `offline_access` **refresh
token**, and prints the account and role the server resolved for you.

That refresh token is what makes unattended use work: every command refreshes the access token
by itself shortly before it expires, so git hooks and background captures never stop to
prompt. Nothing has to be re-run periodically.

`tracevault logout` revokes the refresh token at Keycloak (best-effort — the local file is
removed either way) and deletes the credentials file.

If sign-in succeeds but the server answers "not authorized", the account has no `tracevault`
grant in Control Plane. The credentials are still saved, but `login` exits with status 1. An
administrator has to grant `tracevault` plus the project roles you need (`tracevault.viewer` /
`tracevault.operator`) in Control Plane, or the `ai-tracing-admin` Keycloak realm role. No new
login is needed afterwards.

**CI and automation do not use `tracevault login`.** They keep using a long-lived API key,
which never expires and needs no browser:

```sh
export TRACEVAULT_SERVER_URL=https://your-tracevault-server.example.com
export TRACEVAULT_API_KEY=tvk_...
```

Each setting is taken from the first source that has it: the environment
(`TRACEVAULT_SERVER_URL`, `TRACEVAULT_API_KEY`), then the credentials file, then `server_url`
/ `api_key` in the repo's `.tracevault/config.toml`. So a key set in the environment always
wins on a machine that also has an interactive login. A server deployed without Keycloak
supports only this API-key path, and `tracevault login` says exactly that instead of starting
a flow that cannot complete.

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

**`TRACEVAULT_PROJECT`** is one tier of the project chain the capture hook uses, which is, from
highest precedence: a subagent's per-worktree override → `TRACEVAULT_PROJECT` → the session's
`project switch` binding → the user-level default (`project switch --user`). When none applies,
the hook sends no project and the server derives it from the repo. The capture hook never
reads `default_project` from `.tracevault/config.toml` (a name would need a network lookup), so
a config file baked into a pod image cannot outrank the launcher.

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

Because the caller owns the decision, a forced switch **also binds a project this checkout
is not a member of** — a repo deliberately shared by two projects, say — which an ordinary
switch refuses. It is not silent about it: the switch prints a note saying the project does
not contain this codebase and that the force is what made that deliberate, so a typo still
looks like a typo.

Forcing is a trust claim and the server enforces it at ingest, not at `switch` time: it
requires a Control Plane identity with `Operator` on the target project. A long-lived
`tvk_` API key can never force. A refused force comes back as a `403`; the CLI prints an
error naming the force as a possible cause and the event is **queued for retry, never
re-attributed to some other project** — fix the grant (or drop the force) and the next
drain delivers it.

Use `tracevault project status` to see which tier won, which mode is in effect, and — when
the binding carries a persisted force — whether that force is still live. Its `--project`
flag is a what-if for that display only; nothing on the capture path takes one. `tracevault
status` answers a narrower question as part of the full diagnostic: whether the tier that
won is one the capture path actually honours. It does not report the attribution mode or
the force.

### `tracevault init` — set up tracing in a repo

`tracevault init` wires TraceVault into a repository: it installs the AI-agent hooks that
capture sessions, adds git hooks (a pre-push policy check and a post-commit metadata push),
creates `.tracevault/`, and — when the repo has an `origin` remote — registers the repo with
the server and stores its `repo_id` in `.tracevault/config.toml`. It needs a login (or
`TRACEVAULT_API_KEY`) and fails with exit code 1, installing nothing, without one (see
[Quick start](#one-repository)). Every `init` error exits 1. Run it once per repo, from the
root of the primary checkout (not a linked worktree).

If the server already has a repo record for the checkout's remote, `init` binds to it instead of
registering a second one (a second record would split sessions between the two). When the
remote has several records and none is named like the checkout, `init` registers nothing and
lists them; set `repo_id` in `.tracevault/config.toml` to the right one. `tracevault sync`
follows the same rule.

Re-running `init` is safe: it keeps the existing `.tracevault/config.toml` (`repo_id`,
`user_context`, a pinned `server_url` / `api_key` / `default_project`, and keys it does not
know) and changes only what a flag on that run sets (`--server-url`, `--user-context`,
`--no-user-context`). If that file cannot be parsed, `init` stops before writing anything;
fix or remove it and run `init` again.

**Choosing the agent — `--agent`**

TraceVault captures sessions from more than one coding agent. `--agent` selects which one to
install hooks for (default `claude-code`):

| Command | Installs | For |
|---|---|---|
| `tracevault init` | `.claude/settings.json` (or `.claude/settings.local.json`) hooks | Claude Code (default) |
| `tracevault init --agent codex` | `.codex/hooks.json` hooks | OpenAI Codex CLI |
| `tracevault init --agent gsd` (alias `gsd2`) | the TraceVault extension, written to `gsd-extension/` in the TraceVault config directory and registered for this repo with `gsd install -l` | GSD (pi) |
| `tracevault init --agent opencode` | `.opencode/plugins/tracevault.ts` plugin | OpenCode |

All of them wire the same capture pipeline: the agent's hooks invoke `tracevault`, which
streams the session (transcript, tokens, cost, file changes) to the server tagged with the
agent, so sessions from different agents show up side by side, each with its own badge. Codex
file changes come from the `apply_patch` tool event on Codex >= 0.153 (which carries the patch
in its hook payload) and from the session rollout on older versions.

`--claude-settings shared|local` chooses between `.claude/settings.json` and
`.claude/settings.local.json`. Without it, `init` asks when run in a terminal and otherwise
uses `shared`. It applies only to `--agent claude-code` and is rejected with any other agent.
Hooks you already have in that file are kept; see [What `init` installs](#what-init-installs).

Unless you pass `--no-gitignore`, `init` adds `.tracevault/` and the agent's hook file (for
Claude Code, whichever settings file you chose — `settings.json` included) to the root
`.gitignore`, so nothing needs to be committed and every contributor runs `tracevault init`
for themselves. Pass `--no-gitignore` if you want to commit the hook file.

`--server-url <URL>` stores a server URL in `.tracevault/config.toml`; it is only needed when
the URL does not come from `tracevault login` or `TRACEVAULT_SERVER_URL`.

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
with workspace mode — see [Quick start](#every-session-on-the-machine----global) for how
sessions get bound to a repo:

```sh
tracevault init --global                    # ~/.claude/settings.json + ~/.claude/CLAUDE.md
tracevault init --global --agent codex      # ~/.codex/hooks.json  + ~/.codex/AGENTS.md
tracevault init --global --agent gsd        # GSD extension for all GSD sessions
tracevault init --global --agent opencode   # <config dir>/opencode/plugins/tracevault.ts
```

`<config dir>` is the platform config directory: `~/.config` on Linux (`$XDG_CONFIG_HOME` if
set), `~/Library/Application Support` on macOS. It does not need a git repository. It also writes the user-level `config.toml` in the
TraceVault config directory, which enables the user context layer (see below) for repos that
do not configure it themselves; `--no-user-context` and `--user-context <path>` apply to it.
`--global` cannot be combined with `--server-url`, `--claude-settings` or `--no-gitignore`.

**Claude Code's own cost — telemetry, on by default**

Claude Code reports the cost of every API call it makes, but only over OpenTelemetry. `init`
(per-repo or with `--global`) sends those reports to the TraceVault server, which then shows
Claude Code's own cost instead of an estimate. This is on by default: an interactive `init` asks
(`[Y/n]`), a non-interactive one turns it on. `--claude-telemetry` turns it on without asking,
`--no-claude-telemetry` turns it off:

```sh
tracevault init --global                        # telemetry on
tracevault init --global --no-claude-telemetry  # telemetry off, and it stays off
tracevault telemetry off                        # the same, without re-running init
tracevault telemetry on | status
```

**Turning it off** (`--no-claude-telemetry`, `tracevault telemetry off`, or no at the prompt)
removes only what TraceVault wrote: `otelHeadersHelper` when it is TraceVault's, and the `env`
entries below while they still hold TraceVault's values (`CLAUDE_CODE_ENABLE_TELEMETRY` and
`OTEL_LOGS_EXPORTER` stay when another exporter still uses them). The choice is recorded in
`~/.config/tracevault/install-state.json`, so neither a later `init` nor a CLI upgrade turns it
back on; only `--claude-telemetry` or `tracevault telemetry on` does.

**After a CLI upgrade**, the first Claude Code session that starts (its `SessionStart` hook,
`tracevault session-start`) brings an existing install up to date, once per version:

- it re-merges TraceVault's hooks into the settings file(s) of that session's repo, and into
  `~/.claude/settings.json`, that already hold them, so hooks a newer CLI adds reach existing
  installs (the files are found by looking for `tracevault` commands in `.claude/settings.json`
  and `.claude/settings.local.json`, and remembered). Repos you never open again keep their old
  hooks; re-run `tracevault init` there;
- it turns telemetry on, unless you turned it off, another exporter is set up (below), or Claude
  Code is set to export content (`OTEL_LOG_USER_PROMPTS` and friends, below). It then shows,
  once: *AI Tracing: Claude Code cost telemetry is now on for new sessions (`tracevault
  telemetry off` to disable)*, and `tracevault status` says it was turned on automatically.
  Telemetry is user-wide, so one upgraded repo turns it on for all of them.

This never fails the hook. When it cannot finish (not logged in, a settings file it cannot
parse), it says why on the hook's stderr and the next session tries again.

It writes to the **user-level** `~/.claude/settings.json` in every case — Claude Code ignores
OpenTelemetry exporter settings in a repository's `.claude/settings*.json`:

```json
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL": "http/protobuf",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "https://<server>/otlp/v1/logs",
    "CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS": "180000"
  },
  "otelHeadersHelper": "tracevault otel-headers --server-url https://<server>"
}
```

- No key or token is written to `settings.json`. Claude Code runs `otelHeadersHelper`, and
  `tracevault otel-headers` prints `{"Authorization": "Bearer …"}` from your login (or
  `TRACEVAULT_API_KEY`). It refuses, with an error, when you are not logged in or your login is
  for another server than the one in `--server-url`. Claude Code re-runs it every 3 minutes
  (`CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS`, kept if you set your own; Claude Code's
  default is 29 minutes), since a login's access token is short-lived; each run hands out a
  token valid for at least 4 more minutes.
- It needs a login: `init --claude-telemetry` fails when no credential resolves, and when
  `--server-url` names a server other than the one you are logged in to. Without the flag, a
  missing login is only a warning, and the upgrade step turns telemetry on once you log in.
- Every other key in `settings.json` is kept, and re-running is a no-op. A re-run after logging
  in to another server moves the export there. Restart Claude Code to apply.
- **An existing exporter setup is left alone.** If `~/.claude/settings.json`, the shell
  environment, or managed settings (`managed-settings.json` and `managed-settings.d/` in Claude
  Code's system directory, or the cached server-managed `~/.claude/remote-settings.json`)
  already set a log exporter other than `otlp`, a metrics or traces exporter, an
  OTLP endpoint other than TraceVault's, static OTLP headers, a non-HTTP OTLP protocol (from
  managed settings or the shell), a `BETA_TRACING_ENDPOINT`
  (detailed beta tracing sends logs there instead), or its own `otelHeadersHelper`,
  `init` changes nothing and prints the collector route below instead. Claude Code would send
  the helper's token to every exporter it runs, so the two are never mixed. Managed settings
  delivered only by an MDM profile or the Windows registry are not visible to `init`; if your
  organisation configures Claude Code telemetry that way, use the collector route.
- The server must run the OTLP receiver (`POST /otlp/v1/logs`, TraceVault server with
  VIS-1079) before you enable this; until then the exports are rejected and Claude Code shows
  an export warning.

**What leaves your machine.** Claude Code exports its OpenTelemetry *log events* for every
session on this machine, in any directory, to the TraceVault server: `api_request` (model,
`cost_usd`, token counts, duration, request ids), and also `user_prompt`,
`assistant_response`, `tool_result`, `tool_decision`, `api_error`, `hook_registered`,
`plugin_loaded`, `mcp_server_connection` and similar events, with session and user attributes
(names of hooks, plugins and MCP servers, not their content). With the settings above, **no
prompt text, response text, tool input or tool output is sent**: Claude Code redacts them
unless `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_DETAILS`,
`OTEL_LOG_TOOL_CONTENT` or `OTEL_LOG_RAW_API_BODIES` is set (all off by default; `user_prompt`
then carries only `prompt_length`, `assistant_response` only `<REDACTED>`). TraceVault never
sets them, and neither a plain `init` nor the upgrade step turns telemetry on while any of them
is set (`--claude-telemetry` or `tracevault telemetry on` still does). Only
`OTEL_LOGS_EXPORTER` is configured, so no metrics or traces are exported. The server reads only
an allowlist of `api_request` fields and rejects events for sessions it does not know.
(Checked against Claude Code's
[monitoring documentation](https://code.claude.com/docs/en/monitoring-usage) and by capturing
the export of Claude Code 2.1.312 with exactly these settings: a marker in the prompt did not
appear in it, 2026-10-29.)

`tracevault status` shows whether telemetry is configured, under *Installation*, and whether you
turned it off or an upgrade turned it on; `tracevault telemetry status` prints just that line.

*Collector route.* If your organisation already collects Claude Code telemetry, forward the
`api_request` events from your OpenTelemetry Collector instead, authenticated with a TraceVault
API key:

```yaml
processors:
  filter/tracevault:
    error_mode: ignore
    logs:
      log_record:
        - 'attributes["event.name"] != "api_request"'
exporters:
  otlphttp/tracevault:
    endpoint: https://<server>/otlp
    headers:
      Authorization: "Bearer ${env:TRACEVAULT_API_KEY}"
service:
  pipelines:
    logs/tracevault:
      receivers: [otlp]
      processors: [filter/tracevault]
      exporters: [otlphttp/tracevault]
```

### `tracevault context` — tagging events with flow and metadata

`tracevault context` manages context (flow ID, labels, params) that the capture hook stamps on
every captured event, whichever agent produced it. This drives grouping and filtering in the
Flows view and the analytics UI.

**Three layers, low → high precedence**

```
user  →  repo (global)  →  worktree
```

- **User** — optional, cross-repo, opt-in. Lives outside the repo (by default `context.json`
  in the TraceVault config directory, e.g. `~/.config/tracevault/context.json` on Linux), so
  it follows *you* across every project instead of being scoped to one repo.
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

Whether the user layer is consulted is decided by `user_context` in the repo's
`.tracevault/config.toml`. If the repo config does not set it, the user-level `config.toml`
(written by `tracevault init --global`) decides; if neither sets it, the layer is off.
`tracevault init` writes `user_context = true` into the repo config when it has none yet
(`--no-user-context` to write `false`, `--user-context <path>` to point it at an explicit file
up front); a re-run without either flag keeps the value already there. The field accepts four forms:

| `config.toml` | Meaning |
|---|---|
| `user_context = false` | disabled — no user layer is consulted, whatever the user-level config says |
| `user_context = true` | enabled, reading `context.json` in the TraceVault config directory |
| `user_context = "/custom/path.json"` | enabled, reading from that file |
| `[user_context]` with `enable` / `path = "..."` | `enable` defaults to `true`; `enable = false` disables the layer but keeps the path in the file |

Change the repo's setting after the fact with `tracevault context source` (one of `--enable`,
`--disable`, `--path <file>`, or `--default` is required). It edits only the current repo's
`.tracevault/config.toml` and fails if the repo has none. `--enable` and `--default` both write
`user_context = true`, so they use the default path and discard a remembered one:

```sh
tracevault context source --enable                 # turn on at the default path
tracevault context source --path ~/team-ctx.json    # turn on, reading a custom file
tracevault context source --default                 # same as --enable
tracevault context source --disable                 # turn off
```

To set the user layer up for every repo at once, use `tracevault init --global`
(optionally with `--user-context <path>`), which writes the user-level config.

**Editing each layer**

`context set` / `context update` / `context clear` operate on the repo/worktree file by
scope (the per-worktree file by default in a linked worktree, `--global` to force the
repo-wide file). Pass `--user` on any of them to target the resolved user-context file
instead, regardless of worktree scope:

```sh
tracevault context set --user --flow personal-defaults --label solo-dev --param editor=nvim
tracevault context update --user --remove-param editor
```

`tracevault context show` prints the User section (always — its header says whether the layer
is enabled and which file it reads), Global, This worktree (linked worktrees only), plus an
Effective section that annotates each flow/label/param with the layer it resolved
from — useful for debugging why a value did or didn't win.

**Examples**

```sh
# Personal context, used by every repo that does not opt out
tracevault context set --user --label solo-dev --param editor=nvim

# Per-repo
tracevault context set --flow add-payment-retry --label payments --label backend
tracevault context update --param env=staging --remove-label backend
tracevault context show
tracevault context clear
```

**Letting the agent tag its own work**

Flow IDs, labels and params have no fixed schema; each team decides what they mean. To have
Claude Code set context itself at the start of each task, add a rule like this to the repo's
`CLAUDE.md`:

```markdown
When you begin a new distinct task, run:
  tracevault context set --flow <kebab-case-task-name> --label <area-you-are-touching>

Examples of areas: frontend, backend, auth, payments, infra.
When the task is complete, run: tracevault context clear
```

Sessions in different linked worktrees each write their own worktree file, so parallel
sessions do not overwrite each other's context.

## Command reference

Every command and flag of `tracevault` 0.29. `tracevault <command> --help` prints the same from
the binary you have installed.

### Setup

**`tracevault login --server-url <URL> [--no-browser]`** — sign in through Keycloak; see
[`tracevault login`](#tracevault-login--sign-in-humans). `--server-url` is required.

| Flag | Meaning |
|---|---|
| `--server-url <URL>` | The TraceVault server to sign in to (required) |
| `--no-browser` | Only print the verification URL; also implied in CI, containers and Linux sessions without a display |

**`tracevault logout`** — revoke the refresh token at Keycloak (best-effort) and delete the
credentials file.

**`tracevault init [OPTIONS]`** — set up capture in the current directory, which must be the
root of a git repository's primary checkout; see
[`tracevault init`](#tracevault-init--set-up-tracing-in-a-repo). With `--global`, set it up for
every session on the machine instead.

| Flag | Meaning |
|---|---|
| `--agent <claude-code\|codex\|gsd\|opencode>` | Which agent to install hooks for (default `claude-code`; `gsd2` is an alias of `gsd`) |
| `--claude-settings <shared\|local>` | Claude Code only: `.claude/settings.json` or `.claude/settings.local.json`. Omitted: asks in a terminal, otherwise `shared` |
| `--no-gitignore` | Do not add `.tracevault/` and the hook file to the root `.gitignore` |
| `--server-url <URL>` | Register the repo with this server during this `init` (instead of the URL from the environment or your login), and store it in `.tracevault/config.toml`, where later commands treat it as the lowest-precedence source (see [credentials](#tracevault-login--sign-in-humans)) |
| `--user-context <path>` | Enable the user context layer, reading this file |
| `--no-user-context` | Disable the user context layer (conflicts with `--user-context`) |
| `--global` | Install machine-wide instead of in this repo; does not need git. Conflicts with `--server-url`, `--claude-settings`, `--no-gitignore` |
| `--claude-telemetry` | Claude Code only: send Claude Code's own per-request cost to TraceVault over OpenTelemetry, configured in `~/.claude/settings.json` (see [telemetry](#tracevault-init--set-up-tracing-in-a-repo)), without asking. Neither flag: asks in a terminal (default yes), otherwise on, unless you turned it off before |
| `--no-claude-telemetry` | Claude Code only: turn that telemetry off (removes TraceVault's entries) and remember it across upgrades. Conflicts with `--claude-telemetry` |

**`tracevault telemetry on|off|status`** — turn Claude Code telemetry on (needs a login), off
(remembered across upgrades), or show its state, like `init --claude-telemetry` /
`--no-claude-telemetry` without re-running `init` (see
[telemetry](#tracevault-init--set-up-tracing-in-a-repo)).

**`tracevault sync`** — register the repo with the server, or bind to its existing record, and
write `repo_id` into `.tracevault/config.toml` if the repo has one. Needs a login (or API key)
and an `origin` remote. The pre-push hook runs it before every push.

**`tracevault status [--session-id <id>]`** — diagnose the setup: authentication, global and
repo installation, git and agent hooks (Claude Code hooks in `.claude/settings.json`,
`.claude/settings.local.json` or `~/.claude/settings.json`), Claude Code telemetry
(on, off by your choice, or turned on by an upgrade), whether anything will record, the
session's repo/project binding, the repo on the server (by `repo_id`) and its server-side clone,
and pending events. Exits 1 if any check is an error (`✗`).
`--session-id` picks the session whose binding is checked; default `$TRACEVAULT_SESSION_ID`,
else the most recent session with a workspace binding.

### Day to day

**`tracevault check`** — evaluate the server's policies for the sessions behind the commits
being pushed; exits 1 when a blocking policy fails. The pre-push hook runs it (feeding it git's
pre-push input) and aborts the push on failure.

**`tracevault flush`** — retry events that failed to stream in real time (network blip, server
down) from the local queue.

**`tracevault stats`** — print local session statistics (sessions, events) for this repo.

**`tracevault verify (--range <a..b> | --commits <sha,sha,...>)`** — check that commits are
registered and sealed on the server. `--range` is a git range expanded with `git rev-list`;
`--commits` is a comma-separated list. If both are given, `--range` is used. Exits 1 on
failure. The [GitHub Action](#github-action) runs it.

**`tracevault verify-start [--session-id <id>]`** — open (or re-open) a *verification phase*
for the session: the agent declares it has finished changing code and is now running checks.
Only tool calls after this point are evaluated by policies scoped to the verification phase;
calling it again resets the phase. Without `--session-id` it uses `$TRACEVAULT_SESSION_ID` if
that session has events in this worktree, else detects the session by worktree.

**`tracevault agent-policies`** — print agent-readable instructions rendered from the policies
configured on the server for this repo, for an agent to read at session start (for example from
`CLAUDE.md`).

**`tracevault proxy info`** — print the TraceVault LLM proxy URL and setup instructions for AI
tools (Claude Code, GSD2, Cursor, Codex CLI, …).

### Context

`tracevault context set|update|show|clear|source` — see
[`tracevault context`](#tracevault-context--tagging-events-with-flow-and-metadata).

| Command | Flags |
|---|---|
| `context set` | `--flow <id>`, `--label <l>` (repeatable, comma-separated accepted), `--param key=value` (repeatable), `--global`, `--user`. Replaces the whole context of the target file |
| `context update` | `--flow <id>`, and the repeatable `--label <l>`, `--param key=value`, `--remove-label <l>`, `--remove-param <key>`; `--global`, `--user`. Merges into the target file |
| `context show` | — |
| `context clear` | `--global`, `--user` |
| `context source` | exactly one of `--enable`, `--disable`, `--path <file>`, `--default` |

Target file: the per-worktree file in a linked worktree, the repo file in the primary
checkout; `--global` forces the repo file, `--user` the user-context file (and wins over
`--global`).

### Workspace mode — `repo` and `project`

Commands that bind a session to a repo or a project, used with the `--global` install (see
[Quick start](#every-session-on-the-machine----global)) and by launchers. Every one of them takes
`--session-id <id>` (default `$TRACEVAULT_SESSION_ID`).

| Command | Meaning |
|---|---|
| `repo switch <path> [--repo-id <uuid>] [--user]` | Bind the session to the registered repo that the checkout at `<path>` belongs to (resolved from its `origin` remote) and print the repo's policies. `--repo-id` picks one record when the codebase has several. `--user` (implied when there is no session id) writes the machine-wide default instead. `--name` is deprecated and always errors |
| `repo status [--path <path>]` | Show the session's repo binding; `--path` resolves a checkout once instead |
| `repo reset [--user]` | Clear **all** of the session's bindings — repo and project, including subagent overrides — and stop the hooks binding it automatically from its checkout again; or with `--user` only the machine-wide default repo |
| `project switch <name> [--user] [--project-attribution derived\|explicit]` | Bind the session (or, with `--user`, every session) to the project with this exact, case-sensitive name. `explicit` forces attribution; see [Project attribution](#project-attribution--tracevault_project-and-tracevault_project_attribution) |
| `project status [--project <name>]` | Show which project the session is attributed to and why; `--project` is a display-only what-if |

### Run by hooks, not by hand

These are hidden from `--help`. `init` installs them; you never need to run them.

| Command | Run by |
|---|---|
| `tracevault stream --event <event> [--agent <agent>]` | The agent's hooks, on every captured event (`--agent` defaults to `claude-code`) |
| `tracevault session-start` | The agent's `SessionStart` hook: binds an unbound session to the repo of its checkout's `origin` remote, injects the bound repo's policies; under Claude Code it also exports `TRACEVAULT_SESSION_ID` into the agent's shell. Once per CLI upgrade it also refreshes the hooks and turns telemetry on (see [telemetry](#tracevault-init--set-up-tracing-in-a-repo)). It then sends the session cost earlier sessions left unsent (see [session cost](#what-init-installs)) |
| `tracevault user-prompt` | The agent's `UserPromptSubmit` hook: re-injects the policies when the session's repo has changed |
| `tracevault commit-push` | The git `post-commit` hook, in the background: sends the commit's metadata |
| `tracevault otel-headers [--server-url <URL>]` | Claude Code's `otelHeadersHelper` (`init --claude-telemetry`): prints `{"Authorization": "Bearer …"}` for the OpenTelemetry export; exits 1 when not logged in, or logged in to another server than `--server-url` |

## What `init` installs

**Claude Code** (`.claude/settings.json` or `.claude/settings.local.json`; with `--global`,
`~/.claude/settings.json`).

Every hook install — per-repo or `--global`, Claude Code or Codex — updates the hook file in
place. TraceVault's own hooks are the ones whose `command` starts with `tracevault `; `init`
replaces exactly those with the current set and leaves everything else as it was: your other
hooks (including ones in the same matcher group as a TraceVault hook) and every other setting.
Re-running `init` brings an older install up to date (matchers, timeouts, newly added events),
and a file that is already current is not rewritten. A hook file that is not valid JSON, or
whose `hooks` section has an unexpected shape, is left untouched and `init` fails with an error.

Having the hooks in more than one place — `settings.json` and `settings.local.json`, or a repo
and `--global` — does not stream events twice: Claude Code runs an identical hook command once
even when several settings files define it.

| Claude Code event | Matcher | Command |
|---|---|---|
| `PreToolUse` | `Write\|Edit\|Bash` | `tracevault stream --event pre-tool-use` |
| `PostToolUse` | all tools | `tracevault stream --event post-tool-use` |
| `Notification` | all | `tracevault stream --event notification` |
| `Stop` | all | `tracevault stream --event stop` |
| `SessionEnd` | all | `tracevault stream --event session-end` |
| `SubagentStop` | all | `tracevault stream --event subagent-stop` |
| `SessionStart` | all | `tracevault session-start` |
| `UserPromptSubmit` | all | `tracevault user-prompt` |

**Session cost.** At exit, after `Stop`, Claude Code appends its own figure for the session's
cost to the transcript (a `cost-state` line). `SessionEnd` sends the lines written since the last
hook, that one included, as transcript lines only: `Stop` already ended the session on the server.
If it could not (the hook failed or timed out, or the session ended before an upgrade installed
it), `session-start` sends it later: for each session of the repo whose unsent lines hold a
`cost-state`, it sends them as `SessionEnd` would have. It looks at sessions written in the last
30 days, at most 10 per session start, and stops when the server does not take them. It does
not run when `TRACEVAULT_PROJECT` or `TRACEVAULT_PROJECT_ATTRIBUTION` is set: they attribute what
the current process sends, and an earlier session may have run with other values.

**Subagents.** Claude Code writes each subagent's conversation to its own file,
`<session>/subagents/agent-<id>.jsonl` next to the session transcript, and leaves the subagent's
messages and token usage out of the main transcript. Every `stream` hook therefore also sends what
each of those files gained since the last hook: under the parent session, one file per request
(never mixed with another file's lines or the main transcript's), each file with its own read
offset in `.tracevault/sessions/<session>/.subagent_offsets/`. Subagents running in the background
are sent while they run, and `Stop` reads every file to its end. `SubagentStop` sends only those
files, which covers a background subagent that finishes after the session's last `Stop`. A batch
that cannot be delivered is queued like any other event and sent by a later hook or
`tracevault flush`. This needs a server that keys transcript chunks by the line's `agentId`; an
older one cannot tell subagent lines from the main transcript's.

**Codex** (`.codex/hooks.json`; with `--global`, `~/.codex/hooks.json`): `SessionStart` and
`UserPromptSubmit` as above, plus `PostToolUse` and `Stop` streamed with `--agent codex`. There
is deliberately no `PreToolUse` hook for Codex.

**Claude Code telemetry** (unless `--no-claude-telemetry`, a no at the prompt, or an earlier
opt-out; always `~/.claude/settings.json`): the `env` entries and `otelHeadersHelper` shown
under [telemetry](#tracevault-init--set-up-tracing-in-a-repo). The choice and the version that
last configured the machine go into `~/.config/tracevault/install-state.json`.

**Git hooks** (per-repo `init` only; an existing hook keeps its content and gets a TraceVault
block appended):

- `pre-push` — `tracevault sync`, then `tracevault check`; a failed check aborts the push.
- `post-commit` — `tracevault commit-push`, in the background.

**Files**: `.tracevault/` (with `config.toml`, `sessions/`, `cache/` and its own `.gitignore`),
and — unless `--no-gitignore` — entries for `.tracevault/` and the hook file in the root
`.gitignore`.

## Environment variables

| Variable | Effect |
|---|---|
| `TRACEVAULT_SERVER_URL` | Server URL; wins over the credentials file and `.tracevault/config.toml` |
| `TRACEVAULT_API_KEY` | API key (`tvk_…`) for CI and automation; wins over an interactive login |
| `TRACEVAULT_PROJECT` | Project UUID to attribute events to; see [Project attribution](#project-attribution--tracevault_project-and-tracevault_project_attribution) |
| `TRACEVAULT_PROJECT_ATTRIBUTION` | `explicit` to force attribution to that project; anything else means `derived` |
| `TRACEVAULT_SESSION_ID` | Default session for `status`, `verify-start`, `repo …` and `project …`; the `SessionStart` hook exports it into the agent's shell |
| `TRACEVAULT_NO_BROWSER` | Any value: `login` does not open a browser |
| `XDG_STATE_HOME` | Base for per-session state (`$XDG_STATE_HOME/tracevault/sessions`, default `~/.local/state/tracevault/sessions`) |

The capture hook also reads the agent's own environment and auth files — for Claude Code
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CONFIG_DIR`; for Codex `CODEX_HOME`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` — only to
tell the server how the session is billed (gateway, API key or subscription). It never sends
their values.

## Files

All user-level files live in the TraceVault config directory: `~/.config/tracevault/` on Linux
(`$XDG_CONFIG_HOME/tracevault/` if set), `~/Library/Application Support/tracevault/` on macOS.

| File | Written by | Holds |
|---|---|---|
| `credentials.json` | `login` (mode `0600`) | Server URL, access and refresh token |
| `config.toml` | `init --global` | User-level settings (the user context layer) |
| `context.json` | `context … --user` | The user context layer (default location) |
| `default_repo.toml` | `repo switch --user` | Machine-wide default repo binding |
| `user_project.toml` | `project switch --user` | Machine-wide default project binding |
| `<repo>/.tracevault/config.toml` | `init`, `sync`, `context source` | `repo_id`, `user_context`, the agent, and optionally `server_url` / `api_key` / `default_project` |
| `<repo>/.tracevault/context.json` | `context …` | The repo context layer |
| `<repo>/.tracevault/worktrees/<key>/context.json` | `context …` in a linked worktree | The worktree context layer |

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

It installs the `x86_64-unknown-linux-gnu` build, so it needs an x86_64 Linux runner. It
supports `push`, `pull_request` and `pull_request_target` events and fails on any other. On
the push that creates a branch it verifies only the head commit.

## License

Proprietary. The source is public, but the CLI is licensed only for use with a Visdom AI
Tracing deployment your organisation is authorised to use; see [LICENSE](LICENSE). Versions up
to 0.28.4 were released under Apache-2.0.
