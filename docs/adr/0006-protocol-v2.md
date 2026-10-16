# ADR 0006: 프로토콜 v2 — 인코딩·전달 의미 분리, 명시적 채널, 재접속 의미를 가진 종료 코드

- 상태: **채택 (2026-10-16, 리뷰 반영 후)** — 사용자 지시: "검토해서 문제가 없으면 진행". 리뷰에서 나온 문제 5건을 아래 「리뷰 반영」으로 고쳤다. 구현은 단계별(§8).
- 관련: [PROTOCOL](../PROTOCOL.md)(v1), [ADR 0003](0003-ticket-sources-and-subprotocol.md)(버전 협상), [ADR 0004](0004-product-direction.md)(상태 동기화), [Hot Lane over WebTransport](../design/hot-lane-over-webtransport.md) §3, [성능 아키텍처](../design/performance-architecture.md) §3.5

## 배경

설계 리뷰(2026-10-16)에서 v1 와이어 형식이 상용 게이트웨이(Centrifugo, Pusher 계열, Ably)의 관행과 어긋나는 점들이 나왔다. 클라이언트 계약을 바꾸는 일이라 **한 번에 모아** v2로 낸다. SDK(M2-lite)를 만들기 전에 해야 비용이 가장 작다. v1은 서브프로토콜 협상으로 계속 받는다(§7).

| # | v1의 문제 | 업계 관행 |
|---|---|---|
| ① | 레인(전달 의미)이 WebSocket **프레임 종류**에 묶임: 텍스트 = Ext(JSON·신뢰), 바이너리 = Hot(손실 허용). Ext를 바이너리로 인코딩할 수 없다 | 인코딩은 **연결마다 협상**(Centrifugo JSON/Protobuf, Ably JSON/MessagePack), 전달 의미는 채널·메시지 단위 |
| ② | Hot 프레임에 대상이 없다: 세션의 `active_room`(마지막으로 들어간 방)으로 간다. 여러 방에 보낼 수 없고, join과 전송 중 프레임 사이에 경쟁이 있다 | 대상은 항상 명시(채널) |
| ③ | 메시지마다 버전(JSON `v`, Hot 첫 바이트)과 JSON `flags` 비트마스크(필드 유무로 이미 알 수 있음). `ACK_REQUESTED`는 미구현 | 버전은 연결 단위 협상 |
| ④ | 클라이언트 봉투가 모르는 필드를 거부(1002로 종료) → 클라이언트·서버를 따로 배포하면 호환성 사고 | 모르는 필드는 무시(Protobuf 기본) |
| ⑤ | 종료 코드는 표준 코드만, 1008 하나에 정책·킥·교체·세션 한도·느린 소비자를 모두 담음. 재접속 방법을 알려 주지 않는다 | Pusher: 4000~4099 재접속 금지, 4100~4199 백오프 후, 4200~4299 즉시. Centrifugo: 3000번대 자체 코드 |
| ⑥ | Hot 수신자가 송신자·순서를 알 수 없다(datagram은 순서가 바뀜), 키별 최신값(conflation) 표시가 없다 | — (상태 동기화 의미론, ADR 0004) |

## 결정 (제안)

### 1. 협상: 버전 + 인코딩

```
Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.v2.bin, wsprism.v1, wsprism.auth.<ticket>
```

- 서버는 클라이언트가 제시한 것 중 **서버 선호 순서(v2 → v1)로 첫 번째**를 골라 그것만 돌려준다(인증 항목은 절대 돌려주지 않음, ADR 0003 그대로).
- `wsprism.v2.json`: 제어·Ext 메시지는 텍스트 프레임의 JSON. Hot은 바이너리 프레임.
- `wsprism.v2.bin`: **예약만 한다(리뷰 반영 R1).** 모든 메시지를 바이너리로(제어·Ext는 MessagePack) 보내는 안. JSON 디코드는 프로파일 1.8%로 병목이 아니고, 인코딩이 둘이면 SDK·테스트 표면이 두 배가 된다. 바이너리 Ext 페이로드 수요가 확인되면 추가한다.
- 헤더가 없거나 `wsprism.v1`이면 v1 동작(지금과 같음).
- WebTransport(T)는 연결 URL의 `protocol=v2.json|v2.bin`으로 같은 선택을 한다.

### 2. 프레임 종류와 전달 의미의 분리

v2에서 바이너리 프레임은 첫 바이트(`kind`)로 종류를 나눈다. JSON 인코딩의 텍스트 프레임은 항상 `kind = ctl` 메시지다.

