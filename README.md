# Visdom Trace CLI

The public CLI for Visdom Trace — AI code tracing and attribution. Installs the
`tracevault` and `visdom` binaries.

## `visdom`

`visdom` is the developer CLI for the whole Visdom suite and is replacing `tracevault`. Run it
instead of your coding agent: it checks that the project is wired up, offers to fix what is not,
and starts the agent.

```sh
visdom                      # check → offer fixes → start the agent (Claude Code by default)
visdom --agent codex -- --model gpt-x     # arguments after `--` go to the agent
visdom --mode gateway       # …with the agent's LLM traffic going through the AI Gateway
visdom doctor               # the checks only; exit code 1 when one fails
visdom doctor --fix         # …and offer the fixes (--yes applies all, --json for scripts)
```

| Command | What it does |
|---|---|
| `visdom [--agent <a>] [--mode <m>] [--yes] [--no-instructions] [-- <agent args>]` | Check, offer each fix, start the agent in the current directory. Findings never stop the agent from starting (one exception: a gateway the workspace requires and that cannot be used, see below). |
| `visdom doctor [--fix] [--yes] [--json] [--agent <a>] [--mode <m>]` | The same checks without starting anything. |
| `visdom login` / `visdom logout` | Same as `visdom login` / `logout`. |
| `visdom init [...]` | Sets up this repository, the whole machine (`--global`) or a folder of checkouts (`--workspace`) — asked when no flag says which: tracing (`visdom trace init`, same flags), then registers the bazaar MCP server for the agent, then asks how your sessions are traced when that is yours to choose — and, through the AI Gateway, leaves the gateway ready to use. |
| `visdom instructions [--agent <a>] [--mode <m>]` | Prints what an agent started here with `visdom` is told about Visdom's tools (see below). |
| `visdom env [--agent <a>]` | Prints shell commands that send an agent you start *without* `visdom` through the AI Gateway: `source <(visdom env)`. |
| `visdom trace <command>` | Every `tracevault` command: `visdom trace status`, `visdom trace verify-start`, … |

What is checked:

- **Tracing** — logged in; the repository is set up (or covered by a `--global` install); the
  agent's hooks are installed; the git hooks are installed.
- **Gateway** — which way sessions are traced and who decided it; through the AI Gateway, that
  the gateway is known, your login works there and a project lets you have a personal key.
- **Bazaar** — the bazaar MCP server is registered for the agent with the OAuth client
  `visdom-context-mcp` (without it sign-in fails with a 403). The entry is recognised by its URL,
  whatever it is named. Registration goes through the agent's own CLI (`claude mcp add …`,
  `codex mcp add …`); `visdom` never edits the agent's files.

Nothing is changed without a yes: each fix is shown first, `--yes` applies all of them, and when
stdin is not a terminal nothing is applied. `visdom doctor` changes nothing without `--fix`.
`visdom init` is the exception by definition: asking for it is the yes, for tracing and for the
bazaar registration alike (the command it runs is printed first).

`visdom init` starts by settling **what it sets up**:

| | Tracing | Bazaar MCP | Gateway project kept in |
|---|---|---|---|
| this repository (the default in a checkout) | `visdom trace init` here | for this project directory | your user config |
| every session on this machine (`--global`; the default outside a checkout) | `visdom trace init --global` | for all of your projects | your user config |
| this folder of checkouts (`--workspace --project <name>`) | `visdom trace init --workspace`, and — chosen from the question — the machine-wide hooks it needs | for all of your projects | the folder's `.visdom/config.toml` |

On a terminal with neither flag it asks which; without a terminal, or with an option only a
repository has (`--claude-settings`, `--no-gitignore`), it is the repository. Whichever it is, the
rest is the same: the agent, the login, bazaar, the tracing mode and the gateway. A folder of
checkouts is no git checkout, and its `.visdom/config.toml` applies in it and in the directories
below it that are not checkouts themselves.

On a terminal, `visdom init` asks for what it was not given instead of failing: which agent
(unless `--agent`, `VISDOM_AGENT` or a config names one), the tracing server URL when you are not
logged in (then it logs you in), the bazaar URL when nothing knows it (saved as `bazaar_url`
in the user config), and how your sessions are traced when the installation has an AI Gateway and
the choice is yours (saved as `tracing_mode`). Without a terminal it asks nothing and reports what
is missing.

When your sessions go through the AI Gateway — you chose it just now or earlier, passed
`visdom --mode gateway init` (which is then saved), or the workspace requires it — `visdom init`
ends by making the gateway ready, so that the first `visdom` does not fail on something init
could have settled:

- it writes down the gateway project (`[gateway] project`): the only one open to you, or, with
  several, the one you pick — so that a project opened to you later does not unsettle it;
- it issues your personal key and tries it on the LLM gateway;
- it writes down the model alias the agent will ask for (`[gateway] claude_code_model` /
  `codex_model`): the one picked for the agent, or, when none of the aliases is named for it, the
  one you pick;
- it prints what it set up and **where each setting comes from**:

  ```text
  AI Gateway ready for Claude Code: `visdom` starts it through the gateway.
    mode         gateway                  (your choice)
    console      https://gw.example.com   (published by the tracing server)
    LLM gateway  https://llm.example.com  (named by the console)
    project      payments                 (project under [gateway] in ~/.config/visdom/config.toml)
    model        claude-sonnet            (claude_code_model under [gateway] in ~/.config/visdom/config.toml)
    personal key issued, and accepted by the LLM gateway  (kept for you only, renewed by `visdom`)
  ```

A gateway that cannot be made ready (nobody to ask which project, no login it accepts, an LLM
gateway that refuses the key, no model alias to ask for, a config that cannot be written) makes
`visdom init` fail, with what to set and where.

Every one of these settings can be kept in the config file; **what is left out there is taken
from the environment** (`VISDOM_TRACING_MODE`, `VISDOM_GATEWAY_URL`, `VISDOM_GATEWAY_PROJECT`,
`VISDOM_GATEWAY_MODEL`), and a variable that is set wins over the file — see Settings below.

### What the agent is told

An agent does not use what it does not know it has. So `visdom` tells the agent it starts what
this session has of Visdom's and when to use it, in a short text ahead of your own instructions:

- **Tracing** (when you are logged in and the agent's hooks are installed, which is what binds a
  session to its repository — in both tracing modes): the session is bound to a repository whose policies
  apply; `visdom trace agent-policies` prints them, `visdom trace repo switch <path>` comes before
  work in another repository, `visdom trace repo status` shows the binding, and — with hooks
  tracing — `visdom trace check` comes before a push.
- **Bazaar** (when its MCP server is registered for the agent): what it holds and when to ask it,
  under the name the server is registered with. Not when you start Claude Code without its
  registered servers (`--strict-mcp-config`, `--bare`), or Codex with an option that changes that
  server (`-c mcp_servers.…`).
- **The AI Gateway** (when the session goes through it, beside either of the above: alone it gives
  the agent nothing to use, and nothing is said): the provider settings are left alone.

What you pass the agent counts: Claude Code's `--bare` runs no hooks, so nothing is said of
tracing; a Codex sent to another directory with `-C` is told nothing, since that directory was not
looked at; a Codex profile (`-p`) that configures MCP servers leaves bazaar out.

Only what was found working is mentioned, and where none of it is there the agent is started
exactly as before. `visdom instructions` prints the text for the current directory.

Nothing is written to `CLAUDE.md`, `AGENTS.md` or any other file, and no hook is involved: the
text is an option of the agent's own command line.

| Agent | How | Your own |
|---|---|---|
| Claude Code | `--append-system-prompt` | An `--append-system-prompt` you pass after `--` is kept, after `visdom`'s text (Claude Code itself would keep only the last one). |
| Codex | `-c developer_instructions=…` | Your `developer_instructions` are kept, after `visdom`'s text: from `-c`, else the profile you name (`-p`), your `config.toml` or the machine's `/etc/codex/config.toml`. |

Codex has one `developer_instructions`, and an option replaces it. So where a file that is not
your own sets it — a `.codex/config.toml` in the directory Codex works in or above it, or a
`[profiles.*]` table in your `config.toml` — `visdom` adds nothing and says so: whether Codex
reads a project's file is its decision (the project's trust), and `visdom` neither overrules that
file nor turns its content into an option. Put the output of `visdom instructions` there
yourself, or pass your own with `-c`. Instructions an organisation manages for Codex are not
visible to `visdom` and would be replaced: use `agent_instructions = false` there.

