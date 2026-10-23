# Hot 레인

Hot 레인은 중간 값을 하나하나 볼 필요가 없을 만큼 빠르게 바뀌는 상태를 나릅니다. 위치, 커서, 조준 방향, 믹싱 콘솔의 페이더, 최우선 매수 호가 같은 것들입니다. Ext 레인의 보장을 내려놓는 대신 낮은 지연과 크기가 정해진 큐를 얻습니다.

## 프레임

Hot 프레임은 WebSocket 바이너리 메시지입니다. 프로토콜 v2에서 클라이언트는 이렇게 보내고,

```
[kind=0x01][flags u8][chan u32 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][payload…]
```

받을 때는 같은 구조에 발행자의 `sender u32`가 페이로드 앞에 끼어 있습니다. 선택 필드가 있는지는 플래그가 알려 줍니다.

| 플래그 | 필드 | 의미 |
|---|---|---|
| `0x01` | `key` | 이 키에 대한 상태입니다. 새 값이 옛 값을 대신합니다 |
| `0x02` | `seq` | 일련번호. 받는 쪽이 순서가 뒤바뀐 프레임을 버리는 데 씁니다 |
| `0x04` | `sender` | 서버만 붙입니다. 발행자의 `member_id`입니다 |

`chan`은 `sys.joined`에서 받은 룸 ID입니다. 들어가 있지 않은 룸을 가리키는 프레임은 버리고 셉니다(`policy_decisions{reason="unknown_channel"}`). `hot_error_mode: sys_error`이면 클라이언트에게도 알리고, `silent`이면 그 대역폭을 아낍니다.

페이로드는 게이트웨이가 들여다보지 않습니다. float 몇 개든 FlatBuffer든 protobuf든 애플리케이션에 맞게 인코딩하면 됩니다.

프로토콜 v1 클라이언트는 더 단순한 예전 헤더(`[v][svc][op][flags][seq?][payload]`)를 쓰고, *활성 룸*, 즉 마지막으로 들어간 룸으로 보냅니다. 두 종류의 클라이언트가 한 룸에 있어도 되며, v1 수신자는 페이로드만 받습니다.

## 라우팅과 허가

Hot 프레임은 `svc`에 적힌 ID의 서비스가 처리합니다. 서비스 `1`은 내장 **룸 릴레이**로, 프레임을 룸의 다른 멤버에게 넘겨 줍니다. 직접 만든 Hot 서비스는 Rust에서 `Gateway::builder().hot_service(...)`로 등록합니다.

테넌트의 `policy.hot_allowlist`에는 클라이언트가 보낼 수 있는 `svc:op` 쌍을 적습니다. 예를 들어 `"1:*"`은 릴레이의 모든 opcode입니다. 목록이 비어 있으면(기본값) 그 테넌트의 Hot 레인은 꺼져 있습니다. 목록에 적은 서비스는 실제로 있어야 하며, 없으면 시작하지 못합니다.

## 키와 병합

Hot 레인을 손실 허용 파이프 이상으로 만드는 것이 키입니다. 키가 붙은 프레임은 *상태*입니다. 수신자의 큐가 밀리면 게이트웨이는 그 수신자 몫으로 `(sender, svc, key)`마다 가장 새 프레임만 남기고 그 앞의 것을 버립니다. 0.5초 동안 멈췄던 클라이언트는 룸에 있는 모두의 옛 위치를 서른 개씩 받는 대신 현재 위치를 받습니다.

서로 독립적인 상태마다 키를 하나씩 쓰세요. 플레이어가 조종하는 개체마다 하나, 커서마다 하나 하는 식입니다. 키가 없는 프레임은 이벤트로 보고 하나하나 전달하며, 수신자의 큐가 가득 차면 그 수신자에게는 버립니다.

병합과 버림이 얼마나 일어나는지는 카운터로 봅니다. `wsprism_hot_slot_overwritten_total`(보내기 전에 새 값으로 바뀐 상태. 정상이며, 값이 크면 수신자가 밀리고 있다는 뜻)과 `wsprism_egress_drop_total`입니다.

## 일련번호

