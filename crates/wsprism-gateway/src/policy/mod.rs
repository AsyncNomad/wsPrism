//! Policy layer (allowlists, limits, rate limiting).
//!
//! Compiles tenant policy configuration into fast lookup structures for
//! transport and dispatcher layers to consume at runtime.

pub mod allowlist;
pub mod engine;

pub use engine::{PolicyDecision, TenantPolicyRuntime};
