# Operating wsPrism

For operators: how to run the gateway binary in production. Configuration
reference: [CONFIG.md](CONFIG.md). Several nodes: [CLUSTER.md](CLUSTER.md).

## Command line

| Flag | Environment | Default | |
|---|---|---|---|
| `--config <path>` | `WSPRISM_CONFIG` | `wsprism.yaml` | Configuration file |
| `--check-config` | | | Validate the file (parse, checks, tenant policies) and exit |
| `--log-format text\|json` | `WSPRISM_LOG_FORMAT` | `text` | Log lines (below) |
| | `RUST_LOG` | errors only | Levels, e.g. `info`, `warn,wsprism_redis=info` |

Configuration values can come from the environment: `${NAME}` /
`${NAME:-default}` anywhere in a value ([CONFIG.md](CONFIG.md) "Values
from the environment"), so one file serves every environment.

## Logs

Logs go to stderr. `--log-format json` writes one JSON object per line for
log pipelines (Loki, Elasticsearch, Cloud Logging, ...):

```json
{"error":"Timeout","level":"WARN","msg":"rpc backend call failed","span":{"ip":"203.0.113.7","s":"tab-1","t":"acme","trace_id":"5f0c...","u":"alice"},"target":"wsprism_gateway::rpc","ts":"2026-10-21T09:00:00.123456Z","url":"https://api.example.com/rpc?key=***"}
```

- `ts` (UTC, RFC 3339), `level`, `target` (module), `msg`, and the
  event's own fields (keys in alphabetical order).
- `span`: the session the event happened in: `trace_id` (also in the
  errors the client receives, so a user report can be matched), `t`
  tenant, `u` user, `s` session id, `ip` client address, `c` connection id
  (what delivery warnings such as "closing slow consumer" name). Use
  `trace_id` to follow one session.
- **Not logged:** tickets, tokens, API keys, backend credentials, message
  bodies. Backend URLs are logged without user info and with query values
  replaced by `***`.
- **Personal data** (`u`, `s`, `ip`): `gateway.log_identities` decides how
  they appear.

  | Value | Logs show |
  |---|---|
  | `plain` (default) | the values as they are |
  | `hashed` | `h:` + 16 hex characters of an HMAC-SHA256: one person keeps one hash, the value cannot be read back |
  | `off` | nothing (the fields are left out) |

  With `hashed`, set `gateway.log_identity_key_env` to a variable holding
  a key of at least 16 bytes, the same on every node, to compare hashes
  across nodes and restarts (without it each process picks a random key).
  To find one user's sessions, hash their id with the same key (HMAC-SHA256,
  first 16 hex characters).

## Stopping and updating

SIGTERM (or Ctrl-C) drains: `/readyz` turns 503, new connections and
server API publishes are refused (503), sessions are closed (v2 4103
`draining`: reconnect after a backoff; v1 1001), and a cluster node hands its queued frames to Redis,
all within `gateway.drain_grace_ms` (default 2000). Then the process
exits. Give your orchestrator a longer stop timeout than that.

Several nodes: restart them one at a time ([CLUSTER.md](CLUSTER.md)
"Rolling update"); the chaos tests check that nothing is lost or repeated.

## Container image

`Dockerfile` at the repository root builds a distroless image (non-root,
no shell) with `WSPRISM_LOG_FORMAT=json`; mount the configuration at
`/etc/wsprism/wsprism.yaml`, with `gateway.listen` and `gateway.api.listen`
on `0.0.0.0` (a loopback address is unreachable from outside the
container). `deploy/compose/` runs two nodes and Redis
with one configuration file whose differences come from the environment.
The image has no shell or curl: use your orchestrator's HTTP probes on
`/healthz` and `/readyz`.

## Probes and metrics

The internal listener (`gateway.api.listen`) serves `/healthz` (process up),
`/readyz` (503 while draining, or when a cluster node lost its slot) and
`/metrics` (Prometheus). Health checks are on the public port too;
`/metrics` only with `gateway.public_metrics: true`. See
[CONFIG.md](CONFIG.md) "Server API".

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `wsprism_ws_upgrades_total` | counter | `tenant`, `status` | WebSocket upgrades accepted |
| `wsprism_ws_sessions_active` | gauge | `tenant` | Sessions open now |
| `wsprism_handshake_rejections_total` | counter | `tenant` (`_unknown` if none matched), `reason` | Connections refused before the upgrade |
| `wsprism_policy_decisions_total` | counter | `tenant`, `lane`, `decision`, `reason` | Messages and sessions a tenant policy refused or limited |
| `wsprism_dispatch_duration_seconds` | histogram | `tenant`, `lane` (`ext`/`hot`) | Time to handle one client message (buckets 100 µs to 1 s) |
| `wsprism_decode_errors_total` | counter | `tenant` | Client frames that could not be decoded |
| `wsprism_service_errors_total` | counter | `tenant`, `lane`, `svc` | Client messages a service answered with an error |
| `wsprism_writer_timeouts_total` | counter | `tenant` | Sessions closed because a socket write timed out |
| `wsprism_draining` | gauge | | 1 while draining for shutdown |
| `wsprism_egress_drop_total` | counter | | Lossy frames dropped: a recipient's queue was full |
| `wsprism_egress_slow_consumer_closes_total` | counter | | Sessions closed: a reliable frame did not fit their queue (they resume history rooms on reconnect) |
| `wsprism_hot_slot_overwritten_total` | counter | | Keyed Hot frames replaced by newer ones before being sent (normal; high = readers lag) |
| `wsprism_hot_slot_full_drops_total` | counter | | Keyed Hot frames dropped: a recipient's slot table was full |
| `wsprism_broker_outbox_refused_total` | counter | | Cluster only: frames refused because the outbox was full or stale (publishers got `UNAVAILABLE`) |
| `wsprism_broker_retries_total` | counter | | Cluster only: broker calls retried (Redis slow or away) |
| `wsprism_broker_failed_total` | counter | | Cluster only: frames or commands Redis rejected (dropped; investigate) |

Label values come from the configuration or fixed sets, never from client
input, so the number of series is bounded by your tenants.

Suggested alerts: `wsprism_ws_sessions_active` dropping sharply;
`rate(wsprism_broker_outbox_refused_total[5m]) > 0` or
`rate(wsprism_broker_failed_total[5m]) > 0`; a growing
`wsprism_egress_slow_consumer_closes_total`; `/readyz` failing.
