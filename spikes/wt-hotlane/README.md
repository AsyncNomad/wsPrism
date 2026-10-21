# wt-hotlane spike (throwaway)

Validation step V of [ADR 0004](../../docs/adr/0004-product-direction.md) /
[design §7](../../docs/design/hot-lane-over-webtransport.md). **Not product
code** — it is deleted after V; only the measurement tooling
(`bench/netlab.sh`, `wsprism-loadgen statesync`) is kept.

One relay, three transports, identical relay logic (`hub.rs`):

| Flag | Transport | Client → relay | Relay → client |
|---|---|---|---|
| `--ws` | WebSocket (plain) | binary message per frame | binary message per batch |
| `--wss` | WebSocket over TLS (rustls, ring) | same | same |
| `--wt` | WebTransport (wtransport) | datagram per frame | datagram per batch |

## Wire format (spike only)

- Connect with `?room=<u32>&id=<u32>` (WS: `ws[s]://host:port/?...`, WT: `https://host:port/?...`).
- Frame: `[sender u32 LE][seq u32 LE][ts u64 LE][payload]` (relay only reads `sender`).
- Batch (relay → client): `[count u16 LE]` then `count` × `[len u16 LE][frame]`, at most
  `MAX_BATCH_BYTES` (1,100) per batch — the same cap for every transport.

## Relay semantics (the design's Hot slot, §4.2)

Each member has a slot holding the **latest frame per sender** (conflation).
Publishing overwrites; a writer task drains the slot and sends one or more
batches — immediately when `--tick-hz 0`, otherwise once per tick.

`--fifo` disables conflation (every frame is queued), approximating today's
gateway behaviour, as a reference point.

## TLS

A self-signed ECDSA identity valid for 13 days is generated at start
(browsers accept `serverCertificateHashes` only for ≤14-day certs). Its DER
and SHA-256 are written to `--cert-dir`.
