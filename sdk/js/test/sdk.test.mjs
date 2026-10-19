// SDK tests against a real gateway process (node:test, Node 22+).
// Run: sdk/js/run-tests.sh  (builds the gateway, then `node --test`).
// Everything listens on 127.0.0.1; nothing leaves the machine.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, createHmac } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import {
  WsPrismClient,
  WsPrismError,
  decodeHot,
  encodeHot,
  reconnectPolicy,
  seqNewer,
} from "../wsprism.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const BIN = process.env.WSPRISM_GATEWAY_BIN ?? join(ROOT, "target/debug/wsprism-gateway");
const API_KEY = "sdk-test-key";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function freePort() {
  return new Promise((resolve, reject) => {
    const s = createNetServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}

/** A local HTTP backend answering RPC calls. */
function rpcBackend() {
  return new Promise((resolve) => {
    const srv = createServer((req, res) => {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        const b = JSON.parse(body);
        res.setHeader("content-type", "application/json");
        if (b.type === "fail") {
          res.end(JSON.stringify({ error: { code: "NOT_FOUND", msg: "nope" } }));
        } else {
          res.end(JSON.stringify({ data: { echo: b.data, user: b.user } }));
        }
      });
    });
    srv.listen(0, "127.0.0.1", () => resolve(srv));
  });
}

const procs = [];

/** Start a gateway with a ticket-only tenant `t`; returns its addresses. */
const JWT_SECRET = "sdk-test-secret-sdk-test-secret-0123456789";

/** HS256 JWT for `sub`, valid `ttlS` seconds (what an auth server issues). */
function jwt(sub, ttlS) {
  const b64 = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const head = `${b64({ alg: "HS256", typ: "JWT" })}.${b64({ sub, exp: Math.floor(Date.now() / 1000) + ttlS })}`;
  return `${head}.${createHmac("sha256", JWT_SECRET).update(head).digest("base64url")}`;
}

async function gateway({ rpcUrl, presence } = {}) {
  const [ws, api] = [await freePort(), await freePort()];
  const digest = createHash("sha256").update(API_KEY).digest("hex");
  const dir = mkdtempSync(join(tmpdir(), "wsprism-sdk-"));
  const cfg = join(dir, "gw.yaml");
  const secret = join(dir, "jwt-secret");
  writeFileSync(secret, JWT_SECRET);
  writeFileSync(
    cfg,
    `version: 1
gateway:
  listen: "127.0.0.1:${ws}"
  api: { listen: "127.0.0.1:${api}" }
tenants:
  - id: t
    auth: [{ kind: ticket }]
    api_keys: [{ name: test, sha256: "${digest}" }]
${rpcUrl ? `    rpc: { url: "${rpcUrl}", allow: ["game:*"], timeout_ms: 2000 }\n` : ""}    policy:
      ext_allowlist: ["room:*", "chat:*"${rpcUrl ? ', "game:*"' : ""}]
      hot_allowlist: ["1:*"]
${presence ? "      presence_events: true\n" : ""}  - id: j
    auth: [{ kind: jwt, algorithm: HS256, secret_file: "${secret}", leeway_s: 0 }]
    policy:
      ext_allowlist: ["room:*"]
`,
  );
  const p = spawn(BIN, ["--config", cfg], { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  p.stderr.on("data", (c) => (stderr += c));
  procs.push(p);
  const base = `http://127.0.0.1:${ws}`;
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${base}/healthz`)).ok) break;
    } catch {}
    if (p.exitCode !== null) throw new Error(`gateway exited: ${stderr}`);
    await sleep(50);
  }
  const apiBase = `http://127.0.0.1:${api}`;
  const apiCall = async (method, path, body) => {
    const r = await fetch(apiBase + path, {
      method,
      headers: { authorization: `Bearer ${API_KEY}`, "content-type": "application/json" },
      body: body && JSON.stringify(body),
    });
    return { status: r.status, body: await r.json() };
  };
  return {
    url: `ws://127.0.0.1:${ws}/v1/ws`,
    api: apiCall,
    /** What a backend does: issue a one-time ticket for `user`. */
    ticketFor: (user) => async () => (await apiCall("POST", "/api/v1/tickets", { user })).body.ticket,
  };
}

after(() => {
  for (const p of procs) p.kill();
});

/** Resolve with the next `event` payload that satisfies `pred`. */
function next(client, event, pred = () => true, ms = 5000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      off();
      reject(new Error(`timed out waiting for ${event}`));
    }, ms);
    const off = client.on(event, (x) => {
      if (pred(x)) {
        clearTimeout(timer);
        off();
        resolve(x);
      }
    });
  });
}

