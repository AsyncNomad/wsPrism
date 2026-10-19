//! Deterministic engine workload for instruction counting (ADR 0011 B2).
//!
//! Runs one scenario through the paths a session uses (dispatcher ->
//! built-in services -> core) with no sockets, threads or runtime. Only the
//! code inside [`measured`] is meant to be counted:
//!
//! ```text
//! valgrind --tool=callgrind --toggle-collect='*icount::measured*' \
//!     target/profiling/examples/icount <scenario>
//! ```
//!
//! Prints `units=<n>` (deliveries, or operations for churn/disconnect) so the
//! caller can divide the collected instruction total. `bench/run-icount.sh`
//! does all of this.

use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::mpsc;

use wsprism_engine::services::{ChatService, HotRelayService, RoomService};
use wsprism_engine::spi::{
    Connection, CtxIdentity, Dispatcher, OutFrame, RealtimeCore, SessionSignals,
};
use wsprism_engine::{Outgoing, Payload, Permissions, QoS, RealtimeCtx};
use wsprism_protocol::protocol::hot::{decode_hot_frame_v2, HOT_V2_FLAG_KEY, KIND_HOT};
use wsprism_protocol::protocol::text::decode_envelope_v2;
use wsprism_protocol::protocol::ProtocolVersion;

const MEMBERS: usize = 100;
const ROOM: &str = "arena";
/// Publishes between two (unmeasured) drains; queues hold twice that.
const BATCH: usize = 64;
const ROUNDS: usize = 16; // 1024 publishes per fan-out scenario

/// The counted region. Never inlined, so callgrind can toggle on its name.
#[inline(never)]
fn measured<R>(f: impl FnOnce() -> R) -> R {
    black_box(f())
}

/// Services complete without awaiting anything real; anything else is a bug
/// in this harness.
fn ready<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("service future did not complete synchronously"),
    }
}

struct Session {
    ctx: RealtimeCtx,
    rx: mpsc::Receiver<OutFrame>,
    signals: Arc<SessionSignals>,
}

struct World {
    core: Arc<RealtimeCore>,
    dispatcher: Dispatcher,
    sessions: Vec<Session>,
}

impl World {
    fn new(presence_events: bool) -> Self {
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_text(Arc::new(RoomService::new()));
        dispatcher.register_text(Arc::new(ChatService::new()));
        dispatcher.register_hot(Arc::new(HotRelayService::new(1)));
        let mut w = Self {
            core: Arc::new(RealtimeCore::new()),
            dispatcher,
            sessions: Vec::new(),
        };
        for i in 0..MEMBERS {
            let s = w.session(i, presence_events);
            w.sessions.push(s);
            w.text(
                i,
                &format!(r#"{{"svc":"room","type":"join","room":"{ROOM}"}}"#),
            );
        }
        w.drain();
        w
    }

    fn session(&self, i: usize, presence_events: bool) -> Session {
        let (tx, rx) = mpsc::channel(BATCH * 4);
        let signals = SessionSignals::new();
        let conn = Connection {
            tx,
            signals: Arc::clone(&signals),
        };
        let id = CtxIdentity {
            tenant: "t".into(),
            user: format!("u{i}").into(),
            session_id: "s".into(),
            session_key: format!("t::u{i}::s#{i}").into(),
            trace_id: "tr".into(),
            protocol: ProtocolVersion::V2,
            permissions: Permissions::default(),
            hot_echo_to_sender: false,
            presence_events,
        };
        let ctx = RealtimeCtx::new(id, Arc::default(), conn, Arc::clone(&self.core));
        Session { ctx, rx, signals }
    }

    fn text(&mut self, i: usize, json: &str) {
        let env = decode_envelope_v2(json).expect("envelope");
        ready(
            self.dispatcher
                .dispatch_text(&mut self.sessions[i].ctx, env),
        )
        .expect("dispatch");
    }

    fn hot(&mut self, i: usize, frame: Bytes) {
        let f = decode_hot_frame_v2(frame).expect("hot frame");
        ready(self.dispatcher.dispatch_hot(&mut self.sessions[i].ctx, f)).expect("dispatch");
    }

    /// Empty every queue and Hot slot; returns how many frames arrived.
    fn drain(&mut self) -> u64 {
        let mut n = 0;
        let mut slot = Vec::new();
        for s in &mut self.sessions {
            while s.rx.try_recv().is_ok() {
                n += 1;
            }
            s.signals.hot.drain_into(&mut slot);
            n += slot.len() as u64;
            slot.clear();
        }
        n
    }

    fn room_id(&self) -> u32 {
        self.core
            .presence
            .room_id(&RealtimeCore::room_key("t", ROOM))
            .expect("room")
    }
}

/// Client Hot frame v2: `[0x01][flags][chan][svc][op][key?][payload]`.
fn hot_frame(chan: u32, key: Option<u32>) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(KIND_HOT);
    b.put_u8(if key.is_some() { HOT_V2_FLAG_KEY } else { 0 });
    b.put_u32_le(chan);
    b.put_u8(1);
    b.put_u8(1);
    if let Some(k) = key {
        b.put_u32_le(k);
    }
    b.put_slice(&[7u8; 32]);
    b.freeze()
}

