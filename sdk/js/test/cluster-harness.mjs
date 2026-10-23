// Shared by the chaos tests (chaos.test.mjs) and the randomized soak
// (soak.mjs): gateway nodes on Redis as real processes, SDK clients that
// record what they receive in one history room, and the offset checker.
// Everything listens on 127.0.0.1 or the isolated docker network.

import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { WsPrismClient } from "../wsprism.js";

export const ROOT = join(dirname(fileURLToPath(import.meta.url)), "../../..");
export const BIN = process.env.WSPRISM_GATEWAY_BIN ?? join(ROOT, "target/debug/wsprism-gateway");
export const REDIS = process.env.WSPRISM_TEST_REDIS;
export const CONTAINER = process.env.WSPRISM_TEST_REDIS_CONTAINER ?? "wsp-redis";
export const API_KEY = "chaos-key";
export const ROOM = "chaos:1";
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** Milliseconds on a monotonic clock. Not `Date.now()`: this WSL's wall
 * clock steps back ~2 s every 30 s, which skewed the soak's time windows. */
export const now = () => performance.now();
export const skip = REDIS ? false : "WSPRISM_TEST_REDIS not set";

export function freePort() {
  return new Promise((resolve, reject) => {
    const s = createNetServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}

const procs = [];
/** Kill every gateway process started here (test teardown). */
export function killAll() {
  for (const p of procs) p.kill("SIGKILL");
}

/** One gateway node on Redis with prefix `prefix` (on `ports` if given:
 * a restarted node keeps its address; through `redisUrl` if given, e.g. a
 * fault proxy). */
export async function startNode(prefix, ports, redisUrl = REDIS) {
  const [ws, api] = ports ?? [await freePort(), await freePort()];
  const digest = createHash("sha256").update(API_KEY).digest("hex");
  const cfg = join(mkdtempSync(join(tmpdir(), "wsprism-chaos-")), "gw.yaml");
  writeFileSync(
    cfg,
    `version: 1
gateway:
  listen: "127.0.0.1:${ws}"
  api: { listen: "127.0.0.1:${api}" }
  broker: { kind: redis, url: "${redisUrl}", prefix: "${prefix}", slot_ttl_s: 3 }
tenants:
  - id: t
    auth: [{ kind: ticket }]
    api_keys: [{ name: test, sha256: "${digest}" }]
    policy: { ext_allowlist: ["room:*"] }
    history: [{ rooms: "chaos:*", size: 10000, ttl_s: 600 }]
`,
  );
  const proc = spawn(BIN, ["--config", cfg], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, RUST_LOG: process.env.CHAOS_RUST_LOG ?? "warn" },
  });
  let stderr = "";
  const log = (c) => {
    stderr += c;
    if (process.env.CHAOS_LOG) process.stderr.write(`[${ws}] ${c}`);
  };
  proc.stderr.on("data", log);
  proc.stdout.on("data", log); // tracing writes to stdout
  procs.push(proc);
  const apiBase = `http://127.0.0.1:${api}`;
  for (let i = 0; i < 200; i++) {
    try {
      if ((await fetch(`http://127.0.0.1:${ws}/readyz`)).ok) break;
    } catch {}
    if (proc.exitCode !== null) throw new Error(`gateway exited: ${stderr}`);
    await sleep(50);
  }
  const call = async (method, path, body) => {
    const r = await fetch(apiBase + path, {
      method,
      headers: { authorization: `Bearer ${API_KEY}`, "content-type": "application/json" },
      body: body && JSON.stringify(body),
      // A stopped (SIGSTOP) node accepts the connection but never answers.
      signal: AbortSignal.timeout(3000),
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  };
  return { url: `ws://127.0.0.1:${ws}/v1/ws`, call, proc, alive: true, prefix, ports: [ws, api], redisUrl };
}

/**
 * A TCP proxy between one gateway node and Redis, with faults on that link
 * only: `delay(ms)` holds every chunk that long (each direction in order),
 * `blackhole(on)` keeps the connections open but passes nothing (a peer
 * that vanished; what was held flows again when it ends, like TCP
 * retransmission), `reset()` closes them all. Listens on 127.0.0.1.
 */
export async function startProxy(targetUrl) {
  const { createServer, connect } = await import("node:net");
  const target = new URL(targetUrl);
  const state = { delay: 0, blackhole: false, held: [], sockets: new Set() };
  // One direction of one connection: chunks leave in the order they came
  // (TCP never reorders), each no earlier than `delay` after it arrived —
  // also when the delay shrinks while older chunks still wait.
  const pipe = (to) => {
    const queue = [];
    let timer = null;
    const pump = () => {
      timer = null;
      while (queue.length && queue[0].due <= now()) {
        const chunk = queue.shift().chunk;
        if (state.blackhole) state.held.push(() => to.writable && to.write(chunk));
        else if (to.writable) to.write(chunk);
      }
      if (queue.length) timer = setTimeout(pump, queue[0].due - now());
    };
    return (chunk) => {
      queue.push({ chunk, due: now() + state.delay });
      if (!timer) pump();
    };
  };
  const server = createServer((client) => {
    const upstream = connect(Number(target.port || 6379), target.hostname);
    for (const s of [client, upstream]) {
      state.sockets.add(s);
      s.on("close", () => state.sockets.delete(s));
      s.on("error", () => {});
    }
    const up = pipe(upstream);
    const down = pipe(client);
    client.on("data", up);
    upstream.on("data", down);
    client.on("close", () => upstream.destroy());
    upstream.on("close", () => client.destroy());
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const { port } = server.address();
  return {
    url: `redis://127.0.0.1:${port}`,
    delay(ms) {
      state.delay = ms;
    },
    /** The delay now (ms). */
    get delayMs() {
      return state.delay;
    },
    blackhole(on) {
      state.blackhole = on;
      if (!on) {
        const held = state.held.splice(0);
        for (const f of held) f();
      }
    },
    reset() {
      for (const s of state.sockets) s.destroy();
    },
    close() {
      server.close();
      for (const s of state.sockets) s.destroy();
    },
  };
}

/** Stop one node gracefully (SIGTERM: drain) and wait for it to exit.
 * While draining it may still answer; the backend keeps using it until it
 * stops, like a load balancer that has not noticed yet. */
export async function stopNode(node) {
  // It may have stopped by itself already (fenced): "exit" will not fire again.
  const gone = node.proc.exitCode !== null || node.proc.signalCode !== null;
  const exited = gone ? Promise.resolve() : new Promise((r) => node.proc.once("exit", r));
  node.proc.kill("SIGTERM");
  await exited;
  node.alive = false;
}

/** Rolling update of one node: stop it, start it again on the same ports. */
export async function restartNode(nodes, i) {
  await stopNode(nodes[i]);
  nodes[i] = await startNode(nodes[i].prefix, nodes[i].ports, nodes[i].redisUrl);
}

/** Ask any live node's server API; retries until one answers 200. */
export async function anyNode(nodes, method, path, body, deadlineMs = 20000) {
  const end = now() + deadlineMs;
  for (;;) {
    for (const n of nodes.filter((n) => n.alive)) {
      try {
        const r = await n.call(method, path, body);
        if (r.status === 200) return r.body;
      } catch {}
    }
    if (now() > end) throw new Error(`no node answered ${path}`);
    await sleep(50);
  }
}

/** A client that records what it receives in ROOM. */
/** `first`: the index of the node the client tries first (then the next
 * ones in turn). Events carry the time they arrived (`at`). */
export function recorder(nodes, name, first) {
  const all = nodes.map((n) => n.url);
  const urls = [...all.slice(first % all.length), ...all.slice(0, first % all.length)];
  const client = new WsPrismClient({
    url: urls,
    tenant: "t",
    getTicket: async () => (await anyNode(nodes, "POST", "/api/v1/tickets", { user: name })).ticket,
    reconnect: { minDelayMs: 50, maxDelayMs: 300 },
  });
  // `attempts`, `closes` and `errors`: for the timeout report (a client
  // stuck in "connecting" says why).
  const rec = { name, client, events: [], reconnects: 0, attempts: 0, closes: [], errors: [] };
  client.on("reconnecting", () => (rec.attempts += 1));
  client.on("close", (c) => rec.closes.push(c.code));
  client.on("error", (e) => rec.errors.push(e?.code ?? String(e)));
  client.on("message", (m) => {
    if (m.svc === "server" && m.room === ROOM) rec.events.push({ offset: m.offset, epoch: m.epoch, n: m.data.n, at: now() });
  });
  client.on("gap", (g) => rec.events.push({ gap: true, epoch: g.epoch, at: now() }));
  client.on("rejoined", (rooms) => {
    rec.reconnects += 1;
    for (const r of rooms) if (r.recovered === false) rec.events.push({ gap: true, unrecovered: true, at: now() });
  });
  return rec;
}

/**
 * The offset checker: within each stretch between announced gaps, offsets
 * rise by exactly one (no duplicate, no reordering, no silent loss).
 * Returns the stretches' offset lists.
 */
export function check(rec, { ordered = true, mayRepeat = new Set() } = {}) {
  // Content: the backend published n = 1, 2, 3, ... once each. A client must
  // never see one twice (a retried write that had already run would show
  // up here, not in the offsets). In order, too — unless the backend moved
  // to another node while one was stalled: then the stalled node's
  // accepted messages land after the other's (no order across nodes).
  const ns = rec.events.filter((e) => !e.gap).map((e) => e.n);
  // `mayRepeat`: messages the backend sent again to another node after a
  // timeout; the first node may have accepted them too (a real backend's
  // retry; the gateway cannot tell the two apart).
  const twice = ns.filter((n, i) => ns.indexOf(n) !== i && !mayRepeat.has(n));
  assert.equal(twice.length, 0, `${rec.name}: a message arrived twice (${twice.slice(0, 5)})`);
  for (let i = 1; ordered && i < ns.length; i++) {
    const msgs = rec.events.filter((e) => !e.gap);
    assert.ok(
      ns[i] > ns[i - 1],
      `${rec.name}: message ${ns[i]} after ${ns[i - 1]} (duplicate or reordered): ${JSON.stringify(msgs.slice(Math.max(0, i - 2), i + 3))}`,
    );
  }
  const stretches = [[]];
  for (const e of rec.events) {
    if (e.gap) {
      stretches.push([]);
      continue;
    }
    stretches.at(-1).push(e.offset);
  }
  for (const s of stretches) {
    for (let i = 1; i < s.length; i++) {
      assert.equal(s[i], s[i - 1] + 1, `${rec.name}: offset ${s[i]} after ${s[i - 1]} (${JSON.stringify(s.slice(Math.max(0, i - 3), i + 3))})`);
    }
  }
  return stretches;
}

export async function publish(nodes, from, to, gapMs = 0) {
  for (let n = from; n <= to; n++) {
    await anyNode(nodes, "POST", "/api/v1/publish", { room: ROOM, data: { n } });
    if (gapMs) await sleep(gapMs);
  }
}

export async function until(cond, ms, what, recs = []) {
  const end = now() + ms;
  while (!cond()) {
    if (now() > end) {
      const state = recs.map((r) => {
        const msgs = r.events.filter((e) => !e.gap);
        const gaps = r.events.filter((e) => e.gap).map((e) => (e.unrecovered ? "unrecovered" : e.epoch ? `epoch@${msgs.length}` : `gap@${msgs.length}`));
        return `${r.name}: ${msgs.length} msgs, last n=${msgs.at(-1)?.n} off=${msgs.at(-1)?.offset}, gaps=[${gaps}], reconnects=${r.reconnects}, state=${r.client.state}, attempts=${r.attempts}, closes=[${(r.closes ?? []).slice(-5)}], errors=[${(r.errors ?? []).slice(-3)}]`;
      });
      throw new Error(`timed out: ${what}\n${state.join("\n")}`);
    }
    await sleep(50);
  }
}

export const lastOffset = (rec) => rec.events.filter((e) => !e.gap).at(-1)?.offset;
export const prefix = (name) => `chaos${Date.now() % 1e8}-${name}`; // wall clock: unique names

export async function cluster(name) {
  const p = prefix(name);
  return [await startNode(p), await startNode(p)];
}

export async function joinAll(recs) {
  for (const r of recs) {
    await r.client.connect();
    await r.client.join(ROOM);
  }
}

export function docker(...args) {
  return execFileSync("docker", args, { stdio: ["ignore", "pipe", "pipe"] }).toString();
}
