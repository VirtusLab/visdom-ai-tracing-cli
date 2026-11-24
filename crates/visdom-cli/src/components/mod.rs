//! The parts of Visdom a project is wired to. Each component turns what it
//! finds into [`crate::finding::Finding`]s; none of them changes anything.

use std::path::PathBuf;

use tracevault_cli::agent::Agent;

use crate::agents::Workdir;
use crate::mode::TracingMode;

pub mod bazaar;
pub mod gateway;
pub mod tracing;

/// Whether the tracing library found a credential for this project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialState {
    Present,
    Missing,
    /// There is one, but it cannot be used as configured (the reason).
    Unusable(String),
    /// The credential file exists but could not be read (its path).
    Unreadable(String),
}

/// Everything the checks look at, gathered once.
#[derive(Clone, Debug)]
pub struct CheckContext {
    /// Where `visdom` runs. `.visdom/` and the git hooks belong to the
    /// primary checkout.
    pub workdir: Workdir,
    pub home: PathBuf,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: PathBuf,
    pub agent: Agent,
    pub mode: TracingMode,
    /// The tracing server this project (or the login) points at.
    pub server_url: Option<String>,
    pub credential: CredentialState,
}
