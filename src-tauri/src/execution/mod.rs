//! Shared I0 seam; generated wire types plus native semantic validation and ports.
//! The application does not register execution commands until M1 integration.
mod generated;
pub mod ports;
pub mod validation;
pub use generated::*;
