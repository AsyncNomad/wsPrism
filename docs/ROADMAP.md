# wsPrism Roadmap

wsPrism을 **상용 서비스에서 쉽게 커스텀해 쓸 수 있고, 기존 실시간 게이트웨이보다 측정 가능하게 빠른** 대기업급 소프트웨어로 만들기 위한 계획이다.
결함 수정 기록은 [`HARDENING.md`](HARDENING.md), 클라이언트 계약은 [`PROTOCOL.md`](PROTOCOL.md), 설계 결정은 [`adr/`](adr/README.md)에 있다.

- 작성: 2026-10-14. 코드 리뷰의 상용화 제안을 바탕으로 했고, 이후 경쟁 분석에 따라 방향을 정했다([ADR 0004](adr/0004-product-direction.md)).
- 이 문서는 살아 있는 계획이다. 단계가 끝나거나 방향이 바뀌면 "현재 위치"와 해당 단계를 갱신한다.

## 포지션 (ADR 0004, 검증 V 후 확정)

> **웹·게임을 위한 실시간 상태 동기화 — 키별 최신값·틱 단위 전송·테넌트 정책을 가장 좋은 비신뢰 전송(WebTransport datagram / MoQ) 위에서, 안 되면 WebSocket 폴백. 대기업 수준의 안정성.**

- **간판**: 전송이 아니라 **상태 동기화 의미론**. "WebTransport로 pub/sub"은 MoQ와 표준 인프라가 채우는 자리라 그 자체로는 차별점이 아니다. 손실이 있는 네트워크에서 상태 신선도(AoI)의 꼬리와 끊김을 TCP 기반 게이트웨이보다 크게 줄인다(검증 V: 균등 손실 초과 AoI 6~16× 감소, 링크 복구 후 추가 지연 107~156 → 0~19 ms). 비용은 연결당 메모리 약 2.8×라서 **하이브리드**로 쓴다: 소규모 인터랙티브 룸은 비신뢰 전송, 대규모 방송 룸은 WebSocket(룸 정책). 설계: [Hot Lane over WebTransport](design/hot-lane-over-webtransport.md).
- **대표 시나리오**: 모바일·Wi-Fi 환경의 20~60 Hz 웹 멀티플레이 상태 동기화. 특정 산업 특화는 목표가 아니다.
- **포지션**: 어느 언어로 짠 게임·실시간 서버든, 그 앞에서 브라우저 연결·정책·팬아웃·datagram을 떠맡는 **사이드카**. 권위 게임 서버는 **서버 피어**로 붙는다(설계 §4.6). 엔진에 묶인 lightyear·aeronet과 달리 언어 무관.
- **주장 방식**: 개별 요소는 모두 어딘가에 있다(Centrifugo WT는 실험 단계·스트림 하나, Sockudo는 datagram 없음, lightyear는 Bevy 전용, MoQ는 범용 릴레이). **조합 + 언어 무관 + 측정된 수치**로만 주장한다.
- **기반**: 멀티테넌트 정책 엔진(admission 파이프라인, allowlist, 테넌트 한도, 세션 정책, Origin, 티켓 경로). 대기업급 요구사항의 핵심이지만 한 줄 소개는 아니다.
- **대상 트래픽**: 최신 상태가 이기는 웹·게임 데이터. 게임 백엔드는 만들지 않는다(사용자 백엔드 앞의 전송 계층).
- **따라잡기 기능**(프록시, 발행 API, 이력·복구, 멀티 노드)은 "갈아탈 때 막히지 않을 만큼"만.

## 현재 위치

| 항목 | 상태 |
|---|---|
| 연결 앞단 | admission 파이프라인, Origin·신뢰 프록시·티켓 경로, 프로토콜 v1·v2(에러·종료 코드, `reply_to`, 버전 협상), 배포 전 전체 리뷰 반영([HARDENING](HARDENING.md)). Rust 테스트 약 270개 + JS·카오스 테스트, 퍼징 5종, cargo-deny |
| 전송 | WebSocket만. Hot Lane의 Lossy는 서버 큐에서만 성립(네트워크 구간은 TCP). WebTransport는 단계 T |
| 백엔드 연동 | 인증 체인(1회용 티켓, JWT·JWKS, connect 프록시, 플러그인), 서버 API(발행·티켓·강제 종료·presence), RPC 프록시, JS SDK, 예제 앱 |
| 확장 지점 | `Gateway::builder`로 서비스·인증기 추가(파사드 `wsprism`, [API.md](API.md)) |
| 이력·확장성 | 룸 이력과 재접속 복구. Redis 클러스터(**실험적**, 고가용성 + 제한적 확장): [CLUSTER.md](CLUSTER.md) |
| 운영 | JSON 로그, `${VAR}` 설정, Prometheus, 무손실 드레인, Docker 이미지: [OPERATIONS.md](OPERATIONS.md) |
| 성능 | [BENCHMARKS](BENCHMARKS.md). 경쟁 비교(compete-v2, 이력 켠 A14): Centrifugo 대비 연결당 메모리 2.6~4배 적음(부하 중 작은 룸; 유휴 5만 연결에서는 프로세스 약 1.9배, 커널 포함 서버 전체 약 1.7배), 저·중부하 CPU 1.24~1.5배 적음, 216만 전달/s에서 p99 6~9 ms(Centrifugo는 이력을 켜면 포화). 클러스터(같은 Redis): 이력 처리량 약 1.5~1.7배, 12만 발행/s에서 wsPrism 전부 전달 대 Centrifugo 81%(p50 약 3초). 같은 장치 측정 |

