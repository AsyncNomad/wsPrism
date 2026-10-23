# 认证与权限

每条连接都要在 WebSocket 握手时出示凭证。凭证属于谁、这个用户能做什么，由租户的**认证链**来判定。认证通过之前，任何消息都到不了服务。

## 凭证放在哪里

客户端出示的东西，网关一律称为*票据*（ticket），不管它是一次性票据、JWT，还是你自己的认证器能识别的某种格式。租户按优先级列出客户端可以放置票据的位置（`policy.ticket_sources`）：

| 位置 | 客户端的发送方式 | 适用于 |
|---|---|---|
| `header` | `Authorization: Bearer <ticket>` | 原生应用、游戏引擎、服务器 |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.auth.<ticket>` | 无法给 WebSocket 设置请求头的浏览器 |
| `query` | `?ticket=<ticket>` | 仅限旧客户端 |

默认值是 `[header, subprotocol]`。特意不包含查询字符串，是因为 URL 会被代理和访问日志原样记录，而写进日志的凭证就等于泄露了的凭证。如果确实要开启，请只配合一次性票据使用，用过一次它就作废了。

网关在握手响应中不会回显 `wsprism.auth.*` 这一项。

## 认证链

`tenants[].auth` 是一个有序的认证器列表。每个认证器看到票据后，会做以下四件事之一：

- **接受**：返回用户 ID，以及可选的权限和过期时间；
- **拒绝**：客户端收到 HTTP 401；
- **暂时无法判断**，例如身份提供方连不上：客户端收到 HTTP 503 和 `Retry-After`，于是会重试，而不会误以为自己的凭证有问题；
- **不是我管的票据**：交给下一个认证器处理。

这样，同一个租户就可以同时接受来自浏览器的一次性票据和来自服务的 JWT：

```yaml
tenants:
  - id: acme
    auth:
      - kind: ticket
      - kind: jwt
        algorithm: RS256
        jwks_url: "https://idp.example.com/.well-known/jwks.json"
        issuer: "https://idp.example.com/"
```

无论哪种认证器，返回的用户 ID 都必须是 1 到 64 个 `[A-Za-z0-9_-]` 字符。用户 ID 会成为内部键名的一部分，不符合要求的一律拒绝并记录日志。

没有配置 `auth` 的租户会退回到开发用认证器，并在启动时打印警告。除了你自己的笔记本，在任何环境里看到这条警告，都应当把它当成错误处理。

## 一次性票据

如果用户本来就要登录你的后端，这是最简单的方式。

1. 用户照常登录后端。
2. 后端调用网关服务端 API 的 `POST /api/v1/tickets`，带上用户 ID，以及可选的房间权限。
3. 把拿到的票据（`wpt_…`）返回给客户端。
4. 客户端在票据有效期内（默认 30 秒，最长 300 秒）用它连接。

```bash
curl -s -X POST http://127.0.0.1:8081/api/v1/tickets \
  -H "Authorization: Bearer $API_KEY" \
  -d '{"user":"alice","permissions":{"subscribe":["match:*"],"publish":["match:42"]}}'
# {"ticket":"wpt_...","expires_in_s":30}
```

票据在第一次出示时就被消耗，即使这次尝试失败了（比如用错了租户）也一样，所以泄露出去的票据无法被反复尝试。在集群中，票据存放在 Redis 里（只存 SHA-256，从不存票据本身），因此一个节点签发的票据可以在任意节点上使用一次。Redis 不可用时，票据登录会以 503 拒绝，宁可失败也不放行。

租户的 `auth` 中必须包含 `kind: ticket`，否则签发请求会得到 400。

## JWT

如果你已经在签发 JSON Web Token，网关可以自行验证，不必回调任何服务。

```yaml
      - kind: jwt
        algorithm: HS256
        secret_file: /run/secrets/jwt       # 或 secret_env；不要直接写在配置里
        issuer: "https://auth.example.com"  # 可选；设置后 iss 为必需
        audience: "wsprism"                 # 可选；设置后 aud 为必需
        leeway_s: 30
```

- `sub` 即用户 ID。`exp` 为必需，`nbf` 也会校验。
- 每个认证器的算法是固定的。头部声明了其他算法的令牌会交给下一个认证器，绝不会换一种算法去验证。经典的算法混淆攻击由此堵上。
- HMAC 密钥长度不得短于哈希长度（32、48 或 64 字节）。
- 设置了 `kid` 后，只处理带有该密钥 ID 的令牌。配置两个 `kid` 不同的 `jwt` 条目，就能不停机地轮换静态密钥。

wsPrism 自己的声明统一放在 `wsprism` 对象下面，不会与签发方的声明冲突：

```json
{
  "sub": "alice",
  "exp": 1760000000,
  "wsprism": { "tenant": "acme", "subscribe": ["match:*"], "publish": ["match:42"] }
}
```

声明了其他租户的令牌一律拒绝。设置 `require_tenant: true` 可以要求必须带有这个声明。

### JWKS

如果使用 Auth0、Cognito、Keycloak、Okta 这类身份提供方，就不用密钥文件，而是直接指向提供方的密钥集：

```yaml
      - kind: jwt
        algorithm: RS256
        jwks_url: "https://idp.example.com/.well-known/jwks.json"
        jwks_refresh_s: 300
        jwks_min_refetch_s: 30
        jwks_max_stale_s: 86400
