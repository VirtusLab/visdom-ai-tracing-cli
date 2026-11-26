# Verifying the desktop surface by hand

`visdom` can open Claude Desktop instead of the terminal agent (`surface = "desktop"`). What the
app does with hooks, settings and its environment is the vendor's and can change between releases,
so these checks are run on a real machine before a release that touches the desktop surface. Each
needs a person at the app's window. Record the run at the bottom.

When looking at an agent process's environment, print variable **names**, or the named variables
below only. The environment holds the developer's sign-in token.

## Checks

1. **How the app starts Claude Code.** Open a Code session on a repository where `visdom init` was
   run, send one prompt, and while it runs find the app's Claude Code process:
   - Linux: `pgrep -af "Claude/claude-code"`; macOS: `pgrep -lf "Claude/claude-code"`.
   - Expected: the arguments contain `--setting-sources=user,project,local`.
   - Record `CLAUDE_CODE_ENTRYPOINT` and `PATH` of that process
     (Linux: `tr '\0' '\n' < /proc/<pid>/environ | grep -E '^(PATH|CLAUDE_CODE_ENTRYPOINT)='`;
     macOS: `ps eww -p <pid> | tr ' ' '\n' | grep -E '^(PATH|CLAUDE_CODE_ENTRYPOINT)='`).
   - Expected: `CLAUDE_CODE_ENTRYPOINT=claude-desktop`. Is the directory holding `visdom` on `PATH`?
   - Do this once with the app started from its icon (quit it fully first), and note that: an app
     started from a terminal has that terminal's `PATH`.
2. **The session is traced.** After the prompt: `.visdom/sessions/<session id>/` exists in the
   repository, and the session appears on the tracing server. Then repeat with the hooks only in
   the project's `.claude/settings.json` (move the user-level `visdom trace` hooks out of
   `~/.claude/settings.json` for the test, and put them back).
3. **Bazaar.** In the session, ask the agent which MCP servers it has. Expected: the bazaar server
   is connected.
4. **macOS and Windows: `claude --desktop`.** In a repository, in a terminal: `claude --desktop`.
   Does it start a new session in that directory, or only bring the app forward? Then
   `claude --desktop --continue`.
5. **Linux: the link.** With the app closed: `xdg-open "claude://code/new?path=<absolute dir>"`.
   Is that folder selected for the new session? Does `xdg-open` return while the app runs?
6. **`visdom` itself.** With `surface = "desktop"`: `visdom` opens the app and ends with one of its
   three closing lines; `visdom --mode gateway` refuses and names VIS-1396;
   `visdom doctor --surface desktop` reports the app, the hook path, and whether a desktop session
   was traced.

## Runs

### 2026-11-26 — Fedora 42, Claude Desktop 6.28899.6 (unofficial build), bundled Claude Code 2.1.330, terminal Claude Code 2.1.332

Before any product code; checks 1–3 and part of 5.

| Check | Result |
|---|---|
| 1 | `--setting-sources=user,project,local`. `CLAUDE_CODE_ENTRYPOINT=claude-desktop`. `PATH` not representative: the app had been started with `xdg-open` from an agent's shell and inherited its environment, including that shell's session variables. Not yet run from the icon. |
| 2 | Hooks fired (SessionStart, PreToolUse, PostToolUse, Stop; all exit 0); session state written; nothing queued. Not confirmed on the server. Project-only hooks not tested. |
| 3 | Bazaar tools were available in the session. |
| 4 | Not applicable. On Linux `claude --desktop` exits 1: "--desktop isn't available on this platform. It works on macOS and Windows (x64)." Without a terminal it exits 1 on any platform. |
| 5 | The link started the app; `xdg-open` did not return while the app ran. Whether `path` selected the folder was not observed. |
| 6 | Not built yet. |

macOS: not run.
