# 快速上手

本章会在你的机器上跑起一个网关，接入一个客户端，再看看后端该接在哪里。大约十分钟。你需要 Rust 工具链（1.95 或更新），以及用来跑客户端的 Node.js 22 或更新版本。

## 1. 构建网关

wsPrism 还没有发布到 crates.io，先从源码构建：

```bash
git clone https://github.com/AsyncNomad/wsPrism
cd wsPrism
cargo build --release -p wsprism-gateway
```

生成的二进制在 `target/release/wsprism-gateway`。也可以用仓库根目录的 `Dockerfile` 构建容器镜像（见[运维](guide/operations.md)）。

## 2. 编写配置

把下面的内容保存为 `quickstart.yaml`：

```yaml
version: 1
gateway:
  listen: "127.0.0.1:8080"
  api:
    listen: "127.0.0.1:8081"      # 内部监听：服务端 API、/metrics、健康检查
tenants:
  - id: demo
    auth:
      - kind: dev                 # 仅供开发：接受 "dev.<用户名>"
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]      # 内置的 Hot 转发服务
```

文件虽短，有几点值得留意：

- 一切都归属于某个**租户**。客户端连接时要指明租户，认证、策略和限额都按租户分别设置。
- `dev` 认证器会放行任何出示 `dev.<名字>` 的人，只是为了让你在没有后端的情况下先跑起来。千万不要用于部署。
- 白名单就是客户端能发送的全部内容。少了 `chat:*`，下面用到的 `chat.send` 会被以 `NOT_ALLOWED` 拒绝。

运行前先检查一遍配置。解析器很严格，出错时会精确指出是哪个字段：

```bash
./target/release/wsprism-gateway --config quickstart.yaml --check-config
```

## 3. 启动

```bash
RUST_LOG=info ./target/release/wsprism-gateway --config quickstart.yaml
```

在另一个终端里：

```bash
curl http://127.0.0.1:8080/healthz     # ok
curl http://127.0.0.1:8080/readyz      # ready
curl -s http://127.0.0.1:8081/metrics | head
```

指标只在内部监听地址上提供。指标里会出现租户名，也会暴露负载情况，所以除非你明确开启，公网端口上不会有。

## 4. 接入客户端

JavaScript SDK 只有一个没有任何依赖的文件：`sdk/js/wsprism.js`。把下面的代码保存为仓库根目录下的 `hello.mjs`，然后执行 `node hello.mjs`：

```js
import { WsPrismClient } from "./sdk/js/wsprism.js";

const client = new WsPrismClient({
  url: "ws://127.0.0.1:8080/v1/ws",
  tenant: "demo",
  getTicket: async () => "dev.alice",
});

client.on("message", (m) => console.log("message:", m.svc, m.type, m.data));

await client.connect();
await client.join("lobby");
client.send("chat", "send", { room: "lobby", data: { msg: "hello" } });
```

开两个终端，换个名字各跑一次，就能看到对方的消息。`getTicket` 在每次尝试连接之前都会被调用；生产环境里它负责从后端拿一份新的凭证，所以它是一个函数，而不是一个字符串。

浏览器的用法完全一样，只多一步：页面的 Origin 必须列在租户的 `policy.allowed_origins` 里，否则握手会被 403 拒绝。Node 客户端不发送 `Origin` 头，不受这项检查约束。

## 5. 接入后端

`dev` 认证器只是临时顶替了本该由后端扮演的角色。真正部署时，换成下面任意一种：

- **一次性票据。** 后端按自己原有的方式验证用户，然后向网关申请一张短时有效的票据（`POST /api/v1/tickets`），交给客户端。
- **JWT。** 如果你已经在签发 JWT，网关可以自己验证，静态密钥和身份提供方的 JWKS 端点都支持。
- **连接代理。** 网关把令牌转发给你的某个 HTTP 端点，由它来决定。

接下来，后端就可以向房间发布消息（`POST /api/v1/publish`）、踢用户下线（`POST /api/v1/disconnect`），以及响应通过 HTTP 转发过来的客户端请求。逐项说明见[对接后端](guide/backend.md)。

## 完整示例

`examples/chat-and-state` 是一个由 Node 后端和浏览器页面组成的小应用：用一次性票据和房间权限登录、聊天、掷骰子（由后端通过 RPC 代理作答），以及借助 Hot 通道和在线状态事件实时显示他人的光标。启动方式：

```bash
examples/chat-and-state/run.sh
```

然后在两个浏览器窗口里打开 <http://localhost:3000>。

## 下一步

- [核心概念](guide/concepts.md)：正式介绍租户、会话、房间和通道。
- [配置](reference/configuration.md)：所有配置项及其默认值和取值范围。
- [集群部署](guide/cluster.md)：加一个节点，再加上 Redis。
