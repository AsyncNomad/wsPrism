//! Close codes on the wire (ADR 0006 §6). v1 sends the standard RFC 6455
//! codes it always did. v2 uses 4000-range codes whose range tells the
//! client how to reconnect: 40xx = not unchanged (new credential / config),
//! 41xx = after backoff, 42xx = immediately. Reasons without a v2 code keep
//! the standard one.

use super::ProtocolVersion;

/// The close code to send for a close with standard `code` and `reason`.
pub fn wire_close_code(protocol: ProtocolVersion, code: u16, reason: &str) -> u16 {
    if protocol == ProtocolVersion::V1 {
        return code;
    }
    match reason {
        "auth_expired" => 4001,
        "policy" => 4003,
        "too_many_sessions" => 4004,
        "replaced" => 4005,
        "max_sessions_exceeded" => 4006,
        "disconnected" => 4007,
        "message too big" => 4009,
        "decode_error" => 4010,
        "capacity" => 4100,
        "slow_consumer" => 4101,
        "draining" => 4103,
        "idle_timeout" => 4200,
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_keeps_standard_codes_and_v2_maps_by_reason() {
        assert_eq!(wire_close_code(ProtocolVersion::V1, 1008, "replaced"), 1008);
        assert_eq!(wire_close_code(ProtocolVersion::V2, 1008, "replaced"), 4005);
        assert_eq!(
            wire_close_code(ProtocolVersion::V2, 1000, "idle_timeout"),
            4200
        );
        assert_eq!(
            wire_close_code(ProtocolVersion::V2, 1011, "something new"),
            1011
        );
    }
}
