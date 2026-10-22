// Randomized cluster soak (0.1.x; the pre-release review asked for it):
// three gateway nodes on Redis, SDK clients in one history room, a backend
// publishing 1, 2, 3, ... through any node's server API, and a seeded
// random sequence of faults - sometimes two at once:
//
//   node: kill -9 / graceful stop / short freeze / freeze past the slot TTL
//         / freeze while another node leases its slot (fencing) - each
//         followed by a restart when the node is gone
//   node <-> Redis link (one node's own proxy): latency, a blackhole that
//         keeps connections open (a peer that vanished; past the slot TTL
//         the node is taken for dead), connection resets
//   Redis: pause (ALL or WRITE), restart empty, restart from an older
//          snapshot, drop every pub/sub connection, drop every command
//          connection
//
// Then the faults stop, everything recovers, and a final batch is
// published. Verdict per client:
//   - no message twice, offsets contiguous between announced gaps
//     (sys.gap / recovered:false) - the chaos tests' checker;
//   - no silent loss: a message the backend got a 200 for and the client
//     never received needs a gap announced between its acceptance and the
//     moment the client moved past it (got a later message), or a kill -9
//     of the node that accepted it within a second (its queue dies with
//     it: "200 = queued on that node", docs/CLUSTER.md);
//   - liveness: every client receives the whole final batch.
//
//   WSPRISM_TEST_REDIS=$(bench/redis-test.sh) node sdk/js/test/soak.mjs [--minutes 5] [--seed N]
//
// Needs the gateway built (cargo build -p wsprism-gateway) and the docker
// container of bench/redis-test.sh. Prints a JSON summary; exit 1 on a
// violation. Nothing leaves the machine.

import { parseArgs } from "node:util";
import { writeFileSync } from "node:fs";

import {
  CONTAINER, REDIS, ROOM, check, docker, joinAll, killAll, prefix, recorder,
  sleep, startNode, startProxy, stopNode,
} from "./cluster-harness.mjs";

const { values: opt } = parseArgs({
  options: {
    minutes: { type: "string", default: "5" },
    seed: { type: "string" },
    clients: { type: "string", default: "9" },
    final: { type: "string", default: "50" },
  },
});
if (!REDIS) {
  console.error("WSPRISM_TEST_REDIS not set (bench/redis-test.sh)");
  process.exit(2);
}
const seed = Number(opt.seed ?? Math.floor(Math.random() * 2 ** 31));
const minutes = Number(opt.minutes);

/** mulberry32: a small seeded generator, so a failing run can be replayed
 * (the schedule of faults; timing still varies). */
