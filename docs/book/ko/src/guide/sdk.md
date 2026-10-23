# JavaScript SDK

`sdk/js/wsprism.js`는 공식 클라이언트입니다. 의존성 없는 ES 모듈 하나로 브라우저와 Node.js 22 이상에서 돌고, TypeScript 선언은 `wsprism.d.ts`에 있습니다. 프로토콜 v2를 쓰며, 직접 만든 클라이언트가 흔히 미묘하게 틀리는 부분을 대신 처리합니다. 재접속 정책, 룸 재입장, 이력 이어 받기, 중복 제거, 낡은 상태 걸러 내기, 자격 증명 갱신이 그것입니다.

## 접속

```js
import { WsPrismClient } from "./wsprism.js";

const client = new WsPrismClient({
  url: "wss://rt.example.com/v1/ws",
  tenant: "acme",
  getTicket: () => fetch("/api/realtime-ticket").then((r) => r.text()),
});

await client.connect();
```

`getTicket`은 접속을 시도할 때마다, 그리고 자격 증명을 갱신할 때마다 불립니다. 백엔드에서 받은 1회용 티켓이나 새 JWT를 돌려주면 됩니다. 자격 증명은 URL이 아니라 `Sec-WebSocket-Protocol` 헤더에 실려 갑니다.

로드밸런서 없이 게이트웨이 노드 여럿을 쓴다면 목록을 넘기세요. `url: ["wss://a/v1/ws", "wss://b/v1/ws"]`처럼 주면 재접속할 때마다 다음 노드를 시도합니다.

## 룸과 메시지

```js
const { chan, memberId, members } = await client.join("match:42");   // members: 프레즌스 이벤트가 켜졌을 때
client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
client.on("message", (m) => console.log(m.svc, m.type, m.data));
await client.leave("match:42");
```

