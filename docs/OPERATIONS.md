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
{"ts":"2026-10-21T09:00:00.123456Z","level":"WARN","target":"wsprism_gateway::rpc","msg":"rpc backend call failed","url":"https://api.example.com/rpc?key=***","error":"Timeout","span":{"trace_id":"5f0c...","t":"acme","u":"alice","s":"tab-1","ip":"203.0.113.7"}}
```

- `ts` (UTC, RFC 3339), `level`, `target` (module), `msg`, then the
  event's own fields.
- `span`: the session the event happened in: `trace_id` (also in the
  errors the client receives, so a user report can be matched), `t` tenant, `u` user, `s` session
  id, `ip` client address. Use `trace_id` to follow one session.
- **Not logged:** tickets, tokens, API keys, backend credentials, message
  bodies. Backend URLs are logged without user info and with query values
  replaced by `***`. User ids and client IPs are logged (personal data
  under some regulations: keep log retention accordingly).

## Probes and metrics

The internal listener (`gateway.api.listen`) serves `/healthz` (process up),
`/readyz` (503 while draining, or when a cluster node lost its slot) and
`/metrics` (Prometheus). Health checks are on the public port too;
`/metrics` only with `gateway.public_metrics: true`. See
[CONFIG.md](CONFIG.md) "Server API".
