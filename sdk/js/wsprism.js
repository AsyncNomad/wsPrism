// wsPrism JavaScript client (ADR 0008 §7). One dependency-free ES module
// for browsers and Node 22+ (global WebSocket). Protocol v2 only.
//
//   const client = new WsPrismClient({
//     url: "wss://rt.example.com/v1/ws",
//     tenant: "acme",
//     getTicket: () => fetch("/api/realtime-ticket").then(r => r.text()),
//   });
//   client.on("message", (m) => ...);          // room broadcasts, server publishes
//   client.on("state", (s) => ...);            // Hot frames (latest per sender+key)
//   await client.connect();
//   const { chan } = await client.join("match:42");
//   client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
//   const answer = await client.request("game", "move", { room: "match:42", data: {...} });
//   client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: bytes });
//
// Reconnection follows the v2 close-code ranges (PROTOCOL.md §7): 40xx stop
// (except 4001 auth_expired, retried with a fresh ticket), 41xx back off
// with jitter, 42xx reconnect at once; network failures back off. Joined
// rooms are re-joined after every reconnect. `getTicket` is called for
// every connection attempt, because tickets are single-use.

export const PROTOCOL = "wsprism.v2.json";
const AUTH_PREFIX = "wsprism.auth.";

const HOT_KIND = 0x01;
/** The gateway accepts at most one sys.refresh per second per connection. */
const MIN_REFRESH_GAP_MS = 1100;
const F_KEY = 0x01;
const F_SEQ = 0x02;
const F_SENDER = 0x04;

/** An error answered by the gateway or a backend (`sys.error`). */
export class WsPrismError extends Error {
  constructor(code, message) {
    super(message || code);
    this.name = "WsPrismError";
    this.code = code;
  }
}

/** What to do after a close with `code`: "stop" | "backoff" | "now". */
export function reconnectPolicy(code) {
  if (code === 4001) return "now"; // expired credential: a fresh ticket fixes it
  if (code >= 4000 && code <= 4099) return "stop";
  if (code >= 4100 && code <= 4199) return "backoff";
  if (code >= 4200 && code <= 4299) return "now";
  return "backoff"; // network failure, rejected handshake, unknown
}

/** True if u32 serial `a` is newer than `b` (RFC 1982 style wraparound). */
export function seqNewer(a, b) {
  const d = (a - b) >>> 0;
  return d !== 0 && d < 0x80000000;
}

/** Encode a client v2 Hot frame. */
export function encodeHot({ chan, svc, op, key, seq, payload }) {
  const body = payload instanceof Uint8Array ? payload : new Uint8Array(payload ?? 0);
  let flags = 0;
  let len = 1 + 1 + 4 + 1 + 1;
  if (key !== undefined && key !== null) {
    flags |= F_KEY;
    len += 4;
  }
  if (seq !== undefined && seq !== null) {
    flags |= F_SEQ;
    len += 4;
  }
  const buf = new Uint8Array(len + body.length);
  const dv = new DataView(buf.buffer);
  let o = 0;
  dv.setUint8(o++, HOT_KIND);
  dv.setUint8(o++, flags);
  dv.setUint32(o, chan >>> 0, true);
  o += 4;
  dv.setUint8(o++, svc);
  dv.setUint8(o++, op);
  if (flags & F_KEY) {
    dv.setUint32(o, key >>> 0, true);
    o += 4;
  }
  if (flags & F_SEQ) {
    dv.setUint32(o, seq >>> 0, true);
    o += 4;
  }
  buf.set(body, o);
  return buf;
}

