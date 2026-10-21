# wsprism-engine

The wsPrism realtime engine: rooms and members, reliable / lossy delivery,
the per-session latest-value Hot slot, and the service traits
(`TextService`, `HotService`) with the `RealtimeCtx` services receive. It has
no transport, HTTP, config-format or authentication dependencies.

Service authors should depend on [`wsprism`](https://crates.io/crates/wsprism),
which re-exports the stable part of this crate. The `spi` module (for
building a server or transport on the engine) is **not** covered by semver
in 0.x. See [docs/API.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/API.md).

Minimum supported Rust version: 1.95. License: Apache-2.0.
