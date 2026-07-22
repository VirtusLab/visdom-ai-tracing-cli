# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.25.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.24.0...v0.25.0) - 2026-07-22

### Added

- *(cli)* Phase 4B — hook/stream project attribution (part of #306) ([#32](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/32))
- *(cli)* Phase 4A — project binding & resolution (part of #306) ([#31](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/31))
- *(cli)* Phase 2e — remote-aware repo resolution ([#30](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/30))
- *(cli)* OpenCode agent support — init --agent opencode + bundled plugin + fileless inline capture ([#29](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/29))
- *(cli)* GSD (pi) agent support — init --agent gsd + extension + gsd-tagged streaming ([#27](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/27))

### Other

- *(cli)* drop dead multi-project-refusal warning (server no longer 409s on multi-project ingest) ([#34](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/34))
- Revert "Single-tenant collapse — CLI (drop orgs) ([#35](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/35))" ([#36](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/36))
- Single-tenant collapse — CLI (drop orgs) ([#35](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/35))

## [0.24.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.23.3...v0.24.0) - 2026-07-13

### Added

- *(cli)* Codex CLI agent support (init --agent codex + codex-tagged streaming) ([#24](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/24))

## [0.23.3](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.23.2...v0.23.3) - 2026-07-09

### Other

- update Cargo.lock dependencies

## [0.23.1](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.23.0...v0.23.1) - 2026-07-09

### Added

- *(cli)* derive org from credential in repo switch when unset ([#19](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/19))

## [0.23.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.22.0...v0.23.0) - 2026-07-08

### Added

- *(cli)* user-level default repo binding (switch repo before Claude launches) ([#17](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/17))

## [0.22.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.21.0...v0.22.0) - 2026-07-08

### Added

- *(cli)* detached / user-level context ([#14](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/14))
- *(cli)* repo switch --name binds by project name without a checkout ([#13](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/13))

### Fixed

- *(cli)* make tracevault status mode-aware (global install + repo switch binding) ([#16](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/16))

## [0.21.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.20.2...v0.21.0) - 2026-07-07

### Added

- *(cli)* init --global + session hooks (workspace mode, sub-plan C) ([#12](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/12))
- *(cli)* workspace-mode repo commands + stream wiring (sub-plan B) ([#11](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/11))
- *(cli)* workspace-mode repo resolution foundation (sub-plan A) ([#10](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/10))
- *(cli)* user-level context layer (hierarchical context) ([#8](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/8))

## [0.20.2](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.20.1...v0.20.2) - 2026-07-01

### Fixed

- *(cli)* send explicit Content-Length: 0 on bodyless auth POSTs ([#7](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/7))

### Other

- drop the main-repo link from the tracevault-cli crate README ([#6](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/6))

## [0.20.1](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/v0.20.0...v0.20.1) - 2026-06-24

### Other

- rename protocol crate to visdom-ai-tracing-protocol ([#2](https://github.com/VirtusLab/visdom-ai-tracing-cli/pull/2))
- bootstrap public CLI repo (tracevault-protocol + tracevault-cli)

## [0.20.0](https://github.com/VirtusLab/visdom-ai-tracing/compare/v0.19.0...v0.20.0) - 2026-06-22

### Fixed

- *(cli)* refuse `init` from a linked git worktree ([#273](https://github.com/VirtusLab/visdom-ai-tracing/pull/273))

## [0.19.0](https://github.com/VirtusLab/visdom-ai-tracing/compare/v0.18.0...v0.19.0) - 2026-06-22

## [0.18.0](https://github.com/VirtusLab/visdom-ai-tracing/compare/v0.17.3...v0.18.0) - 2026-06-19

### Fixed

- *(events)* identity dedup + UUIDv7 ordering (retire .event_counter) ([#243](https://github.com/VirtusLab/visdom-ai-tracing/pull/243))

## [0.17.1](https://github.com/softwaremill/tracevault/compare/v0.17.0...v0.17.1) - 2026-06-01

### Fixed

- *(cli)* init only gitignores files it actually modified ([#219](https://github.com/softwaremill/tracevault/pull/219))

## [0.17.0](https://github.com/softwaremill/tracevault/compare/v0.16.1...v0.17.0) - 2026-06-01

### Added

- *(proxy)* per-credential and global concurrency caps ([#210](https://github.com/softwaremill/tracevault/pull/210)) ([#211](https://github.com/softwaremill/tracevault/pull/211))
- [**breaking**] rename validation window to verification phase ([#213](https://github.com/softwaremill/tracevault/pull/213))

### Fixed

- *(auth, cli)* sliding session window + actionable check errors ([#212](https://github.com/softwaremill/tracevault/pull/212))

## [0.16.1](https://github.com/softwaremill/tracevault/compare/v0.16.0...v0.16.1) - 2026-06-01

### Fixed

- *(server,cli)* default session tool to 'claude-code' to fix validation-start 500

## [0.16.0](https://github.com/softwaremill/tracevault/compare/v0.15.0...v0.16.0) - 2026-05-27

### Added

- agent-policies — server-rendered policy instructions for agents

## [0.15.0](https://github.com/softwaremill/tracevault/compare/v0.14.0...v0.15.0) - 2026-05-22

### Changed

- *(cli)* remove unused _cwd param from run_stream

### Documentation

- rename product TraceVault → Visdom Trace across all documentation

### Fixed

- *(cli)* resolve project_root from hook_event.cwd instead of process cwd

## [0.14.0](https://github.com/softwaremill/tracevault/compare/v0.13.0...v0.14.0) - 2026-05-22

### Added

- *(policies)* validation window for scoped policy enforcement

### Fixed

- *(policies)* address self-review findings

## [0.13.0](https://github.com/softwaremill/tracevault/compare/v0.12.0...v0.13.0) - 2026-05-21

### Added

- *(init)* add --no-gitignore flag to skip .gitignore updates
- *(policies)* add must_succeed flag to tool call policies

### Changed

- *(init)* remove unused claude_target param from update_root_gitignore

### Fixed

- *(auth)* move device status poll off strict rate limiter and handle 429 in CLI
- *(init)* always gitignore both .claude/settings.json and settings.local.json

## [0.12.0](https://github.com/softwaremill/tracevault/compare/v0.11.3...v0.12.0) - 2026-05-08

### Added

- *(init)* add --claude-settings flag to choose shared vs local hooks

## [0.11.3](https://github.com/softwaremill/tracevault/compare/v0.11.2...v0.11.3) - 2026-04-23

### Fixed

- *(cli)* keep all tracevault files local, update root .gitignore on init

## [0.11.2](https://github.com/softwaremill/tracevault/compare/v0.11.1...v0.11.2) - 2026-04-23

### Fixed

- *(cli)* remove broken fixed-width box from login URL display

## [0.11.1](https://github.com/softwaremill/tracevault/compare/v0.11.0...v0.11.1) - 2026-04-23

### Fixed

- *(cli)* fix flush 413 loop, add timeout/progress, fix status pending check

## [0.11.0](https://github.com/softwaremill/tracevault/compare/v0.10.0...v0.11.0) - 2026-04-23

## [0.10.0](https://github.com/softwaremill/tracevault/compare/v0.9.0...v0.10.0) - 2026-04-22

### Fixed

- *(cli)* make login work in headless environments (Docker, CI, SSH)

## [0.6.2](https://github.com/softwaremill/tracevault/compare/v0.6.1...v0.6.2) - 2026-04-01

### Fixed

- use rustls-tls for CLI and macos-latest for x86_64 builds

## [0.6.1](https://github.com/softwaremill/tracevault/compare/v0.6.0...v0.6.1) - 2026-03-29

### Test

- add CLI unit tests (config, hooks, init, commit_push)

## [0.6.0](https://github.com/softwaremill/tracevault/compare/v0.5.0...v0.6.0) - 2026-03-29

### Added

- add hook adapter architecture with multi-tool detection
- add tool field to streaming protocol v2

## [0.5.0](https://github.com/softwaremill/tracevault/compare/v0.4.0...v0.5.0) - 2026-03-28

### Changed

- remove git-ai, compute attribution server-side from sessions

## [0.4.0](https://github.com/softwaremill/tracevault/compare/v0.3.2...v0.4.0) - 2026-03-25

### Added

- add commit message storage and display

## [0.3.2](https://github.com/softwaremill/tracevault/compare/v0.3.1...v0.3.2) - 2026-03-25

### Added

- send SessionEnd on Claude Code Stop hook

## [0.3.0](https://github.com/softwaremill/tracevault/compare/v0.2.0...v0.3.0) - 2026-03-25

### Added

- *(init)* update hooks for streaming architecture
- *(cli)* add commit-push and flush commands
- *(cli)* add stream command with transcript piggybacking and pending queue
- *(core)* add streaming types, file change extraction, and repo_id to config

## [0.2.0](https://github.com/softwaremill/tracevault/compare/v0.1.0...v0.2.0) - 2026-03-23

### Fixed

- fix tests
- fix cargo clippy
