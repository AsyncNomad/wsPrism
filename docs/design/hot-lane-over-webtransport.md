# 설계: Hot Lane over WebTransport

- 상태: 초안, **개정 3** (2026-10-14). 개정 3: 서버 피어 멤버 지정 송신(§4.6), 측정 수정(ADR 0004 개정 3). 개정 1: AoI 주 지표, 비용 관문 G5, MoQ 평가, T0 분리, netns 측정. 개정 2: Hot 슬롯 키 버그 수정(송신자 포함), `member_id` 연결별 발급, 서버 피어 경로(§4.6), 초과 AoI·판정 칸 고정·G1/G5 동일 설정(틱 곡선), 작업 번호·일정 정리. 결정 근거는 [ADR 0004](../adr/0004-product-direction.md).
- 범위: ① 목표 아키텍처(단계 T에서 구현), ② 그 전에 수행할 검증 단계 V의 구체 계획.

## 0. 요약

간판은 전송이 아니라 **상태 동기화 의미론**이다: 키별 최신값 유지(conflation), 틱 단위 송신 조절과 묶음 전송, 테넌트 정책, 백엔드 연동. 이를 **가장 좋은 비신뢰 전송** 위에서 제공한다 — WebTransport datagram(직접 구현 또는 MoQ, V에서 결정), 안 되면 WebSocket 폴백. 같은 의미론은 WebSocket 위에서도 동작한다(TCP HOL은 못 없애지만 큐 적체는 줄인다).

순서: ① 검증 V로 체감 성능(AoI)·비용·호환성·안정성·폴백을 숫자로 확인 → ② **T0**: 전송 능력·전달 등급·Hot 슬롯 경계를 WebSocket만으로 구현(M2 공개 트레이트 전에) → ③ M2-lite → ④ **T**: 비신뢰 전송 구현.

대표 시나리오: **모바일·Wi-Fi 환경의 20~60 Hz 웹 멀티플레이 상태 동기화.**

## 1. 목표와 비목표

**목표**
- 손실이 있는 네트워크에서 **상태 신선도(AoI)의 꼬리와 끊김 비율**을 WebSocket 대비 크게 개선한다.
- 비용(메시지당 CPU, 연결당 메모리)이 WSS 대비 2배를 넘지 않는다.
- 같은 서비스·정책 코드가 모든 전송에서 동작한다(전송 투명성).
- 비신뢰 전송이 안 되면 **자동으로 WebSocket 폴백**.
- 기존 보안 모델(admission 파이프라인, 테넌트 정책, 메트릭 라벨 원칙) 재사용.

**비목표**
- 게임 서버 로직(authoritative simulation), 매치메이킹, 계정.
- datagram 위의 신뢰 전송 재구현(그건 QUIC 스트림이 한다).
- WebRTC DataChannel 지원.
- MoQ의 미디어 기능(코덱·카탈로그 등). MoQ를 쓰더라도 상태 동기화 트랙만.

## 2. 전달 클래스

전송과 무관한 핵심 개념. 서비스는 클래스만 고르고, 매핑은 전송 계층이 한다.

| 클래스 | 의미 | WebTransport | WebSocket |
|---|---|---|---|
| **Reliable** | 순서 보장, 누락 없음(또는 수신자를 slow consumer로 종료, C1) | 제어 스트림(양방향 스트림 1개) | 텍스트/바이너리 프레임(현재 그대로) |
| **Unreliable** | 유실·순서 바뀜 허용, 오래된 것은 의미 없음 | **datagram** | 바이너리 프레임(TCP라 네트워크 손실은 허용 안 됨) |
| **Unreliable + key** | 같은 키는 최신 하나만 의미 있음 | datagram (송신 전 슬롯에서 덮어씀) | 바이너리 프레임 (**송신 전 슬롯에서 덮어써** 밀린 큐를 줄임) |

- 현재의 `QoS::Lossy`는 Unreliable, `QoS::Reliable`은 Reliable에 대응한다.
- Ext Lane은 항상 Reliable. Hot Lane은 기본 Unreliable.

## 3. 프로토콜 확장 (v1 하위 호환, 플래그로 추가)

### 3.1 Hot 프레임 헤더

```
[v=1][svc_id][opcode][flags][sender u32?][seq u32?][key u32?][payload ...]
flags: 0x01 SEQ_PRESENT (기존) · 0x02 ACK_REQUESTED (기존, datagram에선 무의미) · 0x04 KEY_PRESENT (신규) · 0x08 SENDER_PRESENT (신규, 서버→클라이언트 전용)
```

