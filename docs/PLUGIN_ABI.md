# wsPrism WASM Plugin ABI (Draft — not implemented)

> Status: design notes only. No plugin runtime exists yet; today services are
> native Rust (`TextService` / `BinaryService`). See `docs/ROADMAP.md` (extension tiers).

WASM plugins are supported **only** for the **Ext Lane** and **Ingress-only**.

Non-negotiable constraints:
- Hot Lane (Binary) is **native-only**: no WASM, no external calls.
- Plugins run in a sandbox: memory + compute limits, no network.

Ingress order: **Policy → Plugin → Service**
