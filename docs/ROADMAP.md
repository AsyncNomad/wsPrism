# wsPrism Roadmap

wsPrism을 **상용 서비스에서 쉽게 커스텀해 쓸 수 있고, 기존 실시간 게이트웨이보다 측정 가능하게 빠른** 대기업급 소프트웨어로 만들기 위한 계획이다.
결함 수정 기록은 [`HARDENING.md`](HARDENING.md), 클라이언트 계약은 [`PROTOCOL.md`](PROTOCOL.md), 설계 결정은 [`adr/`](adr/README.md)에 있다.

- 작성: 2026-10-14. 코드 리뷰의 상용화 제안을 바탕으로 했고, 이후 경쟁 분석에 따라 방향을 정했다([ADR 0004](adr/0004-product-direction.md)).
- 이 문서는 살아 있는 계획이다. 단계가 끝나거나 방향이 바뀌면 "현재 위치"와 해당 단계를 갱신한다.

## 포지션 (ADR 0004, 검증 V 후 확정)

> **브라우저에서 진짜 UDP처럼 동작하는 실시간 상태 동기화 — WebTransport datagram, 안 되면 WebSocket 폴백 — 그리고 테넌트별 정책.**

- **간판**: Hot Lane을 비신뢰·비순서 전달 클래스로 만들어, 패킷 손실이 있는 네트워크에서 상태 업데이트의 꼬리 지연과 신선도를 TCP 기반 게이트웨이보다 크게 개선한다. 설계: [Hot Lane over WebTransport](design/hot-lane-over-webtransport.md).
- **기반**: 멀티테넌트 정책 엔진(admission 파이프라인, allowlist, 테넌트 한도, 세션 정책, Origin, 티켓 경로). 대기업급 요구사항의 핵심이지만 한 줄 소개는 아니다.
- **대상 트래픽**: 최신 상태가 이기는 데이터 — 웹 멀티플레이, 협업 커서·화이트보드, 시세·스포츠 틱, IoT·원격 대시보드. 게임 백엔드는 만들지 않는다(사용자 백엔드 앞의 전송 계층).
- **따라잡기 기능**(프록시, 발행 API, 이력·복구, 멀티 노드)은 "갈아탈 때 막히지 않을 만큼"만.

## 현재 위치

| 항목 | 상태 |
|---|---|
| 연결 앞단 | 탄탄함. 보안·정확성 결함 15건 수정(`hardening`), admission 파이프라인, Origin·신뢰 프록시·티켓 경로, 프로토콜 v1(에러 코드, `reply_to`, 버전 협상). 테스트 79개, 퍼징 5종, cargo-deny |
| 전송 | WebSocket만. Hot Lane의 Lossy는 서버 큐에서만 성립(네트워크 구간은 TCP) |
| 백엔드 연동 | 없음. 서비스는 Rust로 함께 컴파일, 서버 발행 API 없음 |
| 확장 지점 | 서비스는 `AppState::new`에서 직접 등록, 인증은 티켓 `dev`만 통과하는 스텁 |
| 확장성 | 단일 노드 |
| 성능 | 기준선 측정 완료([BENCHMARKS](BENCHMARKS.md)). TCP_NODELAY 수정으로 p99.9 26.7→3.6 ms. 메시지당 최대 비용은 메트릭(275 ns)과 팬아웃(수신자당 ~200 ns) |

**M0, M1 완료 (브랜치 `m0/engine-prep`). 다음: 검증 단계 V.**

## 목표를 측정 가능하게

### 성능

| | 내용 |
|---|---|
| 비교 대상 | **Sockudo**(Rust, 멀티테넌트 앱, 가장 직접적), Centrifugo(기능 기준선), **Nakama relayed**(게임 릴레이), **geckos.io**(브라우저 UDP via WebRTC), uWS 기반 soketi(원시 처리량), Socket.IO(채택률) |
| 승부 지표 | **손실 네트워크에서의 수신 지연 p99와 상태 신선도**(WebTransport datagram vs TCP), **노이즈 이웃 격리**(한 테넌트 폭주 시 다른 테넌트의 p99 변화) |
| 공개 지표 | 팬아웃 p50/p99/p999, 연결당 메모리, 코어당 처리량, **CPU/메시지**(QUIC의 비용도 숨기지 않는다) |
| 원칙 | 수치 없이 성능을 주장하지 않는다. 같은 언어(Rust) 경쟁자와 TCP 위에서 겨뤄서는 "훨씬"을 만들 구조적 근거가 없다 — 구조적 차이(datagram)에서 이긴다 |

### 커스텀 용이성: 확장 계층

| 계층 | 대상 | 방식 |
|---|---|---|
| ① 설정 | 운영자 | YAML, `--check-config`, 핫 리로드(M6) |
| ② 백엔드 프록시 | **대부분의 팀(언어 무관)** | connect / rpc를 백엔드로 위임(HTTP → gRPC), 서버 발행 API, 1회용 티켓 발급 |
| ③ Rust 트레이트 | 성능이 필요한 팀 | `Authenticator`, `Service`(Text/Binary), 이후 `Broker` → 빌더로 조립 |
| ④ WASM 플러그인 | SaaS 테넌트별 커스텀 | Ext Lane 전용, 샌드박스. Hot Lane에는 넣지 않는다 |

