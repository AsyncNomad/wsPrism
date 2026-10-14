# ADR 0004: 제품 방향 — 브라우저용 실시간 상태 동기화 트랜스포트

- 상태: **조건부 채택** (2026-10-14). 검증 단계 V의 관문(G1~G4)을 통과하면 확정한다.
- 관련: [설계: Hot Lane over WebTransport](../design/hot-lane-over-webtransport.md), [ROADMAP](../ROADMAP.md), [ADR 0002](0002-transport-agnostic-out-frame.md)

## 배경

코드 리뷰에서 경쟁 제품과의 기능 비교가 나왔다. 확인된 사실:

- ROADMAP의 M2~M4(백엔드 프록시, 서버 발행 API, 이력·복구, Redis/NATS 멀티 노드)는 **Centrifugo와 Sockudo가 이미 가진 기능**이다. 끝내도 동등해질 뿐이다.
- **Sockudo**는 Rust, Pusher 프로토콜 호환이고 앱(테넌트) 단위로 Origin 허용 목록, 연결 수 한도, 초당 이벤트 한도를 가진다([문서](https://sockudo.io/guide/configuration/origin-validation)). 정책 중심 멀티테넌트 게이트웨이 자리는 이미 Rust 경쟁자가 있다. 다만 "한 테넌트 폭주 시 다른 테넌트의 지연이 보호된다"를 측정해 내세우는 자료는 찾지 못했다.
- 현재 Hot Lane은 "바이너리 프레임 → opcode 정책 → 활성 룸에 Lossy 팬아웃"이다. **Lossy는 서버 큐에서만 성립한다.** 전송이 TCP(WebSocket)라 네트워크에서 패킷 하나가 유실되면 뒤의 모든 프레임이 재전송을 기다린다(head-of-line blocking). 게임 쪽에서는 Nakama relayed 모드와 구조가 같다.
- **WebTransport**가 Safari 26.4(2026-03)로 주요 브라우저 모두에서 쓸 수 있게 되었다는 보고가 여러 2차 자료에서 일치한다. 엔진별 구현 차이(인증서 처리 등) 지적이 있어 직접 검증이 필요하다.
- Rust 서버 라이브러리: `wtransport` 0.7, `web-transport-quinn` 0.13, `h3-webtransport` 0.1 — 모두 1.0 이전.

목표(사용자 정의): **기존 게이트웨이보다 훨씬 좋은 성능, 쉬운 커스텀, 대기업급 품질.**

## 결정

wsPrism의 간판을 **"브라우저에서 진짜 UDP처럼 동작하는 실시간 상태 동기화 — WebTransport datagram, 안 되면 WebSocket 폴백 — 그리고 테넌트별 정책"** 으로 잡는다.

1. **Hot Lane = 비신뢰·비순서 전달 클래스.** WebTransport에서는 datagram으로, WebSocket에서는 키별 최신값 덮어쓰기(conflation)로 최대한 근사한다.
2. **멀티테넌트 정책 엔진은 간판이 아니라 기반**으로 계속 다듬는다(대기업급 요구사항의 핵심).
3. **대상 트래픽은 "최신 상태가 이기는" 데이터 전반**: 웹 멀티플레이, 협업 커서·화이트보드, 시세·스포츠 틱, IoT·원격 대시보드. 게임 백엔드(계정, 매치메이킹, 서버 로직)는 만들지 않는다 — 사용자 백엔드 앞의 전송 계층으로 남는다(사이드카 철학).
4. **따라잡기 기능(M2~M4)은 "갈아탈 때 막히지 않을 만큼"만** 구현한다.
5. 확정 전에 **검증 단계 V**(2~3주)를 둔다. 관문:

| 관문 | 기준 |
|---|---|
| **G1 성능** | 1% 손실·RTT 40 ms·20 Hz에서 WebTransport datagram이 WebSocket 대비 **수신 지연 p99 또는 상태 신선도 p99가 3배 이상** 좋다 |
| **G2 호환성** | Chrome, Firefox, Safari에서 연결·datagram 송수신 성공 |
| **G3 안정성** | 1,000 세션 1시간 부하에서 패닉 0, 워밍업 후 RSS 증가 < 10% |
| **G4 폴백** | UDP 차단 시 클라이언트가 2초 안에 WebSocket으로 전환 |

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| 정책 중심 멀티테넌트 엣지(격리 + WASM 정책)를 간판으로 | Sockudo가 Rust·Pusher 생태계·앱 단위 quota로 같은 자리에 있다. 엄격한 기본값만으로는 갈아탈 이유가 약하다. **단 G1 실패 시 이 방향으로 전환한다**(아래). |
| Centrifugo 기능 폭 전체 따라잡기 | 몇 달을 들여 동등해질 뿐이고, 같은 언어(Go/Rust) 사이에서 "훨씬 빠름"을 만들 구조적 근거가 없다 |
| 게임 백엔드(Nakama·Colyseus 계열) | 계정·매치메이킹·서버 로직은 범위가 크고 이미 성숙한 제품이 있다. 전송 계층이라는 정체성과도 맞지 않는다 |
| gRPC 양방향 ↔ 브라우저 브리지 | 기존 자산과 연결이 약하고(방향 전환), 시장 공백을 검증하지 않았다 |
| WebRTC DataChannel로 브라우저 UDP | 시그널링·ICE·DTLS 등 운영 복잡도가 크다. 같은 문제를 푸는 geckos.io가 있어 비교 대상으로만 둔다 |

## 탈락 시 대안

- **G1 실패**(이득 < 3배): 정책 중심 방향으로 전환한다. 검증 V에서 함께 측정하는 **노이즈 이웃 격리** 결과를 그 방향의 근거로 쓴다.
- **G2에서 Safari만 실패**: 진행한다. Safari는 WebSocket 폴백으로 서비스하고 추적 항목으로 남긴다.
- **G3 실패**: 라이브러리를 `web-transport-quinn`으로 바꿔 재검증한다. 둘 다 실패하면 확정을 미루고 상위 라이브러리 성숙을 기다린다(정책 중심으로 진행).
- **G4 실패**: 폴백 방식을 고친다(관문 자체가 제품 요구사항).

## 결과

- ROADMAP을 재구성한다: **V(검증) → M2-lite → T(WebTransport 전송) → M3/M4(동등 수준만)**.
- 비교 대상에 **Sockudo, Nakama(relayed), geckos.io**를 추가한다.
- 현재 Hot Lane의 한계(WebSocket 위에서는 네트워크 구간 손실 허용이 아님)를 PROTOCOL.md에 명시한다.
- WebTransport에서는 브라우저가 헤더를 붙일 수 없어 **1회용 단기 티켓**(URL)이 필요해진다 → M2-lite에 티켓 발급 API가 들어간다.
