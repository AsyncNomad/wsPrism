# Running a cluster

Several gateway nodes can share rooms through Redis (experimental). The
operations guide lives next to the code, kept in sync with the chaos tests:

- **[docs/CLUSTER.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CLUSTER.md)** — what to deploy, what
  happens when a node or Redis fails, delivery semantics, capacity, known limits.
- **[docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md)** "Cluster broker" — the
  `gateway.broker` options and the least-privilege Redis ACL.
- **[ADR 0011](https://github.com/AsyncNomad/wsPrism/blob/main/docs/adr/0011-broker-history-and-cluster.md)** — the design and every decision behind it.