function rng(a) {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rand = rng(seed);
const between = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (xs) => xs[Math.floor(rand() * xs.length)];

const started = Date.now();
const timeline = [];
const violations = [];
const note = (what) => {
  const at = ((Date.now() - started) / 1000).toFixed(1);
  timeline.push(`${at}s ${what}`);
  if (process.env.SOAK_VERBOSE) console.error(`${at}s ${what}`);
};

// A run that does not finish is a finding too (something never recovered):
// past the expected time, print where it stands and fail.
const watchdogMs = (minutes * 60 + 240) * 1000;
setTimeout(() => {
  const state = {
    stuck: true,
    seed,
    nextN,
    acked: acked.size,
    nodes: nodes.map((n, i) => ({ i, alive: n.alive, exit: n.proc.exitCode, signal: n.proc.signalCode })),
    clients: recs.map((r) => {
      const m = r.events.filter((e) => !e.gap);
      return `${r.name}: ${m.length} msgs, last n=${m.at(-1)?.n}, state=${r.client.state}, reconnects=${r.reconnects}`;
    }),
  };
  console.log(JSON.stringify(state, null, 2));
  console.error(timeline.join("\n"));
  killAll();
  process.exit(3);
}, watchdogMs).unref();

process.on("exit", killAll);
const redisCli = (...args) => docker("exec", CONTAINER, "redis-cli", ...args);
const dropSnapshot = () => docker("exec", CONTAINER, "rm", "-f", "/data/dump.rdb");
dropSnapshot();

// --- cluster and clients -------------------------------------------------
const p = prefix(`soak${seed}`);
// Each node reaches Redis through its own fault proxy.
const proxies = [await startProxy(REDIS), await startProxy(REDIS), await startProxy(REDIS)];
const nodes = [];
for (const px of proxies) nodes.push(await startNode(p, undefined, px.url));
/** Note when node `i` stops by itself (exit 1: fenced), not by a fault. */
const watchExit = (i) => {
  const proc = nodes[i].proc;
  proc.once("exit", (code) => {
    if (code === 1) note(`node ${i} exited by itself (fenced)`);
  });
};
nodes.forEach((_, i) => watchExit(i));
let fenced = 0;
const recs = Array.from({ length: Number(opt.clients) }, (_, i) => recorder(nodes, `s${i}`, i % 3));
await joinAll(recs);
note(`seed=${seed} nodes=3 clients=${recs.length}`);

/** Bring node `i` back on its ports if its process is gone. */
async function ensureUp(i) {
  const n = nodes[i];
  if (n.proc.exitCode === null && n.proc.signalCode === null) {
    n.alive = true;
    return;
  }
  for (let attempt = 0; ; attempt++) {
    try {
      nodes[i] = await startNode(n.prefix, n.ports, n.redisUrl);
      watchExit(i);
      return;
    } catch (e) {
      // e.g. Redis restarting right now: a node cannot start without it.
      if (attempt > 40) throw e;
      await sleep(500);
    }
  }
}
const exited = (n) => new Promise((r) => (n.proc.exitCode !== null || n.proc.signalCode !== null ? r() : n.proc.once("exit", r)));

// --- the backend ---------------------------------------------------------
const acked = new Map(); // n -> { node index, at }
// Where a node's queue may be lost, by design: what node `node` accepted
// in [from, to] (server API 200 = "queued on that node", docs/CLUSTER.md).
// kill -9: the last second before it. Fenced (its slot was taken while it
// was frozen or cut off): what it accepted while it could not reach Redis —
// Redis refuses everything from a fenced node.
const queueLost = []; // { node, from, to }
let stopPublishing = false;
let nextN = 1;
async function publishOne(n) {
  for (let attempt = 0; ; attempt++) {
    for (let k = 0; k < nodes.length; k++) {
      const i = (n + k) % nodes.length;
      if (!nodes[i].alive) continue;
      try {
        const r = await nodes[i].call("POST", "/api/v1/publish", { room: ROOM, data: { n } });
        if (r.status === 200) {
          acked.set(n, { node: i, at: Date.now() });
          return;
        }
      } catch {}
    }
    await sleep(Math.min(50 * (attempt + 1), 500));
  }
}
const publisher = (async () => {
  while (!stopPublishing) {
    await publishOne(nextN++);
    await sleep(between(2, 15));
  }
})();

// --- faults --------------------------------------------------------------
const nodeFaults = {
  async kill9() {
    const i = between(0, 2);
    note(`kill -9 node ${i}`);
    nodes[i].alive = false;
    queueLost.push({ node: i, from: Date.now() - 1000, to: Date.now() });
    nodes[i].proc.kill("SIGKILL");
    await exited(nodes[i]);
    await sleep(between(300, 3000));
    await ensureUp(i);
    note(`node ${i} back`);
  },
  async term() {
    const i = between(0, 2);
    note(`SIGTERM node ${i}`);
    await stopNode(nodes[i]);
    await sleep(between(0, 2000));
    await ensureUp(i);
    note(`node ${i} back`);
  },
  async freeze() {
    const i = between(0, 2);
    const ms = pick([between(300, 2000), between(4000, 7000)]); // slot_ttl_s is 3
    note(`SIGSTOP node ${i} for ${ms} ms`);
    nodes[i].alive = false;
    const at = Date.now();
    nodes[i].proc.kill("SIGSTOP");
    await sleep(ms);
    nodes[i].proc.kill("SIGCONT");
    await sleep(2000); // a fenced node drains and exits meanwhile
    if (nodes[i].proc.exitCode !== null) {
      note(`node ${i} stopped itself (fenced)`);
      queueLost.push({ node: i, from: at - 1000, to: at });
    }
    await ensureUp(i);
    nodes[i].alive = true;
  },
  async takeover() {
    // Freeze past the slot TTL while a newcomer leases the freed slot: the
    // frozen node must stop when it wakes (fencing), not serve on. A
    // newcomer takes the lowest free slot, so lower free ones (left by
    // earlier restarts) are held by placeholders first; and the judge
    // looks at the slot key itself, not at which node was started.
    const i = between(0, 2);
    note(`SIGSTOP node ${i}; a newcomer takes its slot`);
    const before = slotKeys();
    nodes[i].alive = false;
    const at = Date.now();
    nodes[i].proc.kill("SIGSTOP");
    await sleep(3600); // slot_ttl_s is 3
    const after = slotKeys();
    const gone = [...before.keys()].filter((s) => !after.has(s));
    let newcomer = null;
    let taken = false;
    if (gone.length === 1) {
      const slot = gone[0];
      for (let s = 0; s < slot; s++) {
        if (!after.has(s)) redisCli("SET", `${p}:slot:${s}`, "soak-placeholder", "NX", "PX", "8000");
      }
      newcomer = await startNode(nodes[i].prefix).catch(() => null);
      const now = slotKeys().get(slot);
      taken = now !== undefined && now !== before.get(slot) && now !== "soak-placeholder";
      note(`slot ${slot}: ${taken ? "taken by the newcomer" : "not taken; nothing to judge"}`);
    } else {
      note(`frozen node's slot unclear (${gone.length} expired); nothing to judge`);
    }
    nodes[i].proc.kill("SIGCONT");
    const stopped = await Promise.race([exited(nodes[i]).then(() => true), sleep(8000).then(() => false)]);
    if (stopped) {
      fenced += 1;
      note(`node ${i} stopped itself (fenced)`);
      queueLost.push({ node: i, from: at - 1000, to: at });
    } else {
      if (taken) violations.push(`node ${i} kept running after another node took its slot`);
      nodes[i].proc.kill("SIGKILL");
      await exited(nodes[i]);
    }
    if (newcomer) await stopNode(newcomer);
    await ensureUp(i);
  },
};
/** Node slot -> holder, read from Redis. */
function slotKeys() {
  const keys = redisCli("--scan", "--pattern", `${p}:slot:*`).split("\n").filter(Boolean);
  const m = new Map();
  for (const k of keys) {
    const v = redisCli("GET", k).trim();
    if (v) m.set(Number(k.slice(k.lastIndexOf(":") + 1)), v);
  }
  return m;
}
const linkFaults = {
  async linkDelay() {
    const i = between(0, 2);
    const ms = between(20, 800);
    const dur = between(1000, 6000);
    note(`node ${i} <-> Redis: +${ms} ms for ${dur} ms`);
    proxies[i].delay(ms);
    await sleep(dur);
    proxies[i].delay(0);
  },
  async partition() {
    const i = between(0, 2);
    const dur = pick([between(300, 2000), between(4000, 7000)]); // slot_ttl_s is 3
    note(`node ${i} <-> Redis: blackhole for ${dur} ms`);
    const from = Date.now() - 1000;
    proxies[i].blackhole(true);
    await sleep(dur);
    proxies[i].blackhole(false);
    await sleep(1000);
    if (nodes[i].proc.exitCode !== null) {
      note(`node ${i} stopped itself after the partition`);
      queueLost.push({ node: i, from, to: Date.now() });
    }
    await ensureUp(i);
  },
  async linkReset() {
    const i = between(0, 2);
    note(`node ${i} <-> Redis: connections reset`);
    proxies[i].reset();
  },
};
const redisFaults = {
  async pause() {
    const ms = between(200, 2500);
    const mode = pick(["ALL", "WRITE"]);
    note(`Redis CLIENT PAUSE ${ms} ${mode}`);
    redisCli("CLIENT", "PAUSE", String(ms), mode);
    await sleep(ms);
  },
  async restartEmpty() {
    note("Redis restart (empty)");
    docker("restart", CONTAINER);
  },
  async restartFromSnapshot() {
    redisCli("SAVE");
    note("Redis SAVE");
    await sleep(between(500, 3000));
    note("Redis restart (from that snapshot)");
    docker("restart", CONTAINER);
    dropSnapshot();
  },
  async dropPubsub() {
    note("Redis CLIENT KILL TYPE pubsub");
    redisCli("CLIENT", "KILL", "TYPE", "pubsub");
  },
  async dropCommands() {
    note("Redis CLIENT KILL TYPE normal (command connections)");
    redisCli("CLIENT", "KILL", "TYPE", "normal");
  },
};
const all = { ...nodeFaults, ...redisFaults, ...linkFaults };
const names = Object.keys(all);

const until = started + minutes * 60_000;
let faults = 0;
while (Date.now() < until) {
  await sleep(between(1000, 5000));
  if (rand() < 0.2) {
    // Two at once, one of them on a node.
    const a = pick(Object.keys(nodeFaults));
    const b = pick(names);
    note(`together: ${a} + ${b}`);
    await Promise.all([all[a](), all[b]()]);
    faults += 2;
  } else {
    await all[pick(names)]();
    faults += 1;
  }
}

// --- recovery and the final batch ----------------------------------------
note("faults stop");
for (let i = 0; i < nodes.length; i++) await ensureUp(i);
await sleep(20000); // reconciliation (15 s) and reconnects settle
stopPublishing = true;
await publisher;
const firstFinal = nextN;
const finalCount = Number(opt.final);
for (let n = firstFinal; n < firstFinal + finalCount; n++) await publishOne(n);
nextN = firstFinal + finalCount;
note(`final batch ${firstFinal}..${nextN - 1}`);
const deadline = Date.now() + 60000;
const gotFinal = (r) => new Set(r.events.filter((e) => !e.gap && e.n >= firstFinal).map((e) => e.n)).size;
while (Date.now() < deadline && !recs.every((r) => gotFinal(r) === finalCount)) await sleep(200);

// --- verdict ---------------------------------------------------------------
let queueLostCount = 0; // missing at the first client, exempted above
for (const r of recs) {
  try {
    check(r, { ordered: false });
  } catch (e) {
    violations.push(`${r.name}: ${e.message.split("\n")[0]}`);
  }
  if (gotFinal(r) !== finalCount) violations.push(`${r.name}: got ${gotFinal(r)}/${finalCount} of the final batch`);
  const msgs = r.events.filter((e) => !e.gap);
  const seen = new Set(msgs.map((e) => e.n));
  const firstN = msgs[0]?.n ?? Infinity;
  const gaps = r.events.filter((e) => e.gap).map((e) => e.at);
  // When the client moved past n: the first arrival of a later message.
  // (Messages from different nodes may arrive out of order, hence the
  // slack; any gap after that point would not explain this loss.)
  const byN = msgs.map((e) => [e.n, e.at]).sort((x, y) => x[0] - y[0]);
  const movedPast = (n) => {
    let t = Infinity;
    for (let k = byN.length - 1; k >= 0 && byN[k][0] > n; k--) t = Math.min(t, byN[k][1]);
    return t;
  };
  const silent = [];
  for (const [n, a] of acked) {
    if (n < firstN || seen.has(n)) continue;
    const past = movedPast(n);
    const announced = gaps.some((at) => at >= a.at - 2000 && at <= past + 2000);
    const lostQueue = queueLost.some((q) => q.node === a.node && a.at >= q.from && a.at <= q.to);
    if (!announced && !lostQueue) silent.push(n);
    if (!announced && lostQueue && r === recs[0]) queueLostCount += 1;
  }
  if (silent.length) violations.push(`${r.name}: ${silent.length} accepted messages never arrived and no gap was announced (e.g. ${silent.slice(0, 5)})`);
}

const summary = {
  seed,
  minutes,
  faults,
  published: nextN - 1,
  clients: recs.length,
  reconnects: recs.reduce((s, r) => s + r.reconnects, 0),
  gaps_announced: recs.reduce((s, r) => s + r.events.filter((e) => e.gap).length, 0),
  fenced,
  lost_with_a_queue: queueLostCount,
  violations,
};
console.log(JSON.stringify(summary, null, 2));
if (violations.length) console.error(timeline.join("\n"));
// Evidence for a failed run: one client's events and when each n was accepted.
if (violations.length && process.env.SOAK_DUMP) {
  writeFileSync(process.env.SOAK_DUMP, JSON.stringify({ started, events: recs[0].events, acked: [...acked] }));
}
for (const r of recs) r.client.close();
killAll();
for (const px of proxies) px.close();
dropSnapshot();
process.exit(violations.length ? 1 : 0);
