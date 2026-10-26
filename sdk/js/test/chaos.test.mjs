// Chaos tests (ADR 0011 R5): two gateway nodes on Redis, SDK clients in a
// history room, a backend publishing through the server API — then a node
// is killed (-9), the nodes are restarted one by one (rolling update), a
// node is frozen past its slot TTL and its slot is taken (fencing), Redis
// restarts empty or from an older snapshot, or Redis pauses. An offset checker
// judges what every client received: no duplicate, no reordering, and no
// gap unless the gateway announced one (sys.gap / recovered:false).
//
// Needs WSPRISM_TEST_REDIS (bench/redis-test.sh). The Redis restart and
// pause scenarios also need `docker` and the container name in
// WSPRISM_TEST_REDIS_CONTAINER (default wsp-redis). Everything listens on
// 127.0.0.1 or the isolated docker network; nothing leaves the machine.

import { test, after } from "node:test";
import assert from "node:assert/strict";

import {
  CONTAINER, ROOM, check, cluster, docker, joinAll, killAll, lastOffset,
  publish, recorder, restartNode, skip, sleep, startNode, stopNode, until,
} from "./cluster-harness.mjs";

after(killAll);

test("a node killed with -9: its clients resume on the other node, nothing lost or repeated", { skip }, async (t) => {
  const nodes = await cluster("kill");
  const recs = Array.from({ length: 10 }, (_, i) => recorder(nodes, `k${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  const total = 600;
  // The backend publishes through node 1, the one that survives. A publish
  // node 0 had accepted (200 = queued on that node) but not yet handed to
  // Redis when it is killed is lost: documented (docs/CLUSTER.md, delivery
  // semantics) and judged by the soak test's queue-loss windows. Here it
  // would only leave the final offset short of `total` — on a slow CI
  // runner it did (offsets stopped at 561 of 600 with a 100 ms Redis delay
  // in front of node 0). What this test checks is the killed node's
  // clients resuming on the other node, nothing lost or repeated.
  const publishing = publish([nodes[1], nodes[0]], 1, total, 5);
  // Mid-stream, node 0 dies without a goodbye.
  await until(() => recs.every((r) => (lastOffset(r) ?? 0) >= 150), 20000, "first 150", recs);
  nodes[0].proc.kill("SIGKILL");
  nodes[0].alive = false;
  await publishing;
  await until(() => recs.every((r) => lastOffset(r) === total), 30000, "everyone at the last offset", recs);
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

test("a node frozen past its slot TTL whose slot was taken stops; its clients move on", { skip }, async (t) => {
  // Fencing (pre-release review): a node cut off (here: SIGSTOP) longer than
  // slot_ttl_s is taken for dead; a new node may lease its slot and issue
  // the same member ids. When the old node wakes up it must not keep
  // serving with that slot.
  const nodes = await cluster("fence");
  const recs = Array.from({ length: 6 }, (_, i) => recorder(nodes, `f${i}`, i % 2));
  t.after(() => recs.forEach((r) => r.client.close()));
  await joinAll(recs);
  await publish(nodes, 1, 50);
  await until(() => recs.every((r) => lastOffset(r) === 50), 20000, "first 50", recs);

  const frozen = nodes[0];
  frozen.proc.kill("SIGSTOP");
  frozen.alive = false; // the backend skips it meanwhile
  await sleep(4500); // slot_ttl_s is 3
  const newcomer = await startNode(frozen.prefix); // leases the free slot 0
  nodes.push(newcomer);
  const exited = new Promise((r) => frozen.proc.once("exit", (code) => r(code)));
  frozen.proc.kill("SIGCONT");
  const code = await Promise.race([exited, sleep(15000).then(() => "still running")]);
  assert.notEqual(code, "still running", "the fenced node stopped");
  assert.notEqual(code, 0, "and said it failed");

  await publish(nodes, 51, 100);
  await until(() => recs.every((r) => lastOffset(r) === 100), 30000, "everyone at 100", recs);
  for (const r of recs) {
    const stretches = check(r);
    assert.deepEqual(stretches.flat(), Array.from({ length: 100 }, (_, i) => i + 1), `${r.name}: every offset once`);
  }
  assert.ok(recs.filter((r) => r.reconnects > 0).length >= 3, "the fenced node's clients moved");
});


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
