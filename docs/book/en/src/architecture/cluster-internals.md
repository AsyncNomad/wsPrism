# Inside the Cluster

This chapter describes how the Redis broker keeps several nodes consistent, and why each mechanism exists. [Running a Cluster](../guide/cluster.md) covers the same ground from an operator's point of view.

## Keys and channels

Every key and channel starts with the configured prefix (`wsp` here). Keys that belong to a room or a user carry a Redis Cluster hash tag, so that support for Redis Cluster will not need to rename them.

| Key | Holds |
|---|---|
| `wsp:r:{tenant::room}` | Pub/sub channel for the room |
| `wsp:r:{tenant::room}:st` | History state: last offset, epoch, bytes, per-node dedup marks, writing Redis `run_id` |
| `wsp:r:{tenant::room}:log` | History: sorted set scored by offset |
| `wsp:us:{tenant::user}` | Session table for one user |
| `wsp:pr:{tenant::room}` | Room membership across nodes |
| `wsp:nr:<node id>` | Rooms a node has members in |
| `wsp:nodes` | Live nodes |
| `wsp:ctl:<slot>` | Control channel to a node (close a session, …) |

Room names can contain any printable character. They never appear in key patterns, and each key type has its own suffix, so a crafted room name cannot collide with another key.

## Node slots

A node's identity in the cluster is one of 1,024 slots, claimed at startup with `SET NX PX` and renewed by a heartbeat every third of `slot_ttl_s`. Member ids embed the slot, which makes them unique across nodes without a central counter.

The heartbeat and a latency probe share a dedicated connection, so neither waits behind session traffic. If the probe fails four times in a row, the connection is replaced.

## Publishing

A publish on any node goes into the node's outbox. The outbox holds frames for every tenant, but keeps them in per-tenant FIFOs and serves tenants in turn, so one tenant's burst cannot push another's frames to the back of a single queue. Within a room, the order of frames and membership changes is preserved: a frame for a room whose subscription is still being confirmed waits for it, while other rooms go on.

Batches leave the outbox as script calls, one per room per batch:

- **Rooms without history** use `PUBLISH_FENCED`, which checks that this node still owns its slot and then publishes the batch.
- **Rooms with history** use `PUBLISH_KEPT`, which assigns each message the next offset, appends it to the log, trims the log by size, age and bytes, publishes, and records the per-node sequence number of the last frame applied.

That sequence number makes retries safe. When a batch's reply is lost (a timeout, a dropped connection), the outbox sends it again, and the script skips frames it has already applied. A history message is never stored or delivered twice because of the gateway's own retries.

When Redis is slow, frames wait. When the outbox, or a tenant's share of it, is full, or its oldest frame has waited `outbox_max_wait_ms`, new reliable publishes are refused with `UNAVAILABLE` and lossy ones are dropped. The publisher learns immediately, instead of getting a success that turns into a loss later.

## Receiving

A node subscribes to a room's channel only while it has members there. Rooms are spread over `pubsub_connections` by hash; a room always maps to the same connection, which preserves its order. Subscriptions requested together go out together, pipelined on each connection, which made joining new rooms eight to ten times faster under connection storms.

Frames from Redis are handed to rooms in batches of up to 1,024. That is why the session queue should stay at its default size or larger.

When a subscriber connection drops, the node reconnects, resubscribes, and sends `sys.gap` to the members of every room on that connection. It cannot know what it missed, so it says so.

## Joining a history room

In a single node, reading history and registering the new member happen atomically. Across nodes they cannot: the history read is a round trip to Redis, and messages keep arriving meanwhile. The node therefore holds the room's new messages aside (up to 1,000) while it reads, sends the replay, then releases the held messages, skipping any already covered by the replay. If more than 1,000 arrive during the read, it sends `sys.gap` after the replay instead of guessing.

## Epochs and Redis restarts

