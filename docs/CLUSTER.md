# Running a cluster

Several gateway nodes share rooms through Redis (`gateway.broker.kind:
redis`, ADR 0011). This page is for operators: what to deploy, what happens
when something fails, and what the limits are. Configuration reference:
[CONFIG.md](CONFIG.md) "Cluster broker". Status: **experimental** until the
first release that drops the label.

## What to deploy

- **Gateway nodes** behind any load balancer (no sticky sessions needed), or
  give clients the list of nodes (JS SDK: `url: [..]`; each reconnect tries
  the next node).
- **One Redis ≥ 6.2** (`GETDEL`) at one address. Sentinel and Redis
  Cluster are not supported yet: after a failover the nodes must reach the
  new primary at the same address (a proxy or virtual IP in front; keys
  already use hash tags, so Cluster support will not change them).
  - `maxmemory-policy noeviction` (or `volatile-*`): evicting the gateway's
    keys forces resyncs (history restarts in a new epoch).
  - Persistence is optional and does not keep history across a Redis
    restart: a restarted or failed-over Redis is a different process
    (`run_id`), whose data may be older than what clients saw (a snapshot,
    a replica that missed the last writes). Every history room then starts
    a new epoch: clients are told (`sys.gap` with the epoch) and resync,
    instead of silently getting reused offsets (chaos test). The nodes put
    their sessions and memberships back within ~15 s.
  - A least-privilege ACL user: [CONFIG.md](CONFIG.md) has the exact command
    list (verified by the cluster tests).
- **Probes:** `/healthz` (process alive), `/readyz` (503 while draining or
  after this node lost its slot — restart it).

## What happens when ...

| Event | Clients | Recovery |
|---|---|---|
| A node is killed (`kill -9`, crash) | Its clients reconnect to another node and resume history rooms from their last offset: nothing lost, nothing twice (chaos test) | Within `slot_ttl_s` + ~5 s the other nodes remove its members and send `member_left` once (presence events on); its sessions stop counting toward limits |
| A node stops gracefully (SIGTERM) | `/readyz` 503; server API publishes 503 (publish elsewhere); sessions closed (v2 4103 `draining`, v1 1001) and resume on another node: nothing lost or repeated (chaos tests: rolling update, stop while Redis is slow) | Within `drain_grace_ms` the node hands over its queue (publishes it accepted, members leaving) before exiting |
| Redis restarts (without data, or from an older snapshot) | History rooms: the nodes refill from the new history and tell clients the history restarted (`sys.gap` with the new epoch); every message published after the restart arrives once. Rooms without history: `sys.gap` (resync). Nobody is disconnected | Nodes reconnect, re-subscribe, reload scripts, and within ~15 s put their sessions and memberships back |
| Redis pauses or is slow | Publishes wait; past `outbox_max_wait_ms` (1 s) new ones are refused (`UNAVAILABLE`, server API 503). Nothing is lost or delivered twice (chaos test) | Automatic when Redis answers again |
| Redis is down | New connections 1013 (try again later); Reliable publishes `UNAVAILABLE`; Hot state still flows between members of the same node; ticket logins 503 | Automatic when Redis is back (as above) |
| A node is cut off from Redis longer than `slot_ttl_s` | Its rooms look empty to the others for a while (`member_left` sent) | When it reconnects it re-registers and restores its sessions and memberships; if another node took its slot meanwhile, `/readyz` turns 503: restart it |
| A close command between nodes is lost | A replaced session may linger on its node | Closed within ~15 s (the node checks its sessions against Redis) |

## Rolling update

Restart one node at a time; the others take its clients.

1. Send SIGTERM (Kubernetes does on pod deletion). The node turns
   `/readyz` 503, refuses new connections and server API publishes (503,
   `Retry-After: 1`), closes its sessions (v2 4103 `draining`, v1 1001;
   clients reconnect after a backoff to another node and resume their
   history rooms), and hands its queue to Redis, all within
   `gateway.drain_grace_ms` (default 2 s; raise it if Redis can be slow).
2. Start the new version; wait until its `/readyz` answers 200.
3. Next node.

Back-ends: retry a 503 on another node (or through the load balancer).
Clients: give the SDK every node (`url: [..]`) or a load balancer address.
A node cannot start while Redis does not answer: it exits and the
orchestrator restarts it. Kubernetes: `terminationGracePeriodSeconds` above
`drain_grace_ms`, a readiness probe on `/readyz`, `maxUnavailable: 1`.

## Delivery semantics

- **Per room, one order for everyone:** Ext messages go through Redis, so
  every node delivers a room's messages in the same order.
- **Across nodes, no publish order:** a backend that sends message A to node
  1 and B to node 2 gets them in Redis arrival order, which can differ from
  the order it sent them when node 1 is slow. Publish a room's messages to
  one node if their order matters.
- **The server API's 200 means "queued on that node"**, not "delivered".
  A node that is killed (crash, `kill -9`) before flushing its queue
  (normally microseconds) loses what it had queued; a node stopped with
  SIGTERM hands it over first (within `drain_grace_ms`). Back-ends that need certainty publish to rooms with
  history and check the offsets their clients report.
- **Retries never duplicate history messages** (each frame carries a
  per-node sequence the Redis script checks). Plain room messages can be
  duplicated only if a connection to Redis breaks after Redis ran a publish
  but before it answered.

## Capacity (one Redis core, loopback; [BENCHMARKS.md](BENCHMARKS.md))

A cluster today is **high availability with limited scaling**: more nodes
carry more connections and Hot state, but room history stays bounded by one
Redis core, and Redis work for plain room messages grows with the number of
nodes subscribed to a room. Scaling history across several Redis
instances is planned after 0.1.0.

| Traffic | Limit seen |
|---|---|
| Hot state | Same as a single node (delivered locally first) |
| Ext without history | ~200k publishes/s per node (one node's outbox; 240k/s is at the edge: 0.88-1.0 delivered) |
| Two nodes sharing rooms | Every message once, latency as one node, up to 1.08M deliveries/s tested; splitting the same cores costs +10-28% CPU per delivery (less at high load; broadcast rooms cost less) |
| Redis CPU | Grows with the number of nodes subscribed to a room (each publish goes to each); history scripts do not |
| Ext with history | ~150k kept messages/s per Redis core (~200 B messages; fewer for larger ones) |

Sessions queue: keep `gateway.session_queue_capacity` well above 256 (frames
from Redis come in batches); the default 1024 is fine.

## Known limits

- Tenant-wide limits (`max_sessions_total`, room limits) are per node.
- One Redis at one address: no Sentinel or Redis Cluster yet.
- A node cut off from Redis longer than `slot_ttl_s` whose slot another node
  took turns `/readyz` 503 but keeps serving its connected sessions until it
  is restarted, with member ids that may collide with the other node's
  (wrong Hot `sender`, presence). Restart such a node; automatic fencing is
  planned.
- After Redis restarts empty, a user who reconnects with the same `sid` on
  another node before the old node restored its sessions (~15 s) can end up
  with two sessions for that sid until one disconnects.
