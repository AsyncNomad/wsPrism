# Rust API and compatibility

How to build on wsPrism in Rust, what is promised to stay stable, and what is
not. Design and rationale: [ADR 0009](adr/0009-crate-split-and-public-api.md).

## Which crate to depend on

Depend on **`wsprism`** only. It re-exports the stable API of the other crates:

| Crate | What it is | Depend on it directly? |
|---|---|---|
| `wsprism` | Facade: the stable API + `prelude` | **Yes** |
| `wsprism-gateway` | The server: config, handshake, transports, auth, server API, the `wsprism-gateway` binary | No (through `wsprism`) |
| `wsprism-engine` | Rooms, delivery, Hot state, service traits | No, unless you build your own server or transport on its `spi` |
| `wsprism-protocol` | Wire formats (envelopes, Hot frames, close codes, error codes) | Only for a client or tool that speaks the protocol without the server |

All crates are released together with the same version number.

## Running a gateway with your own code

```rust
use std::sync::Arc;
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    Gateway::builder()
        .config_file("wsprism.yaml")?
        .text_service(MyService::default())           // JSON requests: svc "my"
        .hot_service(MyPhysics::new(7))                // binary state frames: svc id 7
        .auth_plugin("corp_sso", |options| Ok(Arc::new(CorpSso::from_options(options)?)))
        .build()?                                      // validates everything; nothing bound yet
        .serve()                                       // binds gateway.listen (+ api.listen); drains on SIGTERM
        .await
}
```

A runnable example: [`crates/wsprism/examples/room_counter.rs`](../crates/wsprism/examples/room_counter.rs).
Your binary also needs `tokio` (with `macros`, `rt-multi-thread`).

- **Services** implement `TextService` (`svc()` + `handle(ctx, envelope)`) or
  `HotService` (`svc_id()` + `handle_hot(ctx, frame)`). Return `Err` to send the
  client a `sys.error`. Use the `RealtimeCtx` to reply, publish to rooms,
  join or leave rooms. From another task (a timer, a backend call), answer
  with `defer_reply` and send with `publisher()`; the context itself is not
  `Clone`, because room membership only changes on the session's own task.
- **Names:** `room`, `chat`, `sys` and Hot id `1` are built in. Registering
  one of them, or the same name twice, fails `build()`.
- **Allowlists still apply:** a tenant must list your `svc:type` in
  `policy.ext_allowlist` (or `hot_allowlist`) before its clients can reach it.
- **Authentication plugins:** `auth_plugin(name, factory)` makes
  `- kind: plugin` / `name: <name>` / `options: {...}` usable in
  `tenants[].auth`. The factory gets `options` at build time. An
  authenticator returns `Ok(Identity)`, `Err(NotMine)` (not its token format:
  the next link decides), `Err(Rejected)` (HTTP 401) or `Err(Unavailable)`
  (HTTP 503 + `Retry-After`). A config naming an unregistered plugin fails
  `build()` (and `--check-config`).
- **Embedding:** `Gateway::serve_on(listener, api_listener, shutdown)` serves
  on listeners you bound. `metrics_text()`, `session_count()` and `drain()`
  are available on any clone of the `Gateway`.

## API levels

| Level | Where | Promise |
|---|---|---|
| **Stable** | Everything re-exported by `wsprism` | Breaking changes only in a minor release while 0.x (`0.1` → `0.2`), listed in the changelog. After 1.0, normal semver |
| **SPI** (provider interface) | `wsprism_engine::spi`, and `RealtimeCtx` methods marked hidden | For building a server or transport on the engine. **No stability promise** in 0.x |
| **Internal** | `wsprism_gateway::__internal` (feature `internals`) | For this repository's tests, benchmarks and fuzz targets. Not an API |

Error types (`ClientCode`, `WsPrismError`) and config types are
`#[non_exhaustive]`: new variants and fields can arrive in any release. Match
them with a wildcard arm. Config structs are only created by parsing YAML.

## Wire contracts come first

The Rust API may change in 0.x minors. These contracts **only grow**: new
optional fields, codes and endpoints are added, and nothing existing is
removed or changes meaning. A breaking change gets a new version alongside the
old one.

- Protocol v2 (`wsprism.v2.json`): envelope fields, Hot frame layout and flags, close-code ranges ([PROTOCOL.md](PROTOCOL.md))
- Server API `/api/v1/*`, and the connect and RPC proxy request/response shapes ([CONFIG.md](CONFIG.md))
- Config file `version: 1`. Unknown fields are rejected, so new fields always come with a default.

## Minimum supported Rust version

**1.95** (`rust-version` in the workspace; a CI job checks it). Policy: a
stable release at least 6 months old. Raising it is not a breaking change in
0.x, but it is listed in the changelog. Dependency updates use Cargo's
MSRV-aware resolver (`resolver = "3"`), so they don't raise it silently.

## Not decided yet

- **Semver checks in CI** (`cargo-semver-checks`) start with the first crates.io release.
