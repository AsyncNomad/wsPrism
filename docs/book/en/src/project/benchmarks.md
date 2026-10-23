# Benchmarks

Performance claims in this book come from measurements kept in the repository: the scenario, the tools, the raw results (`bench/results/*.jsonl`) and a write-up of each run in [`docs/BENCHMARKS.md`](https://github.com/AsyncNomad/wsPrism/blob/main/docs/BENCHMARKS.md). This page summarises them and is explicit about their limits.

## How we measure

- **Load generator:** `wsprism-loadgen` opens real WebSocket connections, joins rooms, and has some connections publish probes stamped with the send time. Receivers record the delay in an HDR histogram. Sender and receiver share a process, so no clock synchronisation is involved. The delivery ratio (probes received ÷ probes sent × room size) exposes any loss.
- **Isolation:** the gateway is pinned to eight cores and the load generator to separate cores. A network namespace (`bench/netlab.sh`) can add delay and loss.
- **Cost:** CPU time per delivered message and process memory (RSS) per connection, measured on the server.
- **Repetition:** three runs per configuration, reported individually rather than averaged, so the spread is visible.
- **Competitors:** Centrifugo 6.9.7 and Sockudo with their default settings plus only what the scenario needs (anonymous access, history where compared). Their configuration files are in `bench/competitors/`.

**Limits.** Everything here was measured on one machine (Intel i7-13700K, WSL2), with the load generator sharing it. Absolute numbers on dedicated servers will differ. Competitors were not tuned. Memory is process RSS, excluding kernel socket buffers, which cost about the same for every server. Treat the ratios as indicative, not as a verdict.

## Single node against other gateways

64-byte messages, each connection a different user, eight server cores. "Rooms" scenarios have rooms of a few dozen members; "broadcast" has a few large rooms. Throughput is deliveries to other members per second.

| Scenario | Server | p50 / p99 (ms) | CPU µs per delivery | KB per connection |
|---|---|---|---|---|
| rooms, 180k/s | wsPrism | 0.32 / 0.7 | 5.02 | 19.7 |
| | Centrifugo | 0.39 / 0.8–1.5 | 7.27 | 78.6 |
| | Sockudo | 0.41 / 0.8–0.9 | 8.64 | 255.8 |
| rooms, 1.08M/s | wsPrism | 0.87 / 2.0–3.3 | 4.30 | 21.8 |
| | Centrifugo | 1.31 / 7.6–9.7 | 5.68 | 83.7 |
| | Sockudo | saturated (delivery 0.87–1.0, p99 up to 7.4 s) | 8.96 | 333.2 |
| rooms, 2.16M/s | wsPrism | 2.10 / 6.6–8.5 | 3.49 | 28.1 |
| | Centrifugo | 11.41 / 126–148 | 3.23 | 89.5 |
| broadcast, 500k/s | wsPrism | 1.76 / 4.5–7.0 | 7.47 | 18.4 |
| | Centrifugo | 2.60 / 8.2–10.8 | 9.82 | 31.5 |
| | Sockudo | 2.76 / 8.3–9.4 | 9.46 | 198.0 |

What this shows:

- **Memory** is the largest difference: a quarter to a third of Centrifugo's per connection in room workloads, about 60% in broadcast, and a tenth or less of Sockudo's.
- **CPU:** at low and medium load, Centrifugo spends 1.3 to 1.45 times as much per delivery and Sockudo 1.3 to 1.9 times. At 2.16 million deliveries per second Centrifugo uses about 7% less CPU, but its p99 is about eighteen times higher. It appears to batch more aggressively at the cost of tail latency.
- **Tail latency** stays in single-digit milliseconds up to the highest load tested.

### With history in every room

Both servers keeping 200 messages per room for ten minutes:

| Scenario | Server | Delivery | p99 (ms) | CPU µs | KB/conn |
|---|---|---|---|---|---|
| rooms, 180k/s | wsPrism | 1.000 | 0.7–1.0 | 5.07–6.93 | 29.9 |
| | Centrifugo | 1.000 | 0.8–1.2 | 7.76–10.01 | 96.5 |
| rooms, 1.08M/s | wsPrism | 1.000 | 2.3–4.5 | 4.34–4.72 | 32.2 |
| | Centrifugo | 1.000 | 8.7–12.2 | 5.31–6.15 | 104.6 |
| rooms, 2.16M/s | wsPrism | 1.000 | 6.4–9.4 | 3.21–3.36 | 37.7 |
| | Centrifugo | 0.937–1.000 | 1,846–3,934 | 3.32–3.80 | 97.4 |

History costs wsPrism 1–3% more CPU and 4–10 KB more per connection. At the highest load, Centrifugo with history saturates (p50 of up to a second), while wsPrism's latency is the same as without history.

## Cluster against Centrifugo's Redis engine

Both products on the same single-core Redis, one or two nodes on the same eight cores.

**With history** (Redis is the ceiling):

| Publishes/s | Server | Delivery | p50 | p99 |
|---|---|---|---|---|
| 120k | wsPrism, 1 node | 1.000 | 8.1–8.9 ms | 15–24 ms |
| | wsPrism, 2 nodes | 1.000 | 13–17 ms | 25–64 ms |
| | Centrifugo, 1 node | 0.79–0.83 | 2.7–3.0 s | 5.6–6.3 s |
| 240k | wsPrism, 1 node | 0.58–0.60 | 118–123 ms | 178–194 ms |
| | Centrifugo, 1 node | 0.34–0.35 | 6.4–7.0 s | 14.7–14.9 s |

On the same Redis, wsPrism sustains about 1.5 to 1.7 times Centrifugo's history throughput (about 140,000 against 85,000 messages a second at saturation). Adding nodes does not raise history throughput for either product: both are bound by one Redis core. Centrifugo supports Redis Cluster and sharding and can go beyond that ceiling; wsPrism cannot yet.

**Without history**, within capacity, both deliver everything. wsPrism uses 3.5 to 4.5 times less memory per connection and 1.25 to 1.3 times less CPU in small-room and broadcast scenarios, about the same in the 1.08M/s scenario, with equal or lower p99.

Beyond a node's capacity the two behave differently by design. Centrifugo queues and delivers late (94–97% delivered, p50 around a second). wsPrism refuses publishes that waited more than a second (89–90% delivered, p50 12–80 ms) and tells the publisher, who can retry. Which is preferable depends on the application.

## Connection storms

Each worker connects, authenticates, joins rooms, and disconnects, in a loop; every session is a different user. One node in a cluster:

| Case | Sessions/s | Join p50 / p99 |
|---|---|---|
| New rooms, 1 join per session | 28,600–33,900 | 1–5 ms / 3–10 ms |
| New rooms, 5 joins per session | 11,700–12,300 | 1 ms / 2 ms |
| Already-subscribed rooms | 37,500–37,900 | 2 ms / 7 ms |

Batching subscriptions raised the first case from about 3,700 to about 30,000 sessions a second. The node's CPU is now the limit. One Redis core handles about 60,000 such sessions a second, which is the worst case, since real connections stay.

## Tenant isolation

One tenant flooding at up to 1.8 million deliveries a second, while another runs a light workload:

- **No starvation.** The light tenant received every message in every run.
- **No full isolation.** Its latency follows the total load. On a single node, p99 rose from 0.5 ms to 2.5–3.1 ms. The per-tenant outbox (0.2) keeps one tenant's batch from queuing in front of another's on the way to Redis, but CPU is shared.

Per-tenant rate limits are the tool for capping a tenant's share today.

## State sync over datagrams

An experiment compared WebTransport datagrams with WebSocket over TLS for 60 Hz state in ten-member rooms, on an emulated link with 20 ms one-way delay:

| Loss | Transport | Excess staleness p99 | Visible stutter | CPU / memory vs WSS |
|---|---|---|---|---|
| 1% uniform | WSS | 49.7 ms | 2.3% of frames | — |
| | WebTransport | 15.8 ms | 0.001% | ~2× / ~2.3× |
| 1% bursty | WSS | 49.7 ms | 2.6% | — |
| | WebTransport | 15.8 ms | 0% | ~2× / ~2.3× |

Datagrams remove stutter almost entirely and cut excess staleness three- to fourfold, but cost about twice the CPU and memory per connection with library defaults. The conclusion was a hybrid: datagrams for small interactive rooms, WebSocket for large broadcast rooms, chosen per room. WebTransport is not in 0.1.0.

## Reproducing

```bash
cargo bench -p wsprism-protocol --bench protocol      # micro-benchmarks
cargo bench -p wsprism-gateway --bench hotpath
bench/run-baseline.sh <label>                         # standard end-to-end scenarios
bench/run-compete.sh                                  # against other gateways
bench/run-cluster.sh                                  # cluster
bench/run-churn.sh                                    # connection storms
bench/run-fairness.sh                                 # tenant isolation
bench/run-v.sh v-judgment 3                           # datagrams against WebSocket
```

Scripts expect the competitors' binaries locally and run all traffic inside network namespaces on one machine.