- **클라이언트→서버**: `key`가 있으면 서버의 conflation 대상. `seq`는 클라이언트가 단조 증가로 붙인다.
- **서버→클라이언트**: 서버가 `sender`(룸 범위 멤버 id)를 붙여 보낸다. 수신 SDK는 `(sender, key)`마다 마지막 `seq`보다 오래된 프레임을 버린다(u32 serial-number 비교, RFC 1982 방식). datagram은 순서가 바뀌므로 이 필터가 필수다.
- `sys.joined`에 자기 `member_id`를 추가한다(수신 측이 자기 자신을 구분).
- **`member_id`는 연결(세션)마다 새로 발급한다.** 재접속한 클라이언트의 `seq`가 0부터 다시 시작해도, 수신 필터는 새 `sender`로 보므로 새 업데이트를 "오래된 것"으로 버리지 않는다. (기각: 헤더에 epoch 추가 — 같은 일을 프레임마다 바이트를 더 써서 한다.) 수신 측은 나간 멤버의 필터 항목을 `sys` 퇴장 이벤트나 일정 시간 무수신으로 정리한다.
- 현재 echo 서비스는 payload만 돌려준다. 표준 헤더를 쓰는 `HotRelayService`를 새로 두고 echo는 예제로 남긴다.

### 3.2 datagram 크기

- QUIC datagram은 경로 MTU 안에 들어가야 한다(보통 약 1,200 B). 연결별 `max_datagram_size()`를 따른다.
- 테넌트 정책 `policy.max_datagram_bytes`(기본 1,100). 인바운드 초과: 드롭 + `policy_decisions{lane="hot",reason="datagram_too_large"}`. 아웃바운드 초과: **조각내지 않고** 드롭 + 메트릭. Hot Lane 상태는 작게 설계하는 것이 계약이다.
- WebSocket 세션은 같은 한도를 적용한다(전송 간 동작 일치).

### 3.3 WebTransport 세션

- URL: `https://<host>:<port>/v1/wt?tenant=<id>&ticket=<1회용 티켓>[&sid=]`
- 브라우저 `WebTransport` API는 헤더를 붙일 수 없다. 서브프로토콜 옵션(`protocols`)은 브라우저 지원이 확인되지 않았다 → **1회용 단기 티켓을 URL로** 받는다(ADR 0003의 "쿼리 금지"는 장수명 토큰이 로그에 남는 문제였고, 1회용·단기 티켓은 재사용이 불가해 허용 가능). 티켓 발급은 M2-lite의 백엔드용 API(`POST /api/v1/tickets`)가 담당.
- Ext Lane: 클라이언트가 연 첫 양방향 스트림을 **제어 스트림**으로 쓴다. 프레이밍은 `u32 BE 길이 + JSON 봉투`(기존 envelope 그대로). 서버→클라이언트 Ext 메시지도 같은 스트림.
- Hot Lane: datagram, 3.1의 헤더.
- 종료: WebTransport `CloseWebTransportSession`의 코드에 PROTOCOL.md의 종료 코드(1000~1013)를 그대로 싣는다.

## 4. 서버 구조

```
                 ┌──────────── admission (공통: client_ip, rate, tenant, origin, auth, capacity)
TCP :8080  ws ──►│
UDP :4433  wt ──►│
                 ▼
        Session (공통 이벤트 루프, 정책, 디스패치, RealtimeCtx)
           │  ▲
           ▼  │  SessionIo 트레이트
     ┌─────────────┬──────────────┐
     │ WsIo        │ WtIo         │
     │ 프레임 1종   │ 제어 스트림 +  │
     │             │ datagram      │
     └─────────────┴──────────────┘
```

### 4.1 `SessionIo` 트레이트

> **구현(T0-1)**: 읽기·쓰기를 동시에 기다리는 `select!` 때문에 단일 트레이트 대신 두 절반 `SessionTx`/`SessionRx`로 나눴다. 비신뢰 송신은 T0-3에서 추가. [ADR 0005](../adr/0005-session-transport-seam.md)

현재 세션 루프는 axum `WebSocket` 타입(`SplitSink/SplitStream`)에 묶여 있다. 이를 트레이트로 추상화한다.

