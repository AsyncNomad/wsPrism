# Roadmap

wsPrism aims to be a realtime gateway that a large company can run with confidence, that a small team can customise without forking, and that measurably outperforms existing gateways. This page describes where it stands and what comes next. The working plan, in more detail, is [`docs/ROADMAP.md`](https://github.com/AsyncNomad/wsPrism/blob/main/docs/ROADMAP.md).

## Where it stands

| Area | State |
|---|---|
| Admission | Pipeline of checks, origin policy, trusted proxies, credential sources, protocol v1 and v2 |
| Integration | Authentication chain (one-time tickets, JWT and JWKS, connect proxy, plugins), server API, RPC proxy, JavaScript SDK, example application |
| Extension | Services and authenticators through `Gateway::builder` |
| History and scale | Room history and recovery; Redis cluster with Sentinel and TLS (experimental) |
| Operations | JSON logs with masking, configuration from the environment, Prometheus metrics, lossless drain, container image |
| Hardening | Randomised soak and chaos tests, fencing, overload protection, a pre-release defect hunt |
| Quality | About 270 Rust tests, SDK and end-to-end tests, five fuzz targets, dependency audit in CI |

## Milestones

| | Milestone | State |
|---|---|---|
| M0 | Clean-up: session context, rooms as a service, transport-agnostic frames | Done |
| M1 | An honest foundation: admission, error codes, protocol v1, fuzzing, baseline benchmarks | Done |
| V | Validation of state sync over datagrams | Mostly done: gains confirmed, costs too high for every room → hybrid |
| T0 | Boundaries: transport seam, delivery classes, Hot slots and ticks | Done |
| M2 | Integration: authentication, server API, proxies, SDK, crate split | Done |
| M3 | Reliability: history, offsets and epochs, recovery | Done |
| M4 | Multiple nodes: Redis broker, shared sessions, presence, tickets | Done |
| M5 | Performance: allocation-free hot path, pre-encoded frames, published comparisons | Done |
| M6 | Operations (0.1.0 scope): environment values, JSON logs, image, rolling updates | Done |
| 0.2 | Hardening: soak tests, TLS, Sentinel, fencing, overload protection | Done |
| **0.1.0** | **First release** | **Next** |

## After 0.1.0

In rough order of priority:

- **Hot reload** of tenant policies, without restarting.
- **WebTransport** for the Hot lane, as an option for small interactive rooms, with WebSocket as fallback, plus **server peers**: authoritative game servers connecting to the gateway as trusted, long-lived members.
- **Room traffic over several Redis instances**, when history throughput beyond one Redis core is needed (design adopted in ADR 0013).
- **More SDKs:** Unity (C#), then Dart.
- **Operations:** OpenTelemetry, an admin API with roles, a Helm chart, guidance for UDP load balancing.
- **WebAssembly plugins** for per-tenant logic on the Ext lane, in a sandbox.
- **Toward 1.0:** semver checks in CI, contribution guidelines, more examples (multiplayer, collaborative cursors), an external security audit.

## What wsPrism will not become

- **A game server.** It sits in front of yours, in whatever language, and carries the traffic.
- **A message queue or database.** History bridges reconnects; durable data belongs in your store.
- **A compatibility layer** for Socket.IO, Pusher or other protocols.
