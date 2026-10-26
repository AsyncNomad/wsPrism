# Design Decisions

Every significant decision in wsPrism is recorded as an Architecture Decision Record: what was decided, why, which alternatives were rejected and what the decision costs. The full records are in the repository under [`docs/adr/`](https://github.com/AsyncNomad/wsPrism/tree/main/docs/adr) (written in Korean). This chapter summarises them and the reasoning that matters most to users.

## The records

| ADR | Decision | Status |
|---|---|---|
| 0001 | Lend the session context to services as `&mut`; rooms are an ordinary service | Adopted |
| 0002 | A transport-independent outbound frame, encoded once and shared by all recipients | Adopted |
| 0003 | Configurable credential sources; protocol version negotiated by subprotocol | Adopted |
| 0004 | Product direction: realtime state synchronisation | Adopted on condition of measurement |
| 0005 | The session/transport seam (`SessionTx` / `SessionRx`) | Adopted |
| 0006 | Protocol v2: explicit Hot channels, sender and keys, close codes that say how to reconnect | Adopted |
| 0007 | Write our own server-side WebSocket framing | Deferred: the first stage missed its target |
| 0008 | Integration: authentication chain, permissions, server API, proxies, SDK | Adopted |
| 0009 | Crate split and public API policy | Adopted |
| 0010 | Presence events | Adopted |
| 0011 | Broker boundary, history and recovery, multiple nodes | Adopted, implemented |
| 0012 | JWKS and in-band token refresh | Adopted |
| 0013 | Spread room traffic over several Redis instances | Design adopted, implementation deferred |
| 0014 | Terminate TLS on the public listener (`gateway.tls`) | Adopted, implemented |

## Decisions worth knowing as a user

### Rooms are a service, not a special case (0001)

`room.join` used to be handled inside the session loop with an `if svc == "room"`, because a service had no way to change session state. The fix was to give each session one context and lend it to services mutably. Rooms became an ordinary registered service, and so can any service of yours that needs to change what a session belongs to. The session loop has no knowledge of particular services.

### Credentials stay out of URLs (0003)

Browsers cannot set headers on a WebSocket, which is why so many systems put tokens in the query string, where proxies and CDNs log them. wsPrism carries them in a subprotocol entry instead, the same workaround Kubernetes uses, and never echoes it back. The query string remains available for legacy clients, off by default.

Cookies were rejected: they do not suit non-browser clients and make cross-site WebSocket hijacking easier, because the browser attaches them automatically.

### Protocol v2 was done once, before the SDK (0006)

A review of v1 against established gateways found six mismatches: lanes tied to WebSocket frame type, Hot frames without an explicit target, per-message version bytes, unknown fields rejected instead of ignored, a single close code for every kind of disconnect, and Hot frames that did not identify their sender. All six were fixed together in v2, before the SDK existed, when the change was cheapest. v1 is still accepted through negotiation.

The close-code ranges (4000s: don't retry, 4100s: back off, 4200s: retry now) follow a convention that client developers already know from Pusher.

### Presence events are opt-in and reliable (0010)

One join in a room of ten thousand produces ten thousand notifications, so presence events are opt-in per tenant. They are sent reliably, because a lost `member_left` leaves a ghost member forever, which is worse than a reconnect. The snapshot of current members comes with the join reply rather than as a separate request, so there is no window in which an event can fall between the two.

### Ext goes through Redis; Hot is delivered locally first (0011)

In a cluster, reliable messages pass through Redis even for members on the same node. That costs one Redis round trip, and in exchange every node delivers a room's messages in exactly the same order. Hot frames are delivered to local members at once and forwarded to other nodes. Routing state through Redis would add latency to the one kind of traffic whose whole point is low latency, and per-key sequence numbers already handle ordering.

Rejected alternatives for the cluster:

| Alternative | Why not |
|---|---|
| Sticky sessions, one node per room | Breaks as soon as a room's members are on several nodes; a node failure takes the room with it |
| A gossip mesh between nodes | Membership, partitions and reconnection would all have to be built and proven from scratch |
| NATS before Redis | Redis covers pub/sub, history, counters, key-value and presence in one store that most teams already operate. The broker trait does not rule out NATS later |
| History on by default | It costs memory per room and means nothing for state rooms |
| Client acknowledgements for recovery | Upstream traffic for every message. Asking again by offset is cheaper and simpler |

### Tickets wait for replicas; nothing else does (0011)

After a failover, everything the nodes wrote to Redis is repaired automatically: history starts a new epoch, nodes re-register and restore sessions. A consumed one-time ticket is the exception: a promoted replica that missed it would accept it again. That single write waits for replica acknowledgement. Everything else stays fast.

### JWKS and in-band refresh (0012)

Identity providers rotate keys and issue access tokens that last five to fifteen minutes. Without JWKS, every rotation meant a configuration change; without in-band refresh, every user reconnected every few minutes. Both were added before the cluster work, because together they decide whether a typical SSO setup works at all.

## Decisions that were deferred

### Our own WebSocket framing (0007)

Profiling showed WebSocket framing at about 14% of CPU and an 8 KB read buffer in every idle connection. A prototype that took over the upgraded socket did not meet its target at the first stage, and the work was set aside in favour of features. The transport seam means it can be resumed without touching sessions or services.

### Room traffic over several Redis instances (0013)

History throughput is bounded by one Redis core, about 140,000 to 150,000 messages a second. The design (16,384 logical slots mapped to shards, per-shard fencing with generation numbers) is adopted. Implementation waits for demand: one node already handles about 120,000 history publishes a second, so today the ceiling is roughly one node's worth of history traffic for the whole cluster. The complexity would sit at the core of the broker, and connection storms would still be bounded by the control Redis.

## Optimisations that were measured and reverted

Not every idea survived measurement. These were implemented, measured and rolled back, and the results are kept in [Benchmarks](../project/benchmarks.md):

- **mimalloc as the global allocator:** no CPU gain, and up to twice the memory per connection under load.
- **Indexing a node's rooms once per room instead of once per member:** 2% less Redis CPU, not worth another piece of state to keep consistent.
- **Batching session registrations into pipelines:** 8–12% less Redis CPU, but 3–6% less node throughput and higher connect latency. The node, not Redis, is the first ceiling.
- **Merging Redis commands into one Lua script:** an empty script costs Redis more than the commands it would replace.
