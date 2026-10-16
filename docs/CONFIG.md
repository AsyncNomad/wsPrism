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