| kind | 의미 | 전달 |
|---|---|---|
| `0x00` ctl | 제어·Ext 메시지(MessagePack, `v2.bin`일 때) | 신뢰 |
| `0x01` hot | Hot 상태 프레임(§3) | 채널 정책(기본 손실 허용) |
| `0x02` batch | 여러 프레임 묶음(§4) | 담긴 프레임을 따름 |

전달 의미(신뢰/손실 허용, conflation)는 **채널 정책**이 정한다. 같은 채널의 프레임은 인코딩과 무관하게 같은 의미를 갖는다.

### 3. Hot 프레임 (v2)

```
[kind=0x01][flags u8][chan u16 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][sender u32 LE?][payload ...]
flags: 0x01 KEY  0x02 SEQ  0x04 SENDER(서버→클라이언트 전용)
```

- **`chan`(채널 별칭, ②)**: `ctl` `join` 응답이 채널 이름마다 연결 범위의 `u16` 별칭을 준다(MQTT 5 토픽 별칭과 같은 발상). 2바이트로 대상이 명시적이 되고, 여러 채널에 동시에 보낼 수 있다. `active_room`은 v2에 없다.
- **버전 바이트 없음(③)**: 버전은 연결에서 협상했다.
- **`key`(⑥)**: 있으면 서버의 키별 최신값 슬롯 대상(설계 §4.2). 슬롯 키는 `(sender, svc, key)`.
- **`seq`**: 클라이언트가 단조 증가로 붙인다. 수신 SDK는 `(sender, key)`마다 마지막 `seq`보다 오래된 프레임을 버린다(RFC 1982 비교) — datagram의 순서 바뀜 대비.
- **`sender`**: 서버가 붙인다. 채널 범위 `member_id`, **연결마다 새로 발급**(재접속 후 `seq`가 0부터 시작해도 오래된 것으로 오인하지 않음). `join` 응답에 자기 `member_id`가 들어 있다.
- 크기 상한: Hot 프레임 전체 ≤ `policy.max_datagram_bytes`(기본 1,100 B) — 전송과 무관하게 같은 상한(설계 §3.2).

### 4. 묶음 프레임 (틱 송신)

```
[kind=0x02][count u16 LE] then count × [len u16 LE][frame (kind 포함)]
```

서버→클라이언트 전용. 틱 묶음 송신(설계 §4.2)과 datagram 하나에 여러 업데이트를 담을 때 쓴다. 클라이언트는 풀어서 각각 처리한다.

### 5. 제어·Ext 메시지

```json
{"svc":"chat","type":"send","ch":"party:1","id":123,"data":{...}}
```

- `v`, `flags` 제거(③). `seq` → `id`(요청 id, 응답의 `reply_to`와 짝 — v1 `seq`의 실제 의미). `room` → `ch`(채널).
- **모르는 필드는 무시**하고 `wsprism_unknown_fields_total` 메트릭만 올린다(④). 테넌트 정책 `strict_envelope: true`면 v1처럼 거부.
- `sys.joined` → `{"svc":"sys","type":"joined","ch":"party:1","alias":3,"member_id":17,"reply_to":…}`.
- 구독과 발행 권한은 채널 정책에서 분리한다(`subscribe` / `publish`, M2-lite의 JWT 채널 권한과 함께). v2의 `join`은 구독, 발행 권한은 별도 검사.

### 6. 종료 코드 (⑤)

서버가 보내는 종료는 4000번대 자체 코드를 쓰고 **범위가 재접속 방법**을 뜻한다(Pusher와 같은 범위 규칙). 표준 코드는 표준 의미일 때만(1000 정상, 1001 서버 종료).

| 범위 | 클라이언트 동작 | 코드 |
|---|---|---|
| 4000~4099 | **그대로 재접속하지 않음**(새 티켓·설정 변경 필요) | 4001 `auth_expired`, 4003 `policy`, 4004 `too_many_sessions`, 4005 `replaced`, 4006 `max_sessions_exceeded`, 4009 `message_too_big`, 4010 `decode_error`, 4011 `unsupported_version` |
| 4100~4199 | **백오프 후 재접속**(최소 1초, 지수 + 지터) | 4100 `capacity`, 4101 `slow_consumer`, 4102 `rate_limited` |
| 4100~4199 (계속) | | 4103 `draining` — 서버가 `sys.reconnect{after_ms}`(무작위 분산된 대기 시간)를 먼저 보낸다. 재배포 시 재접속 폭주를 나눈다 (리뷰 반영 R2: 초안은 42xx "즉시"였는데 분산 목적과 모순) |
| 4200~4299 | **즉시 재접속** | 4200 `idle_timeout` |

