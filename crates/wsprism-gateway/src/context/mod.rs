//! Tenant/session context types shared across layers.
//!
//! Sprint 2: we introduce TenantContext so that policy/presence can be tenant-aware
//! without coupling to transport specifics.

pub mod tenant;
