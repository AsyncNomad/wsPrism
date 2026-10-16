// End-to-end test of the example (ADR 0008 M2 done criterion): the real
// gateway on the example's gateway.yaml (ports changed), the example
// backend, and two SDK clients doing what index.html does.
// Run: examples/chat-and-state/run-tests.sh. Everything is on 127.0.0.1.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { WsPrismClient } from "../../../sdk/js/wsprism.js";

const here = join(dirname(fileURLToPath(import.meta.url)), "..");
const ROOT = join(here, "../..");
const BIN = process.env.WSPRISM_GATEWAY_BIN ?? join(ROOT, "target/debug/wsprism-gateway");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const freePort = () =>
  new Promise((resolve) => {
    const s = createServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });

async function waitFor(url, proc, what) {
  for (let i = 0; i < 100; i++) {
    try {
      await fetch(url);
      return;
    } catch {}
    if (proc.exitCode !== null) throw new Error(`${what} exited`);
    await sleep(50);
  }
  throw new Error(`${what} did not start`);
}

const procs = [];
let ports;

before(async () => {
  ports = { ws: await freePort(), api: await freePort(), backend: await freePort() };
  const yaml = readFileSync(join(here, "gateway.yaml"), "utf8")
    .replaceAll("127.0.0.1:8080", `127.0.0.1:${ports.ws}`)
    .replaceAll("127.0.0.1:8081", `127.0.0.1:${ports.api}`)
    .replaceAll(":3000", `:${ports.backend}`);
  const cfg = join(mkdtempSync(join(tmpdir(), "wsprism-example-")), "gateway.yaml");
  writeFileSync(cfg, yaml);

  const gw = spawn(BIN, ["--config", cfg], { stdio: "ignore" });
  const backend = spawn(process.execPath, [join(here, "backend.mjs")], {
    stdio: "ignore",
    env: { ...process.env, BACKEND_PORT: String(ports.backend), GATEWAY_API: `http://127.0.0.1:${ports.api}` },
  });
  procs.push(gw, backend);
  await waitFor(`http://127.0.0.1:${ports.ws}/healthz`, gw, "gateway");
  await waitFor(`http://127.0.0.1:${ports.backend}/`, backend, "backend");
});

after(() => {
  for (const p of procs) p.kill();
});

/** What the page does: log in at the backend, connect with the ticket. */
async function user(name) {
  const backend = `http://127.0.0.1:${ports.backend}`;
  const client = new WsPrismClient({
    url: `ws://127.0.0.1:${ports.ws}/v1/ws`,
    tenant: "demo",
    getTicket: async () => {
      const r = await fetch(`${backend}/login`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name }),
      });
      assert.equal(r.status, 200);
      return (await r.json()).ticket;
    },
  });
  await client.connect();
  return client;
}

function next(client, event, pred, ms = 5000) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`timed out waiting for ${event}`)), ms);
    const off = client.on(event, (x) => {
      if (pred(x)) {
        clearTimeout(t);
        off();
        resolve(x);
      }
    });
  });
}

test("the backend serves the page and the SDK", async () => {
  const page = await fetch(`http://127.0.0.1:${ports.backend}/`);
  assert.match(await page.text(), /wsPrism lobby/);
  const sdk = await fetch(`http://127.0.0.1:${ports.backend}/wsprism.js`);
  assert.match(sdk.headers.get("content-type"), /javascript/);
  const bad = await fetch(`http://127.0.0.1:${ports.backend}/login`, { method: "POST", body: '{"name":"a b"}' });
  assert.equal(bad.status, 400);
});

test("two users chat, roll dice through the backend, and share positions", async () => {
  const alice = await user("alice");
  const bob = await user("bob");
  const a = await alice.join("lobby");
  await bob.join("lobby");

  // Chat (Ext lane, built-in service).
  const chat = next(bob, "message", (m) => m.svc === "chat");
  alice.send("chat", "send", { room: "lobby", data: { msg: "hello" } });
  assert.deepEqual((await chat).data, { from: "alice", msg: "hello" });

  // Dice: RPC to the backend, which also publishes to the room via the server API.
  const announced = next(bob, "message", (m) => m.svc === "server" && m.data?.event === "roll");
  const { value } = await alice.rpc("game", "roll", { room: "lobby" });
  assert.ok(value >= 1 && value <= 6);
  assert.deepEqual((await announced).data, { event: "roll", by: "alice", value });

  // Unknown actions come back as the backend's own error code.
  await assert.rejects(alice.rpc("game", "fly"), (e) => e.code === "UNKNOWN_ACTION");

  // Positions (Hot lane, paced at 30 Hz): only the latest one matters.
  const latest = new Uint8Array([0xe8, 0x03, 0xf4, 0x01]); // x=1000, y=500
  const moved = next(bob, "state", (s) => s.sender === a.memberId && s.payload[0] === 0xe8);
  for (let x = 0; x < 20; x++) {
    alice.sendState("lobby", { svc: 1, op: 1, key: 0, payload: new Uint8Array([x, 0, 0, 0]) });
  }
  alice.sendState("lobby", { svc: 1, op: 1, key: 0, payload: latest });
  assert.deepEqual([...(await moved).payload], [...latest]);

  alice.close();
  bob.close();
});

test("the ticket's permissions keep users in the lobby", async () => {
  const eve = await user("eve");
  await assert.rejects(eve.join("admin"), (e) => e.code === "NOT_ALLOWED");
  eve.close();
});
