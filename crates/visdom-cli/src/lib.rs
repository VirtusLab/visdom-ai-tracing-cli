//! `visdom`: one developer-facing binary for the Visdom suite. It carries the
//! whole `tracevault` command surface (under `visdom trace`), checks that the
//! current project is wired for the chosen coding agent, offers to fix what is
//! not, and launches the agent.

pub mod agents;
pub mod billing;
pub mod cli;
pub mod components;
pub mod config;
pub mod doctor;
pub mod env_alias;
pub mod finding;
pub mod fixes;
pub mod gateway;
pub mod instructions;
pub mod interactive;
pub mod launch;
pub mod mode;
pub mod subscription;
pub mod suite;
pub mod surface;
#[cfg(test)]
mod test_http;