`visdom --no-instructions` starts the agent without it once; `agent_instructions = false` in the
user config leaves it out always. An agent started without `visdom` (see `visdom env`) is not told
anything: `claude --append-system-prompt "$(visdom instructions)"` does it by hand.

### Hooks or the AI Gateway

A session reaches AI Tracing one of two ways, and **you choose**:

- `hooks` (the default) — the agent's hooks capture the session, as with `tracevault`.
- `gateway` — `visdom` starts the agent with its LLM traffic going through the Visdom AI Gateway,
  which records the calls. The hooks stay installed (they still bring the repository's policies
  into the session and link commits) but their capture is off for that agent, so nothing is
  recorded twice. **Known limit:** the check before a push evaluates the sessions the hooks
  captured on this machine; a gateway session is not among them, so it is not part of that
  check.

Your choice is `--mode`, then `VISDOM_TRACING_MODE`, then `tracing_mode` in the repository's
`.visdom/config.toml`, then in the user config. That variable is only your preference for
`visdom`: an agent you start without `visdom` is captured by its hooks whatever it says (what
switches the capture off is `VISDOM_GATEWAY_SESSION=<agent>`, which `visdom` sets on the agent it has
routed through the gateway, and nobody else should).

**A workspace admin may decide instead**, with the AI traffic policy in Control Plane:
`enforced` puts every session through the gateway, `direct` keeps every session on hooks, and
`per_agent` (the default) leaves it to you. `visdom` reads the policy with your login on every
start and remembers the last answer for when Control Plane cannot be reached; where Control Plane
is, it remembers too, for when the tracing server cannot be asked (both per installation). When no policy can be read at
all, your choice applies.

In gateway mode `visdom`:

- finds Control Plane and the gateway from the tracing server you are logged in to (its
  `public-config`), so there is nothing to configure in a normal installation;
- asks the gateway console for your **personal key** for the project (`[gateway] project` when
  several are open to you), keeps it in `visdom/gateway-keys.json` readable by you only, and
  reuses it until shortly before it expires — a new key revokes the previous one, so sessions
  started side by side share one (the later ones wait for the first). A kept key is the account's
  it was issued for, and `visdom logout` removes them all;
- asks the LLM gateway for its models with that key before starting anything: a kept key the
  gateway no longer takes (you issued a newer one in the console) is replaced once;
- gives everything to the launched agent only (environment and launch options); your agent's own
  configuration files are not touched, and the agent started without `visdom` is as it was.

| | Claude Code | Codex |
|---|---|---|
| Key | from `visdom gateway-key` (`apiKeyHelper`), renewed during the session | fixed at start (`VISDOM_GATEWAY_API_KEY`); a session running when the key expires (24 hours at most) has to be restarted |
| Model | `ANTHROPIC_MODEL` and the small model, set to a gateway alias | `--model <alias>` |
| Session id | chosen by `visdom` and sent with every call; yours when you pass `--session-id` or `--resume <id>`; none when Claude Code picks it (`--continue`, a picker, `--fork-session`) | Codex's own (`thread-id`) |

The model alias is the one under `[gateway]` in your config, else the first alias the key may
call that is named for the agent (`claude…`, `gpt…`); with neither the agent asks for its own
default, which the project may refuse — `visdom` says so.

Your own agent configuration cannot take a gateway session elsewhere:

