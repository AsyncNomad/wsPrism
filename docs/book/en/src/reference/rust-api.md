# Rust API

You can run wsPrism as a binary and never touch Rust. This chapter is for when you want code inside the gateway: a service that answers in microseconds rather than after an HTTP round trip, an authenticator for an in-house SSO, or the gateway embedded in a larger program.

The generated API documentation is published alongside this book under [`/api/`](../api/wsprism/index.html).

## Which crate

Depend on **`wsprism`** only. It re-exports the stable API of the others.

| Crate | Contents | Depend on it directly? |
|---|---|---|
| `wsprism` | Facade: the stable API and a `prelude` | Yes |
| `wsprism-gateway` | The server: configuration, handshake, transports, authentication, server API, the binary | No |
| `wsprism-engine` | Rooms, delivery, Hot state, service traits | Only to build a server or transport on its SPI |
| `wsprism-protocol` | Wire formats | Only for a client or tool that speaks the protocol |
| `wsprism-redis` | The Redis cluster broker | No; enabled by the `redis` feature (on by default) |

All crates share one version number and are released together. `default-features = false` on `wsprism` builds without Redis.

## A gateway with your own code

```rust
use std::sync::Arc;
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    Gateway::builder()
        .config_file("wsprism.yaml")?
        .text_service(MyService::default())        // Ext lane: svc "my"
        .hot_service(MyPhysics::new(7))             // Hot lane: svc id 7
        .auth_plugin("corp_sso", |options| Ok(Arc::new(CorpSso::from_options(options)?)))
        .build()?                                   // validates everything; binds nothing
        .serve()                                    // binds the listeners, drains on SIGTERM
        .await
}
```

Your binary also needs `tokio` with the `macros` and `rt-multi-thread` features.

`build()` checks the configuration against what you registered: an allowlist naming an unregistered service, a configuration naming an unknown plugin, or a service registered under a built-in name (`room`, `chat`, `sys`, Hot id `1`) all fail here rather than at runtime.

## Text services

```rust
use wsprism::prelude::*;
use wsprism::serde_json::json;

struct Echo;

#[async_trait]
impl TextService for Echo {
    fn svc(&self) -> &'static str { "echo" }

    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        ctx.reply(json!({"v": 1, "svc": "echo", "type": "said", "data": env.data}))
    }
}
```

`handle` receives the session's context and the envelope. Through the context a service can:

- `reply(...)` to the caller (the gateway adds `reply_to`);
- publish to rooms (`publish_room_reliable`, `publish_room_lossy`, `publish_room_others`), subject to the user's publish permission;
- send to one user or session (`send_to_user`, `send_to_session`);
- join and leave rooms on the session's behalf (`join_room`, `leave_room`), subject to the tenant's limits;
- read who the session is: `tenant()`, `user()`, `session_id()`, `trace_id()`, `is_in_room(room)`;
- read `room_members(room)` when the tenant has presence events on.

Returning `Err` sends the client a `sys.error` with the matching code (`WsPrismError::NotAllowed` becomes `NOT_ALLOWED`, and so on).

The context is borrowed mutably and is not `Clone`, because room membership changes only on the session's own task. That is what keeps membership free of locks on the hot path. To answer later, from a timer or after an outbound call, take a `defer_reply(...)` handle; to publish from another task, take a `publisher()`.

A service instance serves every tenant. Key any state you keep by `ctx.tenant()`.

The repository has a complete, runnable example in `crates/wsprism/examples/room_counter.rs`:

```bash
cargo run -p wsprism --example room_counter -- my.yaml
```

## Hot services

```rust
#[async_trait]
impl HotService for MyPhysics {
    fn svc_id(&self) -> u8 { self.id }

    async fn handle_hot(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()> {
        // validate, simulate, then publish the authoritative state
        Ok(())
    }
}
```

Hot services see each binary frame addressed to their id and allowed by the tenant's `hot_allowlist`. Keep them fast: they run on the sending session's task.

## Authentication plugins

`auth_plugin(name, factory)` makes `kind: plugin` with `name: <name>` usable in a tenant's `auth`. The factory receives the entry's `options` when the gateway is built, so bad options fail `build()` and `--check-config`.

An authenticator returns one of:

| Result | Effect |
|---|---|
| `Ok(Identity)` | Admitted, with a user id and optional permissions and expiry |
| `Err(AuthError::NotMine)` | Not this authenticator's kind of token; the next one decides |
| `Err(AuthError::Rejected(_))` | 401 |
| `Err(AuthError::Unavailable(_))` | 503 with `Retry-After` |

## Embedding

`Gateway::serve_on(listener, api_listener, shutdown)` serves on listeners you have bound yourself and stops when your shutdown future completes. Any clone of a `Gateway` offers `metrics_text()`, `session_count()` and `drain()`.

## Stability

| Level | Where | Promise |
|---|---|---|
| Stable | everything re-exported by `wsprism` | Breaking changes only in a minor release while at 0.x (0.1 → 0.2), listed in the changelog. Normal semver after 1.0 |
| SPI | `wsprism_engine::spi`, and context methods hidden from the docs | For building a server or transport on the engine. No promise in 0.x |
| Internal | `wsprism_gateway::__internal` (feature `internals`) | For the repository's own tests and benchmarks. Not an API |

Error and configuration types are `#[non_exhaustive]`: match them with a wildcard arm. Configuration structs are created only by parsing YAML.

The wire protocol, the server API, the proxy request shapes and the configuration format are held to a stricter standard than the Rust API: they only grow.

## Build profile

Cargo applies the build profile of the top-level workspace, so wsPrism's own release profile does not reach your binary. For the same fan-out performance as the official build, add:

```toml
[profile.release]
codegen-units = 1
```

Without it, the room fan-out micro-benchmark is about 10% slower; end to end the difference is around 0.1%.

## Minimum supported Rust version

Rust **1.95**. The policy is a stable release at least six months old. Raising it is not a breaking change in 0.x, but the changelog says so.
