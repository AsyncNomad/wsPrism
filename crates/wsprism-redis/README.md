# wsprism-redis

Cluster broker for [wsPrism](https://github.com/AsyncNomad/wsPrism) on Redis
(ADR 0011): gateway nodes share rooms, room history, sessions, presence and
one-time tickets through one Redis. The gateway uses it when
`gateway.broker.kind: redis` is configured (default feature `redis` of
`wsprism-gateway` and `wsprism`).

**Experimental:** chaos-tested (node kill, rolling update, Redis restart
empty or from an older snapshot, Redis pause), but young; one Redis at one
address (no Sentinel or Redis Cluster yet), history throughput bounded by
one Redis core. Operations guide:
[docs/CLUSTER.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CLUSTER.md);
configuration and the least-privilege ACL:
[docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md).

Applications depend on [`wsprism`](https://crates.io/crates/wsprism), not on
this crate.

Minimum supported Rust version: 1.95. License: Apache-2.0.
