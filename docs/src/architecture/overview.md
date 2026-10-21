# Architecture and Decisions

The design is recorded in the repository, decision by decision:

- **[Roadmap](https://github.com/AsyncNomad/wsPrism/blob/main/docs/ROADMAP.md)** — product structure (crates), stages and their completion criteria.
- **[Architecture Decision Records](https://github.com/AsyncNomad/wsPrism/blob/main/docs/adr/README.md)** — every significant decision with the alternatives that were rejected: session context, transport seam, protocol v2, the M2 integration features (auth, server API, proxies, SDK), the crate split and public API policy, presence events, the broker / room history / cluster design (ADR 0011), JWKS and in-band token refresh (ADR 0012).
- **[Wire protocol](https://github.com/AsyncNomad/wsPrism/blob/main/docs/PROTOCOL.md)** — dual lane (JSON Ext lane, binary Hot lane), close codes, error codes.
- **[Rust API and compatibility](https://github.com/AsyncNomad/wsPrism/blob/main/docs/API.md)** — API levels and what stays stable.
- **[Hardening log](https://github.com/AsyncNomad/wsPrism/blob/main/docs/HARDENING.md)** and **[benchmarks](https://github.com/AsyncNomad/wsPrism/blob/main/docs/BENCHMARKS.md)** — defects fixed and how performance is measured.

Crates: `wsprism-protocol` (wire contract) ← `wsprism-engine` (rooms, delivery,
Hot state, services, broker boundary) ← `wsprism-redis` (cluster broker) ←
`wsprism-gateway` (server) ← `wsprism` (facade).

Operating it: [docs/OPERATIONS.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/OPERATIONS.md) and, for several nodes,
[docs/CLUSTER.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CLUSTER.md).