function client(gw, user, extra = {}) {
  return new WsPrismClient({ url: gw.url, tenant: "t", getTicket: gw.ticketFor(user), ...extra });
}

// ---- pure functions ------------------------------------------------------

test("hot frames round-trip through the v2 layout", () => {
  const f = encodeHot({ chan: 7, svc: 1, op: 2, key: 3, seq: 4, payload: new Uint8Array([9, 8]) });
  assert.deepEqual([...f.subarray(0, 2)], [0x01, 0x03]);
  // A server frame is the client layout plus the sender id before the payload.
  const server = new Uint8Array(f.length + 4);
  server.set(f.subarray(0, 16));
  server[1] |= 0x04;
  new DataView(server.buffer).setUint32(16, 42, true);
  server.set(f.subarray(16), 20);
  const d = decodeHot(server);
  assert.equal(d.chan, 7);
  assert.equal(d.key, 3);
  assert.equal(d.seq, 4);
  assert.equal(d.sender, 42);
  assert.deepEqual([...d.payload], [9, 8]);
  assert.equal(decodeHot(new Uint8Array([1, 0x07, 0, 0])), null, "truncated");
});

test("sequence numbers compare with wraparound", () => {
  assert.ok(seqNewer(2, 1));
  assert.ok(!seqNewer(1, 2));
  assert.ok(!seqNewer(5, 5));
  assert.ok(seqNewer(0, 0xffffffff), "0 follows u32::MAX");
});

test("reconnect policy follows the close-code ranges", () => {
  assert.equal(reconnectPolicy(4005), "stop");
  assert.equal(reconnectPolicy(4007), "stop");
  assert.equal(reconnectPolicy(4001), "now", "expired: retry with a fresh ticket");
  assert.equal(reconnectPolicy(4101), "backoff");
  assert.equal(reconnectPolicy(4200), "now");
  assert.equal(reconnectPolicy(1006), "backoff");
});

// ---- against a gateway ---------------------------------------------------

let gw;
let backend;
before(async () => {
  backend = await rpcBackend();
  gw = await gateway({ rpcUrl: `http://127.0.0.1:${backend.address().port}/rpc` });
});
after(() => backend?.close());

test("connects with a one-time ticket, joins, and receives room messages", async () => {
  const a = client(gw, "alice");
  const b = client(gw, "bob");
  assert.equal((await a.connect()).user, "alice");
  await b.connect();
  const ja = await a.join("lobby");
  const jb = await b.join("lobby");
  assert.equal(ja.chan, jb.chan, "same room id for every member");
  assert.notEqual(ja.memberId, jb.memberId);

  const got = next(b, "message", (m) => m.svc === "chat");
  a.send("chat", "send", { room: "lobby", data: { msg: "hi" } });
  const m = await got;
  assert.equal(m.data.msg, "hi");
  assert.equal(m.data.from, "alice");

  const pub = next(b, "message", (m) => m.svc === "server");
  const r = await gw.api("POST", "/api/v1/publish", { room: "lobby", data: { notice: 1 } });
  assert.equal(r.status, 200);
  assert.deepEqual((await pub).data, { notice: 1 });
  a.close();
  b.close();
});

test("requests resolve with replies and reject with gateway errors", async () => {
  const a = client(gw, "carol");
  await a.connect();
  assert.deepEqual(await a.rpc("game", "move", { data: { x: 1 } }), { echo: { x: 1 }, user: "carol" });
  await assert.rejects(a.rpc("game", "fail"), (e) => e instanceof WsPrismError && e.code === "NOT_FOUND");
  await assert.rejects(a.request("room", "leave", { room: "never-joined" }), (e) => e.code === "BAD_REQUEST");
  // Not in the tenant allowlist.
  await assert.rejects(a.request("nope", "x"), (e) => e.code === "NOT_ALLOWED");
  a.close();
});

test("state frames reach other members with sender and key", async () => {
  const a = client(gw, "dan");
  const b = client(gw, "erin");
  await a.connect();
  await b.connect();
  const ja = await a.join("arena");
  await b.join("arena");
  const got = next(b, "state", (s) => s.key === 5);
  a.sendState("arena", { svc: 1, op: 1, key: 5, payload: new Uint8Array([1, 2, 3]) });
  const s = await got;
  assert.equal(s.room, "arena");
  assert.equal(s.sender, ja.memberId);
  assert.deepEqual([...s.payload], [1, 2, 3]);
  assert.throws(() => a.sendState("not-joined", { svc: 1, op: 1, payload: [] }), /not joined/);
  a.close();
  b.close();
});

