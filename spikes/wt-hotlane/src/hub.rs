//! Transport-agnostic relay core: rooms, per-member slots, batching.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::Notify;

/// Max bytes per batch, for every transport (datagram path MTU budget).
pub const MAX_BATCH_BYTES: usize = 1100;

/// Pending frames for one member: latest per sender (or FIFO in reference mode).
pub struct Slot {
    state: Mutex<SlotState>,
    notify: Notify,
    fifo: bool,
}

#[derive(Default)]
struct SlotState {
    latest: HashMap<u32, Bytes>,
    queue: VecDeque<Bytes>,
}

impl Slot {
    pub fn new(fifo: bool) -> Self {
        Self {
            state: Mutex::default(),
            notify: Notify::new(),
            fifo,
        }
    }

    fn put(&self, sender: u32, frame: Bytes, stats: &Stats) {
        let mut st = self.state.lock().unwrap();
        if self.fifo {
            if st.queue.len() >= 1024 {
                st.queue.pop_front();
                stats.overflow.fetch_add(1, Ordering::Relaxed);
            }
            st.queue.push_back(frame);
        } else if st.latest.insert(sender, frame).is_some() {
            stats.overwritten.fetch_add(1, Ordering::Relaxed);
        }
        drop(st);
        self.notify.notify_one();
    }

    /// Take everything pending, encoded as batches of at most MAX_BATCH_BYTES.
    fn take_batches(&self) -> Vec<Bytes> {
        let frames: Vec<Bytes> = {
            let mut st = self.state.lock().unwrap();
            if self.fifo {
                st.queue.drain(..).collect()
            } else {
                st.latest.drain().map(|(_, f)| f).collect()
            }
        };
        encode_batches(&frames)
    }
}

pub fn encode_batches(frames: &[Bytes]) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut cur = BytesMut::new();
    let mut count: u16 = 0;
    let flush = |cur: &mut BytesMut, count: &mut u16, out: &mut Vec<Bytes>| {
        if *count > 0 {
            let mut b = BytesMut::with_capacity(2 + cur.len());
            b.put_u16_le(*count);
            b.extend_from_slice(cur);
            out.push(b.freeze());
        }
        cur.clear();
        *count = 0;
    };
    for f in frames {
        if f.len() + 4 > MAX_BATCH_BYTES {
            continue; // never fragment; oversize frames are dropped
        }
        if 2 + cur.len() + 2 + f.len() > MAX_BATCH_BYTES {
            flush(&mut cur, &mut count, &mut out);
        }
        cur.put_u16_le(f.len() as u16);
        cur.extend_from_slice(f);
        count += 1;
    }
    flush(&mut cur, &mut count, &mut out);
    out
}

#[derive(Default)]
pub struct Stats {
    pub frames_in: AtomicU64,
    pub batches_out: AtomicU64,
    pub overwritten: AtomicU64,
    pub overflow: AtomicU64,
    pub send_dropped: AtomicU64,
}

struct Member {
    conn: u64,
    sender: u32,
    slot: Arc<Slot>,
}

pub struct Hub {
    rooms: Mutex<HashMap<u32, Vec<Member>>>,
    next_conn: AtomicU64,
    pub fifo: bool,
    pub tick: Option<Duration>,
    pub stats: Stats,
}

/// A member's registration; leaves the room on drop.
pub struct Membership {
    hub: Arc<Hub>,
    room: u32,
    conn: u64,
    pub slot: Arc<Slot>,
}

impl Drop for Membership {
    fn drop(&mut self) {
        if let Some(members) = self.hub.rooms.lock().unwrap().get_mut(&self.room) {
            members.retain(|m| m.conn != self.conn);
        }
    }
}

impl Hub {
    pub fn new(fifo: bool, tick_hz: u32) -> Arc<Self> {
        Arc::new(Self {
            rooms: Mutex::default(),
            next_conn: AtomicU64::new(1),
            fifo,
            tick: (tick_hz > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(tick_hz))),
            stats: Stats::default(),
        })
    }

    pub fn join(self: &Arc<Self>, room: u32, sender: u32) -> Membership {
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let slot = Arc::new(Slot::new(self.fifo));
        self.rooms
            .lock()
            .unwrap()
            .entry(room)
            .or_default()
            .push(Member {
                conn,
                sender,
                slot: slot.clone(),
            });
        Membership {
            hub: self.clone(),
            room,
            conn,
            slot,
        }
    }

    /// Fan a frame out to every other member of the room.
    pub fn publish(&self, room: u32, sender: u32, frame: Bytes) {
        self.stats.frames_in.fetch_add(1, Ordering::Relaxed);
        let rooms = self.rooms.lock().unwrap();
        if let Some(members) = rooms.get(&room) {
            for m in members.iter().filter(|m| m.sender != sender) {
                m.slot.put(sender, frame.clone(), &self.stats);
            }
        }
    }

    /// Wait for the next flush point (notification, or tick) and return batches.
    pub async fn next_batches(
        &self,
        slot: &Slot,
        tick: &mut Option<tokio::time::Interval>,
    ) -> Vec<Bytes> {
        match tick {
            Some(t) => {
                t.tick().await;
            }
            None => slot.notify.notified().await,
        }
        slot.take_batches()
    }

    pub fn ticker(&self) -> Option<tokio::time::Interval> {
        self.tick.map(|d| {
            let mut i = tokio::time::interval(d);
            i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            i
        })
    }
}

/// `?room=<u32>&id=<u32>` from a path/query string.
pub fn parse_query(path_and_query: &str) -> Option<(u32, u32)> {
    let q = path_and_query.split_once('?')?.1;
    let mut room = None;
    let mut id = None;
    for kv in q.split('&') {
        match kv.split_once('=') {
            Some(("room", v)) => room = v.parse().ok(),
            Some(("id", v)) => id = v.parse().ok(),
            _ => {}
        }
    }
    Some((room?, id?))
}
