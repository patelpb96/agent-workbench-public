//! Persistent-session broker pieces for Agent Workbench (design: docs/design/session-daemon.md).
pub mod attach;
pub mod auth;
pub mod caps;
pub mod frames;
pub mod log;
pub mod proto;
pub mod ring;
pub mod sync;

#[cfg(windows)]
pub mod broker;
#[cfg(windows)]
pub mod cli;
#[cfg(windows)]
pub mod client;
#[cfg(windows)]
pub mod host;
#[cfg(windows)]
pub mod win;
