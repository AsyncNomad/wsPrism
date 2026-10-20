//! Realtime egress engine and per-message context.
//!
//! The realtime core fans out messages to users, sessions, or rooms with
//! lossy/reliable QoS and keeps lightweight counters for observability. It is
//! intentionally lock-free; small temporary overshoots are possible under high
//! contention to preserve throughput.

use crate::broker::{
    run_outbox, Broker, BrokerFrame, BrokerMode, BrokerOutbox, Incoming, Keep, MemoryBroker,
    OutboxCounters, OutboxLimits, MAX_NODE_SLOT,
};
use crate::realtime::core::history::{self, History, HistoryConfig, HistoryPos, Recover, Ring};
use crate::realtime::core::{
    now_ms, CloseRequest, Connection, Departed, Joined, Member, Presence, Put, SessionRegistry,
    SessionSignals,
};
use crate::realtime::types::{close_code, CloseSpec, HotOut, OutFrame, Outgoing, Payload, QoS};
use crate::sys;
use crate::RoomLimits;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc::error::TrySendError;
use wsprism_protocol::error::ClientCode;
use wsprism_protocol::error::{Result, WsPrismError};
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::ProtocolVersion;

use crate::permissions::Permissions;

/// Egress counters of one engine instance (not process-wide: two gateways
/// in one process, or tests, keep separate numbers).
#[derive(Debug, Default)]
pub struct EgressCounters {
    drops: AtomicU64,
    slow_consumer_closes: AtomicU64,
    slot_overwrites: AtomicU64,
    slot_full_drops: AtomicU64,
}

impl EgressCounters {
    /// Lossy messages dropped because the recipient's queue was full.
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
    /// Recipients closed because a reliable message didn't fit their queue.
    pub fn slow_consumer_closes(&self) -> u64 {
        self.slow_consumer_closes.load(Ordering::Relaxed)
    }
    /// Keyed Hot frames replaced in a recipient's slot before being sent
    /// (normal: latest state wins; high values mean readers lag the update rate).
    pub fn slot_overwrites(&self) -> u64 {
        self.slot_overwrites.load(Ordering::Relaxed)
    }
    /// Keyed Hot frames dropped because a recipient's slot was at capacity.
    pub fn slot_full_drops(&self) -> u64 {
        self.slot_full_drops.load(Ordering::Relaxed)
    }
}

fn sample_every_1024(n: u64) -> bool {
    (n & 1023) == 1
}

/// Shared realtime state: every session and every room of every tenant.
pub struct RealtimeCore {
    /// Connected sessions.
    pub sessions: Arc<SessionRegistry>,
    /// Room membership.
    pub presence: Arc<Presence>,
    counters: EgressCounters,
    broker: Arc<dyn Broker>,
    /// `Some` iff the broker is queued: room frames go through it.
    outbox: Option<Arc<BrokerOutbox>>,
    /// This node's slot (from the broker's `start`).
    node_slot: AtomicU16,
    /// `Some` iff some tenant keeps room history (ADR 0011 §3).
    history: Option<History>,
}

