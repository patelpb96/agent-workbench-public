//! Unwired v2 domain foundation. No process launch, store open, IPC registration
//! or production upgrade is enabled by this module. V1 remains strictly fake.
pub mod admission;
mod generated;
pub mod ports;
pub mod stored;
pub mod validation;
pub use generated::*;

pub type DomainResult<T = ()> = Result<T, crate::execution::Error>;

#[cfg(test)]
mod tests;
