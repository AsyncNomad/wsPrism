// Chaos tests (ADR 0011 R5): two gateway nodes on Redis, SDK clients in a
// history room, a backend publishing through the server API — then a node
// is killed (-9), the nodes are restarted one by one (rolling update),
// Redis restarts empty, or Redis pauses. An offset checker
// judges what every client received: no duplicate, no reordering, and no
// gap unless the gateway announced one (sys.gap / recovered:false).
//
// Needs WSPRISM_TEST_REDIS (bench/redis-test.sh). The Redis restart and
// pause scenarios also need `docker` and the container name in
// WSPRISM_TEST_REDIS_CONTAINER (default wsp-redis). Everything listens on
// 127.0.0.1 or the isolated docker network; nothing leaves the machine.

import { test, after } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, writeFileSync } from "node:fs";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { WsPrismClient } from "../wsprism.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const BIN = process.env.WSPRISM_GATEWAY_BIN ?? join(ROOT, "target/debug/wsprism-gateway");
const REDIS = process.env.WSPRISM_TEST_REDIS;
const CONTAINER = process.env.WSPRISM_TEST_REDIS_CONTAINER ?? "wsp-redis";
const API_KEY = "chaos-key";
const ROOM = "chaos:1";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const skip = REDIS ? false : "WSPRISM_TEST_REDIS not set";

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

const procs = [];
after(() => {
  for (const p of procs) p.kill("SIGKILL");
});

/** One gateway node on Redis with prefix `prefix` (on `ports` if given:
 * a restarted node keeps its address). */
async function startNode(prefix, ports) {
  const [ws, api] = ports ?? [await freePort(), await freePort()];
  const digest = createHash("sha256").update(API_KEY).digest("hex");
  const cfg = join(mkdtempSync(join(tmpdir(), "wsprism-chaos-")), "gw.yaml");
  writeFileSync(
    cfg,
    `version: 1
gateway:
  listen: "127.0.0.1:${ws}"
  api: { listen: "127.0.0.1:${api}" }
  broker: { kind: redis, url: "${REDIS}", prefix: "${prefix}", slot_ttl_s: 3 }
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
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  };
  return { url: `ws://127.0.0.1:${ws}/v1/ws`, call, proc, alive: true, prefix, ports: [ws, api] };
}

/** Stop one node gracefully (SIGTERM: drain) and wait for it to exit.
 * While draining it may still answer; the backend keeps using it until it
 * stops, like a load balancer that has not noticed yet. */
async function stopNode(node) {
  const exited = new Promise((r) => node.proc.once("exit", r));
  node.proc.kill("SIGTERM");
  await exited;
  node.alive = false;
}

/** Rolling update of one node: stop it, start it again on the same ports. */
async function restartNode(nodes, i) {
  await stopNode(nodes[i]);
  nodes[i] = await startNode(nodes[i].prefix, nodes[i].ports);
}

/** Ask any live node's server API; retries until one answers 200. */
async function anyNode(nodes, method, path, body, deadlineMs = 20000) {
  const end = Date.now() + deadlineMs;
  for (;;) {
    for (const n of nodes.filter((n) => n.alive)) {
      try {
        const r = await n.call(method, path, body);
        if (r.status === 200) return r.body;
      } catch {}
    }
    if (Date.now() > end) throw new Error(`no node answered ${path}`);
    await sleep(50);
  }
}

/** A client that records what it receives in ROOM. */
function recorder(nodes, name, first) {
  const urls = first === 0 ? nodes.map((n) => n.url) : [...nodes].reverse().map((n) => n.url);
  const client = new WsPrismClient({
    url: urls,
    tenant: "t",
    getTicket: async () => (await anyNode(nodes, "POST", "/api/v1/tickets", { user: name })).ticket,
    reconnect: { minDelayMs: 50, maxDelayMs: 300 },
  });
  const rec = { name, client, events: [], reconnects: 0 };
  client.on("message", (m) => {
    if (m.svc === "server" && m.room === ROOM) rec.events.push({ offset: m.offset, n: m.data.n });
  });
  client.on("gap", (g) => rec.events.push({ gap: true, epoch: g.epoch }));
  client.on("rejoined", (rooms) => {
    rec.reconnects += 1;
    for (const r of rooms) if (r.recovered === false) rec.events.push({ gap: true, unrecovered: true });
  });
  return rec;
}

/**
 * The offset checker: within each stretch between announced gaps, offsets
 * rise by exactly one (no duplicate, no reordering, no silent loss).
 * Returns the stretches' offset lists.
 */
function check(rec, { ordered = true } = {}) {
  // Content: the backend published n = 1, 2, 3, ... once each. A client must
  // never see one twice (a retried write that had already run would show
  // up here, not in the offsets). In order, too — unless the backend moved
  // to another node while one was stalled: then the stalled node's
  // accepted messages land after the other's (no order across nodes).
  const ns = rec.events.filter((e) => !e.gap).map((e) => e.n);
  assert.equal(new Set(ns).size, ns.length, `${rec.name}: a message arrived twice`);
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

async function publish(nodes, from, to, gapMs = 0) {
  for (let n = from; n <= to; n++) {
    await anyNode(nodes, "POST", "/api/v1/publish", { room: ROOM, data: { n } });
    if (gapMs) await sleep(gapMs);
  }
}

async function until(cond, ms, what, recs = []) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) {
      const state = recs.map((r) => {
        const msgs = r.events.filter((e) => !e.gap);
        const gaps = r.events.filter((e) => e.gap).map((e) => (e.unrecovered ? "unrecovered" : e.epoch ? `epoch@${msgs.length}` : `gap@${msgs.length}`));
        return `${r.name}: ${msgs.length} msgs, last n=${msgs.at(-1)?.n} off=${msgs.at(-1)?.offset}, gaps=[${gaps}], reconnects=${r.reconnects}, state=${r.client.state}`;
      });
      throw new Error(`timed out: ${what}\n${state.join("\n")}`);
    }
    await sleep(50);
  }
}