- `sys.error` 통지는 지금처럼 종료 직전에 보낸다.
- 크기 초과는 tungstenite가 헤더 단계에서 거절한 뒤 **우리가** 종료를 보내므로(S2 경로), v2 세션에는 4009를 보낸다(리뷰 반영 R4). SDK는 다른 구현과의 호환을 위해 1009도 4009와 같게 다룬다.

### 7. v1 호환과 폐기

- v1은 협상으로 계속 받는다. 서버 내부는 v1 메시지를 v2 표현으로 바꿔 처리한다(v1 Hot 프레임 → `chan = active_room의 별칭`).
- v1 폐기 공지는 TS SDK(v2 전용)가 나온 뒤 한 마이너 버전 이상 유지 후.

### 8. 구현 단계

| 단계 | 내용 | 비고 |
|---|---|---|
| v2-1 ✅ | 협상(`wsprism.v2.json`), 세션의 프로토콜 버전, v2 종료 코드 매핑 | 작음. v1 동작 불변. `sys.reconnect` 힌트는 다음 단계 |
| v2-2 | v2 제어 봉투(`ch`/`id`, 모르는 필드 무시 + 메트릭, `strict_envelope`) | v1 봉투는 그대로 |
| v2-3 | 채널 별칭·`member_id`(join 응답), v2 Hot 프레임 디코드·라우팅, `HotRelayService`(`sender` 부착), 버전별 인코딩 캐시 | T0-3(Hot 슬롯)과 함께 |
| v2-4 | 묶음 프레임 + 틱 송신 | T0-3 |

### 9. 리뷰 반영 (2026-10-16)

| # | 문제 | 수정 |
|---|---|---|
| R1 | `v2.bin`(MessagePack)은 근거 없는 범위 확장 | 토큰만 예약, 구현 보류 |
| R2 | `draining`을 "즉시 재접속"에 둠 — 재접속 분산 목적과 모순 | 4103(백오프 범위) + `sys.reconnect{after_ms}` |
| R3 | v1·v2 클라이언트가 섞인 룸에서 "한 번 인코딩해 공유"가 깨짐 | 송신 프레임이 구조화된 Hot 데이터를 들고, **버전별 인코딩을 처음 필요할 때 한 번만 만들어 캐시**(발행당 최대 버전 수만큼). 수신자들은 그 바이트를 공유 |
| R4 | 1009를 표준 코드로 남김 | 종료는 우리가 보내므로 v2는 4009 |
| R5 | 모르는 채널 별칭의 Hot 프레임 처리 미정 | 드롭 + `policy_decisions{reason="unknown_channel"}`, `hot_error_mode: sys_error`면 `sys.error` |

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| Protobuf로 바이너리 인코딩 | 스키마 컴파일 단계가 생겨 "언어 무관·쉬운 커스텀"에 불리. MessagePack은 JSON과 같은 모델이라 서비스 코드가 인코딩을 몰라도 된다. Protobuf는 수요가 확인되면 `wsprism.v2.pb`로 추가 |
| Hot 프레임에 채널 이름 문자열 | 프레임마다 수십 바이트. 별칭이면 2바이트이고 datagram 상한(1,100 B) 안에서 페이로드를 잃지 않는다 |
| v1 `active_room`을 유지하고 별칭을 선택 사항으로 | 암묵 라우팅의 경쟁·다중 채널 불가가 그대로 남는다. v1 호환 경로에서만 유지 |
| 종료 코드를 표준 코드 + reason 문자열로 유지 | reason은 사람이 읽는 값이라 SDK가 분기 근거로 쓰기 어렵고, 재접속 전략을 표현하지 못한다 |
| JSON 봉투의 모르는 필드 계속 거부 | 클라이언트와 서버가 동시에 배포되지 않는 현실에서 호환성 사고의 원인. 엄격 모드는 옵션으로 남긴다 |

## 결과 (채택 시)

- `wsprism-core::protocol`에 v2 코덱(`ctl` JSON/MessagePack, `hot`, `batch`)과 v1→v2 변환. 퍼징 대상 추가.
- 세션: 연결별 채널 별칭 표(`u16 → RoomId`), `member_id`(연결마다).
- PROTOCOL.md를 v2 기준으로 다시 쓰고 v1은 부록으로.
- SDK(M2-lite, TS)는 v2 전용.
- 의존성: MessagePack(`rmp-serde`) 추가 — 네트워크로 받아야 하므로 구현 착수 시 확인.
- T0-4(Hot 헤더 확장·`member_id`·크기 상한)는 이 ADR의 §3으로 대체된다.
