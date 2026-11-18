//! How a project's sessions reach AI Tracing.

use std::path::Path;

use serde::Serialize;

/// The capture path a project uses. Exactly one is meant to be active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TracingMode {
    /// Agent hooks and git hooks capture the session.
    Hooks,
    /// The agent's LLM traffic goes through the AI Gateway; no capture hooks.
    Gateway,
}

/// The tracing mode of the project at `project_dir`.
///
/// The mode is a per-project Control Plane setting that the CLI cannot read
/// yet, so every project is `Hooks` — also the agreed default whenever the
/// mode is unknown. This is the single place that decides it: components take
/// the mode as an input and never assume one.
pub fn resolve_tracing_mode(_project_dir: Option<&Path>) -> TracingMode {
    TracingMode::Hooks
}
