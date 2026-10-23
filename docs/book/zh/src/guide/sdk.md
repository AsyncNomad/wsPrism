# JavaScript SDK

`sdk/js/wsprism.js` 是官方客户端：一个没有任何依赖的 ES 模块，支持浏览器和 Node.js 22 及以上版本，TypeScript 声明在 `wsprism.d.ts` 中。它使用协议 v2，并替你处理那些手写客户端最容易出错的细节：重连策略、重新加入房间、历史续收、去重、过滤过时状态、凭证续期。

## 连接

```js
import { WsPrismClient } from "./wsprism.js";

const client = new WsPrismClient({
  url: "wss://rt.example.com/v1/ws",
  tenant: "acme",
  getTicket: () => fetch("/api/realtime-ticket").then((r) => r.text()),
});

await client.connect();
```

每次尝试连接之前、每次续期凭证之前，都会调用 `getTicket`。返回后端签发的一次性票据或一个新的 JWT 即可。凭证放在 `Sec-WebSocket-Protocol` 头里传输，从不出现在 URL 中。

如果有多个网关节点但没有负载均衡器，可以传入一个列表：`url: ["wss://a/v1/ws", "wss://b/v1/ws"]`，每次重连都会尝试下一个。

## 房间与消息

```js
const { chan, memberId, members } = await client.join("match:42");   // members：开启在线状态事件时才有
client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
client.on("message", (m) => console.log(m.svc, m.type, m.data));
await client.leave("match:42");
```

`join`、`leave`、`request`、`rpc` 返回 Promise，按 `seq` 与网关的回复对应。网关或后端返回错误时，Promise 以 `WsPrismError` 拒绝，其 `code` 是[错误码](../reference/protocol.md#错误码)之一。`send` 不等待回复，错误通过 `error` 事件送达。

## 调用后端

```js
const result = await client.rpc("game", "move", { room: "match:42", data: { x: 1 } });
```

请求经由 RPC 代理发往后端，Promise 以后端返回的 `data` 兑现。

## Hot 通道上的状态

```js
client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: new Uint8Array([1, 2]) });
client.on("state", (s) => render(s.room, s.sender, s.key, s.payload));
```

SDK 为发出的帧按键编号，并丢弃那些比同一 `(room, sender, svc, key)` 最后见到的更旧的帧，`u32` 回绕也已处理。`s.payload` 是指向接收缓冲区的视图，需要长期保留时请先复制一份。

## 重连

SDK 根据关闭码所在的区间采取行动：

| 关闭码 | SDK 的处理 |
|---|---|
| 4000~4099 | 停止。会话被顶替、被后端踢下线、被策略拒绝，原样重连只会再次失败 |
| 4001 `auth_expired` | 立即用新的 `getTicket()` 重连 |
| 4100~4199 | 带抖动的指数退避（1 秒到 30 秒）后重连（容量不足、慢消费者、集群不可用、节点排空） |
| 4200~4299 | 立即重连（空闲超时） |
| 网络故障、握手被拒、`getTicket()` 失败 | 退避后重试 |

在 `connectTimeoutMs`（默认 10 秒）内仍未准入的尝试会被放弃，并像其他失败一样重试。卡住的代理或半开的连接，不会让客户端永远停在 `connecting` 状态。

重连后会重新加入所有之前的房间，并通过 `rejoined` 事件列出它们。房间的 `chan` 和 `memberId` 可能会变。如果重新加入因暂时性原因失败（`UNAVAILABLE`、`RATE_LIMITED`、`TIMEOUT`、`INTERNAL`），SDK 会在同一条连接上以 0.5 秒到 30 秒的退避重试；如果重新加入的过程中连接又断了，下一条连接会再试。只有 `NOT_ALLOWED` 这类明确的拒绝才会放弃该房间，并通过 `error` 事件说明原因。

## 历史与空缺

带历史的房间在重新加入时，会从最后见到的 offset 开始续收：错过的消息作为普通的 `message` 事件按序到达，重复的按 offset 过滤掉。出现下面两种信号时，客户端需要从后端重新加载房间状态：

- `rejoined` 列表中某个房间的 `recovered` 为 `false`；
- 收到 `gap` 事件（`{room, epoch?}`）。带 `epoch` 表示房间的历史重新开始了，SDK 会自动重置该房间的位置。

## 在线状态

开启 `policy.presence_events` 后，`join()` 会返回当前的 `members`，`presence` 事件报告 `{type: "joined" | "left", room, user, memberId}`。`memberId` 等于该成员所发状态帧的 `sender`。

## 凭证续期

当网关报告了过期时间（JWT 和连接代理凭证的 `expires_in_s`），SDK 会在有效期 70% 到 90% 之间的某个随机时刻续期：调用 `getTicket()`，在当前连接上发送 `sys.refresh`，然后触发 `refreshed` 事件。续期失败会重试；如果一直失败，网关会在到期时以 4001 关闭连接，SDK 再用新凭证重连。传入 `refresh: false` 可以关闭自动续期。

## 事件

| 事件 | 内容 |
|---|---|
| `open` | 会话已准入：`{tenant, user, sid, expires_in_s?}` |
| `reconnecting` | 每次重新尝试之前：`{attempt, delayMs}` |
| `message` | Ext 消息：房间广播、服务端发布 |
| `state` | Hot 帧：`{room, chan, sender, svc, op, key, seq, payload}` |
| `presence` | `{type, room, user, memberId}` |
| `rejoined` | 重连后重新加入的房间 |
| `gap` | `{room, epoch?}` |
| `refreshed` | 凭证已续期 |
| `close` | `{code, reason, wasOpen, willReconnect}` |
| `error` | `send` 的错误、重新加入失败、`TICKET_FAILED`、`CONNECT_TIMEOUT`、`REFRESH_FAILED` |

`client.state` 的取值为 `idle`、`connecting`、`open`、`closed`；`client.rooms` 为每个已加入的房间保存 `{room, chan, memberId, members, epoch?, offset?, recovered?}`。

## 测试

SDK 的测试针对真实运行的网关进程：

```bash
sdk/js/run-tests.sh
npx -p typescript@5 tsc -p sdk/js    # 检查类型声明
```

## 其他语言

协议的完整定义见[线路协议](../reference/protocol.md)，任何 WebSocket 库都能实现。Unity（C#）和 Dart 的 SDK 已列入路线图。
