// Ports for the gateways the tests start.
//
// Not `listen(0)`: that hands out a port from the kernel's ephemeral range
// (32768-60999 on Linux), which outgoing connections (Redis clients, fetch)
// also take their source ports from. Between closing the probe socket and
// the gateway binding the port, such a connection can take it, and the
// gateway fails with "Address already in use" (seen in CI). Ports below the
// ephemeral range are only ever taken by listeners.

import { createServer } from "node:net";

const LOW = 20000;
const HIGH = 32768;
const handedOut = new Set(); // a stopped node keeps its ports for its restart

function free(port) {
  return new Promise((resolve) => {
    const s = createServer();
    s.once("error", () => resolve(false));
    s.listen(port, "127.0.0.1", () => s.close(() => resolve(true)));
  });
}

/** A port no listener uses now and this process has not handed out. */
export async function freePort() {
  for (;;) {
    const port = LOW + Math.floor(Math.random() * (HIGH - LOW));
    if (handedOut.has(port) || !(await free(port))) continue;
    handedOut.add(port);
    return port;
  }
}

/** Whether a gateway's output says a listener could not bind. */
export const bindFailed = (stderr) => /Address already in use/.test(stderr);