## 제품 구조

```
wsprism-core      프로토콜 (전송·런타임 무관)
wsprism-engine    트레이트로 확장 가능한 엔진 (현재 gateway를 재구성)
wsprism-server    engine + 기본 플러그인(JWT, HTTP 프록시, 이후 Redis)을 조립한 바이너리
wsprism-sdk-*     클라이언트 SDK (TS → Unity C# → Dart). WebTransport 우선, WebSocket 폴백
```

## 단계

| 단계 | 내용 | 완료 기준 |
|---|---|---|
| **M0 정리** ✅ | 테넌트 조회 `HashMap`, `run_session` 분해, 룸 기능을 서비스로 + 세션 컨텍스트 `&mut`([ADR 0001](adr/0001-session-context-and-room-service.md)), 전송 무관 `OutFrame`([ADR 0002](adr/0002-transport-agnostic-out-frame.md)) | 동작 변화 없이 기존 테스트 통과 |
| **M1 정직한 기반** ✅ | admission 파이프라인, Origin 허용 목록, 신뢰 프록시 + XFF, 티켓 경로 + 버전 협상([ADR 0003](adr/0003-ticket-sources-and-subprotocol.md)), 에러 코드 목록·정보 노출 제거, `reply_to`, `--config`/`--check-config`, 문서 정합, 퍼징 5종, cargo-deny(`serde_yaml` 교체), 벤치마크 기준선(TCP_NODELAY 수정) | 기준선 수치 문서화, 퍼징·감사 CI |
| **V 차별화 검증** (2~3주) | WebTransport datagram vs WebSocket을 손실·지연 매트릭스로 측정, 브라우저 3종 호환, 1시간 소크, UDP 차단 시 폴백, 노이즈 이웃 격리(wsPrism·Sockudo·Centrifugo). 세부: [설계 §7](design/hot-lane-over-webtransport.md#7-검증-단계-v-23주) | 관문 G1~G4 판정 → ADR 0004 확정 또는 전환 |
| **M2-lite 끼워 쓰기** | engine/server crate 분리, `Authenticator` 트레이트 + JWT(테넌트별 JWKS) + **1회용 티켓 발급 API**, HTTP 백엔드 프록시(connect·rpc), **서버 발행 API**(`/api/v1/publish`, `/api/v1/disconnect`), 최소 TS SDK(WS). gRPC·다양한 플러그인은 뒤로 | Node 또는 Spring 백엔드에 붙인 예제 앱 |
| **T WebTransport 전송** | `SessionIo` 추출, 전달 클래스 + Hot 슬롯(conflation), Hot 헤더 확장(KEY/SENDER), `WtIo`, `transport` 메트릭 라벨, SDK의 WT 우선·WS 폴백. 순서: [설계 §9](design/hot-lane-over-webtransport.md#9-단계-t-작업-순서-v-통과-후) | 같은 서비스 코드가 WT·WS 혼합 룸에서 동작, V의 이득이 제품 경로에서 재현 |
| **M3 신뢰성 (동등 수준)** | 채널 일반화, 채널별 이력 + offset/epoch + 재접속 복구, ACK. Hot Lane은 이력 없음 | 끊긴 사이 메시지 복구 테스트 |
| **M4 멀티 노드 (동등 수준)** | Broker(Redis 먼저), 노드 로컬 구독 색인, 클러스터 강제 퇴장, 1회용 티켓 공유 저장소, 전역 한도의 정확/근사 결정 | 노드 강제 종료 카오스 테스트 |
| **M5 성능 공개** | 메시지당 할당 제거(메트릭 라벨, 팬아웃), ID 인터닝, 프레임 사전 인코딩, 표준 메트릭 crate. 비교 대상 전체와 공개 벤치마크 | 재현 가능한 벤치 리포와 결과 |
| **M6 운영** | 환경변수 오버라이드, 핫 리로드(`ArcSwap`), OpenTelemetry, JSON 로그·마스킹, 관리 API(RBAC), Docker/Helm, 재접속 지터, 메트릭 단위 표준화, UDP 로드밸런서 가이드 | 롤링 업데이트 가이드대로 무중단 배포 |
| **M7 채택** | SDK 확장(Unity C#, Dart), semver·CHANGELOG·MSRV, `SECURITY.md`·`CONTRIBUTING.md`, 예제(멀티플레이·협업 커서), 소크·카오스, 외부 보안 감사 | 1.0 릴리스 |

V에서 G1이 실패하면(ADR 0004 "탈락 시 대안") T를 빼고, 정책 중심 방향(측정된 격리, 스키마 검증, WASM 정책)으로 M2 이후를 재구성한다.

## 리뷰 제안에서 바꾼 점과 이유

1. **벤치마크를 M1에서 시작했다**(리뷰에서는 4단계). 설계 결정마다 비용을 재야 비싼 구조가 공개 API로 굳지 않는다.
2. **엔진 API를 굳히기 전에 전송 계층을 분리했다**(ADR 0002). WebTransport를 붙일 수 있는 토대가 됐다.
3. **차별화 검증(V)을 M2 앞에 넣었다.** 따라잡기 기능은 끝내도 경쟁 제품과 같아질 뿐이라, 차별점을 먼저 숫자로 증명한다.
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
