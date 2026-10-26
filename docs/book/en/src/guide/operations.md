# Operations

This chapter is for whoever runs the gateway in production: how to start and stop it, what it logs, what to watch and what to alert on.

## Command line

| Flag | Environment | Default | |
|---|---|---|---|
| `--config <path>` | `WSPRISM_CONFIG` | `wsprism.yaml` | Configuration file |
| `--check-config` | | | Validate the file and exit |
| `--log-format text\|json` | `WSPRISM_LOG_FORMAT` | `text` | Log format |
| | `RUST_LOG` | errors only | Levels, e.g. `info` or `warn,wsprism_redis=info` |

Run `--check-config` in CI and before every deployment. It parses the file, applies every validation rule and builds every tenant policy, including loading JWT keys and checking that allowlisted services exist. A file that passes will start.

## One file for every environment

Any configuration value can come from the environment:

```yaml
gateway:
  listen: "0.0.0.0:${PORT:-8080}"
  broker: { kind: redis, url_env: REDIS_URL, prefix: "${CLUSTER:-wsp}" }
```

`${NAME}` fails at startup if the variable is unset or empty; `${NAME:-default}` falls back. Substitution happens after parsing, so a value can never change the structure of the file. For secrets, prefer the `*_env` settings (`secret_env`, `url_env`), which read the variable at the point of use and keep it out of error messages.

There is no hot reload. Configuration changes take effect on restart, and with several nodes a rolling restart applies them without downtime.

## Logs

Logs go to standard error. `--log-format json` writes one object per line, ready for Loki, Elasticsearch, Cloud Logging and the like:

```json
{"ts":"2026-10-21T09:00:00.123456Z","level":"WARN","target":"wsprism_gateway::rpc",
 "msg":"rpc backend call failed","error":"Timeout",
 "url":"https://api.example.com/rpc?key=***",
 "span":{"trace_id":"5f0c…","t":"acme","u":"alice","s":"tab-1","ip":"203.0.113.7"}}
```

`span` identifies the session an event belongs to: `trace_id`, tenant (`t`), user (`u`), session id (`s`), client address (`ip`), and connection id (`c`). The same `trace_id` is included in errors sent to the client, so a user's bug report can be matched with the gateway's logs.

Some things are never logged: tickets, tokens, API keys, backend credentials and message bodies. Backend URLs appear without user info and with query values replaced by `***`.

### Personal data

User ids, session ids and IP addresses are personal data in many jurisdictions. `gateway.log_identities` decides how they appear:

| Value | Logs show |
|---|---|
| `plain` (default) | the values as they are |
| `hashed` | `h:` followed by 16 hex characters of an HMAC-SHA256 |
| `off` | nothing; the fields are left out |

With `hashed`, set `gateway.log_identity_key_env` to a variable holding a key of at least 16 bytes, identical on every node. One person then has one hash everywhere and across restarts, and to find a user's sessions you hash their id with the same key. Without a key, each process picks a random one.

## Stopping and updating

SIGTERM (or Ctrl-C) starts a drain:

1. `/readyz` turns 503 and `wsprism_draining` becomes 1.
2. New connections and server API publishes are refused with 503.
3. Sessions are closed with 4103 `draining` (protocol v1: 1001), which tells clients to reconnect after a short backoff, to another node.
4. A cluster node hands its queued frames to Redis.
5. The process exits, at the latest after `gateway.drain_grace_ms` (2 s by default).