impl Default for RealtimeCore {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeCore {
    /// Create a new realtime core with fresh registries (single node:
    /// [`MemoryBroker`]).
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(SessionRegistry::new()),
            presence: Arc::new(Presence::new()),
            counters: EgressCounters::default(),
            broker: Arc::new(MemoryBroker),
            outbox: None,
            node_slot: AtomicU16::new(0),
            history: None,
        }
    }

    /// Keep room history as `config` says (ADR 0011 §3). Single node only
    /// until the cluster broker can serve history (R3).
    pub fn with_history(mut self, config: HistoryConfig) -> Result<Self> {
        if config.is_empty() {
            return Ok(self);
        }
        // With a queued broker the broker keeps the history (ADR 0011 R3-2);
        // this node only needs the rules.
        self.history = Some(History::new(config));
        Ok(self)
    }

    /// A core whose room traffic goes through `broker` (ADR 0011;
    /// experimental). A queued broker needs [`RealtimeCore::start`].
    pub fn with_broker(broker: Arc<dyn Broker>, limits: OutboxLimits) -> Result<Self> {
        if limits.capacity == 0 || limits.per_tenant == 0 {
            return Err(WsPrismError::BadRequest(
                "broker outbox limits must be positive".into(),
            ));
        }
        let outbox =
            (broker.mode() == BrokerMode::Queued).then(|| Arc::new(BrokerOutbox::new(limits)));
        Ok(Self {
            sessions: Arc::new(SessionRegistry::new()),
            presence: Arc::new(Presence::with_routing(0, outbox.clone())),
            counters: EgressCounters::default(),
            broker,
            outbox,
            node_slot: AtomicU16::new(0),
            history: None,
        })
    }

    /// Start the broker (connect, claim the node slot) and its tasks (outbox
    /// drain, incoming frames) on the current Tokio runtime; they stop when
    /// the returned guard is dropped. Call before serving anyone: member
    /// ids carry the node slot.
    pub async fn start(self: &Arc<Self>) -> Result<BrokerTasks> {
        let slot = self
            .broker
            .start()
            .await
            .map_err(|e| WsPrismError::Unavailable(e.to_string()))?;
        if slot > MAX_NODE_SLOT {
            return Err(WsPrismError::BadRequest(format!(
                "broker node slot {slot} is above {MAX_NODE_SLOT}"
            )));
        }
        self.node_slot.store(slot, Ordering::Relaxed);
        self.presence.set_node_slot(slot);
        let mut tasks = Vec::new();
        if let Some(outbox) = &self.outbox {
            tasks.push(tokio::spawn(run_outbox(
                Arc::clone(outbox),
                Arc::clone(&self.broker),
            )));
            if let Some(rx) = self.broker.incoming() {
                tasks.push(tokio::spawn(run_incoming(Arc::downgrade(self), rx)));
            }
        }
        Ok(BrokerTasks(tasks))
    }

    /// The outbox's counters, if the broker is queued.
    pub fn broker_counters(&self) -> Option<&OutboxCounters> {
        self.outbox.as_deref().map(BrokerOutbox::counters)
    }

    /// This engine's egress counters (for metrics).
    pub fn counters(&self) -> &EgressCounters {
        &self.counters
    }

    /// Ask every session to close with 1001 during draining. A session that
    /// is already closing keeps its first reason.
    pub fn best_effort_shutdown_all(&self, reason: &str) {
        self.sessions.for_each_session(|conn| {
            let spec = CloseSpec::new(close_code::GOING_AWAY, reason, None);
            conn.signals.request_close(CloseRequest::Spec(spec));
        });
    }

    /// One sweeper pass over every session (performance-architecture §5):
    /// sessions idle for `idle_timeout_ms` are asked to close; sessions due a
    /// keep-alive get a Ping, skipped if their queue is full (C9). Replaces
    /// two timers per session.
    pub fn sweep(&self, idle_timeout_ms: u64, ping_interval_ms: u64) {
        if let Some(h) = &self.history {
            h.sweep(&self.presence);
        }
        let now = now_ms();
        self.sessions.for_each_session(|conn| {
            if conn.signals.expired(now) {
                conn.signals
                    .request_close(CloseRequest::Spec(CloseSpec::new(
                        close_code::POLICY,
                        "auth_expired",
                        Some(sys::kicked("auth_expired", "").to_string()),
                    )));
            } else if conn.signals.idle_ms(now) >= idle_timeout_ms {
                conn.signals.request_close(CloseRequest::Idle);
            } else if conn.signals.take_ping_due(now, ping_interval_ms) {
                let _ = conn.tx.try_send(OutFrame::Ping);
            }
        });
    }

    /// Queue one message for one session according to `qos`. Never awaits,
    /// so a slow recipient can never stall the publisher (C1):
    /// - `Lossy`: if the queue is full, drop it for this recipient.
    /// - `Reliable`: if the queue is full, close the recipient as a slow
    ///   consumer so it resyncs on reconnect, instead of silently missing it.
    fn deliver(&self, session_key: &str, conn: &Connection, frame: OutFrame, qos: &QoS) {
        // Keyed state (latest value wins) goes through the recipient's Hot
        // slot, so a lagging reader never receives a backlog of stale values.
        if let (OutFrame::Hot(h), QoS::Lossy) = (&frame, qos) {
            if let Some(key) = h.slot_key {
                match conn.signals.hot.put(key, Arc::clone(h)) {
                    Put::Inserted => {}
                    Put::Overwritten => {
                        self.counters
                            .slot_overwrites
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Put::Full => {
                        self.counters
                            .slot_full_drops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                return;
            }
        }
        match conn.tx.try_send(frame) {
            Ok(()) | Err(TrySendError::Closed(_)) => {} // closed = session ending
            Err(TrySendError::Full(_)) => match qos {
                QoS::Lossy => {
                    let n = self.counters.drops.fetch_add(1, Ordering::Relaxed);
                    if sample_every_1024(n) {
                        tracing::warn!(%session_key, drops=%n, "lossy egress drop");
                    }
                }
                QoS::Reliable => {
                    let n = self
                        .counters
                        .slow_consumer_closes
                        .fetch_add(1, Ordering::Relaxed);
                    if sample_every_1024(n) {
                        tracing::warn!(%session_key, closes=%n, "reliable egress: closing slow consumer");
                    }
                    let notice =
                        sys::error(ClientCode::SlowConsumer, "outbound queue full", "").to_string();
                    conn.signals
                        .request_close(CloseRequest::Spec(CloseSpec::new(
                            close_code::POLICY,
                            "slow_consumer",
                            Some(notice),
                        )));
                }
            },
        }
    }

    /// Send to every session of a user (honours `out.qos`).
    pub fn send_to_user(&self, user_key: &str, out: Outgoing) -> Result<()> {
        let keys = self.sessions.user_session_keys(user_key);
        if keys.is_empty() {
            return Err(WsPrismError::BadRequest("user not connected".into()));
        }
        let frame = OutFrame::from_payload(&out.payload)?;
        for key in keys {
            if let Some(conn) = self.sessions.get_session(&key) {
                self.deliver(&key, &conn, frame.clone(), &out.qos);
            }
        }
        Ok(())
    }

    /// Send to a single session (honours `out.qos`).
    pub fn send_to_session(&self, session_key: &str, out: Outgoing) -> Result<()> {
        let conn = self
            .sessions
            .get_session(session_key)
            .ok_or_else(|| WsPrismError::BadRequest("session not connected".into()))?;
        let frame = OutFrame::from_payload(&out.payload)?;
        self.deliver(session_key, &conn, frame, &out.qos);
        Ok(())
    }

    /// The internal key of `room` in `tenant` (shared by sessions and the
    /// server API so both address the same room).
    pub fn room_key(tenant: &str, room: &str) -> String {
        format!("{tenant}::{room}")
    }

    /// Publish to every session in a room (honours `out.qos`); returns how
    /// many sessions it was delivered to. Members carry
    /// their delivery handles, so this is one shard read lock per publish and
    /// a `try_send` per recipient — no lookup, key copy or handle clone.
    ///
    /// With a queued broker (ADR 0011 §2), Ext frames reach local members
    /// through the broker, and the count is the local members at publish
    /// time; Hot frames are delivered locally at once and also published. A
    /// full outbox refuses Reliable Ext frames with `UNAVAILABLE` (Lossy
    /// ones are dropped).
    ///
    /// In a history room (ADR 0011 §3), a JSON object message gets the
    /// room's next `offset` (and `room`, if it has none) and is kept for
    /// recovery.
    pub fn publish_room(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        self.publish(room_key, out, None, true)
    }

    /// Every room publish: `except` skips one session, `record` = keep in
    /// history (presence events are not: replaying them after a newer
    /// snapshot would be wrong).
    fn publish(
        &self,
        room_key: &str,
        out: Outgoing,
        except: Option<&str>,
        record: bool,
    ) -> Result<usize> {
        let out = match (&self.history, record, &self.outbox) {
            (Some(h), true, None) => match h.ring(room_key) {
                Some(ring) => match self.publish_recorded(&ring, room_key, out, except)? {
                    Ok(n) => return Ok(n),
                    Err(out) => out,
                },
                None => out,
            },
            (Some(h), true, Some(outbox)) => match h.rule(room_key) {
                Some(rule) => match json_object(out) {
                    Ok((mut v, qos)) => {
                        let room = room_key.split_once("::").map_or(room_key, |(_, r)| r);
                        let frame = OutFrame::text(stamp(&mut v, None, room)?);
                        return self.route_queued(
                            outbox,
                            room_key,
                            frame,
                            qos,
                            except,
                            Some(rule.keep()),
                        );
                    }
                    Err(out) => out,
                },
                None => out,
            },
            _ => out,
        };
        let frame = OutFrame::from_payload(&out.payload)?;
        match (&self.outbox, except) {
            (None, None) => Ok(self.fan_out(room_key, &frame, &out.qos)),
            (None, Some(skip)) => Ok(self.fan_out_except(room_key, &frame, &out.qos, skip)),
            (Some(outbox), _) => self.route_queued(outbox, room_key, frame, out.qos, except, None),
        }
    }

    /// A history room's publish: offset, store and fan-out under the ring
    /// lock (a recovering join takes it too). Hands `out` back if it cannot
    /// be kept (not a JSON object: Hot, binary, raw text).
    fn publish_recorded(
        &self,
        ring: &std::sync::Mutex<Ring>,
        room_key: &str,
        out: Outgoing,
        except: Option<&str>,
    ) -> Result<std::result::Result<usize, Outgoing>> {
        let (mut v, qos) = match json_object(out) {
            Ok(x) => x,
            Err(out) => return Ok(Err(out)),
        };
        let room = room_key.split_once("::").map_or(room_key, |(_, r)| r);
        let mut r = history::lock(ring);
        let text = stamp(&mut v, Some(r.next_offset()), room)?;
        let bytes = text.len();
        let frame = OutFrame::text(text);
        r.push(frame.clone(), bytes);
        Ok(Ok(match except {
            None => self.fan_out(room_key, &frame, &qos),
            Some(skip) => self.fan_out_except(room_key, &frame, &qos, skip),
        }))
    }

    /// Every local member of the room.
    fn fan_out(&self, room_key: &str, frame: &OutFrame, qos: &QoS) -> usize {
        let mut n = 0;
        self.presence.for_each_member(room_key, |m| {
            self.deliver(&m.key, &m.conn, frame.clone(), qos);
            n += 1;
        });
        n
    }

    /// Every local member of the room but session `except`.
    fn fan_out_except(&self, room_key: &str, frame: &OutFrame, qos: &QoS, except: &str) -> usize {
        let mut n = 0;
        self.presence.for_each_member(room_key, |m| {
            if &*m.key != except {
                self.deliver(&m.key, &m.conn, frame.clone(), qos);
                n += 1;
            }
        });
        n
    }

    /// The queued-broker path of every room publish (the one place it
    /// branches off, so no room path can bypass the broker).
    #[cold]
    fn route_queued(
        &self,
        outbox: &BrokerOutbox,
        room_key: &str,
        frame: OutFrame,
        qos: QoS,
        except: Option<&str>,
        keep: Option<Keep>,
    ) -> Result<usize> {
        let hot = matches!(frame, OutFrame::Hot(_));
        let n = match (hot, except) {
            (true, None) => self.fan_out(room_key, &frame, &qos),
            (true, Some(skip)) => self.fan_out_except(room_key, &frame, &qos, skip),
            (false, _) => self.presence.member_count(room_key),
        };
        let reliable = matches!(qos, QoS::Reliable);
        let origin = self.node_slot.load(Ordering::Relaxed);
        let mut bf = BrokerFrame::new(origin, room_key, qos, frame).except(except);
        bf.keep = keep;
        match outbox.push_frame(bf) {
            Ok(()) => Ok(n),
            Err(_) if reliable && !hot => Err(WsPrismError::Unavailable(
                "cluster broker is busy; retry".into(),
            )),
            // Lossy, or Hot (already delivered here): other nodes miss it.
            Err(_) => Ok(n),
        }
    }

    /// Another node's Hot frame: room ids (`chan`) are per node (ADR 0011
    /// §5), so put in ours — once per frame, shared by every recipient.
    fn deliver_remote_hot(&self, f: &BrokerFrame, h: &HotOut) {
        let Some(chan) = self.presence.room_id(&f.room_key) else {
            return; // no members here (any more)
        };
        let mut v2 = bytes::BytesMut::from(&h.v2[..]);
        if let Some(c) = v2.get_mut(2..6) {
            c.copy_from_slice(&chan.to_le_bytes());
        }
        let hot = HotOut {
            v1: h.v1.clone(),
            v2: v2.freeze(),
            slot_key: h
                .slot_key
                .map(|(_, sender, svc, key)| (chan, sender, svc, key)),
        };
        self.fan_out(&f.room_key, &OutFrame::Hot(Arc::new(hot)), &f.qos);
    }

    /// The broker lost frames of these rooms: tell their members (`sys.gap`),
    /// who resync (ADR 0011 §4).
    fn deliver_gap(&self, rooms: &[Arc<str>]) {
        for room_key in rooms {
            let room = room_key.split_once("::").map_or(&**room_key, |(_, r)| r);
            let frame = OutFrame::text(sys::gap(room).to_string());
            self.fan_out(room_key, &frame, &QoS::Reliable);
        }
    }

    /// A frame from the broker: deliver it to this node's members.
    fn deliver_incoming(&self, f: &BrokerFrame) {
        let local = f.origin == self.node_slot.load(Ordering::Relaxed);
        if let OutFrame::Hot(h) = &f.body {
            // Our own Hot frames were delivered when published.
            if !local {
                self.deliver_remote_hot(f, h);
            }
            return;
        }
        // A kept message: the broker gave it its offset; put it in once for
        // every recipient (ADR 0011 §3).
        if let (Some(o), OutFrame::Text(t)) = (f.offset, &f.body) {
            let text = std::str::from_utf8(t.as_bytes()).unwrap_or("{}");
            let frame = OutFrame::text(prepend(&format!("\"offset\":{o}"), text));
            let skip = f.except.as_deref().filter(|_| local);
            self.presence.for_each_member(&f.room_key, |m| {
                // A recovering join already gave this member everything up
                // to `skip_upto` (this one may have been in flight then).
                if o > m.skip_upto && Some(&*m.key) != skip {
                    self.deliver(&m.key, &m.conn, frame.clone(), &f.qos);
                }
            });
            return;
        }
        match (&f.except, local) {
            (Some(skip), true) => self.fan_out_except(&f.room_key, &f.body, &f.qos, skip),
            _ => self.fan_out(&f.room_key, &f.body, &f.qos),
        };
    }

    /// Like [`RealtimeCore::publish_room`], but skipping the session
    /// `except` (e.g. the publisher of a state update).
    pub fn publish_room_except(
        &self,
        room_key: &str,
        out: Outgoing,
        except: &str,
    ) -> Result<usize> {
        self.publish(room_key, out, Some(except), true)
    }

    /// Remove a closed or evicted session from every room; with `notify`
    /// (tenant `presence_events`), tell the remaining members (ADR 0010).
    /// The one place every departure path goes through.
    pub fn remove_session_presence(
        &self,
        tenant: &str,
        user_key: &str,
        session_key: &str,
        notify: bool,
    ) {
        let departed = self.presence.cleanup_session(tenant, user_key, session_key);
        if notify {
            for d in &departed {
                self.notify_member_left(tenant, d);
            }
        }
    }

    /// `sys.member_left` to the remaining members of `d.room_key`.
    fn notify_member_left(&self, tenant: &str, d: &Departed) {
        self.notify_member(
            tenant,
            &d.room_key,
            "member_left",
            &d.user,
            d.member_id,
            None,
        );
    }

    /// A presence event to a room's members (Reliable: a lost leave would
    /// leave a ghost), optionally skipping one session.
    fn notify_member(
        &self,
        tenant: &str,
        room_key: &str,
        kind: &str,
        user: &str,
        member_id: u32,
        except: Option<&str>,
    ) {
        let prefix_len = tenant.len() + 2; // "tenant::"
        let room = room_key.get(prefix_len..).unwrap_or(room_key);
        let out = Outgoing {
            qos: QoS::Reliable,
            payload: Payload::TextJson(sys::member_event(kind, room, user, member_id)),
        };
        let _ = match except {
            Some(skip) => self.publish(room_key, out, Some(skip), false),
            None => self.publish(room_key, out, None, false),
        };
    }

    /// Publish to a room with `QoS::Lossy`, whatever `out.qos` says.
    pub fn publish_room_lossy(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Lossy,
                ..out
            },
        )
    }

    /// Publish to a room with `QoS::Reliable`, whatever `out.qos` says.
    pub fn publish_room_reliable(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Reliable,
                ..out
            },
        )
    }
}

