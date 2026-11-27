# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.1](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v1.2.0...visdom-v1.2.1) - 2026-11-27

### Other

- update Cargo.lock dependencies

## [1.2.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v1.1.1...visdom-v1.2.0) - 2026-11-27

### Added

- *(visdom)* final check of a subscription setup in init, doctor and launch
- *(visdom)* start Claude Code on its claude.ai login through the gateway (billing = subscription)
- *(visdom)* who pays for gateway calls — billing = project | subscription, asked by init

### Fixed

- *(visdom)* Copilot review on the subscription check
- *(visdom)* ask Claude Code's login as the launch sets it up; check every model setting; merge --settings as the launch does
- *(visdom)* the subscription check reads models from Claude's settings, managed provider switches and a free plan
- *(visdom)* the subscription check covers the models in use, an unanswerable login, empty managed overrides and a gateway without the alias list
- *(visdom)* a subscription key the gateway refused is not handed out again; test a termination before the agent runs
- *(visdom)* a termination that comes while the agent is being started is passed on to it
- *(visdom)* logout holds the key store while it revokes and forgets; test the signal path of a subscription session
- *(visdom)* an inherited billing is written with the folder's project; Codex init keeps --billing project
- *(visdom)* init keeps an inherited billing whatever writes the folder's config first; --billing reaches every init step
- *(visdom)* a workspace folder's new config keeps the billing named further up

## [1.1.1](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v1.1.0...visdom-v1.1.1) - 2026-11-25

### Other

- updated the following local packages: tracevault-cli

## [1.1.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v1.0.0...visdom-v1.1.0) - 2026-11-24

### Added

- *(visdom)* the agent visdom starts is told what the session has of Visdom's (VIS-1297)
- *(visdom)* run as visdom, nothing the CLI says or installs names tracevault (VIS-1297)
- *(visdom)* what visdom init installs calls visdom trace, not tracevault (VIS-1297)
- keep files under .visdom/ and visdom/, still reading the tracevault ones (VIS-1297)
- *(visdom)* init asks what to set up, and sets all of it up for a machine or a workspace (VIS-1297)
- *(visdom)* init leaves the AI Gateway ready, and env for an agent started by hand (VIS-1297)

### Fixed

- *(visdom)* the agent's own options decide what it is told of (VIS-1297)
- *(visdom)* Codex options written attached are read; a server the caller's options change is not told of (VIS-1297)
- *(visdom)* a project's Codex instructions are left in charge; tracing is told of only where a session is bound (VIS-1297)
- *(visdom)* the earlier GSD registration goes only after the new one is in; tests for the new variables (VIS-1297)
- name the login file where it is (VIS-1297)
- *(visdom)* a workspace's new config does not drop the mode or keep an empty project (VIS-1297)
- *(visdom)* a machine or workspace setup does not stop at what one directory already has (VIS-1297)
- *(visdom)* the gateway marker names the agent that was routed (VIS-1297)
- *(visdom)* a model alias the project does not have, or a mode that cannot be saved, is not a ready gateway (VIS-1297)
- *(visdom)* init does not call a gateway ready that the next start cannot use (VIS-1297)

### Other

- *(visdom)* a workspace init binds the folder and sets up the gateway (VIS-1297)

## [1.0.0](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v0.29.5...visdom-v1.0.0) - 2026-11-23

The first stable release of `visdom`. From here it has its own version, apart from
`tracevault`'s, which it is still shipped with.

### Added

- *(visdom)* start the agent through the AI Gateway (VIS-1297)

### Fixed

- *(visdom)* every Claude Code provider switch is off in a gateway session (VIS-1297)
- *(visdom)* the gateway key is written as the key helper's output (VIS-1297)
- *(visdom)* only an agent visdom routed through the gateway goes uncaptured (VIS-1297)
- *(visdom)* a key in Claude's settings is not sent in place of the gateway's (VIS-1297)
- *(visdom)* the agent's own configuration cannot take a gateway session elsewhere (VIS-1297)
- *(visdom)* a gateway session cannot slip past the gateway (VIS-1297)

## [0.29.5](https://github.com/VirtusLab/visdom-ai-tracing-cli/compare/visdom-v0.29.4...visdom-v0.29.5) - 2026-11-19

### Added

- *(visdom)* init asks for what it was not given (VIS-1297)
- *(visdom)* init also registers the bazaar MCP server (VIS-1297)
- *(visdom)* add the visdom binary — check, fix, launch (VIS-1297)

### Fixed

- *(visdom)* pass only plain http(s) URLs and option-safe names to agent CLIs (VIS-1297)
- *(visdom)* read Codex project MCP config, disabled servers and a broken credential file (VIS-1297)
- *(visdom)* never replace another MCP server; respect Claude's name precedence (VIS-1297)
- *(visdom)* keep an MCP entry's scope on repair; never apply on EOF; no prompts under --yes (VIS-1297)

### Other

- *(release)* publish visdom-cli to crates.io (VIS-1297)
- *(release)* ship visdom alongside tracevault (VIS-1297)