/// Fan-out: member 0 publishes, everyone else receives.
fn fanout(mut w: World, mut publish: impl FnMut(&mut World, usize)) -> u64 {
    let mut units = 0;
    for r in 0..ROUNDS {
        measured(|| {
            for i in 0..BATCH {
                publish(&mut w, r * BATCH + i);
            }
        });
        units += w.drain();
    }
    units
}

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_default();
    let units = match scenario.as_str() {
        // chat.send through the dispatcher (Reliable text, sender included).
        "ext" => fanout(World::new(false), |w, _| {
            w.text(
                0,
                r#"{"svc":"chat","type":"send","room":"arena","data":{"msg":"hello"}}"#,
            )
        }),
        // Hot relay without a key (Lossy, straight to the queue).
        "hot" => {
            let w = World::new(false);
            let frame = hot_frame(w.room_id(), None);
            fanout(w, move |w, _| w.hot(0, frame.clone()))
        }
        // Hot relay with a key (latest-value slot); distinct keys per batch.
        "hot_keyed" => {
            let w = World::new(false);
            let chan = w.room_id();
            fanout(w, move |w, n| {
                w.hot(0, hot_frame(chan, Some((n % BATCH) as u32)))
            })
        }
        // Server-side publish (what the server API's publish calls).
        "api" => {
            let room_key = RealtimeCore::room_key("t", ROOM);
            fanout(World::new(false), move |w, _| {
                let out = Outgoing {
                    qos: QoS::Reliable,
                    payload: Payload::Utf8Bytes(Bytes::from_static(b"{\"svc\":\"news\"}")),
                };
                w.core.publish_room(&room_key, out).expect("publish");
            })
        }
        // Join + leave by one more session, with presence events and
        // snapshot (ADR 0010) to the 100 members. Unit: one join+leave.
        "churn" => {
            let mut w = World::new(true);
            let extra = w.session(MEMBERS, true);
            w.sessions.push(extra);
            let cycles = 1024;
            for _ in 0..cycles / 8 {
                measured(|| {
                    for _ in 0..8 {
                        w.text(MEMBERS, r#"{"svc":"room","type":"join","room":"arena"}"#);
                        w.text(MEMBERS, r#"{"svc":"room","type":"leave","room":"arena"}"#);
                    }
                });
                w.drain();
            }
            cycles as u64
        }
        // A session in 4 rooms disconnects (presence events on). Unit: one
        // disconnect.
        "disconnect" => {
            let mut w = World::new(true);
            let ops = 1024;
            for n in 0..ops {
                let i = MEMBERS + n;
                let s = w.session(i, true);
                w.sessions.push(s);
                for room in [ROOM, "b", "c", "d"] {
                    w.text(
                        i,
                        &format!(r#"{{"svc":"room","type":"join","room":"{room}"}}"#),
                    );
                }
                w.drain();
                let user_key = format!("t::u{i}");
                let session_key = format!("t::u{i}::s#{i}");
                measured(|| {
                    w.core
                        .remove_session_presence("t", &user_key, &session_key, true)
                });
                w.drain();
            }
            ops as u64
        }
        other => {
            eprintln!(
                "unknown scenario {other:?}: ext | hot | hot_keyed | api | churn | disconnect"
            );
            std::process::exit(2);
        }
    };
    println!("units={units}");
}