/// A publish's payload as a JSON object (what history keeps), or the
/// publish back if it is not one (Hot, binary, raw text).
fn json_object(out: Outgoing) -> std::result::Result<(serde_json::Value, QoS), Outgoing> {
    let v = match out.payload {
        Payload::TextJson(v) if v.is_object() => v,
        Payload::Utf8Bytes(ref b) => match serde_json::from_slice::<serde_json::Value>(b) {
            Ok(v) if v.is_object() => v,
            _ => return Err(out),
        },
        _ => return Err(out),
    };
    Ok((v, out.qos))
}

/// `v` (a JSON object) as text with `"offset"` (replacing any; none if
/// `offset` is `None`) and, if it has none, `"room"`, written first. Spliced
/// into the text rather than inserted into the map: an extra `Map::insert`
/// call site changes how the compiler inlines every other one in this crate
/// (measured: +2.5% on joins, +1.7% per chat delivery; ADR 0011 R2).
fn stamp(v: &mut serde_json::Value, offset: Option<u64>, room: &str) -> Result<String> {
    let encode =
        |e: serde_json::Error| WsPrismError::BadRequest(format!("json encode failed: {e}"));
    let mut fields = Vec::with_capacity(2);
    if let Some(o) = offset {
        fields.push(format!("\"offset\":{o}"));
    }
    if let Some(obj) = v.as_object_mut() {
        obj.remove("offset");
        if !obj.contains_key("room") {
            fields.push(format!(
                "\"room\":{}",
                serde_json::to_string(room).map_err(encode)?
            ));
        }
    }
    let body = serde_json::to_string(v).map_err(encode)?;
    Ok(prepend(&fields.join(","), &body))
}