const lastOffset = (rec) => rec.events.filter((e) => !e.gap).at(-1)?.offset;
const prefix = (name) => `chaos${Date.now() % 1e8}-${name}`;

async function cluster(name) {
  const p = prefix(name);
  return [await startNode(p), await startNode(p)];
}

async function joinAll(recs) {
  for (const r of recs) {
    await r.client.connect();
    await r.client.join(ROOM);
  }
}

test("a node killed with -9: its clients resume on the other node, nothing lost or repeated", { skip }, async (t) => {
  const nodes = await cluster("kill");
  const recs = Array.from({ length: 10 }, (_, i) => recorder(nodes, `k${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  const total = 600;
  const publishing = publish(nodes, 1, total, 5);
  // Mid-stream, node 0 dies without a goodbye.
  await until(() => recs.every((r) => (lastOffset(r) ?? 0) >= 150), 20000, "first 150");
  nodes[0].proc.kill("SIGKILL");
  nodes[0].alive = false;
  await publishing;
  await until(() => recs.every((r) => lastOffset(r) === total), 30000, "everyone at the last offset");
  for (const r of recs) {
    const stretches = check(r);
    assert.equal(stretches.length, 1, `${r.name}: no gap announced`);
    assert.deepEqual(stretches[0], Array.from({ length: total }, (_, i) => i + 1), `${r.name}: every offset once`);
  }
  // Half of them were on the killed node and came back.
  assert.ok(recs.filter((r) => r.reconnects > 0).length >= 5, "the killed node's clients reconnected");
});

test("rolling update: each node restarted in turn, nothing lost or repeated", { skip }, async (t) => {
  const nodes = await cluster("rolling");
  const recs = Array.from({ length: 10 }, (_, i) => recorder(nodes, `u${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  const total = 900;
  // The backend sends to node 0 first, so the first restart catches
  // publishes accepted by a node that is about to stop.
  const publishing = publish(nodes, 1, total, 5);
  for (const [i, at] of [[0, 150], [1, 450]]) {
    await until(() => recs.every((r) => (lastOffset(r) ?? 0) >= at), 30000, `${at} before restarting node ${i}`, recs);
    await restartNode(nodes, i);
  }
  await publishing;
  await until(() => recs.every((r) => lastOffset(r) === total), 30000, "everyone at the last offset", recs);
  for (const r of recs) {
    const stretches = check(r);
    assert.equal(stretches.length, 1, `${r.name}: no gap announced`);
    assert.deepEqual(stretches[0], Array.from({ length: total }, (_, i) => i + 1), `${r.name}: every offset once`);
  }
  assert.ok(recs.every((r) => r.reconnects > 0), "every client moved at least once");
});

function docker(...args) {
  return execFileSync("docker", args, { stdio: ["ignore", "pipe", "pipe"] }).toString();
}

const dockerSkip = skip || (() => {
  try {
    docker("inspect", CONTAINER);
    return false;
  } catch {
    return `docker container ${CONTAINER} not available`;
  }
})();

test("Redis restarts empty: clients are told, then every new message arrives once", { skip: dockerSkip }, async (t) => {
  const nodes = await cluster("restart");
  const recs = Array.from({ length: 6 }, (_, i) => recorder(nodes, `r${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  await publish(nodes, 1, 100);
  await until(() => recs.every((r) => lastOffset(r) === 100), 20000, "first 100");
  docker("restart", CONTAINER);
  // Publishing resumes once Redis is back (503s retried meanwhile).
  await publish(nodes, 101, 200);
  await until(
    () => recs.every((r) => r.events.filter((e) => !e.gap && e.n > 100).length === 100),
    30000,
    "the second 100",
    recs,
  );
  for (const r of recs) {
    const stretches = check(r);
    // Told about the restart: an epoch change (before the new offsets).
    assert.ok(r.events.some((e) => e.gap && e.epoch), `${r.name}: told the history was lost`);
    const after = r.events.filter((e) => !e.gap && e.n > 100).map((e) => e.n);
    assert.deepEqual(after, Array.from({ length: 100 }, (_, i) => 101 + i), `${r.name}: each new message once, in order`);
    assert.deepEqual(stretches.at(-1).slice(-100), Array.from({ length: 100 }, (_, i) => i + 1), `${r.name}: new epoch from 1`);
  }
  assert.equal(recs.filter((r) => r.reconnects > 0).length, 0, "nobody was disconnected");
});

test("Redis pauses: publishing waits or is refused, nothing is lost or repeated", { skip: dockerSkip }, async (t) => {
  const nodes = await cluster("pause");
  const recs = Array.from({ length: 6 }, (_, i) => recorder(nodes, `p${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  const publishing = publish(nodes, 1, 300, 5);
  await until(() => recs.every((r) => (lastOffset(r) ?? 0) >= 50), 20000, "first 50");
  docker("exec", CONTAINER, "redis-cli", "CLIENT", "PAUSE", "2000", "ALL");
  await publishing;
  await until(() => recs.every((r) => lastOffset(r) === 300), 30000, "everyone at 300", recs);
  for (const r of recs) {
    const stretches = check(r, { ordered: false });
    assert.deepEqual(stretches.flat(), Array.from({ length: 300 }, (_, i) => i + 1), `${r.name}: every offset once`);
    const ns = r.events.filter((e) => !e.gap).map((e) => e.n).sort((a, b) => a - b);
    assert.deepEqual(ns, Array.from({ length: 300 }, (_, i) => i + 1), `${r.name}: every message once`);
  }
});

test("a node stopped while Redis is slow still delivers what it accepted", { skip: dockerSkip }, async (t) => {
  const nodes = await cluster("slowstop");
  const recs = Array.from({ length: 6 }, (_, i) => recorder(nodes, `s${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  await publish(nodes, 1, 50);
  await until(() => recs.every((r) => lastOffset(r) === 50), 20000, "first 50", recs);
  // Redis stalls writes; node 0 accepts (200) and queues 51..80, then is
  // stopped gracefully before Redis answers again.
  docker("exec", CONTAINER, "redis-cli", "CLIENT", "PAUSE", "1500", "WRITE");
  for (let n = 51; n <= 80; n++) {
    const r = await nodes[0].call("POST", "/api/v1/publish", { room: ROOM, data: { n } });
    assert.equal(r.status, 200, `publish ${n} accepted while Redis is paused`);
  }
  const paused = Date.now();
  await stopNode(nodes[0]);
  // A node cannot start while Redis does not answer: wait for the pause.
  await sleep(Math.max(0, paused + 1600 - Date.now()));
  nodes[0] = await startNode(nodes[0].prefix, nodes[0].ports);
  await publish(nodes, 81, 100);
  await until(() => recs.every((r) => lastOffset(r) === 100), 30000, "everyone at 100", recs);
  for (const r of recs) {
    const stretches = check(r);
    assert.deepEqual(stretches.flat(), Array.from({ length: 100 }, (_, i) => i + 1), `${r.name}: every offset once`);
  }
});

test("Redis restarts from an older snapshot: clients are told, then every new message arrives once", { skip: dockerSkip }, async (t) => {
  // An RDB snapshot (or a promoted replica) brings back an older state:
  // the same epoch with a lower offset counter. Offsets the clients have
  // already seen would be handed out again.
  const dropSnapshot = () => docker("exec", CONTAINER, "rm", "-f", "/data/dump.rdb");
  dropSnapshot();
  t.after(dropSnapshot);
  const nodes = await cluster("rollback");
  const recs = Array.from({ length: 6 }, (_, i) => recorder(nodes, `b${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  await publish(nodes, 1, 50);
  await until(() => recs.every((r) => lastOffset(r) === 50), 20000, "first 50", recs);
  docker("exec", CONTAINER, "redis-cli", "SAVE");
  // Connected after the snapshot: the restored Redis does not know it.
  const late = recorder(nodes, "late", 0);
  t.after(() => late.client.close());
  await joinAll([late]);
  await publish(nodes, 51, 80);
  await until(() => [...recs, late].every((r) => lastOffset(r) === 80), 20000, "up to 80", [...recs, late]);
  // The test Redis runs with --save "": it does not save on the way down
  // and loads the snapshot taken at 50 on the way up.
  docker("restart", CONTAINER);
  await publish(nodes, 81, 120);
  await until(
    () => recs.every((r) => r.events.filter((e) => !e.gap && e.n > 80).length === 40),
    30000,
    "the 40 messages after the restart",
    recs,
  );
  // The nodes compare their sessions with Redis every 15 s: the late
  // session must be put back, not closed as replaced.
  await sleep(17000);
  await publish(nodes, 121, 125);
  const all = [...recs, late];
  await until(() => all.every((r) => r.events.filter((e) => !e.gap && e.n > 80).length === 45), 30000, "the last 5", all);
  for (const r of all) {
    check(r);
    const after = r.events.filter((e) => !e.gap && e.n > 80).map((e) => e.n);
    assert.deepEqual(after, Array.from({ length: 45 }, (_, i) => 81 + i), `${r.name}: each new message once, in order`);
    const at81 = r.events.findIndex((e) => !e.gap && e.n === 81);
    assert.ok(r.events.slice(0, at81).some((e) => e.gap && e.epoch), `${r.name}: told the history changed before message 81`);
    assert.equal(r.reconnects, 0, `${r.name}: not disconnected`);
  }
});
