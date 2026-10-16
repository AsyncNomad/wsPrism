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

export class WsPrismClient {
  /**
   * @param {object} opts
   * @param {string} opts.url            ws(s)://host/v1/ws
   * @param {string} opts.tenant
   * @param {() => string | Promise<string>} opts.getTicket  called per connection attempt
   * @param {string} [opts.sid]
   * @param {number} [opts.requestTimeoutMs=10000]
   * @param {{minDelayMs?: number, maxDelayMs?: number, enabled?: boolean}} [opts.reconnect]
   * @param {typeof WebSocket} [opts.WebSocket]  implementation (default: global)
   */
  constructor(opts) {
    if (!opts?.url || !opts?.tenant || typeof opts.getTicket !== "function") {
      throw new TypeError("url, tenant and getTicket are required");
    }
    this.opts = opts;
    this.requestTimeoutMs = opts.requestTimeoutMs ?? 10000;
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
    this.lastSeen = new Map(); // `${chan}:${sender}:${key}` -> seq
    this.stateSeq = new Map(); // `${room}:${key}` -> next seq to send
    this.attempt = 0;
    this.stopped = false;
  }

  /** Subscribe to "open" | "close" | "message" | "state" | "error" | "reconnecting" | "rejoined". */
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
    const ticket = await this.opts.getTicket();
    const q = new URLSearchParams({ tenant: this.opts.tenant });
    if (this.opts.sid) q.set("sid", this.opts.sid);
    const url = `${this.opts.url}${this.opts.url.includes("?") ? "&" : "?"}${q}`;
    return new Promise((resolve, reject) => {
      let settled = false;
      const ws = new this.WS(url, [PROTOCOL, AUTH_PREFIX + ticket]);
      ws.binaryType = "arraybuffer";
      this.ws = ws;
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
            this.state = "open";
            this.attempt = 0;
            this.user = msg.data?.user ?? null;
            this.emit("open", msg.data);
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
        const wasOpen = settled;
        if (!settled) {
          settled = true;
          reject(new WsPrismError("CONNECT_FAILED", `connection closed (${ev.code})`));
        }
        this.onClose(ev.code, ev.reason, wasOpen);
      };
      ws.onerror = () => {}; // followed by close
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
    this.emit("message", msg); // includes sys.kicked before a close
  }

  onBinary(data) {
    const f = decodeHot(data);
    if (!f) return;
    if (f.seq !== undefined) {
      const k = `${f.chan}:${f.sender ?? 0}:${f.key ?? "-"}`;
      const last = this.lastSeen.get(k);
      if (last !== undefined && !seqNewer(f.seq, last)) return; // stale or duplicate
      this.lastSeen.set(k, f.seq);
    }
    this.emit("state", { ...f, room: this.chanRoom.get(f.chan) });
  }

  onClose(code, reason, wasOpen) {
    this.ws = null;
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
    } catch {
      // onClose already scheduled the next attempt (or stopped).
    }
  }

  async rejoin() {
    const rooms = [...this.rooms.keys()];
    if (rooms.length === 0) return;
    for (const room of rooms) {
      try {
        await this.join(room);
      } catch (e) {
        this.rooms.delete(room);
        this.emit("error", e);
      }
    }
    this.emit("rejoined", [...this.rooms.values()]);
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

  /** Join `room`; resolves with `{room, chan, memberId}`. Re-joined after reconnects. */
  async join(room) {
    const r = await this.request("room", "join", { room });
    const info = { room, chan: r.chan, memberId: r.member_id };
    this.rooms.set(room, info);
    this.chanRoom.set(r.chan, room);
    return info;
  }

  async leave(room) {
    const info = this.rooms.get(room);
    this.rooms.delete(room);
    if (info) this.chanRoom.delete(info.chan);
    await this.request("room", "leave", { room });
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
    const k = `${room}:${key}`;
    const seq = this.stateSeq.get(k) ?? 1;
    this.stateSeq.set(k, (seq + 1) >>> 0);
    this.ws.send(encodeHot({ chan: info.chan, svc, op, key, seq, payload }));
  }

  /** Close for good (no reconnect). */
  close() {
    this.stopped = true;
    this.rooms.clear();
    if (this.ws) this.ws.close(1000, "client");
    this.state = "closed";
  }
}