/// `body` (a JSON object's text) with `fields` written first.
fn prepend(fields: &str, body: &str) -> String {
    let rest = body.get(1..).unwrap_or("}");
    let mut out = String::with_capacity(fields.len() + body.len() + 2);
    out.push('{');
    out.push_str(fields);
    if !fields.is_empty() && rest != "}" {
        out.push(',');
    }
    out.push_str(rest);
    out
}

/// The offset a kept message carries first (`{"offset":N,...`).
fn offset_of(frame: &OutFrame) -> Option<u64> {
    let OutFrame::Text(t) = frame else {
        return None;
    };
    let rest = t.as_bytes().strip_prefix(b"{\"offset\":")?;
    let end = rest.iter().position(|b| !b.is_ascii_digit())?;
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// The broker tasks of a core ([`RealtimeCore::start`]); dropping this
/// stops them.
#[must_use = "dropping the guard stops the broker tasks"]
pub struct BrokerTasks(Vec<tokio::task::JoinHandle<()>>);

impl Drop for BrokerTasks {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

/// Frames from the broker to local members, until the core is gone.
///
/// The fan-out runs on several worker tasks (one per available core): on a
/// single node it runs on the publishers' session tasks, and one task for a
/// whole node's incoming traffic saturated at 2.16M deliveries/s (ADR 0011
/// A15). A room always goes to the same worker, so its order is kept.
async fn run_incoming(core: Weak<RealtimeCore>, mut rx: tokio::sync::mpsc::Receiver<Incoming>) {
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let txs: Vec<_> = (0..workers)
        .map(|_| {
            let (tx, rx) = tokio::sync::mpsc::channel(INCOMING_WORKER_QUEUE);
            tokio::spawn(incoming_worker(core.clone(), rx));
            tx
        })
        .collect();
    // In batches: one frame per receive left this task waiting its turn
    // among every session task between frames (tokio's per-poll budget),
    // and the broker's queue stayed full while the workers idled (A15).
    let mut batch = Vec::with_capacity(INCOMING_BATCH);
    let mut per_worker: Vec<Vec<Incoming>> = (0..workers).map(|_| Vec::new()).collect();
    while rx.recv_many(&mut batch, INCOMING_BATCH).await > 0 {
        for msg in batch.drain(..) {
            match msg {
                Incoming::Frame(f) => {
                    per_worker[room_worker(&f.room_key, workers)].push(Incoming::Frame(f));
                }
                // Per room, so it stays in order with that room's frames.
                Incoming::Gap(rooms) => {
                    for room in rooms {
                        per_worker[room_worker(&room, workers)].push(Incoming::Gap(vec![room]));
                    }
                }
            }
        }
        for (tx, msgs) in txs.iter().zip(per_worker.iter_mut()) {
            if !msgs.is_empty() && tx.send(std::mem::take(msgs)).await.is_err() {
                return; // the core is gone
            }
        }
    }
}

/// Messages taken from the broker per receive.
const INCOMING_BATCH: usize = 1024;
/// Batches queued per incoming worker.
const INCOMING_WORKER_QUEUE: usize = 64;

/// The worker for `room_key` (FNV-1a).
fn room_worker(room_key: &str, workers: usize) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in room_key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % workers as u64) as usize
}

