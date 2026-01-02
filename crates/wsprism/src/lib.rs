//! Top-level facade crate for wsPrism.
//!
//! Re-exports core types and the gateway library so users can depend on a single crate.

pub mod core {
    pub use wsprism_core::*;
}

pub mod gateway {
    pub use wsprism_gateway::*;
}