/** Decode a server v2 Hot frame; `null` if malformed. */
export function decodeHot(data) {
  const buf = data instanceof Uint8Array ? data : new Uint8Array(data);
  if (buf.length < 8 || buf[0] !== HOT_KIND) return null;
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const flags = buf[1];
  let o = 2;
  const chan = dv.getUint32(o, true);
  o += 4;
  const svc = buf[o++];
  const op = buf[o++];
  const need = o + (flags & F_KEY ? 4 : 0) + (flags & F_SEQ ? 4 : 0) + (flags & F_SENDER ? 4 : 0);
  if (buf.length < need) return null;
  let key;
  let seq;
  let sender;
  if (flags & F_KEY) {
    key = dv.getUint32(o, true);
    o += 4;
  }
  if (flags & F_SEQ) {
    seq = dv.getUint32(o, true);
    o += 4;
  }
  if (flags & F_SENDER) {
    sender = dv.getUint32(o, true);
    o += 4;
  }
  return { chan, svc, op, key, seq, sender, payload: buf.subarray(o) };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Join errors worth another try on the same connection. */
const PASSING = new Set(["UNAVAILABLE", "RATE_LIMITED", "TIMEOUT", "INTERNAL"]);
/** The connection went away: the next one rejoins every room. */
const CONNECTION_GONE = new Set(["DISCONNECTED", "NOT_CONNECTED"]);

export class WsPrismClient {
  /**
   * @param {object} opts
   * @param {string | string[]} opts.url  ws(s)://host/v1/ws, or several nodes:
   *   reconnects move on to the next one (no load balancer needed)
   * @param {string} opts.tenant
   * @param {() => string | Promise<string>} opts.getTicket  called per connection attempt
   * @param {string} [opts.sid]
   * @param {number} [opts.requestTimeoutMs=10000]
   * @param {number} [opts.connectTimeoutMs=10000]  give up an attempt not
   *   admitted (`sys.authed`) by then: close it and retry like any failed one
   *   (a stalled server or proxy, a half-open connection)
   * @param {{minDelayMs?: number, maxDelayMs?: number, enabled?: boolean}} [opts.reconnect]
   * @param {typeof WebSocket} [opts.WebSocket]  implementation (default: global)
   * @param {boolean} [opts.refresh=true]  renew expiring credentials in-band (sys.refresh)
   */
  constructor(opts) {
    const urls = Array.isArray(opts?.url) ? opts.url : [opts?.url];
    if (!urls.length || !urls.every((u) => typeof u === "string" && u) || !opts?.tenant || typeof opts.getTicket !== "function") {
      throw new TypeError("url, tenant and getTicket are required");
    }
    this.opts = opts;
    this.urls = urls;
    this.urlIndex = 0;
    this.requestTimeoutMs = opts.requestTimeoutMs ?? 10000;
    this.connectTimeoutMs = opts.connectTimeoutMs ?? 10000;
    this.reconnect = { enabled: true, minDelayMs: 1000, maxDelayMs: 30000, ...opts.reconnect };
    this.WS = opts.WebSocket ?? globalThis.WebSocket;
    if (!this.WS) throw new TypeError("no WebSocket implementation (Node 22+ or pass opts.WebSocket)");

    this.ws = null;
    this.user = null;
    this.state = "idle"; // idle | connecting | open | closed
    this.listeners = new Map();
    this.nextSeq = 1;
    this.pending = new Map(); // seq -> {resolve, reject, timer}
    this.rooms = new Map(); // room -> {chan, memberId} (null until joined)
    this.chanRoom = new Map(); // chan -> room
    this.lastSeen = new Map(); // `${chan}:${sender}:${svc}:${key}` -> seq (the server's slot key)
    this.stateSeq = new Map(); // `${room}:${svc}:${key}` -> next seq to send
    this.attempt = 0;
    this.stopped = false;
    this.refreshTimer = null;
  }

  /** Subscribe to "open" | "close" | "message" | "state" | "presence" | "gap" | "refreshed" | "error" | "reconnecting" | "rejoined". */
  on(event, fn) {
    if (!this.listeners.has(event)) this.listeners.set(event, new Set());
    this.listeners.get(event).add(fn);
    return () => this.listeners.get(event).delete(fn);
  }

  emit(event, arg) {
    for (const fn of this.listeners.get(event) ?? []) {
      try {
        fn(arg);
      } catch (e) {
        if (event !== "error") this.emit("error", e);
      }
    }
  }

  /** Connect; resolves with the `sys.authed` data once admitted. */
  connect() {
    this.stopped = false;
    return this.open();
  }

  async open() {
    this.state = "connecting";
    let ticket;
    try {
      ticket = await this.opts.getTicket();
    } catch (e) {
      // No socket was created, so no close event will follow: the caller
      // (connect or scheduleReconnect) decides what happens next.
      this.state = "closed";
      throw new WsPrismError("TICKET_FAILED", `getTicket failed: ${e?.message ?? e}`);
    }
    const q = new URLSearchParams({ tenant: this.opts.tenant });
    if (this.opts.sid) q.set("sid", this.opts.sid);
    const base = this.urls[this.urlIndex % this.urls.length];
    const url = `${base}${base.includes("?") ? "&" : "?"}${q}`;
    return new Promise((resolve, reject) => {
      let settled = false;
      const ws = new this.WS(url, [PROTOCOL, AUTH_PREFIX + ticket]);
      ws.binaryType = "arraybuffer";
      this.ws = ws;
      // End an attempt that is not admitted in time, or that reported an
      // error without a close event: as if the socket had closed (1006), so
      // the reconnect policy runs. Every later event of this socket is ignored.
      const abandon = (code, why) => {
        if (settled) return;
        settled = true;
        clearTimeout(deadline);
        clearTimeout(noClose);
        ws.onmessage = ws.onclose = ws.onerror = null;
        try {
          ws.close();
        } catch {}
        this.emit("error", new WsPrismError(code, why));
        reject(new WsPrismError("CONNECT_FAILED", why));
        this.onClose(1006, why, false);
      };
      const deadline = setTimeout(
        () => abandon("CONNECT_TIMEOUT", `not admitted within ${this.connectTimeoutMs} ms`),
        this.connectTimeoutMs,
      );
      let noClose = null;
      ws.onmessage = (ev) => {
        if (typeof ev.data === "string") {
          let msg;
          try {
            msg = JSON.parse(ev.data);
          } catch {
            return;
          }
          if (!settled && msg.svc === "sys" && msg.type === "authed") {
            settled = true;
            clearTimeout(deadline);
            clearTimeout(noClose);
            this.state = "open";
            this.attempt = 0;
            this.user = msg.data?.user ?? null;
            this.emit("open", msg.data);
            this.scheduleRefresh(msg.data?.expires_in_s);
            resolve(msg.data);
            this.rejoin();
            return;
          }
          this.onText(msg);
        } else {
          this.onBinary(ev.data);
        }
      };
      ws.onclose = (ev) => {
        clearTimeout(deadline);
        clearTimeout(noClose);
        const wasOpen = settled;
        if (!settled) {
          settled = true;
          reject(new WsPrismError("CONNECT_FAILED", `connection closed (${ev.code})`));
        }
        this.onClose(ev.code, ev.reason, wasOpen);
      };
      // Normally followed by close; some implementations skip it for a
      // refused handshake (e.g. 503): give it a second.
      ws.onerror = () => {
        if (!settled && !noClose) {
          noClose = setTimeout(() => abandon("CONNECT_ERROR", "connection error without a close"), 1000);
        }
      };
      ws.onopen = () => {
        // The gateway echoes the version it chose; this client speaks v2 only.
        if (ws.protocol !== PROTOCOL) ws.close(1002, "protocol");
      };
    });
  }

  onText(msg) {
    const seq = msg.reply_to;
    if (seq !== undefined && this.pending.has(seq)) {
      const p = this.pending.get(seq);
      this.pending.delete(seq);
      clearTimeout(p.timer);
      if (msg.svc === "sys" && msg.type === "error") {
        p.reject(new WsPrismError(msg.data?.code, msg.data?.msg));
      } else {
        p.resolve(msg);
      }
      return;
    }
    if (msg.svc === "sys" && msg.type === "error") {
      this.emit("error", new WsPrismError(msg.data?.code, msg.data?.msg));
      return;
    }
    // A cluster node may have lost this room's messages (ADR 0011 §4):
    // the app reloads the room's state from its backend.
    if (msg.svc === "sys" && msg.type === "gap") {
      // With an epoch: the room's history was lost and offsets restart
      // there; start counting again (or newer messages would look seen).
      const info = this.rooms.get(msg.room);
      if (info && typeof msg.epoch === "string") {
        info.epoch = msg.epoch;
        info.offset = 0;
      }
      this.emit("gap", { room: msg.room, epoch: msg.epoch });
      return;
    }
    if (msg.svc === "sys" && (msg.type === "member_joined" || msg.type === "member_left")) {
      const info = this.rooms.get(msg.room);
      const memberId = msg.data?.member_id;
      if (msg.type === "member_left" && info) this.forgetSender(info.chan, memberId);
      this.emit("presence", {
        type: msg.type === "member_joined" ? "joined" : "left",
        room: msg.room,
        user: msg.data?.user,
        memberId,
      });
      return;
    }
    // History rooms (ADR 0011 §3): remember the position; drop anything
    // already seen (a replay can overlap what arrived before a reconnect).
    if (typeof msg.offset === "number" && msg.room !== undefined) {
      const info = this.rooms.get(msg.room);
      if (info && info.epoch !== undefined) {
        if (msg.offset <= info.offset) return;
        info.offset = msg.offset;
      }
    }
    this.emit("message", msg); // includes sys.kicked before a close
  }

  onBinary(data) {
    const f = decodeHot(data);
    if (!f) return;
    if (f.seq !== undefined) {
      const k = `${f.chan}:${f.sender ?? 0}:${f.svc}:${f.key ?? "-"}`;
      const last = this.lastSeen.get(k);
      if (last !== undefined && !seqNewer(f.seq, last)) return; // stale or duplicate
      this.lastSeen.set(k, f.seq);
    }
    this.emit("state", { ...f, room: this.chanRoom.get(f.chan) });
  }

  onClose(code, reason, wasOpen) {
    this.ws = null;
    clearTimeout(this.refreshTimer);
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(new WsPrismError("DISCONNECTED", `connection closed (${code})`));
    }
    this.pending.clear();
    this.chanRoom.clear();
    this.lastSeen.clear();
    const policy = this.stopped || !this.reconnect.enabled ? "stop" : reconnectPolicy(code);
    this.state = policy === "stop" ? "closed" : "connecting";
    this.emit("close", { code, reason, wasOpen, willReconnect: policy !== "stop" });
    if (policy !== "stop") this.scheduleReconnect(policy);
  }

  async scheduleReconnect(policy) {
    this.attempt += 1;
    this.urlIndex += 1; // with several nodes, try the next one
    let delay = 0;
    if (policy === "backoff" || this.attempt > 1) {
      const { minDelayMs, maxDelayMs } = this.reconnect;
      const cap = Math.min(maxDelayMs, minDelayMs * 2 ** Math.min(this.attempt - 1, 16));
      delay = cap / 2 + Math.random() * (cap / 2); // jitter: [cap/2, cap)
    }
    this.emit("reconnecting", { attempt: this.attempt, delayMs: Math.round(delay) });
    await sleep(delay);
    if (this.stopped) return;
    try {
      await this.open();
    } catch (e) {
      if (e?.code === "TICKET_FAILED") {
        // Nothing else will retry (no socket, no close event): back off and try again.
        this.emit("error", e);
        if (!this.stopped && this.reconnect.enabled) {
          this.state = "connecting";
          this.scheduleReconnect("backoff");
        }
      }
      // Otherwise onClose already scheduled the next attempt (or stopped).
    }
  }

  async rejoin() {
    const rooms = [...this.rooms.keys()];
    if (rooms.length === 0) return;
    const ws = this.ws;
    for (const room of rooms) {
      // Resume history rooms from the last message seen (ADR 0011 §3).
      const prev = this.rooms.get(room);
      const recover =
        prev?.epoch !== undefined ? { epoch: prev.epoch, offset: prev.offset } : undefined;
      try {
        await this.join(room, { recover });
      } catch (e) {
        // A passing failure keeps the room: the connection is gone (the
        // next one rejoins every room), or the gateway could not take the
        // join just now (retried here). Only a refusal drops it (it was
        // dropped whatever the error, so a drop during a rejoin lost the
        // room for good).
        if (CONNECTION_GONE.has(e?.code)) return;
        this.emit("error", e);
        if (PASSING.has(e?.code)) this.retryRejoin(room, recover, ws, 1);
        else this.rooms.delete(room);
      }
    }
    this.emit("rejoined", [...this.rooms.values()].filter((r) => !r.rejoining));
  }

  /** Join `room` again on connection `ws` after a passing failure, with backoff. */
  retryRejoin(room, recover, ws, attempt) {
    const info = this.rooms.get(room);
    if (!info) return;
    info.rejoining = true;
    const cap = Math.min(30000, 500 * 2 ** Math.min(attempt - 1, 16));
    setTimeout(async () => {
      if (this.stopped || this.ws !== ws || this.state !== "open" || this.rooms.get(room) !== info) return;
      try {
        this.emit("rejoined", [await this.join(room, { recover })]);
      } catch (e) {
        if (CONNECTION_GONE.has(e?.code)) return;
        this.emit("error", e);
        if (PASSING.has(e?.code)) this.retryRejoin(room, recover, ws, attempt + 1);
        else if (this.rooms.get(room) === info) this.rooms.delete(room);
      }
    }, cap / 2 + Math.random() * (cap / 2));
  }


  sendJson(msg) {
    if (!this.ws || this.state !== "open") throw new WsPrismError("NOT_CONNECTED", "not connected");
    this.ws.send(JSON.stringify(msg));
  }

  /** Fire-and-forget Ext message (errors arrive as "error" events). */
  send(svc, type, { room, data } = {}) {
    this.sendJson({ v: 1, svc, type, seq: this.nextSeq++, room, data });
  }

  /** Ext request answered by the gateway or a backend (`sys.reply`, `sys.joined`, ...). */
  request(svc, type, { room, data, timeoutMs } = {}) {
    const seq = this.nextSeq++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(seq);
        reject(new WsPrismError("TIMEOUT", `${svc}.${type} not answered`));
      }, timeoutMs ?? this.requestTimeoutMs);
      this.pending.set(seq, { resolve, reject, timer });
      try {
        this.sendJson({ v: 1, svc, type, seq, room, data });
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(seq);
        reject(e);
      }
    });
  }

  /**
   * Join `room`; resolves with `{room, chan, memberId}` (plus `epoch` and
   * `offset` in a history room). Re-joined after reconnects; history rooms
   * resume from the last message seen, and `recovered: false` on the
   * rejoined room means messages were missed: resync from your backend.
   */
  async join(room, { recover } = {}) {
    const r = await this.request("room", "join", {
      room,
      data: recover ? { recover } : undefined,
    });
    const info = {
      room,
      chan: r.chan,
      memberId: r.member_id,
      // Tenants with presence_events: who was already there (ADR 0010).
      members: (r.members ?? []).map((m) => ({ user: m.user, memberId: m.member_id })),
      membersTruncated: r.members_truncated === true,
    };
    if (typeof r.epoch === "string") {
      info.epoch = r.epoch;
      // Recovered: replayed messages follow and advance this; otherwise
      // start from the room's current position.
      info.offset = r.recovered === true ? recover.offset : r.offset;
    }
    if (typeof r.recovered === "boolean") info.recovered = r.recovered;
    this.rooms.set(room, info);
    this.chanRoom.set(r.chan, room);
    return info;
  }

  async leave(room) {
    const info = this.rooms.get(room);
    this.rooms.delete(room);
    if (info) {
      this.chanRoom.delete(info.chan);
      this.forgetChan(info.chan);
    }
    await this.request("room", "leave", { room });
  }

  /** Drop stale-frame bookkeeping for one member of a room id (it left). */
  forgetSender(chan, memberId) {
    const prefix = `${chan}:${memberId}:`;
    for (const k of this.lastSeen.keys()) if (k.startsWith(prefix)) this.lastSeen.delete(k);
  }

  /** Drop stale-frame bookkeeping for a room id (left, or rejoined under a new id). */
  forgetChan(chan) {
    const prefix = `${chan}:`;
    for (const k of this.lastSeen.keys()) if (k.startsWith(prefix)) this.lastSeen.delete(k);
  }

  /** Backend RPC (`tenants[].rpc`): resolves with the reply's `data`. */
  async rpc(svc, type, { room, data, timeoutMs } = {}) {
    const r = await this.request(svc, type, { room, data, timeoutMs });
    return r.data;
  }

  /**
   * Send keyed state on the Hot lane to a joined room. Only the newest value
   * per (sender, key) matters to receivers; `seq` is managed per key.
   */
  sendState(room, { svc, op, key = 0, payload }) {
    const info = this.rooms.get(room);
    if (!info || info.chan === undefined) throw new WsPrismError("NOT_JOINED", `not joined: ${room}`);
    if (!this.ws || this.state !== "open") throw new WsPrismError("NOT_CONNECTED", "not connected");
    const k = `${room}:${svc}:${key}`;
    const seq = this.stateSeq.get(k) ?? 1;
    this.stateSeq.set(k, (seq + 1) >>> 0);
    this.ws.send(encodeHot({ chan: info.chan, svc, op, key, seq, payload }));
  }

  /**
   * Renew the credential before it expires (ADR 0012): at a random point
   * between 70% and 90% of its lifetime (so clients that connected together
   * don't all hit the token backend at once), get a fresh one from getTicket() and send sys.refresh. If that
   * fails it retries; at expiry the gateway closes with 4001 and the client
   * reconnects with a new credential anyway.
   */
  scheduleRefresh(expiresInS) {
    clearTimeout(this.refreshTimer);
    if (this.opts.refresh === false || typeof expiresInS !== "number") return;
    const share = 0.7 + Math.random() * 0.2;
    // The gateway allows one refresh per second per connection.
    const delayMs = Math.max(MIN_REFRESH_GAP_MS, expiresInS * 1000 * share);
    this.refreshTimer = setTimeout(() => this.refreshNow(), delayMs);
  }

  async refreshNow() {
    if (this.state !== "open") return;
    try {
      const token = await this.opts.getTicket();
      const r = await this.request("sys", "refresh", { data: { token } });
      this.emit("refreshed", r.data ?? {});
      this.scheduleRefresh(r.data?.expires_in_s);
    } catch (e) {
      this.emit("error", new WsPrismError("REFRESH_FAILED", e?.message ?? String(e)));
      if (this.state === "open") {
        clearTimeout(this.refreshTimer);
        // Rate limited: retry as soon as allowed; other failures back off.
        const retryMs = e?.code === "RATE_LIMITED" ? MIN_REFRESH_GAP_MS : 5000;
        this.refreshTimer = setTimeout(() => this.refreshNow(), retryMs);
      }
    }
  }

  /** Close for good (no reconnect). */
  close() {
    this.stopped = true;
    clearTimeout(this.refreshTimer);
    this.rooms.clear();
    if (this.ws) this.ws.close(1000, "client");
    this.state = "closed";
  }
}