async fn incoming_worker(
    core: Weak<RealtimeCore>,
    mut rx: tokio::sync::mpsc::Receiver<Vec<Incoming>>,
) {
    while let Some(msgs) = rx.recv().await {
        let Some(core) = core.upgrade() else { return };
        for msg in msgs {
            match msg {
                Incoming::Frame(f) => core.deliver_incoming(&f),
                Incoming::Gap(rooms) => core.deliver_gap(&rooms),
            }
        }
    }
}

/// An answer to one request, sent later from another task (Reliable, with
/// `reply_to` set like a direct reply). Dropping it without sending frees
/// the slot too; a session that ended in the meantime just ignores it.
pub struct DeferredReply {
    core: Arc<RealtimeCore>,
    conn: Connection,
    session_key: Arc<str>,
    seq: Option<u64>,
}

impl DeferredReply {
    /// Send `msg` as the answer (Reliable; `reply_to` added).
    pub fn send(self, msg: serde_json::Value) {
        let frame = OutFrame::text(sys::reply_to(msg, self.seq).to_string());
        self.core
            .deliver(&self.session_key, &self.conn, frame, &QoS::Reliable);
    }
}

impl Drop for DeferredReply {
    fn drop(&mut self) {
        self.conn.signals.end_deferred();
    }
}

fn check_publish(permissions: &Permissions, room: &str) -> Result<()> {
    if permissions.can_publish(room) {
        Ok(())
    } else {
        Err(WsPrismError::NotAllowed(
            "not allowed to publish to that room".into(),
        ))
    }
}

/// Sends on behalf of one session from any task (see
/// [`RealtimeCtx::publisher`]). Cheap to clone.
#[derive(Clone)]
pub struct Publisher {
    core: Arc<RealtimeCore>,
    tenant: Arc<str>,
    user_key: Arc<str>,
    permissions: Arc<Permissions>,
}

impl Publisher {
    /// Publish to `room`, dropped for recipients whose queue is full. Requires publish permission.
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<usize> {
        check_publish(&self.permissions, room)?;
        self.core
            .publish_room_lossy(&RealtimeCore::room_key(&self.tenant, room), out)
    }

    /// Publish to `room`; a recipient whose queue is full is closed as a slow consumer. Requires publish permission.
    pub fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<usize> {
        check_publish(&self.permissions, room)?;
        self.core
            .publish_room_reliable(&RealtimeCore::room_key(&self.tenant, room), out)
    }

    /// Send to every session of this user.
    pub fn send_to_user(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_user(&self.user_key, out)
    }
}

/// A session's view of the realtime core, handed to services.
///
/// One context lives for the whole session (owned by the session task) and
/// is lent to services as `&mut RealtimeCtx`, so services can update
/// session-scoped state such as the active room without locks, and nothing
/// is re-allocated per message.
///
/// Not `Clone`: room membership must only change on the session's own task
/// (presence relies on it). From a spawned task, answer with
/// [`RealtimeCtx::defer_reply`] and send with [`RealtimeCtx::publisher`].
pub struct RealtimeCtx {
    tenant: Arc<str>,
    user: Arc<str>,
    user_key: Arc<str>,
    session_id: Arc<str>,
    session_key: Arc<str>,
    trace_id: Arc<str>,
    active_room: Option<Arc<str>>,
    /// `seq` of the request being handled, for reply correlation.
    request_seq: Option<u64>,
    limits: Arc<RoomLimits>,
    /// This session's own delivery handles (room membership, replies).
    conn: Connection,
    /// Wire protocol of this session (reply shapes, Hot routing).
    protocol: ProtocolVersion,
    /// What this user may do with rooms (ADR 0008 §2).
    permissions: Permissions,
    /// Rooms this session has joined (small; linear scan), with what the
    /// per-message paths need precomputed.
    joined: Vec<JoinedRoom>,
    hot_echo_to_sender: bool,
    presence_events: bool,
    core: Arc<RealtimeCore>,
}

/// Frames held for one recovering join (ADR 0011 A11).
const RECOVERY_HOLD: usize = 1000;
/// How long a recovering join waits for this node's subscription.
const SUBSCRIBE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What [`RealtimeCtx::join_room_and_reply`] did.
#[derive(Debug, Clone)]
pub struct JoinOutcome {
    /// Room id and member id.
    pub joined: Joined,
    /// The room's history position, if it keeps history.
    pub history: Option<HistoryPos>,
    /// Whether the requested recovery happened (`None` = none requested).
    pub recovered: Option<bool>,
}

/// Most members listed in a `sys.joined` snapshot (ADR 0010).
pub const MAX_SNAPSHOT_MEMBERS: usize = 1000;

/// One room member as presence reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    /// User id (without the tenant).
    pub user: Arc<str>,
    /// Membership id (the member's Hot `sender`).
    pub member_id: u32,
}

/// Who else is in a room (ADR 0010 snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSnapshot {
    /// Up to [`MAX_SNAPSHOT_MEMBERS`] other members.
    pub members: Vec<MemberInfo>,
    /// True if more members were left out.
    pub truncated: bool,
}

/// A joined room as the session keeps it: the internal key and the publish
/// permission are computed once at join, so publishing into a joined room
/// (every Hot frame, every chat message) allocates nothing and scans no
/// patterns.
struct JoinedRoom {
    name: Arc<str>,
    key: Arc<str>,
    ids: Joined,
    can_publish: bool,
}

/// Who a session is, fixed for its lifetime (input to [`RealtimeCtx::new`]).
pub struct CtxIdentity {
    /// Tenant id.
    pub tenant: Arc<str>,
    /// User id.
    pub user: Arc<str>,
    /// Client-facing sid (may repeat across connections).
    pub session_id: Arc<str>,
    /// Gateway-internal, per-connection unique key (see S4 in docs/HARDENING.md).
    pub session_key: Arc<str>,
    /// Trace id (logs, `sys.*` messages).
    pub trace_id: Arc<str>,
    /// Negotiated wire protocol.
    pub protocol: ProtocolVersion,
    /// Room permissions from the authenticator.
    pub permissions: Permissions,
    /// Whether Hot state a session publishes is also sent back to it
    /// (tenant policy `hot_echo_to_sender`).
    pub hot_echo_to_sender: bool,
    /// Tell room members when this session joins or leaves (tenant policy
    /// `presence_events`, ADR 0010).
    pub presence_events: bool,
}

