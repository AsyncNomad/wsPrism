//! Room limits the engine enforces on join (per tenant) and the room name
//! rule every entry point shares.

use wsprism_protocol::error::{Result, WsPrismError};

/// Longest room name, in bytes. Names are stored in several indices, so an
/// unbounded name is a memory amplifier.
pub const MAX_ROOM_NAME_LEN: usize = 128;

/// A room name is 1-128 bytes of UTF-8 without control characters.
pub fn validate_room_name(room: &str) -> Result<()> {
    if room.is_empty() || room.len() > MAX_ROOM_NAME_LEN || room.chars().any(char::is_control) {
        return Err(WsPrismError::BadRequest(format!(
            "room name must be 1-{MAX_ROOM_NAME_LEN} bytes without control characters"
        )));
    }
    Ok(())
}

/// `0` = unlimited for every field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoomLimits {
    /// Rooms that may exist in the tenant at once.
    pub max_rooms_total: u64,
    /// Distinct users per room.
    pub max_users_per_room: u64,
    /// Rooms one user may be in at once.
    pub max_rooms_per_user: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_names_are_bounded_and_printable() {
        assert!(validate_room_name("match:42").is_ok());
        assert!(validate_room_name("로비 1").is_ok());
        assert!(validate_room_name(&"a".repeat(MAX_ROOM_NAME_LEN)).is_ok());
        assert!(validate_room_name("").is_err());
        assert!(validate_room_name(&"a".repeat(MAX_ROOM_NAME_LEN + 1)).is_err());
        assert!(validate_room_name("a\nb").is_err());
        assert!(validate_room_name("a\u{0}b").is_err());
    }
}
