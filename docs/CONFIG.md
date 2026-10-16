# wsPrism Config (`wsprism.yaml`) v1

The gateway is configured with a single YAML file, passed with `--config`
(or `WSPRISM_CONFIG`; default `./wsprism.yaml`). Validate it without serving:

```bash
wsprism-gateway --check-config --config wsprism.yaml
```

The parser is **strict**: unknown fields and out-of-range values fail at
startup (and in `--check-config`), with the offending location.

The annotated reference is the repository's [`wsprism.yaml`](../wsprism.yaml);
every option is documented there.

## Minimal example

```yaml
version: 1
gateway:
  listen: "0.0.0.0:8080"
tenants:
  - id: "acme"
    limits:
      max_frame_bytes: 4096
```

## Reloading

There is **no hot reload yet**: configuration changes need a restart (drain
first; see `drain_grace_ms`). Hot reload of tenant policies is planned for
roadmap milestone M6.

---

## Hot Lane pacing (`tenants[].policy.hot_tick_hz`)

`0` (default): keyed Hot state is sent as soon as it arrives. `1..=240`: each
session's pending keyed state (latest value per sender/svc/key) is sent at
most once per tick, at a per-session phase within the period. Fewer flushes
when updates arrive faster than the tick, at the cost of up to one tick of
extra latency. Unkeyed frames are not paced.

---

## Authentication (`tenants[].auth`, ADR 0008)

An ordered list of authenticators. Each one either decides (admit / reject)
or passes a token it doesn't recognise to the next one.

```yaml
tenants:
  - id: acme
    auth:
      - kind: dev        # development only: `dev` / `dev:<id>`
```

Omitting `auth` means `[dev]` and logs a startup warning: anyone presenting
`dev` is admitted. Production authenticators (one-time tickets, JWT, backend
connect proxy) arrive in the next M2 steps.

---

## Server API (`gateway.api`, `tenants[].api_keys`, ADR 0008 §3)

Backends publish to rooms, disconnect users and read presence over HTTP on a
**separate listener** (off unless configured). Bind it to loopback or an
internal network.

```yaml
gateway:
  api:
    listen: "127.0.0.1:8081"
tenants:
  - id: acme
    api_keys:
      # sha256 of the key, lowercase hex: `printf %s "$KEY" | sha256sum`
      - name: backend
        sha256: "<64 hex chars>"
```

Requests send `Authorization: Bearer <key>`; the key selects the tenant.

| Endpoint | Body | Result |
|---|---|---|
| `POST /api/v1/publish` | `{"room": "r", "data": {...}}` (Ext, reliable by default) or `{"room": "r", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "..."}}` (Hot, lossy by default); optional `"qos": "reliable"\|"lossy"` | `{"recipients": n}` |
| `POST /api/v1/disconnect` | `{"user": "u", "sid": "optional", "reason": "optional"}` | `{"closed": n}` |
| `GET /api/v1/rooms/{room}/presence` | — | `{"room": "r", "users": [...]}` |