impl RealtimeCtx {
    /// Construct the context for one session.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn new(
        id: CtxIdentity,
        limits: Arc<RoomLimits>,
        conn: Connection,
        core: Arc<RealtimeCore>,
    ) -> Self {
        let user_key: Arc<str> = Arc::from(format!("{}::{}", id.tenant, id.user));
        Self {
            tenant: id.tenant,
            user: id.user,
            user_key,
            session_id: id.session_id,
            session_key: id.session_key,
            trace_id: id.trace_id,
            active_room: None,
            request_seq: None,
            limits,
            conn,
            protocol: id.protocol,
            permissions: id.permissions,
            joined: Vec::new(),
            hot_echo_to_sender: id.hot_echo_to_sender,
            presence_events: id.presence_events,
            core,
        }
    }

    /// Trace id of this session (also in its `sys.*` messages).
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Tenant this session belongs to.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
    /// Authenticated user id.
    pub fn user(&self) -> &str {
        &self.user
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn user_key(&self) -> &str {
        &self.user_key
    }
    /// Client-facing session id (`sid`; may repeat across connections).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }
    /// The session's active room: the Hot Lane target (see PROTOCOL.md).
    pub fn active_room(&self) -> Option<&str> {
        self.active_room.as_deref()
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn set_active_room(&mut self, room: Option<&str>) {
        self.active_room = room.map(Arc::from);
    }

    /// The internal key of `room`: cached for joined rooms, built otherwise.
    fn room_key(&self, room: &str) -> std::borrow::Cow<'_, str> {
        match self.joined.iter().find(|j| &*j.name == room) {
            Some(j) => std::borrow::Cow::Borrowed(&j.key),
            None => std::borrow::Cow::Owned(RealtimeCore::room_key(self.tenant(), room)),
        }
    }

    /// `seq` of the request currently being handled, if the client sent one.
    pub fn request_seq(&self) -> Option<u64> {
        self.request_seq
    }

    /// Set by the session around each inbound request.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn begin_request(&mut self, seq: Option<u64>) {
        self.request_seq = seq;
    }

    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn end_request(&mut self) {
        self.request_seq = None;
    }

    /// Answer the current request on this session (Reliable). Adds
    /// `reply_to: <seq>` when the request carried a `seq`, so clients can
    /// match responses to pipelined requests.
    pub fn reply(&self, msg: serde_json::Value) -> Result<()> {
        self.send_to_session(Outgoing {
            qos: QoS::Reliable,
            payload: Payload::TextJson(sys::reply_to(msg, self.request_seq)),
        })
    }

    /// Wire protocol this session negotiated (v1 / v2).
    pub fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    /// A handle that answers the current request later, from another task
    /// (e.g. after a backend call), so the session loop never waits for it.
    /// `None` if this session already has `max_outstanding` such answers
    /// pending.
    pub fn defer_reply(&self, max_outstanding: u32) -> Option<DeferredReply> {
        self.conn
            .signals
            .try_begin_deferred(max_outstanding)
            .then(|| DeferredReply {
                core: Arc::clone(&self.core),
                conn: self.conn.clone(),
                session_key: Arc::clone(&self.session_key),
                seq: self.request_seq,
            })
    }

    /// A handle for sending from another task (a timer, a backend call):
    /// room publishes and sends to this user, with the same permission
    /// checks, but no membership changes.
    pub fn publisher(&self) -> Publisher {
        Publisher {
            core: Arc::clone(&self.core),
            tenant: Arc::clone(&self.tenant),
            user_key: Arc::clone(&self.user_key),
            permissions: Arc::new(self.permissions.clone()),
        }
    }

    /// Every publish through the context is checked against the user's
    /// publish permission, whichever service does it.
    fn check_publish(&self, room: &str) -> Result<()> {
        match self.joined.iter().find(|j| &*j.name == room) {
            Some(j) if j.can_publish => Ok(()),
            Some(_) => Err(WsPrismError::NotAllowed(
                "not allowed to publish to that room".into(),
            )),
            None => check_publish(&self.permissions, room),
        }
    }

    /// Join `room`, enforcing the tenant's room limits. Returns the room id
    /// and this membership's id (protocol v2 `chan` / `sender`).
    pub fn join_room(&mut self, room: &str) -> Result<Joined> {
        self.join_room_via(room, self.conn.clone())
    }

    /// [`RealtimeCtx::join_room`], with the room's frames going to `conn`.
    fn join_room_via(&mut self, room: &str, conn: Connection) -> Result<Joined> {
        crate::limits::validate_room_name(room)?;
        if !self.permissions.can_subscribe(room) {
            return Err(WsPrismError::NotAllowed(
                "not allowed to join that room".into(),
            ));
        }
        let rk = self.room_key(room).into_owned();
        let member = Member {
            key: Arc::clone(&self.session_key),
            conn,
            member_id: 0,
            skip_upto: 0,
            user: Arc::clone(&self.user),
        };
        let (joined, new) = self.core.presence.try_join(
            self.tenant(),
            &rk,
            self.user_key(),
            member,
            &self.limits,
        )?;
        if new && self.presence_events {
            self.core.notify_member(
                &self.tenant,
                &rk,
                "member_joined",
                &self.user,
                joined.member_id,
                Some(&self.session_key),
            );
        }
        if !self.joined.iter().any(|j| &*j.name == room) {
            self.joined.push(JoinedRoom {
                name: Arc::from(room),
                key: Arc::from(rk),
                ids: joined,
                can_publish: self.permissions.can_publish(room),
            });
        }
        Ok(joined)
    }

    /// Join `room` and answer the request with `reply(ctx, outcome)`; in a
    /// history room, resume from `recover` (ADR 0011 §3): the reply, then the
    /// missed messages in order, all before any newer message of the room.
    /// Without history, or when resuming is impossible, `recovered` is
    /// false (the client resyncs from its own backend).
    /// SPI (unstable): for the built-in room service.
    #[doc(hidden)]
    pub fn join_room_and_reply(
        &mut self,
        room: &str,
        recover: Option<&Recover>,
        reply: impl FnOnce(&RealtimeCtx, &JoinOutcome) -> serde_json::Value,
    ) -> Result<Joined> {
        crate::limits::validate_room_name(room)?;
        let ring = match (&self.core.history, &self.core.outbox) {
            (Some(h), None) => h.ring(&RealtimeCore::room_key(self.tenant(), room)),
            _ => None,
        };
        let Some(ring) = ring else {
            let joined = self.join_room(room)?;
            let outcome = JoinOutcome {
                joined,
                history: None,
                recovered: recover.map(|_| false),
            };
            self.reply(reply(self, &outcome))?;
            return Ok(joined);
        };
        // Held until the replay is queued: no publish to this room can
        // slip in between (or be replayed and delivered live as well).
        let mut r = history::lock(&ring);
        let joined = self.join_room(room)?;
        let replay = recover.and_then(|from| r.since(from));
        let outcome = JoinOutcome {
            joined,
            history: Some(r.pos()),
            recovered: recover.map(|_| replay.is_some()),
        };
        self.reply(reply(self, &outcome))?;
        // One queue slot for the whole replay: a slow consumer, closed
        // because its queue overflowed, missed more than the queue holds and
        // must still be able to resume. Bounded by the history's limits.
        if let Some(frames) = replay.filter(|f| !f.is_empty()) {
            let batch = OutFrame::Batch(frames.into());
            self.core
                .deliver(&self.session_key, &self.conn, batch, &QoS::Reliable);
        }
        Ok(joined)
    }

    /// The history limits of `room` if a queued broker keeps its history
    /// (then join with [`RealtimeCtx::join_room_recovering`]).
    /// SPI (unstable): for the built-in room service.
    #[doc(hidden)]
    pub fn cluster_history(&self, room: &str) -> Option<Keep> {
        self.core.outbox.as_ref()?;
        self.core
            .history
            .as_ref()?
            .rule(&RealtimeCore::room_key(self.tenant(), room))
            .map(|r| r.keep())
    }

    /// [`RealtimeCtx::join_room_and_reply`] for a room whose history a
    /// queued broker keeps (ADR 0011 §3): reading it is a round trip, so
    /// the room's frames are held aside meanwhile (at most 1,000; more =
    /// `sys.gap`). Then the reply, the replay,
    /// and the held frames the replay did not cover, in that order.
    /// SPI (unstable): for the built-in room service.
    #[doc(hidden)]
    pub async fn join_room_recovering(
        &mut self,
        room: &str,
        keep: Keep,
        recover: Option<&Recover>,
        reply: impl FnOnce(&RealtimeCtx, &JoinOutcome) -> serde_json::Value,
    ) -> Result<Joined> {
        let (tx, mut held) = tokio::sync::mpsc::channel(RECOVERY_HOLD);
        let hold = Connection {
            tx,
            signals: SessionSignals::new(),
        };
        let joined = self.join_room_via(room, hold.clone())?;
        let rk = self.room_key(room).into_owned();
        // History read before this node receives the room would miss what
        // is published in between.
        if let Some(mut subscribed) = self.core.presence.subscribed(&rk) {
            let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed.wait_for(|s| *s)).await;
        }
        let (history, replay) = match self.core.broker.history(&rk, &keep, recover).await {
            Ok(r) => (
                Some(HistoryPos {
                    epoch: r.epoch,
                    offset: r.offset,
                }),
                r.replay,
            ),
            Err(e) => {
                tracing::warn!(%room, "room history unavailable: {e}");
                (None, None)
            }
        };
        let outcome = JoinOutcome {
            joined,
            history: history.clone(),
            recovered: recover.map(|_| replay.is_some()),
        };
        self.reply(reply(self, &outcome))?;
        // The client has everything up to here: replayed, or skipped as
        // older than its starting position.
        let upto = history.as_ref().map_or(0, |p| p.offset);
        let stamped: Vec<OutFrame> = replay
            .unwrap_or_default()
            .iter()
            .filter_map(|f| match (&f.body, f.offset) {
                (OutFrame::Text(t), Some(o)) => Some(OutFrame::text(prepend(
                    &format!("\"offset\":{o}"),
                    std::str::from_utf8(t.as_bytes()).unwrap_or("{}"),
                ))),
                _ => None,
            })
            .collect();
        if !stamped.is_empty() {
            self.core.deliver(
                &self.session_key,
                &self.conn,
                OutFrame::Batch(stamped.into()),
                &QoS::Reliable,
            );
        }
        let (core, key, real) = (&self.core, &*self.session_key, self.conn.clone());
        core.presence
            .replace_member_conn(&rk, key, real.clone(), upto, || {
                while let Ok(f) = held.try_recv() {
                    if offset_of(&f).is_some_and(|o| o <= upto) {
                        continue;
                    }
                    core.deliver(key, &real, f, &QoS::Reliable);
                }
            });
        if hold.signals.close_requested() {
            // Held frames overflowed: some are lost.
            let gap = OutFrame::text(sys::gap(room).to_string());
            core.deliver(key, &real, gap, &QoS::Reliable);
        }
        Ok(joined)
    }

    /// The room a Hot frame targets and this session's membership in it:
    /// v2 names the room id (`chan`) and must be a room this session joined
    /// (ADR 0006 R5, R7); v1 targets the active room.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn hot_target(&self, frame: &HotFrame) -> Option<(&str, Joined)> {
        let found = match frame.chan {
            Some(chan) => self.joined.iter().find(|j| j.ids.room_id == chan),
            None => {
                let active = self.active_room()?;
                self.joined.iter().find(|j| &*j.name == active)
            }
        };
        found.map(|j| (&*j.name, j.ids))
    }

    /// True if this session has joined `room`.
    pub fn is_in_room(&self, room: &str) -> bool {
        let rk = self.room_key(room);
        self.core.presence.is_member(&rk, self.session_key())
    }

    /// Leave `room` (no-op if not joined).
    pub fn leave_room(&mut self, room: &str) {
        if let Some(j) = self.joined.iter().find(|j| &*j.name == room) {
            // Nothing from the room may reach the client after its sys.left.
            self.conn.signals.hot.purge_room(j.ids.room_id);
        }
        let rk = self.room_key(room).into_owned();
        self.joined.retain(|j| &*j.name != room);
        let departed =
            self.core
                .presence
                .leave(self.tenant(), &rk, self.user_key(), self.session_key());
        if let (Some(d), true) = (departed, self.presence_events) {
            self.core.notify_member_left(&self.tenant, &d);
        }
    }

    /// With tenant `presence_events`: the other members of `room` for the
    /// `sys.joined` snapshot (at most [`MAX_SNAPSHOT_MEMBERS`]; the flag says
    /// whether some were left out). `None` when presence events are off.
    pub fn room_members(&self, room: &str) -> Option<MemberSnapshot> {
        self.presence_events.then(|| {
            let (list, truncated) = self.core.presence.members_except(
                &self.room_key(room),
                &self.session_key,
                MAX_SNAPSHOT_MEMBERS,
            );
            MemberSnapshot {
                members: list
                    .into_iter()
                    .map(|(user, member_id)| MemberInfo { user, member_id })
                    .collect(),
                truncated,
            }
        })
    }

    /// Send to every session of this user (Reliable or Lossy per `out.qos`).
    pub fn send_to_user(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_user(self.user_key(), out)
    }
    /// Send to this session, through its own handles (no registry lookup).
    pub fn send_to_session(&self, out: Outgoing) -> Result<()> {
        let frame = OutFrame::from_payload(&out.payload)?;
        self.core
            .deliver(self.session_key(), &self.conn, frame, &out.qos);
        Ok(())
    }
    /// Publish to `room`, dropped for recipients whose queue is full. Requires publish permission.
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<()> {
        self.check_publish(room)?;
        let rk = self.room_key(room);
        self.core.publish_room_lossy(&rk, out).map(|_| ())
    }
    /// Publish to `room`; a recipient whose queue is full is closed as a slow consumer. Requires publish permission.
    pub fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<()> {
        self.check_publish(room)?;
        let rk = self.room_key(room);
        self.core.publish_room_reliable(&rk, out).map(|_| ())
    }

    /// Publish to everyone in `room` except this session (QoS as given in
    /// `out`). Requires publish permission.
    pub fn publish_room_others(&self, room: &str, out: Outgoing) -> Result<()> {
        self.check_publish(room)?;
        let rk = self.room_key(room);
        self.core
            .publish_room_except(&rk, out, self.session_key())
            .map(|_| ())
    }

    /// Replace the session's room permissions (in-band token refresh, ADR
    /// 0012). Returns the joined rooms the new permissions no longer allow
    /// subscribing to; the caller leaves them.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn set_permissions(&mut self, permissions: Permissions) -> Vec<String> {
        for j in &mut self.joined {
            j.can_publish = permissions.can_publish(&j.name);
        }
        let lost = self
            .joined
            .iter()
            .filter(|j| !permissions.can_subscribe(&j.name))
            .map(|j| j.name.to_string())
            .collect();
        self.permissions = permissions;
        lost
    }

    /// Tenant policy: should Hot state a session publishes come back to it?
    pub fn hot_echo_to_sender(&self) -> bool {
        self.hot_echo_to_sender
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::realtime::core::SessionSignals;
    use tokio::sync::mpsc;

    fn ctx(
        core: &Arc<RealtimeCore>,
        permissions: Permissions,
    ) -> (RealtimeCtx, mpsc::Receiver<OutFrame>) {
        let (tx, rx) = mpsc::channel(8);
        let conn = Connection {
            tx,
            signals: SessionSignals::new(),
        };
        let id = CtxIdentity {
            tenant: "t".into(),
            user: "u".into(),
            session_id: "s".into(),
            session_key: "t::u::s#1".into(),
            trace_id: "tr".into(),
            protocol: ProtocolVersion::V2,
            permissions,
            hot_echo_to_sender: true,
            presence_events: false,
        };
        (
            RealtimeCtx::new(id, Arc::default(), conn, Arc::clone(core)),
            rx,
        )
    }

    fn text(s: &str) -> Outgoing {
        Outgoing {
            qos: QoS::Reliable,
            payload: Payload::TextJson(serde_json::json!(s)),
        }
    }

    #[test]
    fn stamp_puts_offset_first_and_room_only_if_missing() {
        let st = |v: serde_json::Value| {
            let mut v = v;
            let t = stamp(&mut v, Some(7), "chat:1").unwrap();
            (
                t.clone(),
                serde_json::from_str::<serde_json::Value>(&t).unwrap(),
            )
        };
        let (t, _) = st(serde_json::json!({}));
        assert_eq!(t, r#"{"offset":7,"room":"chat:1"}"#);
        let (t, v) = st(serde_json::json!({"type": "m", "offset": 99}));
        assert_eq!(t, r#"{"offset":7,"room":"chat:1","type":"m"}"#);
        assert_eq!(v["offset"], 7, "a payload's own offset is replaced");
        let (t, _) = st(serde_json::json!({"room": "other", "a": "\"q"}));
        assert_eq!(t, r#"{"offset":7,"a":"\"q","room":"other"}"#);
        // Without an offset (a cluster broker gives it later): room only,
        // and a payload's own offset still removed.
        let mut v = serde_json::json!({"offset": 1, "x": 2});
        let t = stamp(&mut v, None, "r").unwrap();
        assert_eq!(t, r#"{"room":"r","x":2}"#);
        assert_eq!(
            prepend("\"offset\":9", &t),
            r#"{"offset":9,"room":"r","x":2}"#
        );
        assert_eq!(prepend("\"offset\":9", "{}"), r#"{"offset":9}"#);
        assert_eq!(
            offset_of(&OutFrame::text(prepend("\"offset\":42", &t))),
            Some(42)
        );
        assert_eq!(offset_of(&OutFrame::text(t)), None);
    }

    #[tokio::test]
    async fn leaving_a_room_drops_its_pending_state() {
        let core = Arc::new(RealtimeCore::new());
        let (mut c, _rx) = ctx(&core, Permissions::default());
        let j = c.join_room("arena").unwrap();
        let hot = Arc::new(crate::realtime::types::HotOut {
            v1: bytes::Bytes::from_static(b"x"),
            v2: bytes::Bytes::from_static(b"x"),
            slot_key: Some((j.room_id, 7, 1, 0)),
        });
        c.publish_room_lossy(
            "arena",
            Outgoing {
                qos: QoS::Lossy,
                payload: Payload::Hot(hot),
            },
        )
        .unwrap();
        c.leave_room("arena");
        let mut out = Vec::new();
        c.conn.signals.hot.drain_into(&mut out);
        assert!(out.is_empty(), "no state from a left room");
    }

    #[tokio::test]
    async fn publisher_sends_from_another_task_with_the_same_permission_checks() {
        let core = Arc::new(RealtimeCore::new());
        let (mut c, mut rx) = ctx(&core, Permissions::restricted(&["*"], &["open"]));
        c.join_room("open").unwrap();
        c.join_room("closed").unwrap();
        let p = c.publisher();
        let n = tokio::spawn(async move {
            assert!(
                p.publish_room_reliable("closed", text("x")).is_err(),
                "no publish permission"
            );
            p.publish_room_reliable("open", text("hi")).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(rx.recv().await, Some(OutFrame::text("\"hi\"".into())));
    }
}
