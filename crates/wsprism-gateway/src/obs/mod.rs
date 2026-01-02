//! Lightweight in-process metrics (dependency-free).
//!
//! Sprint 4 goal: expose minimal Prometheus-compatible metrics without adding
//! external crates. Metrics are stored as atomics and rendered by the `/metrics`
//! handler.

pub mod metrics;
