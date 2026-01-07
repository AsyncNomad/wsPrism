//! Protocol modules (Ext/Text + Hot/Binary).
//!
//! This module hosts the dual-lane wire formats:
//! - Ext Lane: JSON envelopes with optional RawValue payloads.
//! - Hot Lane: binary frames with fixed headers and optional sequence numbers.
//!
//! All parsers are panic-free: malformed input is reported as `WsPrismError`
//! instead of panicking or indexing raw buffers, keeping the gateway resilient
//! to hostile traffic.

pub mod hot;
pub mod text;
