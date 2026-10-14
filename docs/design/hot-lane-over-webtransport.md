# 설계: Hot Lane over WebTransport

- 상태: 초안, **개정 1** (2026-10-14: 2차 리뷰 반영 — AoI 주 지표, 비용 관문 G5, MoQ 평가, T0 분리, netns 측정). 결정 근거는 [ADR 0004](../adr/0004-product-direction.md).
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
- **Hot 슬롯**: 세션마다 작은 "최신값 맵" `(svc_id, key) → Bytes` + 키 없는 프레임용 짧은 FIFO(기본 64). 발행자는 슬롯에 넣기만 하고(덮어쓰기, 블로킹 없음), 세션 루프가 비울 때마다 `send_unreliable`로 보낸다. 깊은 큐가 없으므로 오래된 상태가 쌓이지 않는다.
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
| **상태 신선도 AoI** | 수신자가 **60 Hz 렌더 틱**마다 `지금 - (보유한 가장 새로운 상태의 생성 시각)`을 기록. 유실은 AoI를 키우는 쪽으로 반영되므로 전송 간 공정 | **주 지표** (G1: p99) |
| **끊김 비율** | AoI > 100 ms인 틱의 비율 | 주 지표 (G1), 버스트 손실에 민감 |
| 메시지당 지연 | 송신 → 수신 (도착한 메시지만) | 보조 — 유실 패킷이 빠지는 생존 편향이 있어 판정에 쓰지 않는다 |
| 전달률 | 받은 프로브 / (보낸 프로브 × 룸 인원) | 보조 |
| **CPU/메시지** | 서버 CPU 시간(`/proc/<pid>/stat`) / 전달 메시지 수 | **G5** |
| **연결당 메모리** | (RSS − 기준) / 연결 수 | **G5** |

### 7.2 네트워크 에뮬레이션

- **network namespace + veth 쌍**으로 서버와 클라이언트를 분리하고, 각 veth에 **방향별로** netem을 건다(루프백 netem은 양방향에 동시에 걸려 제어가 부정확). `sudo modprobe sch_netem` 필요.
- 손실 모델: **없음(대조군)**, **균등**(0.5/1/2/5%), **버스트**(Gilbert-Elliott, 평균 손실 1%·평균 버스트 길이 약 4패킷 — 혼잡한 Wi-Fi 근사). 지연: 단방향 0 / 20 ms.
- 20 Hz 스트림에서 TCP는 빠른 재전송에 필요한 중복 ACK가 늦게 모여 TLP/RTO에 의존할 가능성이 높다 — 비신뢰 전송에 유리한 요인이므로 대조군과 버스트 손실로 비교의 공정성을 지킨다.

### 7.3 비용 비교의 기준선 (G5)

- QUIC은 구조상 게이트웨이가 암호화한다. 공정 비교 기준은 **게이트웨이가 TLS를 처리하는 WSS**다(연결마다 암호화한다는 점에서 같은 조건).
- 참고값: **LB가 TLS를 종단하는 평문 WS**(한 번 프레이밍한 바이트를 N개 소켓에 그대로 쓸 수 있는, WebSocket에 가장 유리한 배포). 이 차이는 판정에 쓰지 않고 공개한다.
- 비신뢰 전송 측은 **틱 묶음 전송(§4.2)과 GSO**를 켠 상태로 측정한다.
- 시나리오: 대표(10명 룸 × 다수, 60 Hz)와 큰 방(1,000명 룸, 20 Hz).

### 7.4 작업 계획

**1주차 — 측정 장치와 스파이크**

| # | 작업 | 산출물 |
|---|---|---|
| V1 | `bench/netns.sh up\|down` (서버·클라이언트 namespace + veth), `bench/netem.sh <dir> none\|uniform <p>\|burst <p> <len> [delay_ms]` | 스크립트 + 적용 확인(ping RTT·손실 방향별) |
| V2 | `spikes/wt-hotlane/`(워크스페이스 밖): 같은 룸 팬아웃·틱 묶음 로직의 **최소 릴레이 3종** — WS(평문), WSS(rustls), WT datagram(wtransport, GSO 켬). 공정 비교를 위해 대칭 구현 | `relay` 바이너리, 자체 서명 인증서 + 해시 |
| V3 | 부하 생성기: `--transport ws\|wss\|wt`, 프로브 `seq`, **60 Hz AoI 표본·끊김 비율**, 서버 CPU/RSS 수집 | `wsprism-loadgen` 확장 |

**2주차 — 실험**

| # | 작업 | 산출물 |
|---|---|---|
| V4 | 매트릭스: 손실 모델 {없음, 균등 0.5/1/2/5%, 버스트} × 지연 {0, 20 ms} × {20, 60} Hz, 대표 시나리오, 각 3회. 참고값으로 실제 게이트웨이 WS | `bench/results/v-*.jsonl`, BENCHMARKS.md 「V」 절 (**G1**) |
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
| Safari가 `serverCertificateHashes`를 지원하는가 | V5 |
| 기업망·공용 Wi-Fi의 UDP 차단 비율 (폴백 빈도) | T 이후 실사용 계측 |
| QUIC의 CPU 비용이 처리량 목표를 깎는가 | V4 (CPU/메시지) |
| UDP 로드밸런서 뒤에서 클라이언트 IP(PROXY protocol v2) | T |
| QUIC connection migration이 세션 식별·레이트리밋에 주는 영향 | T |
| `wtransport` 1.0 이전 API 변경 위험 | 라이브러리 경계를 `WtIo` 한 곳에 가둬 완화 |
| 직접 구현 vs MoQ 위 vs MoQ 호환 — MoQ의 "최신 그룹"이 키별 상태에 맞는가, 테넌트 정책을 얹을 수 있는가, Cloudflare 릴레이의 무료·가용성 조건 | V6 |
| QUIC 패킷당 비용을 틱 묶음 + GSO로 G5 안에 넣을 수 있는가 | V5 |

## 9. 작업 순서 (V 통과 후)

**T0 — 경계 (WebSocket만, M2-lite보다 먼저)**: 검증 결과와 무관하게 WebSocket 사용자에게도 이득(큐 적체 감소)이라 버려지지 않는다.

1. `SessionIo` 추출(능력: 순서 스트림 / 비신뢰 datagram, WebSocket은 datagram 능력 없음 → 스트림으로 대체) — 동작 변화 없이 리팩터링, 기존 테스트가 안전망.
2. 송신 단위에 전달 등급: `OutFrame::Hot { key, bytes }`, `RealtimeCore::deliver`의 Lossy 바이너리 → Hot 슬롯.
3. Hot 슬롯(키별 최신값 + 짧은 FIFO)과 틱 묶음 전송 — WebSocket에서 먼저. 지표: 큐 적체·드롭·AoI.
4. Hot 프레임 헤더 확장(KEY/SENDER), `member_id`, `HotRelayService`, 크기 상한(PROTOCOL.md 명시).

**T — 비신뢰 전송 (V6 결정에 따라 WebTransport 직접 구현 또는 MoQ)**

5. `WtIo`(또는 MoQ 어댑터) + 설정 + admission 연결 + `transport` 메트릭 라벨.
6. 1회용 티켓 경로(M2-lite의 발급 API 사용).
7. TS SDK의 비신뢰 전송 우선·WS 폴백, `(sender, key, seq)` 신선도 필터.
8. 통합 테스트(WT 클라이언트), 퍼징 대상 추가(제어 스트림·묶음 프레이밍), 벤치마크 시나리오 표준화(V의 매트릭스를 CI 외 정기 실행).