```rust
#[async_trait]
trait SessionIo: Send {
    /// 다음 인바운드 메시지 (Ext 봉투 / Hot 프레임 / 종료).
    async fn recv(&mut self) -> Option<Result<Inbound>>;
    /// Reliable 송신 (Ext, 신뢰 바이너리).
    async fn send_reliable(&mut self, frame: OutFrame) -> Result<()>;
    /// Unreliable 송신. 즉시 보내지 못하면 드롭 (블로킹 금지).
    fn send_unreliable(&mut self, frame: Bytes) -> Result<(), Dropped>;
    /// 정중한 종료 (C3: 통지 → 종료 코드 → 짧은 대기).
    async fn close(&mut self, spec: CloseSpec);
    fn transport(&self) -> TransportKind; // 메트릭 라벨 "ws" | "wt"
}
```

- `WsIo`: 지금의 `wire.rs`·`close.rs`·`codec.rs`를 감싼다. `send_unreliable`은 바이너리 메시지 송신(실패 시 드롭).
- `WtIo`: 제어 스트림 + `send_datagram`. datagram 송신 버퍼가 가득 차면 드롭 + 메트릭.
- 세션 루프는 `Session<Io: SessionIo>`로 일반화된다. admission·정책·디스패치·`RealtimeCtx`는 그대로.

### 4.2 송신 경로 둘

- **Reliable 큐**: 지금의 `mpsc<OutFrame>`(C9 원칙: 세션 루프는 자기 큐를 기다리지 않음).
- **Hot 슬롯**: 세션마다 작은 "최신값 맵" **`(sender, svc_id, key) → Bytes`** + 키 없는 프레임용 짧은 FIFO(기본 64). 발행자는 슬롯에 넣기만 하고(덮어쓰기, 블로킹 없음), 세션 루프가 비울 때마다 `send_unreliable`로 보낸다. 깊은 큐가 없으므로 오래된 상태가 쌓이지 않는다.
  - **키에 `sender`가 반드시 들어간다.** `key`는 송신자마다 따로 쓰는 이름 공간이라(§3.1), `(svc_id, key)`만 쓰면 두 플레이어가 둘 다 `key=0`("내 위치")으로 보낼 때 수신자 슬롯에서 서로를 덮어써 한 명의 움직임이 사라진다(개정 2에서 수정한 설계 버그).
  - 슬롯 크기는 "룸 인원 × 키 수"로 커지므로 상한 `policy.max_slot_entries`(기본 1,024)를 두고, 초과 시 새 키를 드롭 + `wsprism_hot_dropped_total{reason="slot_full"}`. 기존 키의 덮어쓰기는 항상 허용.
  - V2 스파이크 릴레이는 송신자당 키 1개(`(sender) → frame`)로 같은 구조다.
- `OutFrame`에 전달 클래스를 추가한다: `OutFrame::Hot { key: Option<u32>, bytes }`. `RealtimeCore::deliver`가 `QoS::Lossy` 바이너리를 Hot 슬롯으로 보낸다.
- 슬롯의 동시성(발행자 여러 task ↔ 세션 1개): 처음에는 세션별 `Mutex<작은 맵>` + `Notify`. 마이크로 벤치마크(`publish_room` 수신자당 비용)로 회귀를 측정하고, 문제가 되면 lock-free 구조로 바꾼다.
- **틱 묶음 전송**: 세션 루프가 테넌트 설정 주기(예: 30/60 Hz)마다 슬롯을 비우며 여러 업데이트를 한 datagram(또는 한 WebSocket 프레임)에 묶는다. 패킷 수를 줄여 QUIC의 패킷당 비용(암호화, 시스템 콜)을 낮추는 핵심 수단이고, G5 판정은 이 기능을 켠 상태로 한다. 묶음 형식: `[count u8]` + `[len u16][hot frame]` 반복.
- **이 절(4.1·4.2)의 WebSocket 부분은 T0**로 M2 공개 트레이트보다 먼저 구현한다.

### 4.3 admission 재사용

- `wtransport::SessionRequest`에서 `HandshakeRequest`를 만든다(헤더 맵, `origin()`, 원격 주소, 쿼리).
- `wtransport`의 거절 응답은 403/404/429뿐이다. 매핑: rate_limit·draining·tenant_capacity → 429, unknown_tenant·unsupported_protocol → 404, origin·auth → 403. **메트릭의 `reason` 라벨은 정확히 유지**(HTTP 코드만 거칠어진다).
- QUIC은 L7 프록시를 거치지 않는 경우가 대부분이라 `trusted_proxies`/XFF는 적용하지 않는다. UDP 로드밸런서가 PROXY protocol v2를 쓰는 경우는 열린 질문(§8).

