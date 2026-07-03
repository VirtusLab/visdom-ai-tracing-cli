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
lower-precedence layer (`context show`'s per-value provenance shows exactly which layer a
value — or its absence — came from).

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
