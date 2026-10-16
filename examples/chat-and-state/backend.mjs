// Example backend (Node 22+, no dependencies). It is the part *you* write:
//   - logs users in and asks the gateway for a one-time ticket (server API)
//   - answers `game:*` requests the gateway forwards (RPC proxy)
//   - publishes to rooms through the server API
//   - serves the browser page and the SDK
//
// Run: node backend.mjs   (with the gateway running on gateway.yaml)

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.BACKEND_PORT ?? 3000);
const GATEWAY_API = process.env.GATEWAY_API ?? "http://127.0.0.1:8081";
const API_KEY = process.env.WSPRISM_API_KEY ?? "demo-key";

/** Call the gateway's server API with this tenant's key. */
async function gatewayApi(path, body) {
  const r = await fetch(GATEWAY_API + path, {
    method: "POST",
    headers: { authorization: `Bearer ${API_KEY}`, "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const json = await r.json();
  if (!r.ok) throw new Error(`gateway ${path}: ${r.status} ${JSON.stringify(json)}`);
  return json;
}

const readJson = (req) =>
  new Promise((resolve, reject) => {
    let s = "";
    req.on("data", (c) => (s += c));
    req.on("end", () => {
      try {
        resolve(s ? JSON.parse(s) : {});
      } catch (e) {
        reject(e);
      }
    });
  });

function send(res, status, body, type = "application/json") {
  res.writeHead(status, { "content-type": type });
  res.end(typeof body === "string" ? body : JSON.stringify(body));
}

/** RPC: the gateway forwards `{tenant, user, sid, svc, type, room, data}`. */
async function rpc(req) {
  switch (req.type) {
    case "roll": {
      const value = 1 + Math.floor(Math.random() * 6);
      // Everyone in the room sees it, through the server API.
      if (req.room) await gatewayApi("/api/v1/publish", { room: req.room, data: { event: "roll", by: req.user, value } });
      return { data: { value } };
    }
    default:
      return { error: { code: "UNKNOWN_ACTION", msg: `no such action: ${req.type}` } };
  }
}

const routes = {
  // Your login goes here. The demo trusts any name; the ticket limits the
  // user to the lobby (subscribe + publish), and is valid once, for 30 s.
  "POST /login": async (req, res) => {
    const { name } = await readJson(req);
    if (typeof name !== "string" || !/^[A-Za-z0-9_-]{1,32}$/.test(name)) {
      return send(res, 400, { error: "name: 1-32 letters, digits, _ or -" });
    }
    const { ticket } = await gatewayApi("/api/v1/tickets", {
      user: name,
      permissions: { subscribe: ["lobby"], publish: ["lobby"] },
    });
    send(res, 200, { ticket });
  },
  "POST /wsprism/rpc": async (req, res) => send(res, 200, await rpc(await readJson(req))),
  "GET /": async (_, res) => send(res, 200, await readFile(join(here, "index.html"), "utf8"), "text/html; charset=utf-8"),
  "GET /wsprism.js": async (_, res) =>
    send(res, 200, await readFile(join(here, "../../sdk/js/wsprism.js"), "utf8"), "text/javascript"),
};

const server = createServer(async (req, res) => {
  const route = routes[`${req.method} ${req.url}`];
  if (!route) return send(res, 404, { error: "not found" });
  try {
    await route(req, res);
  } catch (e) {
    console.error(e);
    send(res, 500, { error: "internal" });
  }
});

server.listen(PORT, "127.0.0.1", () => console.log(`backend on http://localhost:${PORT}`));
