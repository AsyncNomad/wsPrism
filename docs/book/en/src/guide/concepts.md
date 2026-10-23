# Core Concepts

This chapter introduces the vocabulary used in the rest of the book. Each idea is short; together they describe almost everything the gateway does.

## Tenant

A tenant is an isolated customer of the gateway: an application, a game, a team, or one customer of your own SaaS product. One deployment can host many. Each tenant has:

- its own **authentication chain** (who may connect);
- its own **policy** (which messages are allowed, rate limits, session rules, browser origins);
- its own **limits** (frame size, sessions, rooms, users per room);
- its own **API keys** for the server API, optional **RPC routes** to its backend, and optional **history rules**.

Room names, user ids and session ids are always scoped by tenant. Two tenants can both have a room called `lobby` and a user called `alice`; they never meet.

Clients choose their tenant in the connection URL (`/v1/ws?tenant=acme`). An unknown tenant is refused before the WebSocket upgrade.

## Session

A session is one authenticated connection. It carries:

- the **user id**, decided by the authenticator, 1 to 64 characters of `[A-Za-z0-9_-]`;
- a **session id** (`sid`), which the client can choose with `?sid=` so that a reconnect replaces its previous connection instead of counting as a second one;
- the **permissions** that came with the credential (which rooms the user may join and publish to);
- a bounded **outbound queue**.

The tenant's session policy decides how many sessions one user may have at a time and what happens when a new one exceeds that number: refuse it, or close the oldest. With a cluster, these rules hold across all nodes.

## Room

A room is a named group of sessions. Joining a room subscribes to everything published in it; leaving unsubscribes. Rooms exist while they have members and need no declaration.

Room names are 1 to 128 bytes of UTF-8 without control characters. Patterns in configuration (`chat:*`) match by prefix, which makes a naming scheme such as `match:42` or `doc:7f3a` convenient.

Rooms can optionally keep a **history** of their recent messages, so that a client that reconnects can receive what it missed. See [Rooms, Presence and History](rooms-and-history.md).

## The two lanes

Every connection carries two kinds of traffic.

| | Ext lane | Hot lane |
|---|---|---|
| WebSocket frame | Text (JSON) | Binary |
| Addressed by | `svc` and `type` names | numeric service id and opcode |
| Delivery | Reliable, in order per room | Lossy: a slow recipient gets the latest value |
| Typical use | Chat, commands, room management, RPC | Positions, cursors, sensor readings, market ticks |
| Kept in history | Yes, if the room has a history rule | Never |

The Ext lane is where requests and their replies live. A request carries a `seq`, and every response to it carries the same number in `reply_to`.

The Hot lane is for state. A Hot frame can carry a **key**; the gateway keeps only the newest frame per sender and key for each recipient, so a client that stalls for a moment catches up with current state instead of replaying old positions.

## Services

Messages are handled by services. On the Ext lane a service has a name (`room`, `chat`, or one of yours); on the Hot lane it has a numeric id (`1` is the built-in room relay).

Built-in services cover rooms and chat. You can add your own in three ways, in order of effort:

1. route `svc:type` patterns to your backend over HTTP (the **RPC proxy**), in any language;
2. register a Rust service with `Gateway::builder`;
3. (planned) a WebAssembly plugin.

Whichever you choose, the tenant's allowlist must still name the messages. Registering a service makes it possible; the allowlist makes it permitted.

## Server API

The server API is how your backend talks to the gateway: issue tickets, publish to rooms, disconnect users, read who is in a room. It listens on a separate, internal address and authenticates with per-tenant API keys. See [Server API](../reference/server-api.md).

## Broker

The broker carries room traffic between sessions. With one node it is in-process memory. With several nodes it is Redis: a message published on any node reaches the room's members on every node, sessions and presence are shared, and history lives in Redis. Your clients and backend see the same protocol either way.

## Delivery classes

Internally every outgoing message is either **Reliable** or **Lossy**.

- A Reliable message that does not fit a recipient's queue closes that recipient as a *slow consumer* (close code 4101). The client reconnects and, in a history room, resumes where it left off. Dropping it quietly would break the promise that Ext messages arrive in order.
- A Lossy message that does not fit is dropped for that recipient only, and counted in `wsprism_egress_drop_total`.

This is the rule that makes the gateway's behaviour under overload predictable: a slow client never slows the room, and it never silently misses reliable data.
