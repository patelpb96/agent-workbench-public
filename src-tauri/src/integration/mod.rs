//! Native-only desktop integration. No real provider or process execution.
mod coordinator;
mod coordinator_v2;
pub mod ipc;
pub mod ipc_v2;
mod reply;
mod request;
mod request_v2;

pub use coordinator::{Coordinator, Reply};
pub use coordinator_v2::{CoordinatorV2, OwnerRuntime, SHUTDOWN_BOUND};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_v2;
