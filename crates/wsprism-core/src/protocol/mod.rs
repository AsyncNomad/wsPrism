//! Protocol modules (Ext/Text + Hot/Binary).
//!
//! This module hosts the dual-lane wire formats:
//! - Ext Lane: JSON envelopes with optional RawValue payloads.
//! - Hot Lane: binary frames with fixed headers and optional sequence numbers.

pub mod hot;
pub mod text;