### 4.4 설정

```yaml
gateway:
  webtransport:
    enabled: false               # 기본 꺼짐
    listen: "0.0.0.0:4433"       # UDP
    cert: "/etc/wsprism/tls.crt" # 실서비스: 브라우저가 신뢰하는 인증서
    key: "/etc/wsprism/tls.key"
    dev_self_signed: false       # 개발용: 14일 이하 ECDSA 자체 서명 + serverCertificateHashes용 해시 출력
    max_idle_timeout_ms: 30000
    keep_alive_interval_ms: 10000
tenants:
  - id: game
    policy:
      max_datagram_bytes: 1100
```

### 4.5 관측

- 모든 세션·메트릭에 `transport` 라벨(`ws`|`wt`) 추가.
- 신규: `wsprism_hot_dropped_total{tenant,transport,reason="oversize|send_buffer_full|slot_overwritten"}`.
- `slot_overwritten`은 정상 동작(최신값 우선)의 지표이지 오류가 아니다 — 대시보드에서 구분.

### 4.6 서버 피어 (권위 게임 서버의 앞단)

클라이언트끼리 릴레이만 하는 모델은 치트를 막을 수 없다. 권위 게임 서버가 wsPrism을 앞단으로 쓰는 경로:

- **연결**: 게임 서버가 **신뢰된 장기 연결**로 붙는다(QUIC 또는 TCP, 인증은 테넌트 API 키 또는 mTLS). 브라우저 경로의 admission(Origin, 1회용 티켓)과는 별도 단계지만 같은 admission 파이프라인에 역할(`role = server_peer`)로 끼운다.
- **수신**: 서버 피어는 자기가 맡은 룸들의 **클라이언트 Hot 프레임(입력)** 을 받는다. 입력은 유실되면 게임이 판단을 못 하므로 **신뢰 채널**(conflation 없음)이 기본이고, 테넌트 정책으로 비신뢰 선택 가능.
- **발행**: 서버 피어가 보낸 Hot 프레임은 권위 상태로서 룸에 팬아웃된다(`sender` = 서버 피어의 member_id, Hot 슬롯·틱·전송 매핑은 일반 경로와 동일). 클라이언트 간 직접 릴레이는 룸 정책으로 끌 수 있다(`policy.hot_peer_relay: false`).
- **멤버 지정 송신 (개정 3)**: 룸 전체 팬아웃만으로는 권위 서버가 **관심 영역 필터링**(벽 너머의 적, 안개 속 유닛을 그 플레이어에게 보내지 않기)을 할 수 없다. 그러면 정보가 모든 클라이언트에 도착해 메모리를 읽는 치트(wallhack·maphack)를 막지 못하고, 서버 피어를 둔 이유가 반쯤 사라진다. 그래서 서버 피어는 **특정 멤버(들)에게** 보낼 수 있어야 하고, 수신자마다 다른 값을 보낼 수 있어야 한다.
  - **슬롯과의 관계**: Hot 슬롯은 이미 **수신 세션마다** 있다(§4.2). 그래서 같은 `(sender, svc_id, key)`에 대해 플레이어 A의 슬롯에는 값이 있고 B의 슬롯에는 없는 상태가 구조상 자연스럽다. 슬롯 키를 바꿀 필요는 없다. 새로 필요한 것은 **누구의 슬롯에 넣을지 정하는 대상 타입**이다.
  - **코어 API — 대상 타입 하나로 통일**: 지금 `RealtimeCore`의 `send_to_user`·`send_to_session`·`publish_room(_lossy/_reliable)`는 대상마다 메서드가 따로 있다. T0에서 이를 `deliver_to(audience: Audience, out: Outgoing)` 하나로 모으고 기존 메서드는 얇은 래퍼로 남긴다.
    ```rust
    enum Audience<'a> {
        Room { room: &'a str, except: Option<MemberId> },      // 일반 릴레이(송신자 제외)
        Members { room: &'a str, ids: &'a [MemberId] },        // 서버 피어의 지정 송신
        Session(&'a str),
        User(&'a str),
    }
    ```
    전달 등급(Reliable / Hot{key})은 `Outgoing`이, 대상은 `Audience`가 정한다. 둘이 독립이라 "특정 멤버에게 Hot" 같은 조합이 따로 코드를 붙이지 않고 나온다.
  - **수신자마다 다른 값**: 서버 피어는 틱마다 수신자 수만큼 서로 다른 프레임을 보낸다. 이를 `Members` 호출 N번으로 하면 룸 조회·잠금이 N번이므로, `deliver_batch(room, &[(MemberId, Outgoing)])`로 한 번에 넘긴다(룸 조회 1회).
  - **와이어(서버 피어 → 게이트웨이 전용)**: Hot 헤더에 `0x10 TARGETS_PRESENT` 플래그를 두고, 헤더 뒤에 `[count u8][member_id u32 × count]`를 붙인다. 게이트웨이는 대상 목록을 떼고 일반 Hot 프레임(§3.1, `sender` = 서버 피어)으로 각 대상의 슬롯에 넣는다. **브라우저 클라이언트가 이 플래그를 쓰면 정책 위반으로 드롭**한다(클라이언트끼리의 귓속말은 Ext Lane의 서비스가 할 일).
  - **인가**: 대상은 서버 피어가 맡은 룸의 현재 멤버여야 한다. 없는 `member_id`는 조용히 건너뛰고 `wsprism_hot_dropped_total{reason="unknown_target"}`로 센다(퇴장과 송신이 경합하는 것은 정상이므로 오류로 끊지 않는다).
  - **관심 영역 이탈(despawn)**: 엔티티가 어떤 플레이어의 관심 영역에서 빠지면, 서버 피어는 그 플레이어에게 Reliable로 "제거"를 보내고 게이트웨이는 그 플레이어 슬롯의 해당 키를 지운다. 이미 날아가는 datagram이 제거 뒤에 도착할 수 있으므로, 클라이언트 SDK는 제거된 `(sender, key)`에 대해 **제거 시점의 seq보다 새 값만** 다시 받아들인다. 세부 순서 규약은 열린 질문(§8).
  - **비용**: 수신자마다 값이 다르면 공유 `Bytes` 하나를 복제하는 팬아웃 이득이 사라진다(수신자당 직렬화·복사). 틱 묶음 전송은 세션별이라 그대로 적용된다. V의 비용 측정은 공유 팬아웃만 쟀으므로, T에서 지정 송신 비용을 따로 잰다.
