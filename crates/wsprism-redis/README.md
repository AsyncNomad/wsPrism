# wsprism-redis

Cluster broker for [wsPrism](https://github.com/AsyncNomad/wsPrism) on Redis
(ADR 0011): gateway nodes share rooms through Redis pub/sub.

**Experimental** until the cluster work (ADR 0011 R3-R5) is complete. The
gateway uses it when `gateway.broker.kind: redis` is configured.
