//! Policy layer (Sprint 2).
//!
//! Ordering: policy → (plugins/WASM later) → service.
//!
//! Policy is intentionally cheap:
//! - max_frame_bytes
//! - allowlist (ext: svc/type, hot: svc_id/opcode)
//! - rate limit (tenant-level token bucket)

pub mod allowlist;
pub mod engine;

pub use engine::{PolicyDecision, TenantPolicyRuntime};
