//! UI-independent execution contracts. No provider runner is enabled here.
pub mod agent_terminals;
pub mod codex;
pub mod execution;
pub mod execution_v2;
#[cfg(windows)]
pub mod files;
pub mod inspector_tags;
pub mod integration;
pub mod locations;
pub mod runtime;
pub mod runtime_v2;
pub mod splash;
pub mod storage;
pub mod settings;
#[cfg(windows)]
pub mod session_end;
#[cfg(windows)]
pub mod single_instance;
pub mod supervisor;
pub mod terminal_layout;
pub mod terminal_windows;
pub mod window_state;
