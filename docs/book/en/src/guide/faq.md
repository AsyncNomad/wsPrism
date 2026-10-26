# FAQ and Troubleshooting

## Connecting

**My browser gets 403 during the handshake.**
The page's origin is not in the tenant's `policy.allowed_origins`. The list is empty by default, which refuses every browser. Add the exact origin (`https://app.example.com`, or `http://localhost:3000` during development). `https://*.example.com` covers subdomains.

**The handshake fails with 401.**
None of the tenant's ticket sources carried a credential, or the authentication chain rejected it. Check that the client sends the ticket where the tenant looks for it (`ticket_sources`; the query string is off by default), and that the chain contains an authenticator for that kind of ticket. Remember that one-time tickets are consumed on first use, including failed attempts, and expire after 30 seconds by default.

**The handshake fails with 400.**
The tenant id is unknown, the `sid` is not 1 to 64 characters of `[A-Za-z0-9_-]`, or the client offered subprotocols without a version the gateway speaks (`wsprism.v2.json` or `wsprism.v1`).

**Clients get 503 when connecting.**
Look at the `Retry-After` header and at `wsprism_handshake_rejections_total`. Common causes: the node is draining (connect to another), the tenant reached `max_sessions_total`, the authentication backend or JWKS endpoint is unreachable, or, in a cluster, Redis is slow enough that the node is shedding new connections (`cluster_busy`).

**Does wsPrism terminate TLS?**
Yes, if you set `gateway.tls` with a certificate and key; SIGHUP reloads them without a restart ([Configuration](../reference/configuration.md#gatewaytls)). It is also the cheapest way to serve `wss://` from the server itself: a TLS proxy such as nginx on the same host doubled CPU per delivery ([TLS](../project/benchmarks.md#tls)). Terminating TLS on a separate load balancer works too; then list it in `gateway.trusted_proxies` so that rate limits see real client addresses.

## Messages

**My request is answered with `NOT_ALLOWED`.**
Either the `svc:type` is not in the tenant's `ext_allowlist`, the user's permissions do not cover the room, or the session has not joined the room it is sending to. The `msg` field says which.

**Messages to one room arrive, but my own service never sees its requests.**
A service must be both registered (built in, `Gateway::builder`, or an `rpc.allow` route) and allowlisted. An allowlist entry for a service that does not exist fails at startup, so if the gateway started, check the allowlist.

**Clients are closed with `slow_consumer` (4101).**
A reliable message did not fit the client's outbound queue. Either the client reads too slowly (a background tab, a weak device, a congested link), or a room publishes more reliable traffic than clients can absorb. The client reconnects and resumes history rooms from its last offset. If it happens often, move high-rate data to the Hot lane, where a slow client gets the latest value instead of a backlog. Raise `session_queue_capacity` only if bursts are short.

**Why doesn't the gateway just drop old messages for slow clients, like it does on the Hot lane?**
Because the Ext lane promises in-order delivery or an explicit failure. A dropped chat message that nobody is told about is worse than a reconnect that recovers it.

**Do messages arrive in the order I published them?**
Within a room, every member sees the same order. Across rooms, and across nodes when your backend publishes to different nodes, there is no global order. See [delivery semantics](cluster.md#delivery-semantics).

**Is delivery guaranteed?**
Delivery is at-most-once per connection, with recovery on top. History rooms let a client that reconnects fetch what it missed, and the gateway says plainly (`recovered: false`, `sys.gap`) when that is not possible. For anything that must never be lost, keep the source of truth in your database and treat those signals as a cue to reload.

## Cluster

**A message published on one node doesn't reach clients on another.**
Check that every node uses `kind: redis` with the same `prefix` and the same Redis. `wsprism_broker_failed_total` counts commands Redis rejected; a non-zero value usually means a missing ACL permission.

**Why did all my history rooms send `sys.gap` with a new epoch?**
Redis restarted, failed over, or evicted the gateway's keys. Its data may be older than what clients saw, so the gateway starts a new epoch rather than risk reusing offsets. Use `maxmemory-policy noeviction` to rule out eviction.

**A node exited on its own with an error.**
It was cut off from Redis longer than `slot_ttl_s`, and another node took its slot. Continuing would have meant publishing as a node the cluster had declared dead, so it drained and stopped. Your orchestrator should restart it; it will claim a fresh slot.

**Can I use Redis Cluster, or Valkey, or a managed Redis?**
Redis Cluster is not supported yet. Any server that speaks the Redis protocol, supports the commands in the ACL list, Lua scripting and pub/sub, and is version 6.2 or newer should work. The tests run against Redis 7.

## Integration

**Do I have to write Rust?**
No. Authentication (tickets, JWT, connect proxy), publishing, disconnecting and request handling (the RPC proxy) all work over HTTP from any language. Rust is needed only for services that run inside the gateway.

**Is there a Socket.IO or Pusher compatibility layer?**
No. Clients speak the wsPrism protocol. The JavaScript SDK covers browsers and Node; other platforms can implement the protocol with any WebSocket library, since it is fully specified in [Wire Protocol](../reference/protocol.md).

**Can I change the configuration without restarting?**
Not yet. Restart the node; with several nodes, restart them one at a time. Hot reload of tenant policies is planned.

**Is it production-ready?**
The single-node gateway is complete, fuzzed, and covered by about 270 Rust tests plus SDK and chaos tests. The cluster is labelled experimental until it has run in production. No external security audit has been done yet. Read [Security](../project/security.md) and decide accordingly.
