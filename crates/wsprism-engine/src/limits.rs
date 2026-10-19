//! Room limits the engine enforces on join (per tenant).

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
