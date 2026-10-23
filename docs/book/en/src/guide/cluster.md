# Running a Cluster

One wsPrism node holds a great many connections, but one node is also one failure away from an outage. With a Redis broker, several nodes share rooms, sessions, presence and history, and clients can land on any of them.

> **Status:** the cluster is feature-complete and passes the failure tests described below, but it is marked **experimental** until it has run in production. Its configuration may still change in 0.x releases.

## What to deploy

- **Gateway nodes** behind any load balancer. Sticky sessions are not needed. Alternatively, give clients the list of nodes (`url: [...]` in the SDK); each reconnect tries the next one.
- **One Redis, version 6.2 or newer**, at a single address or as a primary managed by Redis Sentinel. Redis Cluster is not supported yet. Keys already use hash tags, so adding it later will not rename them.
- **Probes:** `/healthz` for liveness and `/readyz` for readiness. A node that has lost its place in the cluster turns unready and should be restarted.

The configuration:

```yaml
gateway:
  broker:
    kind: redis
    url_env: WSPRISM_REDIS_URL     # e.g. rediss://wsprism:secret@redis.internal:6380/0
    prefix: wsp
```

All the tunables have sensible defaults; [Configuration](../reference/configuration.md#cluster-broker) lists them. `deploy/compose/` in the repository runs two nodes and Redis with Docker Compose, from one configuration file whose differences come from environment variables.

### Preparing Redis

- Set `maxmemory-policy noeviction` (or a `volatile-*` policy). If Redis evicts the gateway's keys, history rooms restart in a new epoch and clients have to resync.
- Persistence is optional, and it does not carry history across a Redis restart (see [Redis restarts](#when-redis-restarts) below).
- Create a least-privilege user. The exact command list, checked by the test suite, is:

  ```
  ACL SETUSER wsprism on >PASSWORD ~wsp:* &wsp:* -@all +publish +subscribe +unsubscribe +set +get +getdel +del +pexpire +time +zadd +zcard +zrange +zrangebyscore +zremrangebyrank +zpopmin +hget +hmget +hgetall +hset +hincrby +hdel +hexists +sadd +srem +smembers +info +eval +evalsha +script|load +client|setinfo
  ```

  Replace `wsp` with your prefix. `client|setinfo` is optional.

- Use TLS (`rediss://`) on any network you do not fully control. A private CA and client certificates are configured under `gateway.broker.tls`.

### Sentinel

To follow a primary that Sentinel fails over, list the sentinels:

```yaml
gateway:
  broker:
    kind: redis
    url_env: REDIS_URL       # credentials and database only; the host comes from Sentinel
    sentinel:
      addresses: ["redis://sentinel-1:26379", "redis://sentinel-2:26379", "redis://sentinel-3:26379"]
      master_name: mymaster
```

Nodes ask Sentinel for the primary every second and move all their connections when it changes. In tests, delivery resumed about 4.5 seconds after the primary was stopped, and 2.3 to 3.2 seconds with mutual TLS everywhere. `rediss://` sentinel addresses connect to the sentinels over TLS, and the `tls` section applies to sentinels and Redis nodes alike.

With Sentinel, consuming a one-time ticket waits until at least one replica has the write (`replica_ack`, on by default with Sentinel). That is the one write a failover could undo without anything repairing it: a promoted replica that missed the consumption would accept the ticket a second time. If no replica acknowledges within the timeout, that login fails with 503. Run two replicas to keep ticket logins working through a failover.

## How the nodes cooperate

- **Startup.** A node claims one of 1,024 *node slots* in Redis before it serves anyone, and renews it with a heartbeat. Member ids embed the slot, so they are unique across the cluster. A node that cannot reach Redis does not start; your orchestrator restarts it.
- **Room traffic.** Ext messages pass through Redis, so every node delivers a room's messages in the same order. Hot state is delivered to local members at once and also sent to the other nodes.
- **Sessions** are cluster-wide. A reconnect with the same `sid` replaces the old connection on whichever node it is, and `max_sessions_per_user` holds exactly across nodes: a Redis script decides, so two concurrent connections cannot both win.
- **Presence** is cluster-wide: the presence endpoint and `sys.joined` snapshots list every node's members.
- **History** lives in Redis. A Lua script assigns each message its offset as it stores and publishes it, so every node sees the same offsets and a client can resume on any node.
- **Self-repair.** Every 15 seconds each node compares itself with Redis and puts back any sessions and memberships Redis has lost.

## What happens when…

Every row below is exercised by a chaos test or the randomised soak test.

| Event | What clients see | Recovery |
|---|---|---|
| A node is killed | Its clients reconnect to another node and resume history rooms from their last offset: nothing lost, nothing twice | Within `slot_ttl_s` plus about 5 s, the other nodes remove its members and send `member_left` once |
| A node stops gracefully (SIGTERM) | `/readyz` turns 503, sessions close with 4103 `draining` and resume elsewhere; nothing lost or repeated | Within `drain_grace_ms` the node hands its queue to Redis, then exits |
| Redis restarts empty, or from an older snapshot | History rooms start a new epoch (`sys.gap` with the epoch), other rooms get `sys.gap`; nobody is disconnected | Nodes reconnect, resubscribe, reload scripts and restore sessions within about 15 s |
| Sentinel fails over | As a Redis restart | Nodes follow within about a second |
| Redis is slow | Publishes wait; past `outbox_max_wait_ms` (1 s) new ones get `UNAVAILABLE`. Past `admission.max_latency_ms` (500 ms) new connections get 503 `cluster_busy` with `Retry-After` 1–5 s; connected sessions stay | Automatic within one 250 ms probe of Redis recovering |
| Redis is down | New connections close with 4102 `unavailable`; reliable publishes get `UNAVAILABLE`; Hot state still flows among members on the same node; ticket logins get 503 | Automatic when Redis returns |
| A node is cut off from Redis for longer than `slot_ttl_s` | Its rooms look empty to the others for a while | On reconnecting it re-registers and restores its sessions. If another node took its slot meanwhile, it drains and exits with an error for the orchestrator to restart it |
| A close command between nodes is lost | A replaced session may linger | Closed within about 15 s by self-repair |

### When Redis restarts

A restarted Redis, or a promoted replica, is a different process with data that may be older than what clients have already seen. Offsets could repeat with different contents. The gateway notices the change through Redis's `run_id`, starts every history room in a new epoch and tells clients so with `sys.gap`. They resync instead of silently receiving reused offsets. Persistence does not change this, because a snapshot is by definition older than the last writes.

### Fencing

A node that was cut off long enough for another to take its slot must not keep publishing as if nothing had happened. Redis enforces this: room publishes are checked against slot ownership inside the same script that performs them, so a node without its slot has every publish refused. It drains and exits at its first refused write or its next heartbeat, whichever comes first. Session and membership writes are not fenced; the other nodes clean them up once the node is gone.

## Rolling updates

Restart one node at a time:

1. Send SIGTERM; Kubernetes does this when it deletes a pod. The node turns unready, refuses new connections and server API publishes with 503, closes its sessions with 4103, and hands its queue to Redis, all within `drain_grace_ms` (2 s by default).
2. Start the new version and wait for `/readyz` to answer 200.
3. Move on to the next node.

On Kubernetes: `terminationGracePeriodSeconds` longer than `drain_grace_ms`, a readiness probe on `/readyz`, and `maxUnavailable: 1`. The chaos tests perform exactly this sequence and check that nothing is lost or repeated.

## Delivery semantics

- **Within a room, one order for everyone.** All nodes deliver a room's Ext messages in the same order.
- **Across rooms, no global order.** Messages to different rooms may leave a node in a different order than they were published.
- **Across nodes, Redis arrival order.** If your backend sends message A to node 1 and B to node 2, members receive them in the order Redis received them. Publish a room's messages through one node when their relative order matters.
- **A server API 200 means "queued on that node".** A node killed outright before flushing (normally microseconds) loses what it had queued. A node stopped with SIGTERM hands it over first.
- **The gateway's own retries never duplicate history messages**: each frame carries a per-node sequence that the Redis script checks. Plain room messages can be duplicated only if a Redis connection breaks after Redis ran a publish and before it replied.
- **Your backend's retries can duplicate.** If a publish times out and you send it again through another node, the first node may have accepted it after all. Put an id in messages where that matters.

## Capacity

A cluster today gives you **high availability with limited scaling**. More nodes carry more connections and more Hot traffic; room history, however, is bounded by a single Redis core.

| Traffic | Observed limit |
|---|---|
| Hot state | Same as a single node (delivered locally first) |
| Ext without history | About 240,000 publishes/s per node delivered in full; Redis spends about 2.5 µs per publish, roughly 600,000/s across the cluster on one core |
| Ext with history | About 150,000 kept messages/s per Redis core with ~200-byte messages |
| Connection storms | About 30,000 new sessions/s per node; one Redis core handles about 60,000/s of the worst case (connect, join, leave, disconnect) |

Keep `gateway.session_queue_capacity` at its default (1024) or higher: frames from Redis arrive in batches, and a small queue closes healthy readers as slow consumers.

## Known limits

- Tenant-wide limits (`max_sessions_total` and the room limits) are counted per node.
- One Redis primary. Spreading room traffic over several Redis instances is designed but not yet implemented.
- After Redis restarts empty, a user who reconnects with the same `sid` on another node before the old node has restored its sessions (about 15 s) can briefly hold two sessions for that sid.
