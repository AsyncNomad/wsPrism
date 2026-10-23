// Rejoining rooms after a reconnect, against a scripted fake WebSocket (no
// gateway): a room whose rejoin fails for a passing reason (the connection
// dropped meanwhile, the cluster was briefly unavailable) must stay joined
// from the client's point of view and be joined again — not be dropped for
// good (it was, whatever the error).

import { test } from "node:test";
import assert from "node:assert/strict";

import { PROTOCOL, WsPrismClient } from "../wsprism.js";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * A WebSocket class whose n-th connection answers joins with `script[n]`
 * (the last entry repeats): `(ws, msg, attempt) => void`, called for every
 * `room.join` with the attempt number on that connection.
 */
function fakeWebSocket(script) {
  const sockets = [];
  class FakeWs {
    constructor() {
      this.protocol = PROTOCOL;
      this.n = sockets.length;
      this.joins = 0;
      this.closed = false;
      sockets.push(this);
      setTimeout(() => {
        this.onopen?.();
        this.recv({ svc: "sys", type: "authed", data: { user: "u" } });
      }, 1);
    }
    recv(msg) {
      if (!this.closed) this.onmessage?.({ data: JSON.stringify(msg) });
    }
    send(text) {
      const msg = JSON.parse(text);
      if (msg.svc === "room" && msg.type === "join") {
        const answer = script[Math.min(this.n, script.length - 1)];
        answer(this, msg, (this.joins += 1));
      }
    }
    joined(msg) {
      this.recv({ svc: "sys", type: "joined", room: msg.room, chan: 7, member_id: 1, reply_to: msg.seq });
    }
    error(msg, code) {
      this.recv({ svc: "sys", type: "error", reply_to: msg.seq, data: { code, msg: "test" } });
    }
    close(code = 1000) {
      if (this.closed) return;
      this.closed = true;
      setTimeout(() => this.onclose?.({ code, reason: "" }), 1);
    }
  }
  return { FakeWs, sockets };
}

function client(FakeWs) {
  return new WsPrismClient({
    url: "ws://fake/v1/ws",
    tenant: "t",
    getTicket: () => "wpt_fake",
    WebSocket: FakeWs,
    reconnect: { minDelayMs: 10, maxDelayMs: 20 },
  });
}

async function until(cond, what, ms = 3000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await sleep(5);
  }
}

test("a room whose rejoin was cut by another drop is joined after the next reconnect", async () => {
  const { FakeWs, sockets } = fakeWebSocket([
    (ws, msg) => ws.joined(msg), // first connection: join
    (ws) => ws.close(1006), // second: the connection drops during the rejoin
    (ws, msg) => ws.joined(msg), // third: rejoin works
  ]);
  const c = client(FakeWs);
  await c.connect();
  await c.join("room:a");
  sockets[0].close(1006);
  await until(() => sockets.length >= 3 && sockets[2].joins >= 1, "a rejoin on the third connection");
  await until(() => c.rooms.has("room:a") && c.state === "open", "room kept");
  c.close();
});

test("a room whose rejoin was refused for a passing reason is retried on the same connection", async () => {
  const { FakeWs, sockets } = fakeWebSocket([
    (ws, msg) => ws.joined(msg),
    (ws, msg, attempt) => (attempt === 1 ? ws.error(msg, "UNAVAILABLE") : ws.joined(msg)),
  ]);
  const c = client(FakeWs);
  const rejoined = [];
  c.on("rejoined", (rooms) => rejoined.push(rooms.map((r) => r.room)));
  await c.connect();
  await c.join("room:a");
  sockets[0].close(1006);
  await until(() => sockets.length === 2 && sockets[1].joins >= 2, "a second join attempt", 5000);
  await until(() => rejoined.some((r) => r.includes("room:a")), "rejoined event for the room");
  assert.ok(c.rooms.has("room:a"));
  assert.equal(sockets.length, 2, "on the same connection");
  c.close();
});

test("a room whose rejoin is not allowed any more is dropped", async () => {
  const { FakeWs, sockets } = fakeWebSocket([
    (ws, msg) => ws.joined(msg),
    (ws, msg) => ws.error(msg, "NOT_ALLOWED"),
  ]);
  const c = client(FakeWs);
  const errors = [];
  c.on("error", (e) => errors.push(e?.code));
  await c.connect();
  await c.join("room:a");
  sockets[0].close(1006);
  await until(() => errors.includes("NOT_ALLOWED"), "the refusal");
  await sleep(50);
  assert.ok(!c.rooms.has("room:a"));
  assert.equal(sockets[1].joins, 1, "not retried");
  c.close();
});
