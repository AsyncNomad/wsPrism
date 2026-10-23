# 와이어 프로토콜

클라이언트와 게이트웨이 사이의 계약입니다. JavaScript SDK를 쓴다면 들여다볼 일이 거의 없고, 다른 플랫폼용 클라이언트를 만든다면 이 장이 필요한 전부입니다.

## 버전

WebSocket 서브프로토콜로 두 버전 가운데 하나를 협상합니다.

- **v2**(`wsprism.v2.json`): 현재 버전이자 SDK의 기본값입니다. Hot 프레임이 대상 룸을 명시하고, 종료 코드가 재접속 방법을 알려 줍니다.
- **v1**(`wsprism.v1`): 예전 클라이언트를 위해 남겨 둔 버전입니다. Hot 프레임은 세션의 *활성 룸*, 즉 마지막으로 들어간 룸으로 갑니다.

서버는 v2, v1 순서로 자기가 지원하는 첫 버전을 골라 그것만 되돌려 줍니다. 서브프로토콜을 보냈는데 지원하는 버전이 하나도 없으면 400으로 거절하고, 아무것도 보내지 않으면 v1로 봅니다.

### 호환성

0.1.0부터 프로토콜은 늘어나기만 합니다. 새 필드, 메시지 종류, 코드가 생길 수는 있어도 기존 것의 이름, 타입, 의미는 바뀌지 않습니다. **클라이언트는 모르는 필드를 무시해야 합니다.**

## 접속

```
ws(s)://<host>/v1/ws?tenant=<tenant>[&sid=<세션 ID>]
```

`sid`는 선택이며 `[A-Za-z0-9_-]` 1~64자입니다. 같은 `sid`로 새로 접속하면 이전 연결을 대체하고, 이전 연결은 `replaced`로 닫힙니다.

자격 증명은 테넌트의 `ticket_sources`가 허락하는 곳에 싣습니다.

