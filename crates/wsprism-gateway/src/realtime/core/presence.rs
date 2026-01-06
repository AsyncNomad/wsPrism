use dashmap::{DashMap, DashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use wsprism_core::error::{Result, WsPrismError};
use crate::config::schema::TenantLimits;

/// Room presence: `room_key -> sessions`, `session_key -> rooms`.
///
/// Sprint 5: Added user-level indexing and tenant counters for governance.
/// Lock-free best-effort design: under heavy contention, limits can be
/// temporarily exceeded by a small margin to preserve throughput.
#[derive(Default)]
pub struct Presence {
    // Routing indices
    room_to_sessions: DashMap<String, DashSet<String>>,
    session_to_rooms: DashMap<String, DashSet<String>>,

    // Governance indices
    room_to_users: DashMap<String, DashSet<String>>,
    user_to_rooms: DashMap<String, DashSet<String>>,
    
    // Multi-session ref-counting: user::room -> session_count
    user_room_refs: DashMap<String, usize>,

    // O(1) Counters
    tenant_room_counts: DashMap<String, AtomicU64>,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            room_to_sessions: DashMap::new(),
            session_to_rooms: DashMap::new(),
            room_to_users: DashMap::new(),
            user_to_rooms: DashMap::new(),
            user_room_refs: DashMap::new(),
            tenant_room_counts: DashMap::new(),
        }
    }

    /// Attempt to join a room with per-tenant/user/room limits enforced.
    ///
    /// Concurrency note: This uses lock-free counters and map lookups for
    /// performance. The check (e.g., `current < limit`) and the subsequent
    /// insert are not atomic across threads, so at high contention the limit
    /// may be exceeded by a small margin (acceptable trade-off for throughput).
    /// Do not rely on this for exact “hard” ceilings; it is designed for fast
    /// best-effort enforcement in a single-node gateway.
    pub fn try_join(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        session_key: &str,
        limits: &TenantLimits
    ) -> Result<()> {
        
        // --- 1. Check Room Capacity (Max Users per Room) ---
        if limits.max_users_per_room > 0 {
            if let Some(users) = self.room_to_users.get(room_key) {
                // If user is not already in room, check limit
                if users.len() as u64 >= limits.max_users_per_room && !users.contains(user_key) {
                     return Err(WsPrismError::ResourceExhausted("room user limit reached".into()));
                }
            }
        }

        // --- 2. Check User's Room Limit (Max Rooms per User) ---
        if limits.max_rooms_per_user > 0 {
            if let Some(rooms) = self.user_to_rooms.get(user_key) {
                if rooms.len() as u64 >= limits.max_rooms_per_user && !rooms.contains(room_key) {
                    return Err(WsPrismError::ResourceExhausted("user room limit reached".into()));
                }
            }
        }

        // --- 3. Check Tenant Total Rooms ---
        // We only increment if the room is NEW (currently has no sessions).
        // Note: This is a slight approximation. A room exists if it has sessions.
        let is_new_room = !self.room_to_sessions.contains_key(room_key);
        if is_new_room && limits.max_rooms_total > 0 {
            let counter = self.tenant_room_counts.entry(tenant_id.to_string()).or_insert_with(|| AtomicU64::new(0));
            if counter.load(Ordering::Relaxed) >= limits.max_rooms_total {
                return Err(WsPrismError::ResourceExhausted("tenant room limit reached".into()));
            }
            // Increment
            counter.fetch_add(1, Ordering::Relaxed);
        } else if is_new_room {
             self.tenant_room_counts.entry(tenant_id.to_string()).or_default().fetch_add(1, Ordering::Relaxed);
        }

        // --- 4. Perform Join (Order: Routing -> Governance) ---
        
        // A. Routing
        self.room_to_sessions.entry(room_key.to_string()).or_default().insert(session_key.to_string());
        self.session_to_rooms.entry(session_key.to_string()).or_default().insert(room_key.to_string());
        
        // B. Governance (Ref counting for multi-session support)
        let ref_key = format!("{}::{}", user_key, room_key);
        let mut refs = self.user_room_refs.entry(ref_key).or_insert(0);
        *refs += 1;
        
        // If this is the first session for this user in this room, add to user indices
        if *refs == 1 {
            self.room_to_users.entry(room_key.to_string()).or_default().insert(user_key.to_string());
            self.user_to_rooms.entry(user_key.to_string()).or_default().insert(room_key.to_string());
        }

        Ok(())
    }

    pub fn leave(&self, tenant_id: &str, room_key: &str, user_key: &str, session_key: &str) {
        // 1. Remove from routing
        let mut room_empty = false;
        if let Some(set) = self.room_to_sessions.get(room_key) {
            set.remove(session_key);
            room_empty = set.is_empty();
        }
        // Cleanup empty routing set outside lock
        if room_empty { self.room_to_sessions.remove(room_key); }

        if let Some(set) = self.session_to_rooms.get(session_key) {
            set.remove(room_key);
            if set.is_empty() { drop(set); self.session_to_rooms.remove(session_key); }
        }

        // 2. Remove from governance (Ref counting)
        let ref_key = format!("{}::{}", user_key, room_key);
        let mut remove_user_mapping = false;
        
        if let Some(mut refs) = self.user_room_refs.get_mut(&ref_key) {
            *refs -= 1;
            if *refs == 0 {
                remove_user_mapping = true;
            }
        }
        if remove_user_mapping {
            self.user_room_refs.remove(&ref_key);
            
            // Remove from user_to_rooms
            if let Some(set) = self.user_to_rooms.get(user_key) {
                set.remove(room_key);
                if set.is_empty() { drop(set); self.user_to_rooms.remove(user_key); }
            }

            // Remove from room_to_users
            if let Some(set) = self.room_to_users.get(room_key) {
                set.remove(user_key);
                if set.is_empty() { drop(set); self.room_to_users.remove(room_key); }
            }
        }
        
        // 3. Decrement Tenant Room Count if room empty
        if room_empty {
             if let Some(counter) = self.tenant_room_counts.get(tenant_id) {
                 counter.fetch_sub(1, Ordering::Relaxed);
             }
        }
    }

    pub fn sessions_in(&self, room_key: &str) -> Vec<String> {
        self.room_to_sessions.get(room_key)
            .map(|set| set.iter().map(|u| u.key().to_string()).collect())
            .unwrap_or_default()
    }

    // Called by RAII Drop
    pub fn cleanup_session(&self, tenant_id: &str, user_key: &str, session_key: &str) {
        if let Some(rooms) = self.session_to_rooms.remove(session_key).map(|(_, v)| v) {
            for r in rooms.iter() {
                let room_key = r.key();
                // Use the full leave logic to ensure ref-counts and limits are updated correctly
                self.leave(tenant_id, room_key, user_key, session_key);
            }
        }
    }
}
