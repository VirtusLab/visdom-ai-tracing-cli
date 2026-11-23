# visdom-cli

The `visdom` binary: the developer CLI for the Visdom suite. It carries every `tracevault`
command under `visdom trace`, checks that a project is wired for the coding agent (tracing,
bazaar MCP), offers to fix what is not, and starts the agent — traced by hooks, or with its LLM
traffic going through the Visdom AI Gateway (`visdom --mode gateway`).

Installed hooks call `tracevault`, so install both: `cargo install tracevault-cli visdom-cli`.

See the [repository README](https://github.com/VirtusLab/visdom-ai-tracing-cli#visdom) for usage.