(2026-10-21 기준) **M0~M5 완료, M6은 0.1.0 범위 완료. 검증 V: G1·G3·G4 통과, G5 실패 → "하이브리드", G2(브라우저) 남음. 다음: 0.1.0 출시 전 점검과 게시(사용자).**

## 목표를 측정 가능하게

### 성능

| | 내용 |
|---|---|
| 비교 대상 | **Sockudo**(Rust, 멀티테넌트 앱), Centrifugo(기능 기준선, WT 실험 단계), **MoQ**(moq-rs / Cloudflare 릴레이), **Nakama relayed**, Colyseus, **lightyear·aeronet**(Bevy, WT datagram), geckos.io(브라우저 UDP via WebRTC), uWS 기반 soketi(원시 처리량), Socket.IO(채택률). Ably conflation은 수요 근거 |
| 승부 지표 | **초과 AoI p99**(손실 0% 바닥값 대비 증가분)와 **끊김 비율** — 판정 칸: 단방향 20 ms·60 Hz·균등 1%와 버스트 둘 다. 메시지당 지연은 보조(생존 편향). AoI와 비용은 **같은 설정(틱 주기)** 에서 판정 |
| 비용 지표 | **CPU/메시지, 연결당 메모리** — WSS(게이트웨이 TLS) 대비 2배 이내가 관문(G5). LB TLS 종단 WS와의 차이도 공개 |
| 공개 지표 | 팬아웃 p50/p99/p999, 코어당 처리량, 노이즈 이웃 격리(정책 방향 전환 시) |
| 원칙 | 수치 없이 성능을 주장하지 않는다. 같은 언어(Rust) 경쟁자와 TCP 위에서 겨뤄서는 "훨씬"을 만들 구조적 근거가 없다 — 구조적 차이(datagram)에서 이긴다 |

### 커스텀 용이성: 확장 계층

| 계층 | 대상 | 방식 |
|---|---|---|
| ① 설정 | 운영자 | YAML, `--check-config`, `${VAR}`, 핫 리로드(0.1.0 이후) |
| ② 백엔드 프록시 | **대부분의 팀(언어 무관)** | connect / rpc를 백엔드로 위임(HTTP → gRPC), 서버 발행 API, 1회용 티켓 발급 |
| ③ Rust 트레이트 | 성능이 필요한 팀 | `Authenticator`, `TextService`·`HotService`, 이후 `Broker` → `Gateway::builder`로 조립 (✅ ADR 0009) |
| ④ WASM 플러그인 | SaaS 테넌트별 커스텀 | Ext Lane 전용, 샌드박스. Hot Lane에는 넣지 않는다 |

## 제품 구조

```
wsprism-protocol  와이어 계약 (전송·런타임 무관)
wsprism-engine    룸·전달·Hot 상태·서비스 트레이트 (axum·HTTP·설정 형식 무관)
wsprism-redis     클러스터 Broker(Redis): 노드 간 룸·이력·세션·presence·티켓 공유
wsprism-gateway   서버: 설정·핸드셰이크·전송·인증(JWT·티켓·프록시·플러그인)·서버 API + 바이너리
wsprism           파사드: 안정 API + GatewayBuilder (사용자가 의존하는 크레이트)
sdk/js            클라이언트 SDK (JS 완료 → Unity C# → Dart). WebTransport 우선, WebSocket 폴백
```

(ADR 0009로 확정: 처음 구상한 `wsprism-server`는 `wsprism-gateway`가 맡고, 사용자 진입점은 파사드 `wsprism`의 `Gateway::builder`.)

## 단계