받는 쪽은 같은 송신자·키에서 마지막으로 본 것보다 오래된 `seq`의 프레임을 버려야 합니다. WebSocket에서는 순서대로 도착하지만, datagram을 쓰는 전송에서는 순서가 바뀔 수 있습니다. JavaScript SDK가 이 처리를 해 주며(`u32` 넘침도 고려합니다), 보낼 때도 키마다 번호를 매깁니다.

## 틱으로 속도 맞추기

키가 붙은 상태는 기본적으로 도착하는 즉시 보냅니다. `policy.hot_tick_hz`를 1~240 사이로 정하면, 세션마다 쌓인 상태를 초당 그 횟수까지만 내보냅니다. 내보내는 시점은 세션마다 다르게 잡아서 모든 세션이 한순간에 몰리지 않게 합니다. 업데이트가 틱보다 빨리 들어오면 CPU와 대역폭을 함께 아끼고, 대신 지연이 최대 한 틱만큼 늘어납니다. 키 없는 프레임에는 틱을 적용하지 않습니다.

커서나 가벼운 게임이라면 30 Hz로 시작해 보고, 액션 게임이라면 60 Hz가 적당합니다.

## 보낸 사람에게 되돌리기

`policy.hot_echo_to_sender`(기본 `true`)는 세션이 보낸 프레임을 릴레이를 거쳐 자신도 다시 받을지 정합니다. 상태 동기화 클라이언트는 대개 자기 상태를 이미 알고 있으니, `false`로 두면 룸 Hot 트래픽의 약 1/N을 아낍니다.

## 백엔드에서 발행하기

서버 API로 Hot 프레임도 보낼 수 있습니다.

```json
POST /api/v1/publish
{"room": "match:42", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "AAEC"}}
```

이렇게 보낸 프레임의 송신자는 `0`입니다. 클러스터에서는 모든 노드의 멤버에게 갑니다.

## 지금의 "손실 허용"이 뜻하는 것

WebSocket에서 Hot 프레임이 손실을 허용하는 건 *게이트웨이 안에서만*입니다. 서버를 떠난 프레임은 TCP를 타므로, 패킷 하나를 잃으면 재전송될 때까지 뒤의 프레임이 모두 기다립니다(head-of-line blocking). 깨끗한 네트워크에서는 아무 비용이 없지만, 손실이 있는 모바일 회선에서는 끊김으로 나타납니다.

진짜 비신뢰 전달에는 datagram이 필요합니다. 단방향 지연 20 ms에 손실 1%인 회선을 흉내 내고 60 Hz로 WebTransport datagram과 TLS 위 WebSocket을 비교했습니다. 상태가 낡는 정도(초과 AoI)는 1/3~1/4로 줄었고, 눈에 띄게 낡은 프레임의 비율은 약 3%에서 거의 0이 됐습니다. 대신 연결당 CPU와 메모리가 약 두 배 들었습니다. 그래서 WebTransport는 작은 인터랙티브 룸을 위한 선택지로, 큰 방송형 룸에는 WebSocket을 그대로 쓰는 방향으로 계획하고 있습니다. [벤치마크](../project/benchmarks.md)와 [로드맵](../project/roadmap.md)을 참고하세요.

## 예제

문서 룸에서 커서를 공유하되, 30 Hz로 보내고 자기 커서는 되돌려 받지 않는 설정입니다.

```yaml
tenants:
  - id: docs
    policy:
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:1"]
      hot_tick_hz: 30
      hot_echo_to_sender: false
      presence_events: true
```

```js
await client.join("doc:7f3a");
window.addEventListener("pointermove", (e) => {
  const buf = new Float32Array([e.clientX, e.clientY]);
  client.sendState("doc:7f3a", { svc: 1, op: 1, key: 0, payload: new Uint8Array(buf.buffer) });
});
client.on("state", (s) => drawCursor(s.sender, new Float32Array(s.payload.slice().buffer)));
client.on("presence", (p) => p.type === "left" && removeCursor(p.memberId));
```

각 사용자는 마우스가 움직이는 만큼 보내고, 보는 쪽은 커서 하나당 초당 최대 30번, 언제나 최신 값만 받습니다.