The `:st` hash records the `run_id` of the Redis process that wrote it. A restarted Redis, or a promoted replica, has a different `run_id`, and its data may be older than what clients have seen. When a node sees a mismatch, or finds the state missing, the room starts a new epoch. Clients holding offsets from the old epoch are told (`sys.gap` with the new epoch) and resync, instead of resuming from offsets that now refer to different messages.

## Sessions across nodes

A user's sessions live in one hash, changed only by Lua scripts. `CLAIM` decides a new connection atomically: replace the same `sid`, enforce `max_sessions_per_user`, evict the oldest if configured, and drop entries of nodes whose slot key no longer names them. The node that loses a session is told through its control channel.

Two races needed explicit handling and are covered by tests:

- An eviction can target a session that its node has claimed in Redis but not yet registered locally. The node records the target as *doomed* and refuses the registration when it arrives.
- A close command between nodes can be lost. Every 15 seconds each node compares its local sessions with the table and closes those the table no longer has.

## Presence and dead nodes

Membership lives in `wsp:pr:{…}`, one field per session. Every node looks for dead nodes every few seconds (their slot key no longer holds their id). One node wins a short lock for each dead node and removes its members. A member is reported only by the `HDEL` that actually removed it, so each `member_left` is sent exactly once, whichever node did the cleanup.

## Fencing

A node that froze, or lost Redis for longer than `slot_ttl_s`, may find that another node now holds its slot. If it carried on publishing, two nodes would speak with the same identity. Both publish scripts therefore check slot ownership inside Redis, in the same atomic step as the publish. A fenced node gets `FENCED`, marks itself unhealthy (`/readyz` turns 503), refuses new publishes at once, drains and exits with an error for the orchestrator to restart it.

Fencing covers room traffic, which is what clients see. Session and membership writes are not fenced; the other nodes clean them up once the node is gone.

Fencing doubled Redis CPU for plain publishes (about 1.2 µs more each) and, because a batch now returns one reply instead of one per message, raised what a single node can push through to Redis.

## Sentinel and failover

With Sentinel configured, the node asks the sentinels for the primary every second. When it changes, every connection (publish, subscribe, control, heartbeat) moves to the new primary. History rooms then start new epochs, and nodes register again and restore their sessions and memberships within one self-repair cycle.

One write cannot be repaired that way: consuming a one-time ticket. A promoted replica that missed it would accept the ticket again. So redemption waits for `WAIT` on a connection of its own until `replica_ack.replicas` replicas hold it, and fails closed if they do not answer in time.

## Overload protection

Every 250 ms the node pings Redis on its dedicated connection. While the reply takes longer than `admission.max_latency_ms`, or more than `max_pending_claims` session claims are waiting, new connections are refused at the handshake with 503 `cluster_busy` and a `Retry-After` spread over 1 to 5 seconds. This sits before authentication, so a refused client spends no ticket. Without it, a reconnect storm after an incident would queue thousands of session claims behind a struggling Redis, delay every node's heartbeat, and eventually get healthy nodes fenced.

## After Redis returns

Scripts are reloaded on `NOSCRIPT`. The heartbeat runs as soon as Redis answers, so a node that is starting up cannot lease a live node's slot that vanished with the old data. Reads that are safe to repeat (history and membership) are retried once when they hit a connection that died with the old process. Without that retry, the first gap-fill after a restart failed and was hidden only by the next heartbeat.

## Testing

The cluster's behaviour is pinned by three layers of tests:

- **Integration tests** against a real Redis in Docker, including TLS-only Redis with client certificates and Sentinel with mutual TLS.
- **Chaos tests** that kill nodes, restart Redis empty or from a snapshot, pause it, cut nodes off longer than `slot_ttl_s`, and perform rolling updates, then check that nothing was lost or delivered twice.
- **A randomised soak** that runs for ten minutes or more, injecting process faults, link faults (delay, blackhole, reset) and Redis restarts at random, with a judge that tracks every published message and every probe, and reports any loss or duplicate not explained by an announced gap.

The soak found several real defects before release, described in [Security](../project/security.md#how-defects-were-found).