Give your orchestrator a longer stop timeout than `drain_grace_ms`. For several nodes, see [rolling updates](cluster.md#rolling-updates).

## TLS certificates

With `gateway.tls`, the gateway serves `wss://` itself. To rotate the certificate, replace the files and send `SIGHUP`: new handshakes use the new certificate, open connections are not touched, and a failed reload is logged while the previous certificate stays in use. Certificates from Let's Encrypt and similar CAs can be renewed this way without dropping anyone.

## Containers

The `Dockerfile` at the repository root builds a distroless image that runs as non-root, has no shell, and sets `WSPRISM_LOG_FORMAT=json`. Mount the configuration at `/etc/wsprism/wsprism.yaml`, and bind `gateway.listen` and `gateway.api.listen` to `0.0.0.0`; a loopback address cannot be reached from outside the container.

```bash
docker build -t wsprism-gateway .
docker run -v $PWD/my.yaml:/etc/wsprism/wsprism.yaml:ro -p 8080:8080 wsprism-gateway
```

The image has no `curl`; use your orchestrator's HTTP probes. It does include CA certificates, which the gateway needs to call `https` backends or a JWKS URL. If you build your own minimal image, add them.

## Probes

| Endpoint | Answers | Where |
|---|---|---|
| `/healthz` | 200 while the process is up | public and internal listener |
| `/readyz` | 503 while draining, or after a cluster node lost its slot | public and internal listener |
| `/metrics` | Prometheus text format | internal listener; public only with `gateway.public_metrics: true` |

## Metrics

The full list, with types and labels, is in [Metrics](../reference/metrics.md). Label values come from configuration or fixed sets, never from client input, so the number of series is bounded by the number of tenants.

The ones to watch first:

- `wsprism_ws_sessions_active` by tenant: a sudden drop means clients are being disconnected.
- `wsprism_handshake_rejections_total` by reason: `rate_limit`, `tenant_capacity`, `auth_failed`, `auth_unavailable`, `origin_not_allowed`, `cluster_busy` and the rest tell you why people cannot get in.
- `wsprism_egress_slow_consumer_closes_total`: clients that cannot keep up with reliable traffic. A rising rate points at undersized queues or at rooms that publish more than clients can read.
- `wsprism_dispatch_duration_seconds`: how long one client message takes to handle.
- In a cluster, `wsprism_broker_outbox_refused_total` and `wsprism_broker_failed_total`: publishes refused because Redis is saturated, and commands Redis rejected.

## Suggested alerts

```yaml
- alert: WsPrismSessionsDropped
  expr: sum(wsprism_ws_sessions_active) < 0.5 * sum(wsprism_ws_sessions_active offset 10m)
- alert: WsPrismBrokerRefusing
  expr: rate(wsprism_broker_outbox_refused_total[5m]) > 0
- alert: WsPrismBrokerFailures
  expr: rate(wsprism_broker_failed_total[5m]) > 0
- alert: WsPrismSlowConsumers
  expr: rate(wsprism_egress_slow_consumer_closes_total[10m]) > 1
```

Add a probe alert on `/readyz` for each node.

## Sizing

Measured on a desktop CPU with the gateway on eight cores (see [Benchmarks](../project/benchmarks.md)):

- **Memory:** about 18 to 38 KB of process memory per connection, plus kernel socket buffers; the upper end is with history kept in every room. A history room adds up to its `max_bytes`.
- **CPU:** about 3 to 8 µs per delivered message, depending on room shape, and falling as load rises because writes are coalesced. At two million deliveries per second the p99 latency stayed under 10 ms.
- **Connection storms:** about 30,000 new sessions per second per node in a cluster.

Measure your own traffic before committing to numbers. Message size, room size and the share of Hot to Ext traffic all move these figures.

## Hardening checklist

- Serve `wss://`: set `gateway.tls`, or terminate TLS on a separate load balancer. Avoid a TLS proxy on the same host, which doubled CPU per delivery.
- Replace the `dev` authenticator; a tenant without `auth` logs a warning at startup.
- Keep the server API on loopback or a private network, and give each backend its own key.
- List exact browser origins in `allowed_origins`.
- Behind a load balancer, set `trusted_proxies` so that per-client rate limits see real addresses.
- Leave `ticket_sources` at its default unless you have legacy clients.
- Set `limits` for each tenant: frame size, sessions, rooms.
- In a cluster: a dedicated Redis user with the ACL shown in [Running a Cluster](cluster.md), TLS, and `noeviction`.
- Consider `log_identities: hashed`.