- **경계**: 서버 피어는 `SessionIo` 구현 하나 + 역할로 표현된다. 따라서 T0의 `SessionIo`·전달 등급 설계가 "브라우저 세션"을 전제하지 않는지 확인하는 것이 T0의 완료 조건에 포함된다. 구현은 T.
- 이로써 포지션은 "어느 언어로 짠 게임·실시간 서버든, 그 앞에서 브라우저 연결·정책·팬아웃·datagram을 떠맡는 사이드카"가 된다(ADR 0004).

## 5. 클라이언트 SDK (TypeScript, M2-lite에서 최소판)

- `connect()`: 브라우저에 `WebTransport`가 있으면 WT를 1.5초 제한으로 시도 → 실패 시 WebSocket. 티켓은 시도마다 새로 발급(1회용).
- API는 전송과 무관: `request(svc, type, data)`(seq/reply_to로 Promise), `sendState(svcId, opcode, key, bytes)`, `onState(cb)`, `onMessage(cb)`.
- 수신 필터: `(sender, key)`별 최신 `seq`보다 오래된 Hot 프레임 폐기(§3.1).
- 재접속: 지수 백오프 + 지터, 재접속 후 룸 재입장.

## 6. 보안

- 1회용 티켓: 발급 후 짧은 TTL(기본 30초), 첫 사용 시 소멸. 저장소는 처음엔 프로세스 메모리, M4에서 공유 저장소.
- QUIC 주소 검증: 핸드셰이크 폭주 시 stateless retry로 증폭 공격 완화(`remote_address_validated`), 기존 IP별 핸드셰이크 리미터 적용.
- Origin 검사: `SessionRequest::origin()`에 기존 `OriginPolicy` 적용.

## 7. 검증 단계 V (약 3주)

산출물 중 **측정 장치(netns 스크립트, 부하 생성기의 전송·AoI·비용 지표)는 유지**하고, **스파이크 서버(`spikes/wt-hotlane/`)는 버린다**. 제품 구현은 T0·T에서 §4 설계로 새로 한다.

### 7.1 측정 정의

