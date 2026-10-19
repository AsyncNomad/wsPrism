# wsprism-gateway

The wsPrism gateway server: a multi-tenant WebSocket gateway (library and the
`wsprism-gateway` binary).

```sh
cargo install wsprism-gateway
wsprism-gateway --config wsprism.yaml           # serve
wsprism-gateway --config wsprism.yaml --check-config
```

To add your own services or authenticators in Rust, depend on the
[`wsprism`](https://crates.io/crates/wsprism) facade crate (it re-exports this
crate's public API: `Gateway::builder`, `config`, `auth`).

Configuration: [docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md) · reference: [wsprism.yaml](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml) · protocol: [docs/PROTOCOL.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/PROTOCOL.md)

Minimum supported Rust version: 1.95. License: Apache-2.0.
