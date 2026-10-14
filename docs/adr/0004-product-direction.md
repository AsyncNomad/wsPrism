# ADR 0004: 제품 방향 — 실시간 상태 동기화

- 상태: **조건부 채택**. 검증 단계 V의 관문(G1~G5)을 통과하면 확정한다.
- 개정 이력: 초안 2026-10-14 → **개정 1 (2026-10-14)**: 2차 리뷰 반영(측정 지표의 생존 편향, 비용 관문, MoQ, 경계 순서, 결정 분기, 대표 시나리오). 아직 확정 전이라 새 ADR 대신 본문을 개정했다.
- 관련: [설계: Hot Lane over WebTransport](../design/hot-lane-over-webtransport.md), [ROADMAP](../ROADMAP.md), [ADR 0002](0002-transport-agnostic-out-frame.md)

## 배경

목표(사용자 정의): **웹·게임 영역에서 기존 게이트웨이보다 훨씬 좋은 성능, 쉬운 커스텀, 대기업 수준의 안정성.** 특정 산업(예: 교육) 특화는 목표가 아니다.

경쟁 분석에서 확인한 사실:

- ROADMAP의 따라잡기 기능(백엔드 프록시, 서버 발행 API, 이력·복구, Redis/NATS 멀티 노드)은 **Centrifugo와 Sockudo가 이미 가진 기능**이다.
- **Sockudo**: Rust, Pusher 호환, 앱(테넌트) 단위 Origin 허용 목록·연결 한도·초당 이벤트 한도([문서](https://sockudo.io/guide/configuration/origin-validation)). 정책 중심 멀티테넌트 자리에 이미 Rust 경쟁자가 있다.
- 현재 Hot Lane은 **서버 큐에서만 손실 허용**이다. 전송이 TCP라 네트워크 손실 시 뒤의 프레임이 재전송을 기다린다(head-of-line blocking). 구조는 Nakama relayed와 같다.
- **WebTransport**: Safari 26.4(2026-03)로 주요 브라우저 모두 지원했다는 보고가 2차 자료에서 일치(엔진별 인증서 처리 차이 지적 있음).
- **MoQ(Media over QUIC)**: IETF 표준화 중. Rust 구현 moq-rs를 Cloudflare가 유지보수하며 `moq-relay-ietf`를 production-ready로 소개한다([repo](https://github.com/cloudflare/moq-rs)). Cloudflare가 MoQ 릴레이 생성 API를 제공한다([문서](https://4da580d2.previews.developers.cloudflare.com/moq/index.md)). track→group→object 구조에서 오래된 그룹을 버리는 방식은 "최신 상태 우선"과 닮았다. → **"WebTransport로 pub/sub"은 그 자체로 차별점이 될 수 없다.**
- **Colyseus**: 실험 단계의 WebTransport 전송이 있다(datagram 사용 여부 미확인).
- Rust WebTransport 서버 라이브러리: `wtransport` 0.7, `web-transport-quinn` 0.13 — 모두 1.0 이전.

## 결정

1. **간판은 "실시간 상태 동기화 의미론"이다.** 키별 최신값 유지(conflation), 틱 단위 송신 조절과 묶음 전송, 관심 영역 필터링(이후), 테넌트 정책, 백엔드 연동을 **가장 좋은 비신뢰 전송 위에서** 제공한다.
   - 전송: WebTransport datagram(직접 구현 **또는 MoQ 위/호환** — V에서 결정), 안 되면 WebSocket 폴백.
   - 전송 자체는 수단이다. 같은 의미론이 WebSocket 위에서도 동작해야 한다(conflation으로 큐 적체 감소).
2. **대표 시나리오(검증과 첫 사용자)**: 모바일·Wi-Fi 환경의 **20~60 Hz 웹 멀티플레이 상태 동기화**. 협업 커서(손실 시 수십 ms 차이가 체감되기 어려움), 시세(순서·누락 없음이 더 중요한 경우가 많음)는 대표에서 뺀다.
3. **멀티테넌트 정책 엔진은 기반**이다(대기업급 안정성·운영의 핵심). 한 줄 소개는 아니다.
4. **따라잡기 기능은 "갈아탈 때 막히지 않을 만큼"만** 구현한다.
5. **경계를 먼저 연다(T0).** 전송 능력(순서 스트림 vs 비신뢰 datagram), 세션당 두 채널, 송신 단위의 전달 등급, Hot 슬롯(키별 최신값)을 **M2 공개 트레이트를 정하기 전에** WebSocket만으로 구현한다. M2가 "순서 있는 바이트 채널 하나"를 전제로 굳으면 나중에 공개 API를 깨야 한다.
6. 확정 전에 **검증 단계 V**를 둔다. 관문:

| 관문 | 기준 |
|---|---|
| **G1 체감 성능** | 주 지표는 **상태 신선도(AoI, 60 Hz 렌더 틱마다 "보유한 최신 상태의 나이")**. 균등 손실 1%와 버스트 손실(Gilbert-Elliott)에서 비신뢰 전송의 **AoI p99가 WebSocket 대비 3배 이상** 좋고, **끊김 비율(AoI > 100 ms인 틱의 비율)**도 낮다. 손실 0% 대조군에서 **WebSocket보다 나쁘지 않다**(AoI p50·p99 10% 이내). 메시지당 지연은 보조 지표(유실 패킷이 빠지는 생존 편향 때문) |
| **G2 호환성** | Chrome, Firefox, Safari에서 연결·datagram 송수신 성공 |
| **G3 안정성** | 1,000 세션 1시간 부하에서 패닉 0, 워밍업 후 RSS 증가 < 10% |
| **G4 폴백** | UDP 차단 시 클라이언트가 2초 안에 WebSocket으로 전환 |
| **G5 비용** | 같은 팬아웃(대표 시나리오 + 큰 방)에서 **메시지당 CPU와 연결당 메모리가 WSS(게이트웨이 TLS) 대비 2배 이내**. 틱 묶음 전송과 GSO를 켠 상태로 판정. LB가 TLS를 종단하는 WS(WebSocket에 가장 유리한 배포)와의 차이도 함께 공개 |

## 기각·보류한 대안

| 대안 | 판단 |
|---|---|
| 정책 중심 멀티테넌트 엣지를 간판으로 | 기각 — Sockudo가 같은 자리(Rust·Pusher 생태계·앱 단위 quota). **G1·G5 모두 실패 시** 재검토(아래) |
| Centrifugo 기능 폭 전체 따라잡기 | 기각 — 동등해질 뿐이고 같은 언어 사이에서 "훨씬 빠름"의 구조적 근거가 없다 |
| 게임 백엔드(Nakama·Colyseus 계열) | 기각 — 계정·매치메이킹·서버 로직은 범위가 크고 성숙한 제품이 있다. 전송 계층(사이드카) 정체성과도 맞지 않다 |
| gRPC 양방향 ↔ 브라우저 브리지 | 기각 — 기존 자산과 연결이 약하고 시장 공백을 검증하지 않았다 |
| WebRTC DataChannel로 브라우저 UDP | 기각 — 시그널링·ICE·DTLS 운영 복잡도. geckos.io를 비교 대상으로만 둔다 |
| **MoQ와 경쟁 / MoQ 위에 구축 / MoQ 호환** | **보류 — V에서 결정.** moq-rs 릴레이로 대표 시나리오의 AoI를 같은 조건에서 재고, "최신 그룹" 의미론이 키별 상태에 맞는지, 테넌트 인증·정책을 얹을 수 있는지를 본다. 비슷한 성능이면 직접 구현보다 MoQ 호환·활용이 유지보수 면에서 낫다 |
| "브라우저 UDP"를 간판으로 (초안) | 개정 1에서 철회 — MoQ·표준 인프라가 그 자리를 채운다. 전송은 수단으로 내린다 |

## 결정 분기

| 결과 | 다음 |
|---|---|
| 전부 통과 | ADR 확정. T(WebTransport 또는 MoQ 전송) 진행 |
| **G5만 실패**(체감은 좋지만 비용이 큼) | **하이브리드**: 소규모 인터랙티브 룸은 비신뢰 전송, 대규모 방송 룸은 WebSocket — 테넌트·룸 정책으로 선택. 실패가 범위 축소가 아니라 운영 선택지가 된다 |
| **G1 실패** | 비신뢰 전송을 간판에서 내린다. 상태 동기화 의미론(conflation·틱)은 WebSocket 위에서 유지하고, 정책 중심으로 재구성. 이때 노이즈 이웃 격리를 wsPrism·Sockudo·Centrifugo에서 측정해 근거로 쓴다(Pusher 어댑터와 private 채널 인증 엔드포인트 필요, 수일 분량) |
| **G1·G5 모두 실패** | 위와 같되 범위를 좁힌다(특정 트래픽 유형의 멀티테넌트 엣지 또는 임베드 라이브러리 포지션) |
| G2에서 Safari만 실패 | 진행. Safari는 WebSocket 폴백, 추적 항목 |
| G3 실패 | 라이브러리를 `web-transport-quinn`(또는 MoQ 경로)으로 바꿔 재검증 |
| G4 실패 | 폴백 방식 수정(관문 자체가 제품 요구사항) |

## 결과

- ROADMAP 재구성: **V → T0(경계, WebSocket만) → M2-lite → T(비신뢰 전송) → M3/M4(동등 수준)**.
- 비교 대상: Sockudo, Centrifugo, **MoQ(moq-rs / Cloudflare 릴레이)**, Nakama relayed, Colyseus, geckos.io.
- **프로토콜 변경 예고**: 비신뢰 전송의 Hot 프레임에 크기 상한(기본 1,100 B, 경로 MTU)이 생긴다. 현재 테넌트 설정은 Hot 프레임을 64 KB까지 허용하므로, T에서 PROTOCOL.md에 명시하고 WebSocket에도 같은 상한을 적용해 전송 간 동작을 일치시킨다.
- WebTransport에서는 브라우저가 헤더를 붙일 수 없어 **1회용 단기 티켓**(URL)이 필요 → M2-lite에 티켓 발급 API.
- 현재 Hot Lane의 한계(WebSocket 위에서는 네트워크 구간 손실 허용이 아님)를 PROTOCOL.md에 명시했다.