`join`, `leave`, `request`, `rpc`는 게이트웨이의 응답을 `seq`로 짝지어 돌려주는 Promise입니다. 게이트웨이나 백엔드가 오류를 내면 `WsPrismError`로 거부되며, `code`는 [오류 코드](../reference/protocol.md#오류-코드) 가운데 하나입니다. `send`는 기다리지 않고, 오류는 `error` 이벤트로 옵니다.

## 백엔드 호출

```js
const result = await client.rpc("game", "move", { room: "match:42", data: { x: 1 } });
```

RPC 프록시를 거쳐 백엔드로 가고, 백엔드가 돌려준 `data`로 풀립니다.

## Hot 레인 상태

```js
client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: new Uint8Array([1, 2]) });
client.on("state", (s) => render(s.room, s.sender, s.key, s.payload));
```

SDK는 보내는 프레임에 키마다 번호를 매기고, 받은 프레임 가운데 같은 `(room, sender, svc, key)`에서 마지막으로 본 것보다 오래된 것은 버립니다. `u32` 넘침도 처리합니다. `s.payload`는 받은 버퍼를 가리키는 뷰이므로, 오래 들고 있으려면 복사하세요.

## 재접속

SDK는 종료 코드의 범위를 보고 움직입니다.

| 종료 코드 | SDK의 동작 |
|---|---|
| 4000~4099 | 멈춥니다. 세션이 대체됐거나, 백엔드가 내보냈거나, 정책에 막힌 경우라 그대로 다시 붙어 봐야 또 실패합니다 |
| 4001 `auth_expired` | 새 `getTicket()`으로 곧바로 재접속합니다 |
| 4100~4199 | 지터를 섞은 지수 백오프(1초~30초) 뒤 재접속합니다(수용량 초과, 느린 소비자, 클러스터 장애, 드레인) |
| 4200~4299 | 곧바로 재접속합니다(유휴 시간 초과) |
| 네트워크 장애, 핸드셰이크 거절, `getTicket()` 실패 | 백오프하며 다시 시도합니다 |

`connectTimeoutMs`(기본 10초) 안에 입장하지 못한 시도는 포기하고 다른 실패와 똑같이 다시 시도합니다. 멈춰 버린 프록시나 반쯤 열린 연결 때문에 클라이언트가 `connecting`에 갇히는 일은 없습니다.

재접속하면 들어가 있던 룸에 모두 다시 들어가고, `rejoined` 이벤트로 그 목록을 알려 줍니다. 룸의 `chan`과 `memberId`는 바뀔 수 있습니다. 재입장이 일시적인 이유(`UNAVAILABLE`, `RATE_LIMITED`, `TIMEOUT`, `INTERNAL`)로 실패하면 같은 연결에서 0.5초부터 30초까지 백오프하며 다시 시도합니다. 재입장 도중에 연결이 끊기면 다음 연결에서 다시 시도합니다. `NOT_ALLOWED` 같은 거절일 때만 그 룸을 버리고, `error` 이벤트로 이유를 알립니다.

## 이력과 공백

이력이 있는 룸은 재입장할 때 마지막으로 본 offset부터 이어 받습니다. 놓친 메시지는 평범한 `message` 이벤트로 순서대로 오고, 중복은 offset으로 걸러 냅니다. 다음 두 신호가 오면 백엔드에서 룸 상태를 다시 불러와야 합니다.

- `rejoined` 목록에서 `recovered: false`인 룸
- `gap` 이벤트(`{room, epoch?}`). `epoch`가 있으면 룸의 이력이 새로 시작된 것이며, SDK가 그 룸의 위치를 알아서 초기화합니다.

## 프레즌스

`policy.presence_events`가 켜져 있으면 `join()`이 현재 `members`를 돌려주고, `presence` 이벤트가 `{type: "joined" | "left", room, user, memberId}`를 알려 줍니다. `memberId`는 그 멤버가 보내는 상태 프레임의 `sender`와 같습니다.

## 자격 증명 갱신

게이트웨이가 만료 시각을 알려 주면(JWT와 connect 프록시 자격 증명의 `expires_in_s`) SDK는 수명의 70~90% 사이 임의의 시점에 갱신합니다. `getTicket()`을 불러 열린 연결로 `sys.refresh`를 보내고 `refreshed` 이벤트를 냅니다. 실패하면 다시 시도합니다. 끝내 실패하면 게이트웨이가 만료 시각에 4001로 닫고, SDK는 새 자격 증명으로 재접속합니다. 갱신을 끄려면 `refresh: false`를 주세요.

## 이벤트

| 이벤트 | 내용 |
|---|---|
| `open` | 세션 입장 완료: `{tenant, user, sid, expires_in_s?}` |
| `reconnecting` | 새 시도 직전: `{attempt, delayMs}` |
| `message` | Ext 메시지: 룸 브로드캐스트, 서버 발행 |
| `state` | Hot 프레임: `{room, chan, sender, svc, op, key, seq, payload}` |
| `presence` | `{type, room, user, memberId}` |
| `rejoined` | 재접속 뒤 다시 들어간 룸 목록 |
| `gap` | `{room, epoch?}` |
| `refreshed` | 자격 증명을 갱신함 |
| `close` | `{code, reason, wasOpen, willReconnect}` |
| `error` | `send`의 오류, 재입장 실패, `TICKET_FAILED`, `CONNECT_TIMEOUT`, `REFRESH_FAILED` |

`client.state`는 `idle`, `connecting`, `open`, `closed` 가운데 하나이고, `client.rooms`는 들어가 있는 룸마다 `{room, chan, memberId, members, epoch?, offset?, recovered?}`를 담고 있습니다.

## 테스트

SDK 테스트는 실제 게이트웨이 프로세스를 띄워 놓고 돕니다.

```bash
sdk/js/run-tests.sh
npx -p typescript@5 tsc -p sdk/js    # 타입 선언 검사
```

## 다른 언어

프로토콜 전체가 [와이어 프로토콜](../reference/protocol.md)에 정리돼 있어서 어떤 WebSocket 라이브러리로도 구현할 수 있습니다. Unity(C#)와 Dart SDK는 로드맵에 있습니다.