```

密钥在首次使用时拉取，之后在后台定期刷新。遇到 `kid` 未知的令牌（提供方刚轮换了密钥），会立即重新拉取一次，但有频率限制，防止有人用随机 `kid` 去轰炸身份提供方。如果刷新一直失败，最后一份有效密钥最多再用 `jwks_max_stale_s`；超过之后，与其信任可能已被提供方吊销的密钥，不如直接返回 503。通过 URL 获取的对称密钥一律不用。

## 连接代理

如果是否放行需要后端自己的业务逻辑来判断，就让网关去问后端：

```yaml
      - kind: proxy
        url: "http://10.0.0.5:9000/wsprism/connect"
        timeout_ms: 2000
        max_in_flight: 256
        secret_env: BACKEND_SECRET
```

每来一条连接，网关就发送：

```json
POST /wsprism/connect
{"tenant": "acme", "token": "<ticket>", "client_ip": "203.0.113.7"}
```

后端放行时返回

```json
200 {"user": "alice", "permissions": {"subscribe": ["room:*"]}, "expires_at": 1760000000}
```

（`permissions` 和 `expires_at` 可选），拒绝时返回 401 或 403。其他任何状态码、超时或后端不可达，都会变成 503 加 `Retry-After`：后端故障永远不会被当成"凭证错误"报给客户端。

连接代理会接受任何令牌，所以必须放在认证链的**最后**。令牌会被转发到后端，因此在不可信的网络上请使用 `https`。

## 自定义认证器

在 Rust 中用 `Gateway::builder().auth_plugin(...)` 以某个名字注册一个工厂函数，然后在配置里引用它：

```yaml
    auth:
      - kind: plugin
        name: corp_sso
        options: { realm: "staff" }
```

工厂函数在构建网关时拿到 `options`，因此配置错误会在启动时和 `--check-config` 中暴露出来。认证器要么返回身份，要么返回与内置认证器相同的三种拒绝之一。见 [Rust API](../reference/rust-api.md)。

## 权限

认证器可以给会话附加两份列表：

- `subscribe`：可以加入的房间；
- `publish`：可以发布消息的房间。

条目可以是精确的房间名，也可以是 `prefix*` 模式。省略某份列表表示不限制；空列表表示什么都不允许。没有附带权限的用户只受租户策略约束。

加入房间需要 `subscribe`。服务代表用户进行的每一次发布都需要 `publish`，不论是 `chat`、Hot 转发这样的内置服务，还是你自己写的服务。这项检查就放在服务上下文里，自定义服务想忘也忘不掉。

## 过期与连接内续期

凭证过期时（JWT 的 `exp`，或连接代理给出的 `expires_at`），网关会关闭会话，协议 v2 下关闭码为 4001 `auth_expired`。过期检查每秒一次，使用单调时钟。

为了避免重连，客户端可以在当前连接上直接续期凭证：

```json
{"v":1,"svc":"sys","type":"refresh","seq":7,"data":{"token":"<new token>"}}
{"v":1,"svc":"sys","type":"refreshed","reply_to":7,"data":{"expires_in_s":900}}
```

新令牌和新连接一样要走一遍认证链，并且必须属于同一个用户。新令牌的权限立即取代旧权限：新令牌不再允许的房间会被退出，客户端会为每个房间收到一条 `sys.left`。续期失败时，会话会持续到原来的过期时间再关闭。续期同一时间只能有一个在进行，且每秒最多一次。

JavaScript SDK 会自动完成这一切。它在凭证有效期的 70% 到 90% 之间随机挑一个时间点续期，以免大批客户端同时涌向你的令牌服务。

## 凭证之前：准入检查

在任何认证器看到票据之前，会先按开销从小到大进行几项检查：

1. **握手限流**，分为全局限额和按客户端地址的限额（IPv6 默认按 /64 前缀归组）。超出的请求得到 429 和 `Retry-After`。如果网关位于负载均衡器之后，请把负载均衡器写进 `gateway.trusted_proxies`，这样才能从 `X-Forwarded-For` 中取得真实客户端地址。网关只采用从右往左第一个不可信的地址，客户端无法伪造。
2. **`sid` 校验**（400）。
3. **是否正在排空**：正在关闭的节点返回 503，让客户端去连别的节点。
4. 集群模式下的**过载保护**：Redis 太慢时，新连接以 503 `cluster_busy` 拒绝。
5. **租户查找**（400）和**协议协商**（客户端给出的版本网关一个都不支持时返回 400）。
6. 浏览器的 **Origin 检查**，对照 `policy.allowed_origins`（403）。没有 `Origin` 头的请求不是浏览器发起的，不做这项检查。
7. **租户容量**（`max_sessions_total`）：503 加 `Retry-After`。

全部通过后，才会读取票据、交给认证链。把认证放在最后，意味着因其他原因被拒的连接不会白白消耗一次 JWT 验签、一次后端调用，也不会烧掉一张一次性票据。
