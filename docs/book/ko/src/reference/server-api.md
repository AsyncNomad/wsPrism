# 서버 API

서버 API는 백엔드가 게이트웨이를 움직이는 통로입니다. 내부 리스너(`gateway.api.listen`)에서 열리며, 인터넷에서 닿을 수 있어서는 안 됩니다.

## 인증

`/api/v1/*` 요청에는 모두 테넌트 API 키를 싣습니다.

```
Authorization: Bearer <key>
```

키가 곧 테넌트를 정합니다. 키는 테넌트마다 SHA-256 해시로 설정합니다.

```yaml
tenants:
  - id: acme
    api_keys:
      - name: backend
        sha256: "<printf %s \"$KEY\" | sha256sum>"
```

모르는 키는 401을 받습니다. 같은 리스너에서 `/metrics`, `/healthz`, `/readyz`는 키 없이 열립니다.

## `POST /api/v1/tickets`

사용자에게 1회용 티켓을 발급합니다. 테넌트의 `auth`에 `kind: ticket`이 있어야 합니다.

```json
{
  "user": "alice",
  "ttl_s": 30,
  "permissions": { "subscribe": ["match:*"], "publish": ["match:42"] }
}
```

| 필드 | 기본값 | 의미 |
|---|---|---|
| `user` | 필수 | 사용자 ID, `[A-Za-z0-9_-]` 1~64자 |
| `ttl_s` | 30 | 수명, 1~300초 |
| `permissions.subscribe` | 무제한 | 들어갈 수 있는 룸. `[]`이면 없음 |
| `permissions.publish` | 무제한 | 발행할 수 있는 룸. `[]`이면 없음 |

응답: `200 {"ticket": "wpt_…", "expires_in_s": 30}`

오류: 테넌트가 티켓을 받지 않거나 본문이 잘못되면 400. 미사용 티켓이 너무 많으면(노드당 100만 개) 503, 클러스터의 티켓 저장소에 닿지 않으면 `Retry-After`와 함께 503.

## `POST /api/v1/publish`

룸에 발행합니다.

Ext(JSON) 메시지. 기본은 신뢰 전달입니다.

```json
{ "room": "match:42", "data": { "event": "goal" } }
```

Hot(바이너리) 프레임. 기본은 손실 허용입니다.

```json
{ "room": "match:42", "hot": { "svc": 1, "op": 2, "key": 0, "data_b64": "AAEC" } }
```

`"qos": "reliable" | "lossy"`로 기본값을 바꿀 수 있습니다.

응답: `200 {"recipients": n}`. `n`은 요청을 받은 노드에 있는 멤버 수입니다. 클러스터에서는 다른 노드의 멤버도 메시지를 받지만 이 수에 들어가지 않습니다.

오류: 룸이나 본문이 잘못되면 400. 노드가 드레인 중이거나 클러스터의 Redis 큐가 가득 차면 `Retry-After`와 함께 503이며, 다른 노드로 다시 보내면 됩니다.

200은 전달 대기열에 들어갔다는 뜻이지, 전달을 마쳤다는 뜻이 아닙니다.

## `POST /api/v1/disconnect`

사용자의 세션을 닫습니다.

```json
{ "user": "alice", "sid": "tab-1", "reason": "account suspended" }
```

`sid`와 `reason`은 선택입니다. `sid`를 빼면 그 사용자의 세션이 모두 닫힙니다. `reason`은 WebSocket 종료 프레임에 실리므로 최대 123바이트입니다. 클라이언트는 `sys.kicked`와 종료 코드 4007을 받습니다.

응답: `200 {"closed": n}`. 클러스터의 세션 저장소에 닿지 않으면 503.

## `GET /api/v1/rooms/{room}/presence`

```json
{ "room": "match:42", "users": ["alice", "bob"] }
```

세션이 여럿인 사용자도 한 번만 나옵니다. 클러스터에서는 모든 노드를 합친 목록입니다. 클러스터의 프레즌스 저장소에 닿지 않으면 503.

## 운영용 엔드포인트

| 엔드포인트 | 의미 |
|---|---|
| `GET /healthz` | 프로세스가 돌고 있으면 200 |
| `GET /readyz` | 서비스 중이면 200. 드레인 중이거나 클러스터 노드가 슬롯을 잃으면 503 |
| `GET /metrics` | Prometheus 메트릭 |

## 게이트웨이가 백엔드로 보내는 요청

참고로, 게이트웨이가 백엔드에 보내는 요청 두 가지의 형태입니다. `secret_env`를 지정했다면 둘 다 `Authorization: Bearer <secret>`을 싣습니다.

**connect 프록시**(`auth: kind: proxy`):

```json
POST <url>
{ "tenant": "acme", "token": "<ticket>", "client_ip": "203.0.113.7" }
```

받아들이려면 `200 {"user": "...", "permissions": {...}?, "expires_at": <유닉스 초>?}`, 거절하려면 401이나 403으로 답합니다. 그 밖의 응답은 장애로 보고 클라이언트에게 503을 줍니다.

**RPC 프록시**(`rpc`):

```json
POST <url>
{ "tenant": "acme", "user": "alice", "sid": "tab-1", "svc": "game", "type": "move",
  "room": "match:42", "joined": true, "data": { } }
```

`200 {"data": ...}`나 `200 {"error": {"code": "...", "msg": "..."}}`로 답합니다.

이 형태들도 안정 계약에 속하며 늘어나기만 합니다.
