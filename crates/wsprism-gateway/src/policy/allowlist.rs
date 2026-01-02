//! Allowlist compilation and matching utilities.
//!
//! Supports simple wildcard matching for Ext lane (`svc:*`) and Hot lane
//! (`svc_id:*`) entries.

use wsprism_core::error::{Result, WsPrismError};

/// Compiled allowlist rule for Ext Lane.
#[derive(Debug, Clone)]
pub struct ExtRule {
    pub svc: String,
    pub msg_type: Option<String>, // None => wildcard
}

/// Compiled allowlist rule for Hot Lane.
#[derive(Debug, Clone)]
pub struct HotRule {
    pub svc_id: u8,
    pub opcode: Option<u8>, // None => wildcard
}

pub fn compile_ext_rules(raw: &[String]) -> Result<Vec<ExtRule>> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        // format: "svc:type" or "svc:*"
        let (svc, ty) = s.split_once(':').ok_or_else(|| {
            WsPrismError::BadRequest(format!("invalid ext_allowlist entry: {s} (expected svc:type)"))
        })?;
        let ty = if ty == "*" { None } else { Some(ty.to_string()) };
        out.push(ExtRule { svc: svc.to_string(), msg_type: ty });
    }
    Ok(out)
}

pub fn compile_hot_rules(raw: &[String]) -> Result<Vec<HotRule>> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        // format: "svc_id:opcode" where opcode may be "*"
        let (svc_id_s, op_s) = s.split_once(':').ok_or_else(|| {
            WsPrismError::BadRequest(format!("invalid hot_allowlist entry: {s} (expected svc_id:opcode)"))
        })?;

        let svc_id: u8 = svc_id_s.parse().map_err(|_| {
            WsPrismError::BadRequest(format!("invalid hot_allowlist svc_id: {svc_id_s}"))
        })?;

        let opcode = if op_s == "*" {
            None
        } else {
            Some(op_s.parse().map_err(|_| {
                WsPrismError::BadRequest(format!("invalid hot_allowlist opcode: {op_s}"))
            })?)
        };

        out.push(HotRule { svc_id, opcode });
    }
    Ok(out)
}

pub fn is_ext_allowed(rules: &[ExtRule], svc: &str, msg_type: &str) -> bool {
    rules.iter().any(|r| {
        if r.svc != svc { return false; }
        match &r.msg_type {
            None => true,
            Some(t) => t == msg_type,
        }
    })
}

pub fn is_hot_allowed(rules: &[HotRule], svc_id: u8, opcode: u8) -> bool {
    rules.iter().any(|r| {
        if r.svc_id != svc_id { return false; }
        match r.opcode {
            None => true,
            Some(op) => op == opcode,
        }
    })
}
