# wsprism

A multi-tenant realtime gateway for WebSocket clients: rooms, reliable and
lossy delivery, latest-value state sync (the "Hot lane"), per-tenant policy,
JWT / one-time-ticket / backend authentication, a server API for your
backend, and an RPC proxy so business logic can live in any language.

**This is the crate to depend on** when you extend the gateway in Rust:

```rust
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    Gateway::builder()
        .config_file("wsprism.yaml")?
        .text_service(MyService::default())   // your Ext-lane (JSON) service
        .build()?
        .serve()
        .await
}
```

To run the gateway as-is, install the binary instead: `cargo install wsprism-gateway`.

- Rust API, extension points and the compatibility policy: [docs/API.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/API.md)
- Configuration: [docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md) · reference config: [wsprism.yaml](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml)
- Wire protocol: [docs/PROTOCOL.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/PROTOCOL.md)
- JavaScript client SDK: [sdk/js](https://github.com/AsyncNomad/wsPrism/blob/main/sdk/js/README.md)

Minimum supported Rust version: 1.95. License: Apache-2.0.
