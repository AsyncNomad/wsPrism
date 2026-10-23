# Metrics

`GET /metrics` on the internal listener returns Prometheus text format. Label values come from configuration or from fixed sets, never from client input, so the number of series is bounded by the number of tenants.

## Connections

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `wsprism_ws_upgrades_total` | counter | `tenant`, `status` | WebSocket upgrades accepted |
| `wsprism_ws_sessions_active` | gauge | `tenant` | Sessions open now |
| `wsprism_handshake_rejections_total` | counter | `tenant`, `reason` | Connections refused before the upgrade |
| `wsprism_draining` | gauge | | 1 while the node drains for shutdown |

`tenant` is `_unknown` when the request named no configured tenant. `reason` is one of:

| Reason | HTTP | Cause |
|---|---|---|
| `rate_limit` | 429 | Handshake rate limit |
| `bad_sid` | 400 | Invalid `sid` |
| `draining` | 503 | The node is shutting down |
| `cluster_busy` | 503 | Redis too slow, or too many session claims pending |
| `unknown_tenant` | 400 | No such tenant |
| `unsupported_protocol` | 400 | No supported subprotocol offered |
| `origin_not_allowed` | 403 | Browser origin not allowed |
| `tenant_capacity` | 503 | `max_sessions_total` reached |
| `auth_missing` | 401 | No credential where the tenant looks for one |
| `auth_failed` | 401 | Credential rejected |
| `auth_unavailable` | 503 | Authentication backend or JWKS unavailable |

## Messages

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `wsprism_policy_decisions_total` | counter | `tenant`, `lane`, `decision`, `reason` | Messages and sessions a tenant policy refused or limited |
| `wsprism_dispatch_duration_seconds` | histogram | `tenant`, `lane` | Time to handle one client message (buckets 100 µs to 1 s; Hot lane sampled 1 in 1024) |
| `wsprism_decode_errors_total` | counter | `tenant` | Client frames that could not be decoded |
| `wsprism_service_errors_total` | counter | `tenant`, `lane`, `svc` | Client messages a service answered with an error |

## Delivery

| Metric | Type | Meaning |
|---|---|---|
| `wsprism_egress_drop_total` | counter | Lossy frames dropped because a recipient's queue was full |
| `wsprism_egress_slow_consumer_closes_total` | counter | Sessions closed because a reliable frame did not fit their queue |
| `wsprism_writer_timeouts_total` (`tenant`) | counter | Sessions closed because a socket write stalled past `writer_send_timeout_ms` |
| `wsprism_hot_slot_overwritten_total` | counter | Keyed Hot frames replaced by newer ones before being sent. Normal; high values mean readers lag |
| `wsprism_hot_slot_full_drops_total` | counter | Keyed Hot frames dropped because a recipient's slot table was full |

## Cluster

| Metric | Type | Meaning |
|---|---|---|
| `wsprism_broker_outbox_refused_total` | counter | Frames refused because the queue to Redis was full or stale; publishers received `UNAVAILABLE` |
| `wsprism_broker_retries_total` | counter | Broker calls retried because Redis was slow or away |
| `wsprism_broker_failed_total` | counter | Frames or commands Redis rejected. These are dropped; investigate |

## Reading them

- A rising `wsprism_broker_retries_total` with zero refusals is Redis hiccuping and the gateway absorbing it. Refusals mean the queue gave up and publishers were told.
- `wsprism_broker_failed_total` should stay at zero. The usual causes are a missing ACL permission or a Redis that runs out of memory under `noeviction`.
- `wsprism_egress_drop_total` grows in normal operation on busy Hot rooms with slow readers. Unexpected growth on Ext-only tenants points at server API publishes sent as `lossy`.
- Compare `wsprism_handshake_rejections_total{reason="auth_failed"}` with `auth_unavailable`. The first is users, the second is your infrastructure.