| 단계 | 내용 | 완료 기준 |
|---|---|---|
| **M0 정리** ✅ | 테넌트 조회 `HashMap`, `run_session` 분해, 룸 기능을 서비스로 + 세션 컨텍스트 `&mut`([ADR 0001](adr/0001-session-context-and-room-service.md)), 전송 무관 `OutFrame`([ADR 0002](adr/0002-transport-agnostic-out-frame.md)) | 동작 변화 없이 기존 테스트 통과 |
| **M1 정직한 기반** ✅ | admission 파이프라인, Origin 허용 목록, 신뢰 프록시 + XFF, 티켓 경로 + 버전 협상([ADR 0003](adr/0003-ticket-sources-and-subprotocol.md)), 에러 코드 목록·정보 노출 제거, `reply_to`, `--config`/`--check-config`, 문서 정합, 퍼징 5종, cargo-deny(`serde_yaml` 교체), 벤치마크 기준선(TCP_NODELAY 수정) | 기준선 수치 문서화, 퍼징·감사 CI |
| **V 차별화 검증** (약 3주) | ✅ V1 netlab(비특권, root 불필요) ✅ V2 스파이크 릴레이(WS·WSS·WT) ✅ V3 `loadgen statesync`(AoI·끊김·CPU·RSS). 남은 것: V4 매트릭스(틱 {즉시, 60, 30} 포함)·V5 비용 → G1·G5 우선, V6 MoQ(상한 3일), V7 브라우저, V8 소크, V9 폴백. 노이즈 이웃 격리는 G1 실패 시에만. 세부: [설계 §7](design/hot-lane-over-webtransport.md#7-검증-단계-v-약-3주) | 관문 G1~G5 → ADR 0004 확정 / 하이브리드 / 전환 |
| **T0 경계** | `SessionIo`(전송 능력), 전달 등급(`OutFrame::Hot`), Hot 슬롯(`(sender, svc, key)` 최신값, 상한) + 틱 묶음 전송, Hot 헤더 확장·연결별 `member_id`·크기 상한 — **WebSocket만으로, M2 공개 트레이트 전에**. 서버 피어 경로를 막지 않을 것. 세부: [설계 §9](design/hot-lane-over-webtransport.md#9-작업-순서-v-통과-후) | 동작 변화 없는 리팩터링 + WS에서 큐 적체 감소 측정 |
| **M2-lite 끼워 쓰기** | engine/server crate 분리, `Authenticator` 트레이트 + JWT(테넌트별 JWKS) + **1회용 티켓 발급 API**, HTTP 백엔드 프록시(connect·rpc), **서버 발행 API**(`/api/v1/publish`, `/api/v1/disconnect`), 최소 TS SDK(WS). gRPC·다양한 플러그인은 뒤로 | Node 또는 Spring 백엔드에 붙인 예제 앱 — **충족 (2026-10-16)**: [ADR 0008](adr/0008-m2-lite-integration.md) M2-1~M2-8, `examples/chat-and-state`. M2-9 크레이트 분리 ✅([ADR 0009](adr/0009-crate-split-and-public-api.md)), JWKS ✅([ADR 0012](adr/0012-jwks-and-token-refresh.md)) |
| **T 비신뢰 전송 + 서버 피어** | V6 결정에 따라 WebTransport 직접 구현(`WtIo`) 또는 MoQ 경로, **서버 피어**(권위 게임 서버의 신뢰 장기 연결: 입력 수신·권위 상태 발행), `transport` 메트릭 라벨, 1회용 티켓 경로, SDK의 비신뢰 우선·WS 폴백 | 같은 서비스 코드가 혼합 전송 룸에서 동작, V의 이득이 제품 경로에서 재현 |
| **M3 신뢰성 (동등 수준)** ✅ | 채널 일반화, 채널별 이력 + offset/epoch + 재접속 복구, ACK. Hot Lane은 이력 없음 | 끊긴 사이 메시지 복구 테스트 — 충족 (2026-10-20, [ADR 0011](adr/0011-broker-history-and-cluster.md) R2·R3-2. ACK는 없음: offset 기반 복구) |
| **M4 멀티 노드 (동등 수준)** ✅ | Broker(Redis 먼저), 노드 로컬 구독 색인, 클러스터 강제 퇴장, 1회용 티켓 공유 저장소, 전역 한도의 정확/근사 결정 | 노드 강제 종료 카오스 테스트 — 충족 (2026-10-20, ADR 0011 R3~R5) |
| **M5 성능 공개** ✅ | 메시지당 할당 제거(메트릭 라벨, 팬아웃), ID 인터닝, 프레임 사전 인코딩, 표준 메트릭 crate. 비교 대상 전체와 공개 벤치마크 | 재현 가능한 벤치 리포와 결과 — 충족 (2026-10-21, BENCHMARKS 「M5」. ID 인터닝·표준 메트릭 crate는 우선순위가 낮아 하지 않음. 전용 호스트 측정은 아직) |
| **M6 운영** | 환경변수 오버라이드, 핫 리로드(`ArcSwap`), OpenTelemetry, JSON 로그·마스킹, 관리 API(RBAC), Docker/Helm, 재접속 지터, 메트릭 단위 표준화, UDP 로드밸런서 가이드 | 롤링 업데이트 가이드대로 무중단 배포 — **0.1.0 범위 충족 (2026-10-21)**: 카오스 테스트로 검증(docs/CLUSTER.md "Rolling update"). 핫 리로드·OpenTelemetry·RBAC·Helm·UDP 가이드는 0.1.0 이후 |
| **0.1.x·0.2 견고화** ✅ | 무작위 장애 soak(노드·링크·Redis 장애, 유실·중복 판정기), Redis TLS(사설 CA·상호 TLS)·Sentinel·Sentinel+TLS, 정식 펜싱(Redis가 슬롯 없는 노드의 발행 거부), 1회용 티켓의 복제 확인(`replica_ack`), 과부하 보호(Redis 지연에 따른 접속 거절), 구독 묶음(새 룸 join 8~10배), 테넌트별 송신 큐, 출시 전 전수 결함 탐색(F1~F5) | 카오스·soak 위반 0, 2코어 CI 통과 — **충족 (2026-10-23)**: HARDENING 「0.2」「출시 전 전수 결함 탐색」, BENCHMARKS 「세션 churn」. 이력 샤딩(ADR 0013)은 설계만 채택하고 보류 |
| **M7 채택** | SDK 확장(Unity C#, Dart), semver·CHANGELOG·MSRV, `SECURITY.md`·`CONTRIBUTING.md`, 예제(멀티플레이·협업 커서), 소크·카오스, 외부 보안 감사 | 1.0 릴리스 |

결정 분기(ADR 0004): **G5만 실패** → 하이브리드(소규모 인터랙티브 룸은 비신뢰 전송, 대규모 방송은 WS — 룸 정책으로 선택). **G1 실패** → T를 빼고 상태 동기화 의미론은 WS에서 유지, 정책 중심(측정된 격리, 스키마 검증, WASM 정책)으로 재구성. **둘 다 실패** → 범위를 좁힌 정책 중심 또는 임베드 라이브러리 포지션.

## 리뷰 제안에서 바꾼 점과 이유

1. **벤치마크를 M1에서 시작했다**(리뷰에서는 4단계). 설계 결정마다 비용을 재야 비싼 구조가 공개 API로 굳지 않는다.
2. **엔진 API를 굳히기 전에 전송 계층을 분리했다**(ADR 0002). WebTransport를 붙일 수 있는 토대가 됐다.
3. **차별화 검증(V)과 경계 작업(T0)을 M2 앞에 넣었다.** 따라잡기 기능은 끝내도 경쟁 제품과 같아질 뿐이라 차별점을 먼저 숫자로 증명하고, M2 공개 트레이트가 "순서 있는 바이트 채널 하나"를 전제로 굳지 않게 전송 능력·전달 등급 경계를 먼저 연다.
4. **최소 TS SDK를 M2-lite에서 만든다.** 프로토콜(재접속, `reply_to`, WT 폴백)은 실제 클라이언트로 검증해야 결함이 드러난다.
5. **SDK 순서는 TS → Unity C# → Dart.**

## 코드 밖에서 필요한 것

- **호환성 약속**: semver, 폐기 정책, 프로토콜 버전 협상. 클라이언트 SDK는 서버보다 느리게 업데이트된다.
- **결정 기록**: 설계 결정은 [`adr/`](adr/README.md), 설계 문서는 [`design/`](design/), 결함 수정은 `HARDENING.md`.
- **보안 대응 체계**: 제보 경로, 패치 SLA, 의존성 감사.
- **테넌트 계량**: 연결 수·메시지 수를 SaaS 과금 근거로 남긴다.

## 작업 원칙

- 결함 수정은 재현 테스트 먼저. 수정을 빼면 테스트가 실패하는지 확인한다.
- 동작 변화와 리팩터링은 커밋을 섞지 않는다. 커밋 하나에 수정 하나.
- 게이트: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --locked`, `cargo deny check`. 통합 테스트는 반복 실행으로 결과가 흔들리지 않는지 확인한다.
- 클라이언트에 보이는 변화는 `PROTOCOL.md`에, 설계 결정과 기각한 대안은 ADR에 남긴다.
- **기능을 덧붙이지 않는다.** 새 기능은 올바른 경계(트레이트, 모듈, 레지스트리, 설정)를 먼저 만들고 거기에 끼운다. 코어 경로에 `if svc == "x"` 같은 특수 처리를 넣지 않는다. 불가피하면 명시하고 후속 과제로 기록한다.
- **스파이크 코드는 제품 코드가 아니다.** 검증용 코드는 워크스페이스 밖(`spikes/`)에 두고 버린다. 측정 장치만 남긴다.