test("a backend disconnect (4007) is final", async () => {
  const a = client(gw, "frank");
  await a.connect();
  const closed = next(a, "close");
  await gw.api("POST", "/api/v1/disconnect", { user: "frank" });
  const c = await closed;
  assert.equal(c.code, 4007);
  assert.equal(c.willReconnect, false);
  assert.equal(a.state, "closed");
});

test("an expired token (4001) reconnects at once with a fresh one and rejoins", async () => {
  let issued = 0;
  const a = new WsPrismClient({
    url: gw.url,
    tenant: "j",
    getTicket: () => {
      issued += 1;
      return jwt("gina", issued === 1 ? 2 : 600);
    },
  });
  await a.connect();
  const first = await a.join("hall");
  const closed = next(a, "close", () => true, 6000);
  const reopened = next(a, "open", () => true, 8000);
  const c = await closed;
  assert.equal(c.code, 4001);
  assert.equal(c.willReconnect, true);
  await reopened;
  assert.equal(issued, 2, "a fresh credential per connection");
  // Re-joined automatically, with a new membership.
  const rejoined = await next(a, "rejoined", () => true, 3000).catch(() => null);
  assert.ok(rejoined, "rejoined event");
  assert.notEqual(a.rooms.get("hall").memberId, first.memberId);
  a.close();
});

test("a failing getTicket during reconnect is retried with backoff", async () => {
  let calls = 0;
  const a = new WsPrismClient({
    url: gw.url,
    tenant: "j",
    reconnect: { minDelayMs: 200, maxDelayMs: 400 },
    getTicket: () => {
      calls += 1;
      if (calls === 2) throw new Error("auth server down");
      return jwt("hana", calls === 1 ? 2 : 600);
    },
  });
  const errors = [];
  a.on("error", (e) => errors.push(e));
  await a.connect();
  const reopened = next(a, "open", () => true, 9000);
  await reopened;
  assert.equal(calls, 3, "first ticket, failed ticket, then a retry");
  assert.ok(errors.some((e) => e.code === "TICKET_FAILED"), "the failure is reported");
  a.close();
});

test("stale-frame dropping is per (room, sender, svc, key)", () => {
  const c = new WsPrismClient({ url: "ws://x", tenant: "t", getTicket: () => "", WebSocket: class {} });
  const got = [];
  c.on("state", (s) => got.push(`${s.svc}:${s.seq}`));
  // A server frame = client layout + sender before the payload.
  const server = (svc, seq) => {
    const f = encodeHot({ chan: 9, svc, op: 1, key: 0, seq, payload: new Uint8Array([1]) });
    const out = new Uint8Array(f.length + 4);
    out.set(f.subarray(0, 16));
    out[1] |= 0x04;
    new DataView(out.buffer).setUint32(16, 3, true);
    out.set(f.subarray(16), 20);
    return out;
  };
  c.onBinary(server(1, 5));
  c.onBinary(server(2, 1)); // other svc, same key: its own sequence
  c.onBinary(server(1, 4)); // stale for svc 1
  assert.deepEqual(got, ["1:5", "2:1"]);
  c.forgetChan(9);
  assert.equal(c.lastSeen.size, 0);
});

test("presence events and the join snapshot", async () => {
  const pg = await gateway({ presence: true });
  const a = new WsPrismClient({ url: pg.url, tenant: "t", getTicket: pg.ticketFor("ivy") });
  const b = new WsPrismClient({ url: pg.url, tenant: "t", getTicket: pg.ticketFor("jon") });
  await a.connect();
  await b.connect();
  const ja = await a.join("hall");
  assert.deepEqual(ja.members, []);
  const joined = next(a, "presence", (p) => p.type === "joined");
  const jb = await b.join("hall");
  assert.deepEqual(jb.members, [{ user: "ivy", memberId: ja.memberId }]);
  assert.deepEqual(await joined, { type: "joined", room: "hall", user: "jon", memberId: jb.memberId });
  const left = next(a, "presence", (p) => p.type === "left");
  b.close();
  assert.equal((await left).memberId, jb.memberId);
  a.close();
});
