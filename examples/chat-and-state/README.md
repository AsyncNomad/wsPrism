# Example: chat and shared state

A lobby where everyone chats, rolls dice, and sees each other's cursors.
It shows the pieces you need to put wsPrism behind your own app:

| Piece | Where | What it shows |
|---|---|---|
| Login → one-time ticket | `backend.mjs` `POST /login` | Your backend authenticates the user, asks the gateway for a ticket (`POST /api/v1/tickets`) with **room permissions** (lobby only), and returns it to the page |
| Connect with the ticket | `index.html` (`getTicket`) | The SDK sends the ticket in the WebSocket subprotocol and asks for a new one on every reconnect |
| Chat | `client.send("chat", "send", ...)` | Built-in service on the Ext lane |
| Dice | `client.rpc("game", "roll", ...)` → `backend.mjs` `POST /wsprism/rpc` | **RPC proxy**: the gateway forwards `game:*` to your backend; the backend answers *and* broadcasts to the room via `POST /api/v1/publish` |
| Cursors | `client.sendState(...)` / `on("state")` | Hot lane state, paced at 30 Hz (`hot_tick_hz`), latest value per user |
| Who is here | `client.on("presence", ...)` | Presence events (`presence_events: true`): a member's cursor disappears when it leaves |

## Run

Requires Rust and Node 22+.

```sh
examples/chat-and-state/run.sh      # gateway on :8080 (+ API on 127.0.0.1:8081), backend on :3000
```

Open http://localhost:3000 in two browser windows, pick two names.

Configuration is in `gateway.yaml`. The server API key is the published
string `demo-key`, so don't reuse this config anywhere real (the file says
how to make your own key). The page must be served from an origin listed
in `allowed_origins`.

## Test

```sh
examples/chat-and-state/run-tests.sh
```

This starts the real gateway (on `gateway.yaml` with other ports) and the
backend, then two SDK clients follow the page's flow: log in, chat, roll
through the backend, share positions. It also checks that the ticket's
permissions keep users out of other rooms.
