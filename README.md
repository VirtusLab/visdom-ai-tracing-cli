# Visdom Trace CLI

The public CLI for [Visdom Trace](https://github.com/VirtusLab/visdom-ai-tracing) —
AI code tracing and attribution. Installs the `tracevault` binary.

## Crates

- **`tracevault-cli`** — the `tracevault` command-line tool (hooks, session capture, verification).
- **`tracevault-protocol`** — the wire-protocol types (stream + hook events) shared between the
  CLI and the Visdom Trace server. Published so both sides depend on one source of truth.

## Install

```bash
brew install VirtusLab/visdom-ai-tracing/tracevault
```

or from crates.io:

```bash
cargo install tracevault-cli
```

## License

Apache-2.0.