| 위치 | 요청 |
|---|---|
| `header` | `Authorization: Bearer <ticket>` |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.auth.<ticket>` |
| `query` | `?ticket=<ticket>`(기본적으로 꺼짐) |

서브프로토콜 토큰은 HTTP 토큰 문자로만 이뤄져야 합니다. 패딩 없는 base64url과 JWT는 조건을 만족합니다.

성공하면 첫 메시지가 이렇게 옵니다.

```json
{"v":1,"svc":"sys","type":"authed","data":{"tenant":"acme","user":"alice","sid":"tab-1"},"trace_id":"5f0c…"}
```

만료가 있는 자격 증명이면 `data`에 `expires_in_s`도 실립니다.

### 업그레이드 전 거절

| HTTP | 원인 |
|---|---|
| 400 | 없는 테넌트, 잘못된 `sid`, 지원하는 프로토콜 버전 없음 |
| 401 | 자격 증명이 없거나 거절됨 |
| 403 | 브라우저 `Origin`이 `allowed_origins`에 없음 |
| 429 + `Retry-After` | 핸드셰이크 속도 제한 |
| 503 | 노드가 드레인 중. 다른 노드로 접속(`Retry-After` 없음) |
| 503 + `Retry-After` | 테넌트 수용량 초과, 인증 백엔드 장애, 클러스터 과부하 |

## Ext 레인: JSON 봉투

```json
{
  "v": 1,
  "svc": "chat",
  "type": "send",
  "seq": 123,
  "room": "party:1",
  "data": { }
}
```

| 필드 | 의미 |
|---|---|
| `v` | 봉투 형식 버전. 언제나 1이며 프로토콜 버전이 아닙니다 |
| `svc`, `type` | 서비스와 메시지 종류. 허용 목록은 이 둘을 함께 봅니다 |
| `seq` | 선택. 요청 번호이며, 이 요청에 대한 모든 응답에 `"reply_to": <seq>`가 붙습니다 |
| `room` | 메시지가 관련된 룸 |
| `data` | 페이로드. 서비스에 그대로 넘깁니다 |
| `flags` | 선택. 비트 필드: `0x01` seq 있음, `0x02` room 있음, `0x04` ack 요청 |

요청을 처리하면서 나온 응답(`sys.joined`, `sys.reply`, `sys.error`, 정책에 따른 종료 알림까지)에는 `reply_to`가 붙습니다. 룸 브로드캐스트, 프레즌스 이벤트, 강제 종료처럼 클라이언트가 요청하지 않은 메시지에는 붙지 않습니다.

### 필드 규칙

- 클라이언트가 직접 한 요청의 결과는 최상위에 둡니다(`room`, `chan`, `member_id`, `members`, `epoch`, `offset`, `recovered`). 페이로드나 다른 사람에 관한 이벤트는 `data` 안에 둡니다.
- `epoch`, `trace_id`, `chan`, `member_id`는 불투명한 값입니다. 비교하거나 되돌려 보내기만 하고 해석하지 마세요.
- `offset`은 JavaScript 숫자에 담기는 정수입니다.

### 내장 메시지

| 클라이언트가 보내는 것 | 게이트웨이 응답 |
|---|---|
| `room.join {room, data?: {recover: {epoch, offset}}}` | `sys.joined {room, chan, member_id, members?, epoch?, offset?, recovered?}` |
| `room.leave {room?}` | `sys.left {room}`. `room`이 없으면 활성 룸에서 나감 |
| `chat.send {room, data}` | 룸에 브로드캐스트. 보낸 사람이 그 룸에 들어가 있어야 함 |
| `sys.refresh {data: {token}}` | `sys.refreshed {data: {expires_in_s}}` |
| 백엔드로 라우팅되는 요청 | `sys.reply {data}` |

| 게이트웨이가 먼저 보내는 것 | 의미 |
|---|---|
| `server.publish {room, data}` | 백엔드가 발행한 메시지 |
| `sys.member_joined` / `sys.member_left {room, data: {user, member_id}}` | 프레즌스 이벤트(켜져 있을 때) |
| `sys.gap {room, epoch?}` | 룸 메시지가 빠졌을 수 있음. `epoch`가 있으면 이력이 새로 시작됨 |
| `sys.kicked {data: {reason}}` | 서버가 곧 세션을 닫음 |
| `sys.error {data: {code, msg}}` | 오류. 요청에 대한 답이면 `reply_to`가 붙음 |

이력이 있는 룸의 메시지에는 모두 `offset`이 붙고, `room`이 없던 메시지에는 `room`이 더해집니다.

## Hot 레인: 바이너리 프레임

정수는 모두 리틀 엔디언입니다.

**v2, 클라이언트 → 서버:**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[payload…]
```