| 지표 | 정의 | 역할 |
|---|---|---|
| **상태 신선도 AoI** | 수신자가 **60 Hz 렌더 틱**마다 `지금 - (보유한 가장 새로운 상태의 생성 시각)`을 기록. 유실은 AoI를 키우는 쪽으로 반영되므로 전송 간 공정 | 주 지표의 원값 |
| **초과 AoI** | AoI p99 − (같은 지연·같은 설정·손실 0%의 AoI p99). AoI의 바닥값(2구간 지연 + 송신 주기 톱니)은 어떤 전송도 줄일 수 없으므로 증가분으로 판정 | **G1** |
| **끊김 비율** | AoI > 100 ms인 틱의 비율. 기준 민감도를 보려고 50/150/200/300 ms 비율도 함께 낸다(`stall_ratio_at`) | 주 지표 (G1), 단절에 민감 |
| 메시지당 지연 | 송신 → 수신 (도착한 메시지만) | 보조 — 유실 패킷이 빠지는 생존 편향이 있어 판정에 쓰지 않는다 |
| 전달률 | 받은 프로브 / (보낸 프로브 × 룸 인원) | 보조 |
| **CPU/메시지** | 서버 CPU 시간(`/proc/<pid>/stat`) / 전달 메시지 수 | **G5** |
| **연결당 메모리** | **한계 비용**: 두 연결 수(예: 100, 300)에서 RSS 증가분의 기울기. (RSS − 기준) / 연결 수는 고정 비용을 섞어 쓰지 않는다 | **G5** |

- **클라이언트 위상 (개정 3)**: 클라이언트마다 송신·렌더 타이머의 시작 위상을 한 주기 안에서 무작위로 둔다(`--phase random --seed <run>`). 개정 2까지의 측정은 모든 클라이언트가 같은 순간에 시작해 렌더 틱이 송신 틱에 고정됐고, AoI가 계단값(지연 + k·주기)이 되어 백분위와 끊김 기준이 계단 위치에 좌우됐다(BENCHMARKS 「측정 결함」).

### 7.2 네트워크 에뮬레이션

- **network namespace + veth 쌍**으로 서버와 클라이언트를 분리하고, 각 veth에 **방향별로** netem을 건다(루프백 netem은 양방향에 동시에 걸려 제어가 부정확). `sudo modprobe sch_netem` 필요.
- 손실 모델: **없음(대조군)**, **균등**(0.5/1/2/5%), **링크 단절**(개정 3: 양방향을 동시에 일정 시간 끊음, `netlab.sh --outage 평균주기:길이` — Wi-Fi 끊김·망 전환 근사). 지연: 단방향 0 / 20 ms.
  - 개정 2의 **버스트**(Gilbert-Elliott, 패킷 단위)는 veth 하나를 모든 연결이 공유해 버스트가 연결마다 흩어졌고, 실측도 균등 손실과 같았다. 그래서 판정에서 뺐다.
  - 단절은 어떤 전송도 줄일 수 없는 정지(단절 길이)를 포함한다. 따라서 단절 조건의 초과 AoI 배수는 균등 손실보다 작게 나오는 것이 정상이다. 판정 기준은 사전 등록값 그대로 두고, 해석에서 이 바닥값을 따로 밝힌다.
- 비용 측정: 손실 주입에는 오프로드(TSO/GSO/GRO)를 꺼야 하지만(netem이 묶음 단위로 버리지 않게), 실서버는 오프로드를 켠다. 그래서 G5는 **손실 없는 조건에서 오프로드를 켜고**(`NETLAB_OFFLOAD=on`) 따로 잰다.
- 20 Hz 스트림에서 TCP는 빠른 재전송에 필요한 중복 ACK가 늦게 모여 TLP/RTO에 의존할 가능성이 높다 — 비신뢰 전송에 유리한 요인이므로 대조군과 버스트 손실로 비교의 공정성을 지킨다.

### 7.3 비용 비교의 기준선 (G5)

- QUIC은 구조상 게이트웨이가 암호화한다. 공정 비교 기준은 **게이트웨이가 TLS를 처리하는 WSS**다(연결마다 암호화한다는 점에서 같은 조건).
- 참고값: **LB가 TLS를 종단하는 평문 WS**(한 번 프레이밍한 바이트를 N개 소켓에 그대로 쓸 수 있는, WebSocket에 가장 유리한 배포). 이 차이는 판정에 쓰지 않고 공개한다.
- 비신뢰 전송 측은 **틱 묶음 전송(§4.2)과 GSO**를 켠 상태로 측정한다.
- 시나리오: 대표(10명 룸 × 다수, 60 Hz)와 큰 방(1,000명 룸, 20 Hz).