- **Claude Code** applies the `env` of its settings files over the environment it is started in, so
  `visdom` gives the gateway's address, the attribution headers and the key helper as settings on
  the command line (`--settings`), which come after your `settings.json` and the project's; there
  it also switches off Claude Code's switches to another provider (`CLAUDE_CODE_USE_BEDROCK`,
  `…_VERTEX`, `…_FOUNDRY`, `…_MANTLE`, `…_ANTHROPIC_AWS`, `…_ANTHROPIC_GOOGLE_CLOUD`,
  `…_GATEWAY`), each of which would send the session there instead, and clears
  an `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` of yours, which would be sent in place of the
  gateway key. Settings you
  pass with `--settings` yourself are kept, merged under these; a `--settings` value that cannot be
  read stops the gateway launch. (Settings your organisation manages come after everything and are
  not `visdom`'s to overrule.)
- **Codex** takes your `-c` overrides after `visdom`'s, so one that chooses the model provider
  (`-c model_provider=…`, `-c model_providers.visdom-gateway.…`, `--oss`, `--local-provider`) stops
  the gateway launch: leave it out, or use `--mode hooks`.

If gateway mode cannot be set up (no gateway, an API-key login, no project open to you, an LLM
gateway that does not answer or refuses the key, a key store that cannot be locked or written,
options of yours that choose another provider):

- you chose it: the agent is started with hooks tracing and `visdom` says why;
- the workspace requires it: the agent is **not** started, since it would bypass the policy.

An API key (`VISDOM_API_KEY`) is only ever sent to the tracing server; the gateway and Control
Plane need a Visdom account login (`visdom login`).

#### The agent started without `visdom` — `visdom env`

`visdom` is the way to start an agent through the gateway. For the times it is started some
other way — by an editor, a script, out of habit — `visdom env` prints the shell commands that
send it through the gateway too:

```sh
source <(visdom env)                 # Claude Code; `--agent codex` for Codex
claude                               # now goes through the gateway
```

- The key is **not** in the output: the script only names the command that gets it
  (`visdom gateway-key …`), so it can be kept in a shell profile or an `.envrc` and shown around.
  The key is read when the script is sourced and lasts 24 hours at most: source it again then.
- For Claude Code it sets `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, the attribution headers
  and the model alias, and unsets `ANTHROPIC_API_KEY` and the provider switches. For Codex, whose
  provider is given as options, it defines a `codex` shell function that adds them (so it works
  for `codex …` typed in that shell, not for a program that runs the `codex` executable itself).
- It sets `VISDOM_GATEWAY_SESSION` to the agent's name, which switches that agent's capture
  hooks off when it is started from that shell: the gateway records it. Another agent started
  there is not routed by this script, and is captured by its hooks as usual. (`visdom` 1.0.0 set
  the variable to `1` for whichever agent it started; `tracevault` still takes that.)
- The repository in the attribution headers is the one `visdom env` was run in; run it per
  repository (an `.envrc` does that).
- It is weaker than `visdom`: `env` in the agent's own settings files still wins over the
  environment, and nothing checks the gateway again before each start. Where the workspace keeps
  sessions off the gateway (`direct`), or the gateway is not ready (no model alias to ask for
  included), it prints nothing and fails.

Settings:

- The agent: `--agent`, then `VISDOM_AGENT`, then `agent = "…"` in the repository's
  `.visdom/config.toml`, then in the user config, then `claude-code`. Supported: `claude-code`,
  `codex`.
- User config, `visdom/config.toml` in the user config directory (`~/.config` on Linux):

  ```toml
  agent = "codex"
  bazaar_url = "https://context.example.com/mcp/remote"   # until the server publishes it

  tracing_mode = "gateway"                                # or "hooks" (the default)
  agent_instructions = false                              # start agents without visdom's text

  [agents]                 # run another executable for an agent
  claude_code = "/opt/claude/bin/claude"

  [gateway]                # only for tracing_mode = "gateway"; all optional
  project = "payments"                 # the gateway project, when several are open to you
  claude_code_model = "claude-sonnet"  # model aliases to ask the gateway for
  codex_model = "gpt-general"
  url = "https://ai-gateway.example.com"   # the console, until the server publishes it
  ```
- A repository may name its own in `.visdom/config.toml`: `agent`, `tracing_mode`, and
  `[gateway] project`.
- The gateway project: `VISDOM_GATEWAY_PROJECT`, then `[gateway] project` in the repository's
  config, then in the user config, then the only project open to you.
- The model alias: `VISDOM_GATEWAY_MODEL`, then `[gateway] claude_code_model` / `codex_model` in
  the user config, then the first alias the key may call that is named for the agent.
- The bazaar URL: `VISDOM_BAZAAR_URL`, then `bazaar_url` above, then the tracing server's
  `public-config` (`bazaar_mcp_url`, when it publishes one).
- Control Plane and the gateway console: `VISDOM_CONTROL_PLANE_URL` / `VISDOM_GATEWAY_URL`, then
  `control_plane_url` / `[gateway] url` in the user config, then the tracing server's
  `public-config` (`control_plane_url`, `ai_gateway_url`).
- The tracing variables are `VISDOM_SERVER_URL`, `VISDOM_API_KEY`, `VISDOM_SESSION_ID`,
  `VISDOM_NO_BROWSER`, `VISDOM_PROJECT` and `VISDOM_PROJECT_ATTRIBUTION`. `visdom` also takes
  the `TRACEVAULT_*` spelling of each, which an earlier release used; the `VISDOM_*` one wins.

`tracevault` keeps working during the transition and both binaries share one login
(`visdom/credentials.json`). Run interactively, `tracevault` prints a one-line deprecation note
on stderr.

### Which command the hooks call

What `init` installs for an agent or for git to run later — the Claude Code and Codex hooks,
the git `pre-push` and `post-commit` hooks, the Amp, OpenCode and GSD plugins, the Claude Code
telemetry helper and the workspace-mode block in `CLAUDE.md` / `AGENTS.md` — names the binary
that installed it: `visdom init` (and `visdom trace init`) installs `visdom trace …` commands,
the deprecated `tracevault init` installs `tracevault …` ones. A machine set up with `visdom` therefore needs only `visdom` on `PATH`.

`visdom init` also takes over what `tracevault init` installed in the same place: it rewrites
the `tracevault …` commands of ours to `visdom trace …` (in both Claude Code settings files of
a repository, and in our blocks of the git hooks) and leaves your own hooks alone. It goes one
way only: `tracevault init`, and the automatic refresh of the hooks after an upgrade, keep
commands that already call `visdom`. Each place is taken over by the `init` that covers it —
`visdom init` in a repository does not touch the machine-wide hooks; `visdom init --global`
does.

### Files from before the `visdom` names

Both binaries keep their files under `.visdom/` (in a repository or workspace folder) and
`visdom/` (in your config and state directories). Releases up to `tracevault` 0.29.6 /
`visdom` 1.0.0 used `.tracevault/` and `tracevault/`, and the tracing config was called
`config.toml` there. Those files are still read: each one is looked for under the new name
first, then under the old one, so an existing setup keeps working as it is. New files are
always written under the new names, with two exceptions that keep a file where it is — a
refreshed login token goes back into the file the login came from, and a session an earlier
release started keeps its directory until it ends.

`visdom init` (or `visdom trace init`) moves what is left over and says what it moved: the
login, the user-level files, the checkout's tracing config and context, and every session that
has been pushed. A session not pushed yet stays in `.tracevault/sessions/` and is still read
there. The user context file is copied rather than moved, because a repository's config may
name the old file by its path. `login` writes the new file and removes the old login; `logout` removes both.

Upgrade both binaries together: a `tracevault` from before this change does not read the new
directories, so after a new `login` or `init` its hooks would find no login and no `repo_id`.

## Crates

- **`visdom-cli`** — the `visdom` command-line tool.
- **`tracevault-cli`** — the `tracevault` command-line tool (hooks, session capture, verification).
- **`visdom-ai-tracing-protocol`** — the wire-protocol types (stream + hook events) shared between the
  CLI and the Visdom Trace server. Published so both sides depend on one source of truth.

## Install

```bash
brew install VirtusLab/visdom/visdom
```

(`brew install VirtusLab/visdom/tracevault` for the `tracevault` binary; the tap was
`VirtusLab/visdom-ai-tracing` before, and that name still resolves.)

or from crates.io:

```bash
cargo install tracevault-cli visdom-cli
```

`visdom-cli` provides `visdom` and is enough on its own: the hooks `visdom init` sets up call
`visdom trace …`. Install `tracevault-cli` as well only while something still calls `tracevault`.

## Quick start

There are two ways to set up capture. Start with the per-repo setup unless you know you need
the machine-wide one.

### One repository

Run these from the root of the repository you want traced — the primary checkout (not a
linked worktree), with an `origin` remote:

```sh
visdom login --server-url https://your-tracevault-server.example.com   # once per machine
visdom trace init                                                            # once per repo
```

Then start a new Claude Code session in the repo. Its sessions are captured and streamed to
the server as they happen.

**Log in before `init`.** `init` registers the repo with the server and writes the repo's
server id (`repo_id`) into `.visdom/tracing.toml`; the hooks send events under that id, and
without it — and without any of the workspace-mode bindings described
[below](#every-session-on-the-machine----global) — they send nothing. So per-repo `init` refuses
to run without credentials (a login, or an API key in `VISDOM_API_KEY`): it exits 1 before
installing anything and prints the commands to run instead:

```sh
visdom login --server-url https://your-tracevault-server.example.com
visdom trace init
```

(With `--server-url`, or `VISDOM_SERVER_URL` set, the printed `login` command carries that
URL.) A repo set up by an older CLI, which installed the hooks without a login and exited 0, has
hooks but no `repo_id`: log in and run `visdom trace sync` in the repo — it registers the repo and
writes the missing `repo_id`. Events from before that were not recorded and cannot be recovered.

### Every session on the machine — `--global`

```sh
visdom login --server-url https://your-tracevault-server.example.com
visdom trace init --global
```

This installs the hooks once, into `~/.claude/settings.json`, so they run in every Claude Code
session on the machine. **A session started in a checkout of a repo the server knows is
recorded and attributed to that repo with nothing more to do:** on its first hook (normally
`SessionStart`) the CLI takes the checkout's `origin` remote, asks the server which repo it
belongs to — once per session — and binds the session to it exactly as `visdom trace repo
switch` would. The repo must already be registered, by someone's `visdom trace init` or
`visdom trace sync` in a checkout of it. If it is not, the session stays unrecorded and Claude
Code shows a message saying so, once; run `visdom trace sync` in the checkout to register it,
then `visdom trace repo switch <path>` to bind the running session. A remote whose codebase has
several repo records on the server binds the one the checkout's `.visdom/tracing.toml` names
or the one named like the checkout's directory; with neither, it stays unbound and the message
lists the records to pick from with `repo switch --repo-id`.

That automatic binding applies only to a session nothing else binds. In order of precedence,
a session is bound to a repo by:

1. **`visdom trace repo switch <path>` inside the session** — also how a session moves to work in
   another repo. `init --global` adds a note to `~/.claude/CLAUDE.md` telling the agent to run
   it when it does. The command reads the session id from `$VISDOM_SESSION_ID`, which the
   session-start hook exports into Claude Code's shell (through Claude Code's
   `CLAUDE_ENV_FILE`; other agents do not get it, so pass `--session-id <id>` there). Run
   without a session id, it sets the user-level default (point 3) instead. The automatic
   binding is stored the same way, so a later `repo switch` replaces it, and `repo reset`
   clears it for good (the session is not bound automatically again).
2. **The repo has its own `visdom trace init`** (a `.visdom/tracing.toml` with a `repo_id`):
   sessions started in it are recorded as with the per-repo setup.
3. **A user-level default: `visdom trace repo switch --user <path>`.** Every session not bound
   by points 1–2 is then attributed to that one repo, whatever directory it runs in — the
   automatic binding never applies while a user-level default is set.
4. **The automatic binding** from the session's checkout, described above.

A session with no repo is still recorded, repo-less, when it resolves to a project:
`VISDOM_PROJECT` set to a project UUID, a `visdom trace project switch` in the session, a
[workspace directory](#a-folder-of-checkouts--init---workspace) it was started under, or a
user-level `visdom trace project switch --user` default (which then catches every unbound session
on the machine) — see [Project attribution](#project-attribution--visdom_project-and-visdom_project_attribution).
A session that resolves to neither — one started outside any checkout, or in one whose remote
the server does not know — records nothing. If something else regenerates
`~/.claude/CLAUDE.md`, the note from point 1 is lost; re-run `visdom trace init --global` to
restore it (it is idempotent). Without it, sessions are still bound automatically to the repo
they start in, but the agent is not told to `repo switch` when it moves to another repo.

### A folder of checkouts — `init --workspace`

```sh
cd ~/src/my-platform            # an umbrella directory, not itself a git repo
visdom trace init --workspace --project my-platform
```

Every session started in that directory, or in a non-git directory below it, is then recorded
to project `my-platform` with no per-session step — without `--user`, so sessions elsewhere on
the machine are not affected. `init --workspace` works outside git, creates `.visdom/`
there if needed, looks the project name up on the server **once** (an unknown or ambiguous name
is an error and writes nothing), and writes its id to `.visdom/tracing.toml` as `project_id`
(with `project_name` for display). It binds no repo and installs no hooks: pair it with
`init --global`. A session's own `project switch` still overrides it. A session started inside
a git checkout below it is not covered: it follows that checkout (its own `visdom trace init`, or
the automatic binding to its repo, whose project the server deduces). Running `visdom trace repo
switch <path>` when an agent started in the workspace moves into a checkout is optional: it adds
the repo, but the session is recorded to the workspace project either way.

### Check the result

The definitive check is end to end: start a Claude Code session in the repo, have it do
something (any tool call), and look for the session in the dashboard under **Traces →
Sessions**. The checks below are quicker but only show that the setup is right, not that
events arrived:

- In a per-repo setup, `.visdom/tracing.toml` contains a `repo_id = "…"` line. Without it,
  sessions in the repo are captured only if bound some other way (workspace mode, a user-level
  default, a project) — and then not necessarily to this repo.
- `visdom trace status` shows `✓ Logged in`, `✓ Recording` and `✓ Repo registered on server`.
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

### `visdom login` — sign in (humans)

`visdom login --server-url <URL>` signs you in through your organisation's Keycloak using
the OAuth 2.0 **device authorization grant** (RFC 8628) — the same flow as `gh auth login` or
a smart TV. The CLI asks the server which realm it trusts
(`GET /api/v1/auth/public-config`) and then talks to Keycloak directly; your password never
touches Visdom.

```sh
visdom login --server-url https://your-tracevault-server.example.com
```

It prints a verification URL and a one-time code, tries to open your browser, and waits while
you approve. It skips the browser with `--no-browser`, or when `VISDOM_NO_BROWSER` or `CI`
/ `GITHUB_ACTIONS` is set, inside a container (`/.dockerenv`), or — on Linux and other
non-macOS Unix — when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set. On success it writes
`credentials.json` (mode `0600`) into the Visdom config directory — `~/.config/visdom/`
on Linux (`$XDG_CONFIG_HOME/visdom/` if set), `~/Library/Application Support/visdom/`
on macOS — holding a short-lived access token plus a long-lived `offline_access` **refresh
token**, and prints the account and role the server resolved for you.

That refresh token is what makes unattended use work: every command refreshes the access token
by itself shortly before it expires, so git hooks and background captures never stop to
prompt. Nothing has to be re-run periodically.

`visdom logout` revokes the refresh token at Keycloak (best-effort — the local file is
removed either way) and deletes the credentials file.

If sign-in succeeds but the server answers "not authorized", the account has no `tracevault`
grant in Control Plane. The credentials are still saved, but `login` exits with status 1. An
administrator has to grant `tracevault` plus the project roles you need (`tracevault.viewer` /
`tracevault.operator`) in Control Plane, or the `ai-tracing-admin` Keycloak realm role. No new
login is needed afterwards.

**CI and automation do not use `visdom login`.** They keep using a long-lived API key,
which never expires and needs no browser:

```sh
export VISDOM_SERVER_URL=https://your-tracevault-server.example.com
export VISDOM_API_KEY=tvk_...
```

Each setting is taken from the first source that has it: the environment
(`VISDOM_SERVER_URL`, `VISDOM_API_KEY`), then the credentials file, then `server_url`
/ `api_key` in the repo's `.visdom/tracing.toml`. So a key set in the environment always
wins on a machine that also has an interactive login. A server deployed without Keycloak
supports only this API-key path, and `visdom login` says exactly that instead of starting
a flow that cannot complete.

### Project attribution — `VISDOM_PROJECT` and `VISDOM_PROJECT_ATTRIBUTION`

Two environment variables control which project captured events are attributed to, and who
owns that decision. They are separate on purpose: one says *which* project, the other says
*who vouches for it*.

```sh
# WHICH project: a launcher asserting attribution for everything in this process.
export VISDOM_PROJECT=22222222-2222-4222-8222-222222222222

# WHO owns the decision: this caller does, so do not check repo/project membership.
export VISDOM_PROJECT_ATTRIBUTION=explicit
```

**`VISDOM_PROJECT`** is one tier of the project chain the capture hook uses, which is, from
highest precedence: a subagent's per-worktree override → `VISDOM_PROJECT` → the session's
`project switch` binding → the workspace `.visdom/tracing.toml` `project_id` (written by
[`init --workspace`](#a-folder-of-checkouts--init---workspace)) → the user-level default
(`project switch --user`). When none applies, the hook sends no project and the server derives
it from the repo. A config file baked into a pod image therefore cannot outrank the launcher.
The hook reads the config's `project_id` (a UUID) and never a project name: the legacy
`default_project` key (a name) is not used for capture. `visdom trace init` and `visdom trace sync`
migrate it — they look the name up once and replace it with `project_id` + `project_name`, or
fail naming it if it is unknown or ambiguous — and until then `status` / `project status` report
it as unmigrated.

> **Only the UUID form is honoured.** Resolving a name needs a `list_projects` round trip,
> and the capture hook runs per event in a short-lived process, so it never makes one: a
> name is ignored and attribution falls through to the next tier. `visdom trace project status`
> reports exactly what the capture path does, so it does not resolve a name either — it
> warns that the value is unused. `visdom trace status` does resolve it, for display only, and
> flags the tier as one the wire ignores. **Export the UUID.**

**`VISDOM_PROJECT_ATTRIBUTION=explicit`** declares that the caller owns the attribution,
so the server stamps the named project without checking that the repo belongs to it. Any
other value (or none) means `derived`, today's always-checked behaviour. It is process-wide
and **never expires** — it is re-asserted at every launch, which is the point for a pod
launcher.

The persisted equivalent is a flag on a switch:

```sh
visdom trace project switch payments --project-attribution explicit
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

Use `visdom trace project status` to see which tier won, which mode is in effect, and — when
the binding carries a persisted force — whether that force is still live. Its `--project`
flag is a what-if for that display only; nothing on the capture path takes one. `visdom trace
status` answers a narrower question as part of the full diagnostic: whether the tier that
won is one the capture path actually honours. It does not report the attribution mode or
the force.

### `visdom trace init` — set up tracing in a repo

`visdom trace init` wires Visdom into a repository: it installs the AI-agent hooks that
capture sessions, adds git hooks (a pre-push policy check and a post-commit metadata push),
creates `.visdom/`, and — when the repo has an `origin` remote — registers the repo with
the server and stores its `repo_id` in `.visdom/tracing.toml`. It needs a login (or
`VISDOM_API_KEY`) and fails with exit code 1, installing nothing, without one (see
[Quick start](#one-repository)). Every `init` error exits 1. Run it once per repo, from the
root of the primary checkout (not a linked worktree).

If the server already has a repo record for the checkout's remote, `init` binds to it instead of
registering a second one (a second record would split sessions between the two). When the
remote has several records and none is named like the checkout, `init` registers nothing and
lists them; set `repo_id` in `.visdom/tracing.toml` to the right one. `visdom trace sync`
follows the same rule.

Re-running `init` is safe: it keeps the existing `.visdom/tracing.toml` (`repo_id`,
`user_context`, a pinned `server_url` / `api_key` / `project_id`, and keys it does not
know) and changes only what a flag on that run sets (`--server-url`, `--user-context`,
`--no-user-context`). A legacy `default_project` name is migrated to `project_id` (see
[Project attribution](#project-attribution--visdom_project-and-visdom_project_attribution));
an unknown or ambiguous name stops `init` before it writes anything. If that file cannot be
parsed, `init` stops before writing anything; fix or remove it and run `init` again.

**Choosing the agent — `--agent`**

Visdom captures sessions from more than one coding agent. `--agent` selects which one to
install hooks for (default `claude-code`):

| Command | Installs | For |
|---|---|---|
| `visdom trace init` | `.claude/settings.json` (or `.claude/settings.local.json`) hooks | Claude Code (default) |
| `visdom trace init --agent codex` | `.codex/hooks.json` hooks | OpenAI Codex CLI |
| `visdom trace init --agent gsd` (alias `gsd2`) | the Visdom extension, written to `gsd-extension/` in the Visdom config directory and registered for this repo with `gsd install -l` | GSD (pi) |
| `visdom trace init --agent opencode` | `.opencode/plugins/visdom.ts` plugin | OpenCode |
| `visdom trace init --agent amp` | `.amp/plugins/visdom/index.ts` plugin | Amp ([ampcode.com](https://ampcode.com)); see **Amp** below |

All of them wire the same capture pipeline: the agent's hooks invoke `visdom trace …`, which
streams the session (transcript, tokens, cost, file changes) to the server tagged with the
agent, so sessions from different agents show up side by side, each with its own badge. Codex
file changes come from the `apply_patch` tool event on Codex >= 0.153 (which carries the patch
in its hook payload) and from the session rollout on older versions.

`--claude-settings shared|local` chooses between `.claude/settings.json` and
`.claude/settings.local.json`. Without it, `init` asks when run in a terminal and otherwise
uses `shared`. It applies only to `--agent claude-code` and is rejected with any other agent.
Hooks you already have in that file are kept; see [What `init` installs](#what-init-installs).

Unless you pass `--no-gitignore`, `init` adds the agent's hook file (for
Claude Code, whichever settings file you chose — `settings.json` included) to the root
`.gitignore`. `.visdom/` ignores its own contents through the `.gitignore` inside it — all but
the `config.toml` a team may commit for the `visdom` launcher — so nothing needs to be committed and every contributor runs `visdom trace init`
for themselves. Pass `--no-gitignore` if you want to commit the hook file.

`--server-url <URL>` stores a server URL in `.visdom/tracing.toml`; it is only needed when
the URL does not come from `visdom login` or `VISDOM_SERVER_URL`.

**Amp**

Amp has no shell hooks, only plugins, so `init --agent amp` installs a plugin
(`.amp/plugins/visdom/index.ts`; with `--global`, `$XDG_CONFIG_HOME/amp/plugins/` or
`~/.config/amp/plugins/`). It runs `visdom trace stream --agent amp` for each prompt and each tool
result as they happen, and at the end of each turn for what Amp reports only then: the
assistant's messages and token usage, and tools that fire no tool events (`Task`,
`create_thread`, `wait_for_threads`, `read_thread`). Each tool call is sent once. Only
`.amp/plugins/visdom/` is added to `.gitignore`: the rest of `.amp/` is Amp's own project
configuration. If `visdom` is missing or fails, the plugin logs it and Amp carries on.

- **Headless runs need `--plugin-ready-timeout`.** Without it `amp -x` may start the turn before
  plugins are ready and skip their `agent.start` / `agent.end` hooks, so the prompt, the
  assistant's messages and the token usage are not captured:
  `amp -x "<prompt>" --plugin-ready-timeout 30` (the prompt is the value of `-x`, so it comes
  right after it).
- **`amp -x` waits forever on an approval dialog**, for example when a tool writes under a
  guarded path. Allow what an unattended run needs beforehand.
- **Policies name Amp's own tools.** A `visdom trace check` policy matches tool names exactly, and
  Amp's (in its default `medium` mode) are `create_file`, `edit_file`, `shell_command`, `Task` and
  `create_thread` (also `wait_for_threads`, `read_thread`), not Claude Code's `Write`, `Edit` or
  `Bash`. Amp has no read tool: the model reads files with `shell_command`. A tool counts as
  failed only when Amp reports `status: "error"`: a command with a non-zero exit code, and a
  command rejected by a permission rule, both succeed as far as Amp is concerned.
- **A session never ends explicitly.** Amp has no session-end event, and a terminal that is
  killed mid-turn sends no end of turn either; what was sent before stays.
- **Agent policies are injected at the start of a thread** (its first turn in an Amp process,
  through `visdom trace session-start`), and again on a later turn only when the session's repo
  changed (`visdom trace user-prompt`), as with Claude Code. Amp hides the text in its UI, but
  **unlike Claude Code it stores it in the thread**, as part of your message: anyone who opens
  the thread on ampcode.com, and `amp threads export`, see the policy text. A `Task` subagent
  runs inside its parent's turn and gets no injection of its own. If `visdom` is missing,
  fails or takes more than 10 s, the turn starts without policies.
- **The session id in Amp's shell is the thread id.** The plugin cannot export
  `VISDOM_SESSION_ID` into Amp's shell, so `visdom trace repo switch`, `repo status`,
  `project switch`, `status` and `verify-start` fall back to `$AMP_CURRENT_THREAD_ID`, which Amp
  sets there. No `--session-id` is needed.
- A `Task` subagent's tool calls are recorded under the parent session without a link to the
  `Task` call.

**Codex hook trust — required once, or nothing is captured**

Codex >= 0.153 gates hooks behind persisted *hook trust*. Until it is granted, a
non-interactive `codex exec` skips the hooks **silently**: `hooks.json` looks installed, and
no session ever reaches the server. Neither `codex doctor` nor `codex features` reports the
trust state, so there is nothing to check after the fact.

After `visdom trace init --agent codex` (or `--global --agent codex`), run `codex` once
interactively in a repo and approve the hook-trust prompt. Trust then persists for
`codex exec` runs too. For unattended use (CI images), pass
`--dangerously-bypass-hook-trust` instead — it runs the hooks without a persisted trust
record, which is appropriate when the hook source is your own installed CLI.

**Global install — `--global`**

`--global` installs hooks once for every session on the machine instead of per-repo, paired
with workspace mode — see [Quick start](#every-session-on-the-machine----global) for how
sessions get bound to a repo:

```sh
visdom trace init --global                    # ~/.claude/settings.json + ~/.claude/CLAUDE.md
visdom trace init --global --agent codex      # ~/.codex/hooks.json  + ~/.codex/AGENTS.md
visdom trace init --global --agent gsd        # GSD extension for all GSD sessions
visdom trace init --global --agent opencode   # <config dir>/opencode/plugins/visdom.ts
visdom trace init --global --agent amp        # $XDG_CONFIG_HOME/amp/plugins/visdom/index.ts
```

`<config dir>` is the platform config directory: `~/.config` on Linux (`$XDG_CONFIG_HOME` if
set), `~/Library/Application Support` on macOS. Amp reads `$XDG_CONFIG_HOME/amp/`, or
`~/.config/amp/` when that is unset, on every platform, and the Amp plugin goes there. It does not need a git repository. It also writes the user-level `tracing.toml` in the
Visdom config directory, which enables the user context layer (see below) for repos that
do not configure it themselves; `--no-user-context` and `--user-context <path>` apply to it.
`--global` cannot be combined with `--server-url`, `--claude-settings` or `--no-gitignore`.

**Claude Code's own cost — telemetry, on by default**

Claude Code reports the cost of every API call it makes, but only over OpenTelemetry. `init`
(per-repo or with `--global`) sends those reports to the Visdom server, which then shows
Claude Code's own cost instead of an estimate. This is on by default: an interactive `init` asks
(`[Y/n]`), a non-interactive one turns it on. `--claude-telemetry` turns it on without asking,
`--no-claude-telemetry` turns it off:

```sh
visdom trace init --global                        # telemetry on
visdom trace init --global --no-claude-telemetry  # telemetry off, and it stays off
visdom trace telemetry off                        # the same, without re-running init
visdom trace telemetry on | status
```

**Turning it off** (`--no-claude-telemetry`, `visdom trace telemetry off`, or no at the prompt)
removes only what Visdom wrote: `otelHeadersHelper` when it is Visdom's, and the `env`
entries below while they still hold Visdom's values (`CLAUDE_CODE_ENABLE_TELEMETRY` and
`OTEL_LOGS_EXPORTER` stay when another exporter still uses them). The choice is recorded in
`~/.config/visdom/install-state.json`, so neither a later `init` nor a CLI upgrade turns it
back on; only `--claude-telemetry` or `visdom trace telemetry on` does.

**After a CLI upgrade**, the first Claude Code session that starts (its `SessionStart` hook,
`visdom trace session-start`) brings an existing install up to date, once per version:

- it re-merges Visdom's hooks into the settings file(s) of that session's repo, and into
  `~/.claude/settings.json`, that already hold them, so hooks a newer CLI adds reach existing
  installs (the files are found by looking for our commands in `.claude/settings.json`
  and `.claude/settings.local.json`, and remembered). Repos you never open again keep their old
  hooks; re-run `visdom trace init` there;
- it turns telemetry on, unless you turned it off, another exporter is set up (below), or Claude
  Code is set to export content (`OTEL_LOG_USER_PROMPTS` and friends, below). It then shows,
  once: *AI Tracing: Claude Code cost telemetry is now on for new sessions (`visdom trace
  telemetry off` to disable)*, and `visdom trace status` says it was turned on automatically.
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
  "otelHeadersHelper": "visdom trace otel-headers --server-url https://<server>"
}
```

- No key or token is written to `settings.json`. Claude Code runs `otelHeadersHelper`, and
  `visdom trace otel-headers` prints `{"Authorization": "Bearer …"}` from your login (or
  `VISDOM_API_KEY`). It refuses, with an error, when you are not logged in or your login is
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
  OTLP endpoint other than Visdom's, static OTLP headers, a non-HTTP OTLP protocol (from
  managed settings or the shell), a `BETA_TRACING_ENDPOINT`
  (detailed beta tracing sends logs there instead), or its own `otelHeadersHelper`,
  `init` changes nothing and prints the collector route below instead. Claude Code would send
  the helper's token to every exporter it runs, so the two are never mixed. Managed settings
  delivered only by an MDM profile or the Windows registry are not visible to `init`; if your
  organisation configures Claude Code telemetry that way, use the collector route.
- The server must run the OTLP receiver (`POST /otlp/v1/logs`, Visdom server with
  VIS-1079) before you enable this; until then the exports are rejected and Claude Code shows
  an export warning.

**What leaves your machine.** Claude Code exports its OpenTelemetry *log events* for every
session on this machine, in any directory, to the Visdom server: `api_request` (model,
`cost_usd`, token counts, duration, request ids), and also `user_prompt`,
`assistant_response`, `tool_result`, `tool_decision`, `api_error`, `hook_registered`,
`plugin_loaded`, `mcp_server_connection` and similar events, with session and user attributes
(names of hooks, plugins and MCP servers, not their content). With the settings above, **no
prompt text, response text, tool input or tool output is sent**: Claude Code redacts them
unless `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_DETAILS`,
`OTEL_LOG_TOOL_CONTENT` or `OTEL_LOG_RAW_API_BODIES` is set (all off by default; `user_prompt`
then carries only `prompt_length`, `assistant_response` only `<REDACTED>`). Visdom never
sets them, and neither a plain `init` nor the upgrade step turns telemetry on while any of them
is set (`--claude-telemetry` or `visdom trace telemetry on` still does). Only
`OTEL_LOGS_EXPORTER` is configured, so no metrics or traces are exported. The server reads only
an allowlist of `api_request` fields and rejects events for sessions it does not know.
(Checked against Claude Code's
[monitoring documentation](https://code.claude.com/docs/en/monitoring-usage) and by capturing
the export of Claude Code 2.1.312 with exactly these settings: a marker in the prompt did not
appear in it, 2026-10-29.)

`visdom trace status` shows whether telemetry is configured, under *Installation*, and whether you
turned it off or an upgrade turned it on; `visdom trace telemetry status` prints just that line.

*Collector route.* If your organisation already collects Claude Code telemetry, forward the
`api_request` events from your OpenTelemetry Collector instead, authenticated with a Visdom
API key:

```yaml
processors:
  filter/visdom:
    error_mode: ignore
    logs:
      log_record:
        - 'attributes["event.name"] != "api_request"'
exporters:
  otlphttp/visdom:
    endpoint: https://<server>/otlp
    headers:
      Authorization: "Bearer ${env:VISDOM_API_KEY}"
service:
  pipelines:
    logs/visdom:
      receivers: [otlp]
      processors: [filter/visdom]
      exporters: [otlphttp/visdom]
```

### `visdom trace context` — tagging events with flow and metadata

`visdom trace context` manages context (flow ID, labels, params) that the capture hook stamps on
every captured event, whichever agent produced it. This drives grouping and filtering in the
Flows view and the analytics UI.

**Three layers, low → high precedence**

```
user  →  repo (global)  →  worktree
```

- **User** — optional, cross-repo, opt-in. Lives outside the repo (by default `context.json`
  in the Visdom config directory, e.g. `~/.config/visdom/context.json` on Linux), so
  it follows *you* across every project instead of being scoped to one repo.
- **Repo (global)** — `.visdom/context.json`, shared by all worktrees of the repo.
- **Worktree** — `.visdom/worktrees/<key>/context.json`, present only in a linked git
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
`.visdom/tracing.toml`. If the repo config does not set it, the user-level `tracing.toml`
(written by `visdom trace init --global`) decides; if neither sets it, the layer is off.
`visdom trace init` writes `user_context = true` into the repo config when it has none yet
(`--no-user-context` to write `false`, `--user-context <path>` to point it at an explicit file
up front); a re-run without either flag keeps the value already there. The field accepts four forms:

| `config.toml` | Meaning |
|---|---|
| `user_context = false` | disabled — no user layer is consulted, whatever the user-level config says |
| `user_context = true` | enabled, reading `context.json` in the Visdom config directory |
| `user_context = "/custom/path.json"` | enabled, reading from that file |
| `[user_context]` with `enable` / `path = "..."` | `enable` defaults to `true`; `enable = false` disables the layer but keeps the path in the file |

Change the repo's setting after the fact with `visdom trace context source` (one of `--enable`,
`--disable`, `--path <file>`, or `--default` is required). It edits only the current repo's
`.visdom/tracing.toml` and fails if the repo has none. `--enable` and `--default` both write
`user_context = true`, so they use the default path and discard a remembered one:

```sh
visdom trace context source --enable                 # turn on at the default path
visdom trace context source --path ~/team-ctx.json    # turn on, reading a custom file
visdom trace context source --default                 # same as --enable
visdom trace context source --disable                 # turn off
```

To set the user layer up for every repo at once, use `visdom trace init --global`
(optionally with `--user-context <path>`), which writes the user-level config.

**Editing each layer**

`context set` / `context update` / `context clear` operate on the repo/worktree file by
scope (the per-worktree file by default in a linked worktree, `--global` to force the
repo-wide file). Pass `--user` on any of them to target the resolved user-context file
instead, regardless of worktree scope:

```sh
visdom trace context set --user --flow personal-defaults --label solo-dev --param editor=nvim
visdom trace context update --user --remove-param editor
```

`visdom trace context show` prints the User section (always — its header says whether the layer
is enabled and which file it reads), Global, This worktree (linked worktrees only), plus an
Effective section that annotates each flow/label/param with the layer it resolved
from — useful for debugging why a value did or didn't win.

**Examples**

```sh
# Personal context, used by every repo that does not opt out
visdom trace context set --user --label solo-dev --param editor=nvim

# Per-repo
visdom trace context set --flow add-payment-retry --label payments --label backend
visdom trace context update --param env=staging --remove-label backend
visdom trace context show
visdom trace context clear
```

**Letting the agent tag its own work**

Flow IDs, labels and params have no fixed schema; each team decides what they mean. To have
Claude Code set context itself at the start of each task, add a rule like this to the repo's
`CLAUDE.md`:

```markdown
When you begin a new distinct task, run:
  visdom trace context set --flow <kebab-case-task-name> --label <area-you-are-touching>

Examples of areas: frontend, backend, auth, payments, infra.
When the task is complete, run: visdom trace context clear
```

Sessions in different linked worktrees each write their own worktree file, so parallel
sessions do not overwrite each other's context.

## Command reference

Every tracing command and flag (`tracevault` 0.29, run as `visdom trace <command>`). `visdom trace <command> --help` prints the same from
the binary you have installed.

### Setup

**`visdom login --server-url <URL> [--no-browser]`** — sign in through Keycloak; see
[`visdom login`](#visdom-login--sign-in-humans). `--server-url` is required.

| Flag | Meaning |
|---|---|
| `--server-url <URL>` | The Visdom server to sign in to (required) |
| `--no-browser` | Only print the verification URL; also implied in CI, containers and Linux sessions without a display |

**`visdom logout`** — revoke the refresh token at Keycloak (best-effort) and delete the
credentials file.

**`visdom trace init [OPTIONS]`** — set up capture in the current directory, which must be the
root of a git repository's primary checkout; see
[`visdom trace init`](#visdom-trace-init--set-up-tracing-in-a-repo). With `--global`, set it up for
every session on the machine instead.

| Flag | Meaning |
|---|---|
| `--agent <claude-code\|codex\|gsd\|opencode\|amp>` | Which agent to install hooks for (default `claude-code`; `gsd2` is an alias of `gsd`) |
| `--claude-settings <shared\|local>` | Claude Code only: `.claude/settings.json` or `.claude/settings.local.json`. Omitted: asks in a terminal, otherwise `shared` |
| `--no-gitignore` | Do not add the hook file to the root `.gitignore` |
| `--server-url <URL>` | Register the repo with this server during this `init` (instead of the URL from the environment or your login), and store it in `.visdom/tracing.toml`, where later commands treat it as the lowest-precedence source (see [credentials](#visdom-login--sign-in-humans)) |
| `--user-context <path>` | Enable the user context layer, reading this file |
| `--no-user-context` | Disable the user context layer (conflicts with `--user-context`) |
| `--global` | Install machine-wide instead of in this repo; does not need git. Conflicts with `--server-url`, `--claude-settings`, `--no-gitignore` |
| `--workspace --project <name>` | Bind the current directory to a project for every session started under it (see [A folder of checkouts](#a-folder-of-checkouts--init---workspace)); does not need git, installs no hooks, writes no `repo_id`. Takes `--server-url`; conflicts with the other flags |
| `--claude-telemetry` | Claude Code only: send Claude Code's own per-request cost to Visdom over OpenTelemetry, configured in `~/.claude/settings.json` (see [telemetry](#visdom-trace-init--set-up-tracing-in-a-repo)), without asking. Neither flag: asks in a terminal (default yes), otherwise on, unless you turned it off before |
| `--no-claude-telemetry` | Claude Code only: turn that telemetry off (removes Visdom's entries) and remember it across upgrades. Conflicts with `--claude-telemetry` |

**`visdom trace telemetry on|off|status`** — turn Claude Code telemetry on (needs a login), off
(remembered across upgrades), or show its state, like `init --claude-telemetry` /
`--no-claude-telemetry` without re-running `init` (see
[telemetry](#visdom-trace-init--set-up-tracing-in-a-repo)).

**`visdom trace sync`** — register the repo with the server, or bind to its existing record, and
write `repo_id` into `.visdom/tracing.toml` if the repo has one. Needs a login (or API key)
and an `origin` remote. It first migrates a legacy `default_project` name in that config to
`project_id` — also in a directory without a remote — and exits 1 naming the value if it is
unknown or ambiguous. The pre-push hook runs it before every push.

**`visdom trace status [--session-id <id>]`** — diagnose the setup: authentication, global and
repo installation, git and agent hooks (Claude Code hooks in `.claude/settings.json`,
`.claude/settings.local.json` or `~/.claude/settings.json`), Claude Code telemetry
(on, off by your choice, or turned on by an upgrade), whether anything will record, the
session's repo/project binding, the repo on the server (by `repo_id`) and its server-side clone,
and pending events. Exits 1 if any check is an error (`✗`).
`--session-id` picks the session whose binding is checked; default `$VISDOM_SESSION_ID`,
else the most recent session with a workspace binding.

### Day to day

**`visdom trace check`** — evaluate the server's policies for the sessions behind the commits
being pushed; exits 1 when a blocking policy fails. The pre-push hook runs it (feeding it git's
pre-push input) and aborts the push on failure.

**`visdom trace flush`** — retry events that failed to stream in real time (network blip, server
down) from the local queue.

What a hook cannot send it keeps in the session's queue (`pending-*.jsonl` in the session
directory), and the next hook of the session sends the queue first. A transcript line is queued
once: the hook moves on past it in the transcript, so it is never read and queued again. The
queue holds at most 32 MB and a hook reads at most 4 MB of a transcript at a time; what does not
fit stays in the agent's transcript and is sent by later hooks, so neither the queue nor a hook's
memory grows with how long the server was unreachable. A queue written by 0.29.7 or earlier,
which repeated the unsent transcript on every hook, is reduced to its events the first time a
hook or `flush` of this release touches the session.

**`visdom trace stats`** — print local session statistics (sessions, events) for this repo.

**`visdom trace verify (--range <a..b> | --commits <sha,sha,...>)`** — check that commits are
registered and sealed on the server. `--range` is a git range expanded with `git rev-list`;
`--commits` is a comma-separated list. If both are given, `--range` is used. Exits 1 on
failure. The [GitHub Action](#github-action) runs it.

**`visdom trace verify-start [--session-id <id>]`** — open (or re-open) a *verification phase*
for the session: the agent declares it has finished changing code and is now running checks.
Only tool calls after this point are evaluated by policies scoped to the verification phase;
calling it again resets the phase. Without `--session-id` it uses `$VISDOM_SESSION_ID` if
that session has events in this worktree, else detects the session by worktree.

**`visdom trace agent-policies`** — print agent-readable instructions rendered from the policies
configured on the server for this repo, for an agent to read at session start (for example from
`CLAUDE.md`).

**`visdom trace proxy info`** — print the Visdom LLM proxy URL and setup instructions for AI
tools (Claude Code, GSD2, Cursor, Codex CLI, …).

### Context

`visdom trace context set|update|show|clear|source` — see
[`visdom trace context`](#visdom-trace-context--tagging-events-with-flow-and-metadata).

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
`--session-id <id>` (default `$VISDOM_SESSION_ID`).

| Command | Meaning |
|---|---|
| `repo switch <path> [--repo-id <uuid>] [--user]` | Bind the session to the registered repo that the checkout at `<path>` belongs to (resolved from its `origin` remote) and print the repo's policies. `--repo-id` picks one record when the codebase has several. `--user` (implied when there is no session id) writes the machine-wide default instead. `--name` is deprecated and always errors |
| `repo status [--path <path>]` | Show the session's repo binding; `--path` resolves a checkout once instead |
| `repo reset [--user]` | Clear **all** of the session's bindings — repo and project, including subagent overrides — and stop the hooks binding it automatically from its checkout again; or with `--user` only the machine-wide default repo |
| `project switch <name> [--user] [--project-attribution derived\|explicit]` | Bind the session (or, with `--user`, every session) to the project with this exact, case-sensitive name. `explicit` forces attribution; see [Project attribution](#project-attribution--visdom_project-and-visdom_project_attribution) |
| `project status [--project <name>]` | Show which project the session is attributed to and why; `--project` is a display-only what-if |

### Run by hooks, not by hand

These are hidden from `--help`. `init` installs them; you never need to run them.

| Command | Run by |
|---|---|
| `visdom trace stream --event <event> [--agent <agent>]` | The agent's hooks, on every captured event (`--agent` defaults to `claude-code`) |
| `visdom trace session-start` | The agent's `SessionStart` hook: binds an unbound session to the repo of its checkout's `origin` remote, injects the bound repo's policies; under Claude Code it also exports `VISDOM_SESSION_ID` into the agent's shell (Amp runs it from its plugin on a thread's first turn; its shell has `AMP_CURRENT_THREAD_ID` instead). Once per CLI upgrade it also refreshes the hooks and turns telemetry on (see [telemetry](#visdom-trace-init--set-up-tracing-in-a-repo)). It then sends the session cost earlier sessions left unsent (see [session cost](#what-init-installs)) |
| `visdom trace user-prompt` | The agent's `UserPromptSubmit` hook: re-injects the policies when the session's repo has changed |
| `visdom trace commit-push` | The git `post-commit` hook, in the background: sends the commit's metadata |
| `visdom trace otel-headers [--server-url <URL>]` | Claude Code's `otelHeadersHelper` (`init --claude-telemetry`): prints `{"Authorization": "Bearer …"}` for the OpenTelemetry export; exits 1 when not logged in, or logged in to another server than `--server-url` |

## What `init` installs

**Claude Code** (`.claude/settings.json` or `.claude/settings.local.json`; with `--global`,
`~/.claude/settings.json`).

Every hook install — per-repo or `--global`, Claude Code or Codex — updates the hook file in
place. Visdom's own hooks are the ones whose `command` is `visdom trace …` or `tracevault …` running `stream`, `session-start` or `user-prompt`; `init`
replaces exactly those with the current set and leaves everything else as it was: your other
hooks (including ones in the same matcher group as a Visdom hook) and every other setting.
Re-running `init` brings an older install up to date (matchers, timeouts, newly added events),
and a file that is already current is not rewritten. A hook file that is not valid JSON, or
whose `hooks` section has an unexpected shape, is left untouched and `init` fails with an error.

Having the hooks in more than one place — `settings.json` and `settings.local.json`, or a repo
and `--global` — does not stream events twice: Claude Code runs an identical hook command once
even when several settings files define it.

| Claude Code event | Matcher | Command |
|---|---|---|
| `PreToolUse` | `Write\|Edit\|Bash` | `visdom trace stream --event pre-tool-use` |
| `PostToolUse` | all tools | `visdom trace stream --event post-tool-use` |
| `Notification` | all | `visdom trace stream --event notification` |
| `Stop` | all | `visdom trace stream --event stop` |
| `SessionEnd` | all | `visdom trace stream --event session-end` |
| `SubagentStop` | all | `visdom trace stream --event subagent-stop` |
| `SessionStart` | all | `visdom trace session-start` |
| `UserPromptSubmit` | all | `visdom trace user-prompt` |

**Session cost.** At exit, after `Stop`, Claude Code appends its own figure for the session's
cost to the transcript (a `cost-state` line). `SessionEnd` sends the lines written since the last
hook, that one included, as transcript lines only: `Stop` already ended the session on the server.
If it could not (the hook failed or timed out, or the session ended before an upgrade installed
it), `session-start` sends it later: for each session of the repo whose unsent lines hold a
`cost-state`, it sends them as `SessionEnd` would have. It looks at sessions written in the last
30 days, at most 10 per session start, and stops when the server does not take them. It does
not run when `VISDOM_PROJECT` or `VISDOM_PROJECT_ATTRIBUTION` is set: they attribute what
the current process sends, and an earlier session may have run with other values. For the same
reason it sends a session only under the repo and project its own hooks sent under (recorded in
`.visdom/sessions/<session>/metadata.json`).

**Subagents.** Claude Code writes each subagent's conversation to its own file,
`<session>/subagents/agent-<id>.jsonl` next to the session transcript, and leaves the subagent's
messages and token usage out of the main transcript. Every `stream` hook therefore also sends what
each of those files gained since the last hook: under the parent session, one file per request
(never mixed with another file's lines or the main transcript's), each file with its own read
offset in `.visdom/sessions/<session>/.subagent_offsets/`. Subagents running in the background
are sent while they run, and `Stop` reads every file to its end. `SubagentStop` sends only those
files, which covers a background subagent that finishes after the session's last `Stop`. A batch
that cannot be delivered is queued like any other event and sent by a later hook or
`visdom trace flush`. This needs a server that keys transcript chunks by the line's `agentId`; an
older one cannot tell subagent lines from the main transcript's.

**Codex** (`.codex/hooks.json`; with `--global`, `~/.codex/hooks.json`): `SessionStart` and
`UserPromptSubmit` as above, plus `PostToolUse` and `Stop` streamed with `--agent codex`. There
is deliberately no `PreToolUse` hook for Codex.

**Amp** (`.amp/plugins/visdom/index.ts`; with `--global`,
`$XDG_CONFIG_HOME/amp/plugins/visdom/index.ts` or `~/.config/amp/plugins/visdom/index.ts`):
a plugin, rewritten in full by every `init`. See **Amp** under [`visdom trace init`](#visdom-trace-init--set-up-tracing-in-a-repo).

**Claude Code telemetry** (unless `--no-claude-telemetry`, a no at the prompt, or an earlier
opt-out; always `~/.claude/settings.json`): the `env` entries and `otelHeadersHelper` shown
under [telemetry](#visdom-trace-init--set-up-tracing-in-a-repo). The choice and the version that
last configured the machine go into `~/.config/visdom/install-state.json`.

**Git hooks** (per-repo `init` only; an existing hook keeps its content and gets a Visdom
block appended):

- `pre-push` — `visdom trace sync`, then `visdom trace check`; a failed check aborts the push.
- `post-commit` — `visdom trace commit-push`, in the background.

**Files**: `.visdom/` (with `tracing.toml`, `sessions/`, `cache/` and its own `.gitignore`),
and — unless `--no-gitignore` — an entry for the hook file in the root
`.gitignore`.

## Environment variables

| Variable | Effect |
|---|---|
| `VISDOM_SERVER_URL` | Server URL; wins over the credentials file and `.visdom/tracing.toml` |
| `VISDOM_API_KEY` | API key (`tvk_…`) for CI and automation; wins over an interactive login |
| `VISDOM_PROJECT` | Project UUID to attribute events to; see [Project attribution](#project-attribution--visdom_project-and-visdom_project_attribution) |
| `VISDOM_PROJECT_ATTRIBUTION` | `explicit` to force attribution to that project; anything else means `derived` |
| `VISDOM_SESSION_ID` | Default session for `status`, `verify-start`, `repo …` and `project …`; the `SessionStart` hook exports it into the agent's shell |
| `VISDOM_NO_BROWSER` | Any value: `login` does not open a browser |
| `XDG_STATE_HOME` | Base for per-session state (`$XDG_STATE_HOME/visdom/sessions`, default `~/.local/state/visdom/sessions`) |

The capture hook also reads the agent's own environment and auth files — for Claude Code
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CONFIG_DIR`; for Codex `CODEX_HOME`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` — only to
tell the server how the session is billed (gateway, API key or subscription). It never sends
their values.

## Files

All user-level files live in the Visdom config directory: `~/.config/visdom/` on Linux
(`$XDG_CONFIG_HOME/visdom/` if set), `~/Library/Application Support/visdom/` on macOS.

| File | Written by | Holds |
|---|---|---|
| `credentials.json` | `login` (mode `0600`) | Server URL, access and refresh token |
| `tracing.toml` | `init --global` | User-level tracing settings (the user context layer) |
| `config.toml` | `visdom init`, by hand | The `visdom` launcher's settings (agent, tracing mode, gateway) |
| `context.json` | `context … --user` | The user context layer (default location) |
| `default_repo.toml` | `repo switch --user` | Machine-wide default repo binding |
| `user_project.toml` | `project switch --user` | Machine-wide default project binding |
| `<repo>/.visdom/tracing.toml` | `init`, `sync`, `context source` | `repo_id`, `user_context`, the agent, and optionally `server_url` / `api_key` / `project_id` + `project_name` |
| `<dir>/.visdom/tracing.toml` | `init --workspace` | `project_id` + `project_name` for every session started under `<dir>` (no `repo_id`) |
| `<repo>/.visdom/context.json` | `context …` | The repo context layer |
| `<repo>/.visdom/worktrees/<key>/context.json` | `context …` in a linked worktree | The worktree context layer |

## GitHub Action

This repo ships a composite action that verifies commits in a PR or push have corresponding
traces sealed on the server. It installs the CLI, detects the commit range from the event,
runs `visdom trace verify --range`, and writes a pass/fail summary to the Actions step summary.

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

## Releasing

Releases are automated with [release-plz](https://release-plz.dev) by
`.github/workflows/release-plz.yml`, which runs on every push to `main`:

1. It opens (or updates) a release PR that bumps versions and the changelog.
2. Merging that release PR publishes the crates, creates the tags and GitHub releases, and, when
   `tracevault-cli` was released, starts `release-binaries.yml` for the binaries and the
   Homebrew formulae (`visdom` and `tracevault`, in the tap `VirtusLab/homebrew-visdom`; the
   workflow needs the `HOMEBREW_TAP_TOKEN` secret, a token that may write that repository's
   contents).

`tracevault-cli` and `visdom-ai-tracing-protocol` share the workspace version. `visdom-cli` has
its own (1.0 and on), set in `crates/visdom-cli/Cargo.toml`; its binary still ships in the
`tracevault-cli` release archives, so the archive and its download URLs carry `tracevault`'s
version while `visdom --version` and the `visdom` formula carry its own.

GitHub sometimes does not fire the `push` event after a squash merge. If a merge to `main`
produced no "Release-plz" run (check `gh run list --workflow release-plz.yml`), start one by
hand:

```sh
gh workflow run release-plz.yml --ref main
```

or use **Run workflow** on the workflow's page in the Actions tab. Runs are serialised, so a
manual run that overlaps an automatic one waits for it instead of colliding.

## License

Proprietary. The source is public, but the CLI is licensed only for use with a Visdom AI
Tracing deployment your organisation is authorised to use; see [LICENSE](LICENSE). Versions up
to 0.28.4 were released under Apache-2.0.