**v2, 서버 → 클라이언트:**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[sender u32][payload…]
```

| 플래그 | 필드 |
|---|---|
| `0x01` | `key` 있음 |
| `0x02` | `seq` 있음 |
| `0x04` | `sender` 있음(서버만. 클라이언트가 켜거나 모르는 비트를 켜면 디코딩 오류) |

- `chan`은 `sys.joined`에서 받은 룸 ID로, 한 노드에서는 룸의 모든 멤버에게 같습니다.
- `sender`는 발행자의 `member_id`입니다. 서버 API로 발행한 프레임은 송신자 `0`입니다.
- `key`가 있는 프레임은 수신자마다 병합됩니다. 밀린 수신자는 `(sender, svc, key)`마다 가장 새 프레임을 받습니다.
- 받는 쪽은 같은 송신자·키에서 마지막으로 본 것보다 오래된 `seq`의 프레임을 버려야 합니다.

**v1:**

```
[v=1][svc u8][op u8][flags u8][seq u32]?[payload…]      flags: 0x01 seq, 0x02 ack 요청
```

v1 프레임은 활성 룸으로 가고, v1 수신자는 페이로드만 받습니다.

## 오류 코드

`sys.error`의 `data.code`는 아래의 정해진 집합 가운데 하나이고, `data.msg`는 사람이 읽는 설명입니다. 분기는 `msg`가 아니라 반드시 `code`로 하세요. 내부 오류는 원인을 드러내지 않습니다.

| 코드 | 의미 |
|---|---|
| `BAD_REQUEST` | 형식이 틀렸거나 올바르지 않은 요청 |
| `AUTH_FAILED` | 인증 실패 |
| `RATE_LIMITED` | 속도 제한 |
| `PAYLOAD_TOO_LARGE` | 페이로드가 너무 큼 |
| `NOT_ALLOWED` | 허락되지 않음: 허용 목록 밖, 사용자 권한 밖, 들어가지 않은 룸 |
| `RESOURCE_EXHAUSTED` | 테넌트·룸·사용자 한도 도달 |
| `UNSUPPORTED_VERSION` | 지원하지 않는 프로토콜 버전 |
| `TOO_MANY_SESSIONS` | 사용자당 세션 한도 도달 |
| `SLOW_CONSUMER` | 클라이언트가 따라오지 못함 |
| `TIMEOUT` | 유휴 시간 안에 아무 활동이 없음 |
| `UNAVAILABLE` | 의존하는 대상이 응답하지 않거나 포화됨(백엔드, 클러스터의 Redis). 나중에 다시 시도 |
| `INTERNAL` | 내부 오류 |

RPC 프록시를 거쳐 백엔드가 답한 요청에는 백엔드 고유의 코드(`[A-Z0-9_]{1,32}`)가 올 수도 있습니다.

## 종료 코드

게이트웨이는 설명할 `sys.*` 메시지가 있으면 먼저 보내고 연결을 닫습니다. v2에서는 코드의 범위가 클라이언트가 다음에 할 일을 알려 줍니다.

| 범위 | 클라이언트가 할 일 |
|---|---|
| 4000~4099 | 그대로 재접속하지 않습니다. 자격 증명이나 설정이 먼저 바뀌어야 합니다 |
| 4100~4199 | 1초 이상, 지터를 섞은 지수 백오프 뒤 재접속합니다 |
| 4200~4299 | 곧바로 재접속합니다 |

| v2 | v1 | 사유 | 먼저 오는 메시지 | 언제 |
|---|---|---|---|---|
| 4001 | 1008 | `auth_expired` | `sys.kicked` | 갱신 없이 자격 증명이 만료됨. 새 자격 증명을 받아 재접속 |
| 4003 | 1008 | `policy` | `sys.error` | 정책에 따라 닫음 |
| 4004 | 1008 | `too_many_sessions` | `sys.error TOO_MANY_SESSIONS` | `on_exceed: deny`인 사용자당 한도 |
| 4005 | 1008 | `replaced` | `sys.kicked` | 다른 연결이 이 `sid`를 씀 |
| 4006 | 1008 | `max_sessions_exceeded` | `sys.kicked` | 새 세션에 밀려남(`kick_oldest`) |
| 4007 | 1008 | `disconnected` | `sys.kicked {reason}` | 백엔드가 사용자를 내보냄 |
| 4009 | 1009 | `message too big` | | 프레임이 `max_frame_bytes`를 넘음 |
| 4010 | 1002 | `decode_error` | `sys.error BAD_REQUEST` | 봉투나 프레임 형식이 틀림 |
| 4100 | 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | 입장 중 테넌트 수용량 도달 |
| 4101 | 1008 | `slow_consumer` | `sys.error SLOW_CONSUMER` | 신뢰 메시지가 송신 큐에 들어가지 못함 |
| 4102 | 1013 | `unavailable` | `sys.error UNAVAILABLE` | 클러스터의 세션 저장소에 닿지 않음 |
| 4103 | 1001 | `draining` | | 노드가 종료 중 |
| 4200 | 1000 | `idle_timeout` | `sys.error TIMEOUT` | 들어오는 활동이 없음 |

표준 코드 1009는 4009와 똑같이 다루세요.

## 핑과 유휴 시간

게이트웨이는 `ping_interval_ms`마다 핑을 보냅니다. 퐁을 포함해 들어오는 프레임은 모두 활동으로 치며, `idle_timeout_ms` 동안 조용한 연결은 4200으로 닫습니다.