### 7.4 작업 계획

**판정 칸과 동일 설정 원칙** (ADR 0004 관문): G1은 단방향 20 ms·60 Hz·균등 1%와 버스트(평균 1%, 길이 4)에서 판정, 지연 0은 참고값. 모든 실험은 틱 주기 {즉시, 60 Hz, 30 Hz} 각각에서 **같은 실행으로** AoI와 비용을 함께 기록하고, G1과 G5를 동시에 만족하는 틱이 있는지로 판정한다.

**우선순위**: 판정의 대부분은 G1·G5가 정하므로 **V1~V5를 먼저** 끝내고, V6(MoQ)은 **시간 상한 3일**(Rust MoQ 클라이언트를 부하 생성기에 붙여야 해 무거울 수 있다 — 상한 안에 못 끝내면 정성 평가로 대체).

**1주차 — 측정 장치와 스파이크**

| # | 작업 | 산출물 |
|---|---|---|
| V1 ✅ | `bench/netlab.sh` — 비특권 user namespace 안의 서버·클라이언트 netns + veth, 방향별 netem(`none` / `delay` / `uniform` / `burst`), offload 끔. root 불필요 | 스크립트 + 적용 확인: 단방향 20 ms → RTT 약 40 ms, 2% 균등·버스트 → ping 1,000회 1.8%·1.6% |
| V2 ✅ | `spikes/wt-hotlane/`(워크스페이스 밖): 같은 룸 팬아웃·틱 묶음 로직의 **최소 릴레이 3종** — WS(평문), WSS(rustls), WT datagram(wtransport, GSO 켬). 공정 비교를 위해 대칭 구현 | `relay` 바이너리, 자체 서명 인증서 + 해시 |
| V3 ✅ | 부하 생성기 `statesync`: `--transport ws\|wss\|wt`, 프로브 `seq`, **60 Hz AoI 표본·끊김 비율**, 서버 CPU/RSS 수집 | `wsprism-loadgen` 확장 |

**2주차 — 실험**

| # | 작업 | 산출물 |
|---|---|---|
| V4 | 매트릭스: 손실 모델 {없음, 균등 0.5/1/2/5%, 버스트} × 지연 {0, 20 ms} × {20, 60} Hz × 틱 {즉시, 60, 30 Hz}, 대표 시나리오, 각 3회. 참고값으로 `--fifo`(현재 게이트웨이 큐잉 근사) | `bench/results/v-*.jsonl`, BENCHMARKS.md 「V」 절 (**G1**) |
| V5 | 비용: §7.3의 기준선 3종 × 대표·큰 방 시나리오 | BENCHMARKS.md 「비용」 절 (**G5**) |
| V6 | **MoQ 평가**: moq-rs 릴레이에 대표 시나리오를 얹는다(엔티티별 track, 틱마다 새 group). AoI·끊김 비율·CPU를 같은 조건으로 측정, 테넌트 인증·정책을 얹을 수 있는 지점 조사 | 「직접 구현 vs MoQ 위 vs MoQ 호환」 판단 메모 |
| V7 | 브라우저 확인 페이지: `serverCertificateHashes`로 접속, datagram 왕복. Windows의 Chrome·Edge·Firefox, Mac의 Safari 수동 확인(Safari가 해시 방식을 거부하면 mkcert 인증서로 재시도) | 체크리스트 결과 (**G2**) |

**3주차 — 안정성, 폴백, 결정**

| # | 작업 | 산출물 |
|---|---|---|
| V8 | 소크: 세션 1,000개 × 1시간, 60 Hz. RSS·패닉·에러율 | **G3** |
| V9 | 폴백: 클라이언트 namespace에서 UDP 차단(`iptables`) 상태로 부하 생성기·브라우저 페이지가 2초 안에 WS 전환 | **G4** |
| V10 | 결과 정리: ADR 0004 상태 갱신(확정 / 하이브리드 / 전환), ROADMAP 확정 | ADR·ROADMAP 갱신 |

**조건부 (G1 실패 시에만)**: 노이즈 이웃 격리 측정 — wsPrism·Sockudo·Centrifugo. 부하 생성기에 Pusher 프로토콜 어댑터와 private 채널 인증 엔드포인트(클라이언트 이벤트는 private/presence 채널에서만 가능), Centrifugo 프로토콜 어댑터가 필요하다(수일 분량). 전환 방향의 근거로만 쓰이므로 V 본 일정에서 뺀다.

