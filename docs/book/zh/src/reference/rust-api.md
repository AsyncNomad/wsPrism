# Rust API

只把 wsPrism 当作二进制程序运行，完全不必碰 Rust。本章面向想把代码放进网关内部的场景：需要在微秒级而不是一次 HTTP 往返后作答的服务、对接公司内部 SSO 的认证器，或者把网关嵌入到更大的程序里。

自动生成的 API 文档与本文档一起发布在 [`/api/`](../../api/wsprism/index.html)。

## 依赖哪个 crate

只依赖 **`wsprism`** 即可，它会重新导出其他 crate 的稳定 API。

| Crate | 内容 | 需要直接依赖吗？ |
|---|---|---|
| `wsprism` | 门面：稳定 API 和 `prelude` | 是 |
| `wsprism-gateway` | 服务端：配置、握手、传输、认证、服务端 API、二进制程序 | 否 |
| `wsprism-engine` | 房间、投递、Hot 状态、服务 trait | 只有在其 SPI 之上自建服务端或传输层时才需要 |
| `wsprism-protocol` | 线路格式 | 只有编写使用该协议的客户端或工具时才需要 |
| `wsprism-redis` | Redis 集群 broker | 否；由 `redis` feature（默认开启）引入 |

所有 crate 共用同一个版本号，一起发布。给 `wsprism` 设置 `default-features = false` 可以不带 Redis 构建。

## 带上你自己代码的网关

```rust
use std::sync::Arc;
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    Gateway::builder()
        .config_file("wsprism.yaml")?
        .text_service(MyService::default())        // Ext 通道：svc "my"
        .hot_service(MyPhysics::new(7))             // Hot 通道：svc id 7
        .auth_plugin("corp_sso", |options| Ok(Arc::new(CorpSso::from_options(options)?)))
        .build()?                                   // 校验一切；此时尚未绑定任何端口
        .serve()                                    // 绑定监听地址，收到 SIGTERM 时排空
        .await
}
```

你的二进制程序还需要启用了 `macros` 和 `rt-multi-thread` feature 的 `tokio`。

`build()` 会把配置与你注册的内容相互核对：白名单里出现未注册的服务、配置引用了未知插件、用内置名称（`room`、`chat`、`sys`、Hot ID `1`）注册服务，这些都会在这里失败，而不是等到运行时。

## 文本服务

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

`handle` 接收会话上下文和信封。通过上下文，服务可以：

- `reply(...)` 回复调用方（网关会自动加上 `reply_to`）；
- 向房间发布（`publish_room_reliable`、`publish_room_lossy`、`publish_room_others`），受用户发布权限约束；
- 发给某个用户或会话（`send_to_user`、`send_to_session`）；
- 代表会话加入或离开房间（`join_room`、`leave_room`），受租户限额约束；
- 读取会话身份：`tenant()`、`user()`、`session_id()`、`trace_id()`、`is_in_room(room)`；
- 租户开启在线状态事件时，读取 `room_members(room)`。

返回 `Err` 时，客户端会收到对应错误码的 `sys.error`（`WsPrismError::NotAllowed` 对应 `NOT_ALLOWED`，以此类推）。

上下文以可变借用的方式传入，并且不实现 `Clone`，因为房间成员关系只在会话自己的任务里变更，热路径上的成员关系因此不需要加锁。要稍后再回复（比如在定时器或外部调用之后），先取一个 `defer_reply(...)` 句柄；要从别的任务发布消息，取一个 `publisher()`。

一个服务实例服务于所有租户。如果服务里保存了状态，请把 `ctx.tenant()` 放进键里。

仓库中有一个可直接运行的完整示例 `crates/wsprism/examples/room_counter.rs`：

```bash
cargo run -p wsprism --example room_counter -- my.yaml
```

## Hot 服务

```rust
#[async_trait]
impl HotService for MyPhysics {
    fn svc_id(&self) -> u8 { self.id }

    async fn handle_hot(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()> {
        // 校验、模拟，然后发布权威状态
        Ok(())
    }
}
```

Hot 服务会看到所有发往其 ID、并通过租户 `hot_allowlist` 的二进制帧。它运行在发送方会话的任务里，所以务必要快。

## 认证插件

`auth_plugin(name, factory)` 注册后，租户的 `auth` 中就可以使用 `kind: plugin`、`name: <name>`。工厂函数在构建网关时拿到该条目的 `options`，因此错误的选项会在 `build()` 和 `--check-config` 中报出来。

认证器返回以下结果之一：

| 结果 | 效果 |
|---|---|
| `Ok(Identity)` | 放行，带上用户 ID，以及可选的权限和过期时间 |
| `Err(AuthError::NotMine)` | 不是本认证器处理的令牌，交给下一个 |
| `Err(AuthError::Rejected(_))` | 401 |
| `Err(AuthError::Unavailable(_))` | 503 加 `Retry-After` |

## 嵌入使用

`Gateway::serve_on(listener, api_listener, shutdown)` 在你自己绑定好的监听器上提供服务，并在你传入的关闭 future 完成时停止。任意一个 `Gateway` 克隆上都可以调用 `metrics_text()`、`session_count()` 和 `drain()`。

## 稳定性

| 级别 | 范围 | 承诺 |
|---|---|---|
| 稳定 | `wsprism` 重新导出的全部内容 | 0.x 期间只在次版本（0.1 → 0.2）中做不兼容变更，并写进变更日志。1.0 之后遵循常规 semver |
| SPI | `wsprism_engine::spi`，以及文档中隐藏的上下文方法 | 用于在引擎之上构建服务端或传输层。0.x 期间不做承诺 |
| 内部 | `wsprism_gateway::__internal`（`internals` feature） | 仅供仓库自身的测试和基准测试使用，不是 API |

错误类型和配置类型都标注了 `#[non_exhaustive]`，匹配时请加上通配分支。配置结构体只能通过解析 YAML 来创建。

线路协议、服务端 API、代理请求格式和配置格式的标准比 Rust API 更严格：只增不改。

## 构建配置

Cargo 使用顶层工作区的构建配置，所以 wsPrism 自己的 release 配置不会作用到你的二进制程序上。想获得与官方构建相同的扇出性能，请加上：

```toml
[profile.release]
codegen-units = 1
```

不加的话，房间扇出的微基准会慢约 10%；端到端的差距在 0.1% 左右。

## 最低支持的 Rust 版本

Rust **1.95**。策略是：发布至少六个月的稳定版。在 0.x 期间提高最低版本不算不兼容变更，但会写进变更日志。
