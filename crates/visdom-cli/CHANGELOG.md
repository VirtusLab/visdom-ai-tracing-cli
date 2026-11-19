# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
