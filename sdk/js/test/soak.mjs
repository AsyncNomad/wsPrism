// Randomized cluster soak (0.1.x; the pre-release review asked for it):
// three gateway nodes on Redis, SDK clients in one history room, a backend
// publishing 1, 2, 3, ... through any node's server API, and a seeded
// random sequence of faults - sometimes two at once:
//
//   node: kill -9 / graceful stop / short freeze / freeze past the slot TTL
//         (fencing) - each followed by a restart when the node is gone
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

import {
  CONTAINER, REDIS, ROOM, check, docker, joinAll, killAll, prefix, recorder,
  sleep, startNode, stopNode,
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
const nodes = [await startNode(p), await startNode(p), await startNode(p)];
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
      nodes[i] = await startNode(n.prefix, n.ports);
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
const killedAt = []; // { node, at } of kill -9
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
    killedAt.push({ node: i, at: Date.now() });
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
    nodes[i].proc.kill("SIGSTOP");
    await sleep(ms);
    nodes[i].proc.kill("SIGCONT");
    await sleep(2000); // a fenced node drains and exits meanwhile
    if (nodes[i].proc.exitCode !== null) note(`node ${i} stopped itself (fenced)`);
    await ensureUp(i);
    nodes[i].alive = true;
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
const all = { ...nodeFaults, ...redisFaults };
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
const violations = [];
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
    const killed = killedAt.some((k) => k.node === a.node && k.at >= a.at && k.at - a.at < 1000);
    if (!announced && !killed) silent.push(n);
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
  violations,
};
console.log(JSON.stringify(summary, null, 2));
if (violations.length) console.error(timeline.join("\n"));
for (const r of recs) r.client.close();
killAll();
dropSnapshot();
process.exit(violations.length ? 1 : 0);
