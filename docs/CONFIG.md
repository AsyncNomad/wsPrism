# wsPrism Config (wsprism.yaml) v1 (Draft)

wsPrism Gateway is configured via a single YAML file. The parser is **strict**:
unknown fields cause startup failure.

---

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

---

## Hot Reload (Sprint 0 contract)
Reloadable:
- `tenants[].limits.*`
- `tenants[].policy.*` (future)
- `plugins.*` enable/disable (future)

Restart required:
- `gateway.listen` (for now)