### 7.5 공정성·한계 메모

- 스파이크 릴레이는 정책·메트릭이 없는 최소 구현이다. 판정은 **같은 최소 구현끼리의 비교**로 하고, 실제 게이트웨이 WS는 참고값이다.
- WSL2에서 측정한다. 절대값보다 같은 조건의 상대 비교가 판정 근거다. 결과가 경계선(G1 2~4배, G5 1.5~2.5배)이면 리눅스 실기기에서 재측정한다.
- 손실 0%에서 차이가 작을 것으로 예상하며, 그 결과도 그대로 공개한다.

## 8. 열린 질문과 위험

| 질문 | 확인 시점 |
|---|---|
| Safari가 `serverCertificateHashes`를 지원하는가 | V7 |
| 기업망·공용 Wi-Fi의 UDP 차단 비율 (폴백 빈도) | T 이후 실사용 계측 |
| QUIC의 CPU 비용이 처리량 목표를 깎는가 | V4·V5 (CPU/메시지) |
| UDP 로드밸런서 뒤에서 클라이언트 IP(PROXY protocol v2) | T |
| QUIC connection migration이 세션 식별·레이트리밋에 주는 영향 | T |
| `wtransport` 1.0 이전 API 변경 위험 | 라이브러리 경계를 `WtIo` 한 곳에 가둬 완화 |
| 직접 구현 vs MoQ 위 vs MoQ 호환 — MoQ의 "최신 그룹"이 키별 상태에 맞는가, 테넌트 정책을 얹을 수 있는가, Cloudflare 릴레이의 무료·가용성 조건 | V6 |
| 서버 피어의 입력 채널을 신뢰/비신뢰 중 무엇으로 기본값을 둘 것인가, 서버 피어 1개당 룸 수 한도 | T |
| 관심 영역 이탈의 순서 규약(Reliable 제거 ↔ 늦게 도착한 datagram), 지정 송신의 수신자 수 상한 | T0(타입), T(규약) |
| QUIC 패킷당 비용을 틱 묶음 + GSO로 G5 안에 넣을 수 있는가 | V5 |

## 9. 작업 순서 (V 통과 후)

**T0 — 경계 (WebSocket만, M2-lite보다 먼저)**: 검증 결과와 무관하게 WebSocket 사용자에게도 이득(큐 적체 감소)이라 버려지지 않는다.

1. `SessionIo` 추출(능력: 순서 스트림 / 비신뢰 datagram, WebSocket은 datagram 능력 없음 → 스트림으로 대체) — 동작 변화 없이 리팩터링, 기존 테스트가 안전망.
2. 송신 단위에 전달 등급: `OutFrame::Hot { key, bytes }`, `RealtimeCore::deliver`의 Lossy 바이너리 → Hot 슬롯.
3. Hot 슬롯(키별 최신값 + 짧은 FIFO)과 틱 묶음 전송 — WebSocket에서 먼저. 지표: 큐 적체·드롭·AoI.
4. Hot 프레임 헤더 확장(KEY/SENDER), 연결별 `member_id`, `HotRelayService`, 크기 상한(PROTOCOL.md 명시).
5. **완료 조건**: `SessionIo`·전달 등급이 브라우저 세션을 전제하지 않음을 확인(서버 피어 §4.6을 막지 않을 것). 코어 전달 API가 `Audience`(룸·멤버 지정·세션·사용자) 하나로 모여, 멤버 지정 Hot 송신이 새 코드 경로 없이 표현될 것.

**T — 비신뢰 전송 (V6 결정에 따라 WebTransport 직접 구현 또는 MoQ)**

6. `WtIo`(또는 MoQ 어댑터) + 설정 + admission 연결 + `transport` 메트릭 라벨.
7. **서버 피어**(§4.6): 역할, 신뢰 장기 연결, 입력 수신·권위 상태 발행, `hot_peer_relay` 정책.
8. 1회용 티켓 경로(M2-lite의 발급 API 사용).
9. TS SDK의 비신뢰 전송 우선·WS 폴백, `(sender, key, seq)` 신선도 필터.
10. 통합 테스트(WT·서버 피어), 퍼징 대상 추가(제어 스트림·묶음 프레이밍), 벤치마크 시나리오 표준화(V의 매트릭스를 CI 외 정기 실행).
