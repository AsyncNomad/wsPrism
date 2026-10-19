// Compile-only check that wsprism.d.ts describes the API the way apps use
// it (tsc --noEmit --strict; never executed). Lines marked @ts-expect-error
// must stay errors.

import {
  WsPrismClient,
  WsPrismError,
  decodeHot,
  encodeHot,
  reconnectPolicy,
  type Joined,
  type State,
} from "../wsprism.js";

const client = new WsPrismClient({
  url: "wss://rt.example.com/v1/ws",
  tenant: "acme",
  getTicket: async () => "wpt_x",
  reconnect: { maxDelayMs: 10_000 },
});

const off: () => void = client.on("message", (m) => {
  const svc: string = m.svc;
  void svc;
});
off();
client.on("state", (s: State) => {
  const bytes: Uint8Array = s.payload;
  const room: string | undefined = s.room;
  void bytes, room;
});
client.on("presence", (p) => {
  const kind: "joined" | "left" = p.type;
  const sender: number = p.memberId;
  void kind, sender;
});
client.on("close", (c) => {
  const again: boolean = c.willReconnect;
  void again;
});
client.on("error", (e) => {
  if (e instanceof WsPrismError) {
    const code: string = e.code;
    void code;
  }
});

async function flow(): Promise<void> {
  const authed = await client.connect();
  const user: string = authed.user;
  const j: Joined = await client.join("match:42");
  const chan: number = j.chan;
  client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
  const reply = await client.request("room", "leave", { room: "match:42" });
  const moved = await client.rpc<{ ok: boolean }>("game", "move", { data: { x: 1 } });
  const ok: boolean = moved.ok;
  client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: new Uint8Array([1]) });
  client.close();
  void user, chan, reply, ok;
}
void flow;

const frame: Uint8Array = encodeHot({ chan: 1, svc: 1, op: 1, payload: new Uint8Array() });
const decoded = decodeHot(frame);
if (decoded) {
  const sender: number | undefined = decoded.sender;
  void sender;
}
const policy: "stop" | "backoff" | "now" = reconnectPolicy(4001);
void policy;

// @ts-expect-error getTicket is required
new WsPrismClient({ url: "ws://x", tenant: "t" });
// @ts-expect-error unknown event names are rejected
client.on("opened", () => {});
