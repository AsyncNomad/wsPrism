# wsPrism WASM Plugin ABI (Draft)

WASM plugins are supported **only** for the **Ext Lane** and **Ingress-only**.

Non-negotiable constraints:
- Hot Lane (Binary) is **native-only**: no WASM, no external calls.
- Plugins run in a sandbox: memory + compute limits, no network.

Ingress order: **Policy → Plugin → Service**
