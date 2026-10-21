# Hardening Log

wsPrism을 "엔터프라이즈급에 가깝게" 끌어올리기 위한 작업 기록이다.
결함 목록, 우선순위, 내린 결정과 기각한 대안, 아직 못 본 사각지대를 남긴다.
작업 브랜치: `hardening` (main 기준).

## 작업 원칙

- 수정 하나 = 커밋 하나 (conventional prefix). 포맷/리팩터와 동작 변경은 커밋을 섞지 않는다.
- 결함 수정은 **재현 테스트 먼저** 작성 → 실패 확인 → 수정 → 통과.
- 게이트: `cargo fmt --all --check` / `cargo clippy --workspace --all-targets --locked -- -D warnings` / `cargo test --workspace --locked` (CI: `.github/workflows/ci.yml`).

## Phase 0 — 기준선 (완료)

| 항목 | 결과 |
|---|---|
| 초기 상태 | 빌드 OK(경고 0), 테스트 5개 통과, clippy 경고 10, rustfmt 불일치 102 hunk |
| rustfmt 일괄 적용 | 별도 커밋 (동작 변경 없음) |
| clippy 정리 | `-D warnings` 통과 |
| gateway lib에 `deny(unwrap_used, expect_used, panic)` | core와 동일 원칙. `ws.rs`의 unwrap 3곳 제거 |
| `Cargo.lock` 추적 | gitignore에서 제거 |
| CI 추가 | fmt / clippy / test (`--locked`) |

### 결정

- **`Cargo.lock` 커밋**: 이 워크스페이스는 바이너리(게이트웨이)를 배포한다. 재현 가능한 빌드와 CI `--locked`를 위해 lock을 고정한다.
- **gateway lib의 panic 금지**: 문서상 "panic-free"를 표방하므로 lint로 강제한다. `main.rs`(bin 타깃)는 기동 실패 시 즉시 종료가 맞으므로 예외로 둔다.
- **MSRV 미선언** (당시): clippy 제안으로 `u32::is_multiple_of`(Rust 1.87+)를 쓰게 됐다. → 이후 MSRV 1.95를 선언하고 CI에서 검사한다(ADR 0009 §5-4).

## 결함 목록

ID는 커밋 메시지/테스트 이름에서 참조한다. 상태: ☐ 미착수 / ◐ 진행 / ☑ 완료.

### Phase 1 — 보안·자원

| ID | 상태 | 내용 | 위치 |
|---|---|---|---|
| S6 | ☑ | **HandshakeDefender 교착**: DashMap 엔트리 guard를 쥔 채 `per_ip.len()` 호출 → 핸드셰이크 리미터를 켜면(`wsprism.yaml` 기본값) 첫 허용 연결에서 워커 스레드 영구 정지. 추가로 전역→IP 순서라 IP 하나가 전역 예산 고갈 가능, 무작위 축출이 공격자 버킷 리셋 | `transport/handshake.rs` |
| S1 | ☑ | Hot Lane에 연결 단위 레이트리밋 미적용 (Text 분기에서만 검사) | `transport/ws.rs` |
| S2 | ☑ | WS `max_message_size` 미설정(axum 기본 64MB) + `check_len`이 디코드 *후* 실행 → 대용량 JSON 파싱 DoS | `transport/ws.rs`, `transport/codec.rs` |
| S3 | ☑ | `chat.send`가 발신자의 룸 멤버십을 검사하지 않음 | `services/chat.rs` |
| S4 | ☑ | 클라이언트 지정 `sid` 충돌 시 기존 세션 덮어쓰기 → 테넌트 카운터 누수, 기존 연결 고아화 | `realtime/core/session_registry.rs` |
| S5 | ☑ | 핸드셰이크 거절 메트릭 라벨에 미검증 `tenant` 사용 → 라벨 카디널리티 폭발(메모리 DoS) | `transport/ws.rs` |
| S7 | ☑ | **JSON 배열 봉투 수용**: serde 파생 `Deserialize`가 구조체에 JSON 배열(필드 위치 순)도 받아 `[1,"chat","send"]` 같은 문서화되지 않은 와이어 형식이 통과. 필드 순서에 의미가 달려 있어 v2 디코더(필드 순서 다름)와 해석이 갈렸다 — **퍼징으로 발견**(2026-10-16). 두 디코더 모두 최상위 JSON 객체만 받도록 수정(`2a1ac43`) | `wsprism-core/protocol/text.rs` |

#### S3 결정 기록

- **수정**: `chat.send`는 발신 *세션*이 해당 룸에 join한 경우에만 발행하고, 아니면 `NOT_ALLOWED`. `Presence::is_member` / `RealtimeCtx::is_in_room` 추가.
- **세션 단위 vs 유저 단위**: 세션 단위로 판정한다. 라우팅(Presence)이 세션 단위이고, 같은 유저의 다른 탭이 join했다고 이 탭이 그 룸에 말할 수 있게 하면 "받지 못하는 룸에 쓰기"가 가능해진다.
- Hot Lane(echo)은 join 성공 시에만 설정되는 `active_room`으로만 발행하므로 별도 검사 불필요. 단, 강제 종료(kick)된 세션이 루프를 계속 도는 동안 로컬 `active_room`으로 발행할 수 있는 문제는 C8로 별도 추적.

#### S4 결정 기록

- **원인 확인**: 수정 전 동작을 임시 테스트로 관찰했다. 같은 `sid` 재접속 후 예전 연결이 끊기면 그 정리 코드가 **살아 있는 새 연결의 레지스트리 항목을 삭제**했다(연결은 붙어 있는데 서버는 모르는 고아 상태). 테넌트 카운터도 +1 누수.
- **근본 수정 — 내부 키 분리**: 레지스트리/Presence 키를 `tenant::user::sid#<conn_id>`로 바꿔 연결마다 유일하게 만든다. 클라이언트가 고르는 `sid`는 표시용 식별자로만 남는다(`RealtimeCtx::session_id()`). 키 공유 자체가 불가능해지므로 정리 순서·타이밍과 무관하게 안전하다.
  - 기각: *세대 번호로 정리 조건부 실행* — 레지스트리는 막을 수 있지만, kick된 예전 루프가 같은 키로 다시 join하면 Presence에서 새 연결의 키로 섞여 들어간다(C8과 결합). 키를 분리하는 쪽이 문제를 원천 제거.
- **정책 — 같은 sid는 교체(takeover)**: 같은 user가 사용 중인 `sid`로 접속하면 예전 연결을 `sys.kicked{reason:"replaced"}` + close 1008로 내보내고 새 연결을 받는다. 세션 한도 평가 *전에* 교체하므로 교체가 한도를 소모하지 않는다.
  - 기각: *거부* — 네트워크 단절 후 재접속하는 탭이 흔한 정상 사례인데, 예전 연결이 idle 타임아웃(기본 60s)으로 정리될 때까지 재접속이 막힌다.
  - 전제: 현재 인증 스텁에선 모든 클라이언트가 `user:dev`라 서로의 sid를 교체할 수 있다. 실제 인증(Phase 4) 후엔 *같은 유저*만 교체 가능.
- **강제 종료 경로 통합**: kick_oldest와 replace가 `evict_session` 하나를 공유한다(레지스트리 제거 → Presence 정리 → 게이지 감소(C2 원칙) → 통지·close).
- **프로토콜 변화**: 강제 종료 close 프레임의 reason이 고정 `"kicked"`에서 사유 문자열(`max_sessions_exceeded` / `replaced`)로 바뀌었다. `sys.kicked.data.reason`은 기존과 동일 형식.

#### S5 결정 기록

- **수정**: `ws_upgrade`에서 테넌트를 먼저 조회하고, 메트릭 라벨은 *설정에 있는 id* 또는 고정값 `_unknown`만 쓴다. 레이트리밋은 여전히 테넌트 검증보다 먼저 적용된다(조회는 읽기 전용이라 순서를 바꿔도 비용 차이 없음).
- **신규 계측**: 미등록 테넌트로 인한 400도 `handshake_rejections{reason="unknown_tenant",tenant="_unknown"}`로 센다(이전엔 미계측).
- **예약어**: `_unknown`과 실제 테넌트가 겹치지 않도록 `_`로 시작하는 테넌트 id를 설정 검증에서 거부한다.
- **감사**: 모든 메트릭 라벨 값을 점검. `tenant` 외에는 전부 정적 문자열 또는 `ClientCode::as_str()`(고정 집합)이다.

#### S6 결정 기록 (S5 재현 테스트 작성 중 발견)

- **원인**: `entry(ip).or_insert_with(..)`가 돌려준 `RefMut`(샤드 쓰기 락)이 `self.per_ip.len()`(전 샤드 읽기 락) 호출 시점까지 살아 있었다. 같은 스레드의 자기 교착이며, async가 아닌 블로킹 락이라 tokio 워커 스레드 자체가 멈춘다. `tokio::Mutex`를 샤드 락 안에서 `.await`하는 것도 같은 위험이 있었다.
- **수정**:
  - `check()`를 동기 함수로 바꾸고 DashMap guard는 **한 표현식 안에서만** 쓴다. `if let` 조건식에 guard를 두지 않는다(edition 2021에서는 else 블록까지 살아 있음).
  - 짧은 임계 구역이라 `tokio::Mutex` → `std::Mutex`. poison은 버킷이 단순 데이터이므로 `into_inner()`로 복구.
  - 검사 순서 **IP → 전역**: 한 IP의 폭주가 자기 버킷에서 막혀 전역 예산을 소모하지 못한다(이전엔 IP 거절 전에 전역 토큰을 먼저 소비).
  - 메모리 상한: `max_ip_entries` 하드캡. 캡에서 새 IP가 오면 (최대 초당 1회) *버킷이 가득 찬* 엔트리만 축출(새 엔트리와 구별 불가 → 무손실). 그래도 자리가 없으면 그 IP는 추적하지 않고 전역 버킷만 적용.
  - 기각: 기존 무작위 10% 축출 — 공격자 엔트리가 지워지면 burst가 리셋되어 우회 가능.
- **테스트 장치 변경**: 서버를 별도 OS 스레드·런타임에서 띄우고 핸드셰이크에 타임아웃을 건다. 서버가 멈추면 테스트가 *걸리는* 대신 *실패*한다.
- **동일 패턴 감사**: `SessionRegistry`, `Presence`, `obs::metrics`의 DashMap 사용을 점검. guard는 문장 단위로 해제되거나 명시적 `drop` 후 재접근하며, 맵 간 락 순서 역전 경로도 없음.

#### S1 결정 기록

- **수정**: Hot 분기에도 연결 단위 토큰버킷 검사 추가. 초과 시 조용히 drop하고 `policy_decisions{lane="hot",reason="conn_rate_limit"}` 증가(Ext와 동일한 처리).
- **버킷 공유**: Ext와 Hot이 연결당 **하나의** 버킷을 공유한다. 설정에 `rate_limit_rps`가 하나뿐이고, 테넌트 스코프 리미터도 이미 두 레인이 공유하기 때문이다. 레인별 예산이 필요해지면 `hot_rate_limit_rps` 같은 별도 설정으로 확장한다. 공유 동작은 `hot_and_ext_share_one_connection_budget` 테스트가 고정한다.
- 참고: `select!` 매크로 내부는 rustfmt가 포맷하지 않아 들여쓰기가 고르지 않다. `run_session` 분해(Phase 3) 때 해소한다.

#### S2 결정 기록

- **수정**: 업그레이드 시 `max_message_size`/`max_frame_size`를 테넌트 `max_frame_bytes`로 설정. 초과 시 tungstenite가 *헤더만 보고* 거절하므로 본문 버퍼링·JSON 파싱이 일어나지 않는다. 기존 `check_len`은 2차 방어로 유지.
- **클라이언트 통지**: 초과 시 RFC 6455 close **1009**(Message Too Big)를 보내고 `policy_decisions{lane="transport",reason="frame_too_large"}`를 증가.
- **에러 판별**: `axum::Error` → `tungstenite::Error::Capacity(MessageTooLong)` downcast. 이를 위해 gateway가 `tungstenite = "0.24"`를 직접 의존한다. **axum이 쓰는 버전과 반드시 같아야 한다** — 다르면 downcast가 조용히 실패하고 1009 대신 그냥 끊긴다. `tests/s2_message_size.rs`가 이를 잡는다(`Cargo.lock`에 tungstenite가 1개 버전만 있어야 함).
- **Lingering close (500ms)**: 처음 구현에선 테스트가 약 1/6 확률로 실패했다(close 프레임 대신 연결 리셋). 원인: tungstenite는 초과 프레임의 헤더만 소비하고 본문을 건너뛰지 못해 이후 읽기도 같은 에러만 반복한다. 미수신 데이터가 남은 소켓을 닫으면 커널이 FIN 대신 RST를 보내 피어가 close 프레임을 버릴 수 있다. close 전송 후 500ms 대기로 해결(60/60 통과).
  - 기각: *본문 드레인* — axum이 raw 소켓을 노출하지 않고 tungstenite가 본문 skip을 지원하지 않음.
  - 기각: *정상 close 핸드셰이크 대기* — 위와 같은 이유로 피어의 close 응답을 읽을 수 없음.
  - 비용: 초과 전송 연결마다 task가 500ms 더 산다. 읽지 않으므로 메모리는 TCP 흐름제어로 묶이고, 연결 수는 핸드셰이크 리미터가 묶는다.
- **테스트 장치**: `tests/common/mod.rs` — 실제 게이트웨이를 임시 포트에 띄우고 `tokio-tungstenite` 클라이언트로 붙는다. 이후 결함 재현 테스트도 이 장치를 쓴다.

### Phase 2 — 정확성

| ID | 상태 | 내용 | 위치 |
|---|---|---|---|
| C1 | ☑ | Reliable 발행이 발신 세션 루프를 최대 timeout×1 동안 블로킹 (head-of-line). 발신자 자신도 룸 멤버라 **자기 큐**를 최대 1.5s 기다리는 경우 포함(C9의 유한 버전) | `services/chat.rs`, `realtime/core/realtime.rs` |
| C2 | ☑ | kick_oldest 시 `ws_active_sessions` 게이지 이중 감소 | `transport/ws.rs` |
| C3 | ☑ | "에러를 큐에 넣고 `break`"하는 모든 경로에서 에러가 클라이언트에 도달하지 않음: `on_exceed: deny`(writer 루프 시작 전 큐잉), policy `Close`, 디코드 오류, idle 타임아웃(큐잉 직후 루프 탈출 → out 큐 미플러시). 인증 실패도 업그레이드 *후* 판정 | `transport/ws.rs` |
| C4 | ☑ | `room:leave`가 active_room만 떠남 → 다른 룸 탈출 불가 | `transport/ws.rs` |
| C5 | ☑ | "빈 집합이면 제거" 경쟁: empty 판정과 `remove` 사이에 다른 스레드가 추가하면 그 항목이 함께 소실. Presence `leave`(신규 join 멤버십 소실)와 `SessionRegistry::remove_session`의 `user_index`(신규 세션이 user 색인에서 누락) 두 곳 | `realtime/core/presence.rs`, `realtime/core/session_registry.rs` |
| C8 | ☑ | 강제 종료(kick/replace)된 세션의 루프가 클라이언트가 닫을 때까지 계속 돈다 → Presence에선 지워졌지만 로컬 `active_room`으로 Hot 발행·재join 가능. 서버 측 종료 신호 필요 | `transport/ws.rs`, `realtime/core/session_registry.rs` |
| C9 | ☑ | 세션 루프가 자기 응답을 자기 송신 큐에 `send().await` — 큐를 비우는 것도 같은 루프라 큐가 가득 차면(느린 클라이언트 + 브로드캐스트) **세션이 영구 정지**. idle 검사도 같은 루프라 동작 안 함 | `transport/ws.rs` |
| C6 | ☑ | 경미: ~~span guard를 await 너머로 유지~~(`.instrument` + clippy.toml 금지), ~~Ext `v` 미검증~~(core `decode_envelope`), ~~ping 이중 응답~~(C9에서 제거), ~~토큰버킷 소수 시간 버림~~(이월 처리) | 여러 곳 |

#### C6 결정 기록

- **Ext 버전**: 검증을 core로 올렸다(`protocol::text::decode_envelope`, `EXT_PROTOCOL_VERSION`). Hot Lane과 같은 계층에서 프로토콜 계약을 강제하고, gateway codec은 이를 호출만 한다. v≠1 → `UNSUPPORTED_VERSION` + 1002.
- **토큰버킷**: 토큰으로 바뀐 시간만 소비하고 나머지는 이월. 가득 찬 버킷은 추가 시간을 적립하지 않는다(표준 토큰버킷). 테스트 작성 중 "burst=1이면 10s에 95개"를 버그로 오인했으나, 상한에 닿아 있는 동안 적립하지 않는 것이 정의상 맞다 — 지속율 테스트는 burst=2로 상한 영향을 배제. 구 로직으로 돌려 2개 테스트가 실패함을 확인.
- **Span**: `run_session` 안에서 `span.enter()` guard를 await 너머로 쥐던 것을, 업그레이드 클로저에서 span을 만들고 세션 future 전체를 `.instrument(span)`으로 감싸도록 변경. 재발 방지로 `clippy.toml`의 `await-holding-invalid-types`에 `tracing::span::Entered`/`EnteredSpan` 등록 → CI clippy가 차단(도입 시 기존 위반을 정확히 잡는 것 확인). 실제 바이너리 로그에 `ws{trace_id=.. t=.. u=.. s=..}` 필드가 붙음을 확인.

#### C1 결정 기록

- **재현 조건이 중요했다**: 읽기를 *완전히 멈춘* 수신자로는 재현되지 않는다(서버 writer 타임아웃 1.5s가 그 수신자를 끊어 금방 풀림). 문제는 **느리지만 꾸준히 읽는** 수신자(모바일 등): writer가 조금씩 진행해 타임아웃이 안 걸리는 동안 큐가 계속 차 있어, 발신자(및 룸 전체)가 가장 느린 멤버의 속도로 끌려간다. 50ms마다 1개씩 읽는 수신자로 3/3 재현(메시지 198개째에서 8s 초과).
- **기존 "Reliable"의 실제 의미**: 수신자 큐를 최대 1.5s 기다린 뒤 **조용히 버림**. 발신자를 막으면서도 신뢰성은 보장하지 못했다.
- **새 의미**: `QoS::Reliable` = *순서대로 빠짐없이 전달하거나, 불가능하면 수신자를 `slow_consumer`(1008)로 끊는다*. 발신자는 절대 기다리지 않는다. `QoS::Lossy`는 큐가 차면 그 수신자에게만 버린다. 끊긴 수신자는 재접속으로 상태를 다시 맞춘다.
  - 기각: *팬아웃을 별도 task로 spawn* — 발신자는 안 막히지만 같은 발신자의 연속 메시지 순서가 수신자별로 뒤섞일 수 있고, 느린 수신자 문제(무한 대기 task 누적)는 그대로.
  - 기각: *발신자별 순차 팬아웃 워커* — 순서는 지키지만 구조가 크고, 결국 느린 수신자에 대해 같은 선택(버림/끊음/대기)을 해야 한다.
- **통합**: 모든 송신 경로(`send_to_user`, `send_to_session`, `publish_room*`)가 `deliver` 하나를 거치며 `out.qos`를 따른다. (이전엔 `send_to_user`/`send_to_session`이 QoS를 무시하고 항상 lossy.) `publish_room_lossy`/`_reliable`은 QoS를 강제하는 래퍼로 유지.
- **API 변화**: `QoS::Reliable { timeout_ms }` → `QoS::Reliable`(단위 variant), `publish_room_reliable`은 더 이상 `async`가 아니다.
- **메트릭 변화**: `wsprism_egress_send_fail_total` 제거 → `wsprism_egress_slow_consumer_closes_total`(reliable 메시지 때문에 끊은 수신자 수). 대시보드가 있다면 갱신 필요.
- **복구와의 정합 (2026-10-20, ADR 0011 R2)**: 이력 룸(`tenants[].history`)에서는 끊긴 수신자가 재접속하면서 마지막 offset부터 이어 받는다("끊고 재동기화" → "끊고 자동 복구"). 다시 보내는 메시지는 큐 한 칸(`OutFrame::Batch`)으로 들어가므로, 큐보다 많이 놓쳤어도 복구할 수 있다. 이력이 없는 룸은 여전히 클라이언트가 자기 백엔드에서 다시 맞춘다.

#### C4 결정 기록

- **수정**: `room:leave`는 `room`이 있으면 그 룸을, 없으면 active room을 떠난다(기존 클라이언트 호환). 떠난 룸이 active room이면 active room을 비운다(다른 룸으로 자동 전환하지 않음 — Hot Lane 대상이 암묵적으로 바뀌는 것을 피하려고).
- **응답**: `sys.left`에 `room` 필드 추가.
- **동작 변화**: 가입하지 않은 룸(또는 active room이 없는데 `room` 생략)을 떠나려 하면 `sys.error BAD_REQUEST`. 이전엔 무조건 `sys.left`를 돌려줘 클라이언트 버그를 가렸다.
- 테스트 주의: "한도 슬롯이 풀린다"만 보면 수정 전에도 통과한다(엉뚱한 active room이 빠져도 슬롯은 생김). 그래서 r2 멤버십이 유지되는지까지 확인한다.

#### C5 결정 기록

- **재현**: 단위 스트레스 테스트(스레드 2~4개가 같은 룸/유저에서 join·leave 4만 회 반복)로 5/5 재현.
  - leave가 순간적으로 빈 룸 집합을 지우면서 동시 join의 멤버십이 소실.
  - `user_index`에서 같은 유저의 동시 삽입 세션이 소실.
  - **테넌트 룸 카운터가 음수로 드리프트**(u64 wrap → 약 1.8e19). `max_rooms_total`을 쓰는 테넌트는 이후 **새 룸을 영구히 만들 수 없게 된다**.
- **수정**:
  - "비었으면 제거"를 `remove_if`로 원자화(`remove_from_set` 헬퍼). 삽입은 모두 `entry`(같은 샤드 쓰기 락)를 거치므로 두 연산이 직렬화된다.
  - 룸 집합 *생성*(`Entry::Vacant`)과 *제거*(`remove_if` 성공) 시점에만 테넌트 룸 카운터를 증감. 기존 `contains_key` 선판단 제거.
  - `max_rooms_total`은 CAS로 검사+증가를 한 번에 하여 **정확한 상한**이 됐다. (`tenant_room_cap_is_never_exceeded`는 수정 전에도 통과했으므로 과거 초과의 증거가 아니라 회귀 방지용.)
  - 유저↔룸 색인은 해당 user+room 참조 카운트 엔트리를 잡은 채로 갱신(join·leave 모두). 마지막 세션이 나갈 때의 색인 제거와 새 join이 섞이지 않는다.
  - 감소는 `checked_sub`로 0 아래로 내려가지 않게 한다.
- **락 순서**: `user_room_refs` → (`room_to_users`, `user_to_rooms`), `room_to_sessions` → `tenant_room_counts`. 역순으로 잡는 경로 없음.
- **여전히 best-effort**: `max_users_per_room`, `max_rooms_per_user` 검사는 색인을 잠그지 않고 읽는다. 정확하게 하려면 거버넌스 갱신을 라우팅보다 먼저 하고 실패 시 롤백해야 해서 보류.
- **CAS 헬퍼**: `AtomicU64::fetch_update`는 Rust 1.99에서 deprecated(`try_update`로 개명). MSRV를 더 올리지 않으려고 `compare_exchange_weak` 루프(`cas_update`)를 직접 쓴다.

#### C9 결정 기록

- **재현**: `session_queue_capacity: 1`로 요청 50개를 파이프라이닝하면 6/6회 응답 1~14개 후 세션이 영구 정지. 느린 클라이언트가 아니어도, 브로드캐스트로 큐가 *순간적으로* 찬 시점에 요청이 오면 같은 일이 생긴다. writer 타임아웃은 쓰기 arm에서만 동작해 이를 못 잡는다.
- **수정**: 루프 내 응답은 `reply!` 매크로(`queue_reply`, `try_send`)로만 넣는다. 큐가 차 있으면 `SLOW_CONSUMER` 통지 + 1008 `slow_consumer`로 종료하고 `policy_decisions{lane="transport",reason="slow_consumer"}` 증가. keep-alive ping은 큐가 차 있으면 이번 회차만 건너뛴다.
  - 기각: *응답을 버리기* — `sys.joined` 같은 응답을 조용히 잃으면 클라이언트 상태가 서버와 어긋난다.
  - 기각: *응답 전용 무제한 큐* — 메모리 상한이 사라지고 메시지 순서가 섞인다.
- **오탐 여유**: 기본 용량 1024에 연결당 레이트리밋(기본 burst 400)이 있어, 정상 클라이언트가 자기 응답만으로 큐를 채우기는 어렵다.
- **설정 추가**: `gateway.session_queue_capacity`(기본 1024, 1..=1048576). 테스트에서 결정적 재현을 위해 필요했고 운영에서도 조정할 만한 값이라 노출.
- **부수 정리(C6 일부)**: 클라이언트 Ping에 대한 수동 Pong 제거. tungstenite 0.24가 이미 자동 응답함을 소스로 확인(`protocol/mod.rs`의 `OpCtl::Ping` 처리).

#### C3 · C8 결정 기록

- **구조**: 세션 루프가 `break` 대신 종료 사유 `Exit`(`Peer` / `Abort` / `Close(CloseSpec)` / `TooBig`)를 반환하고, 종료 처리는 루프 밖 한 곳에서 한다.
- **정중한 종료 `close_session`**: ① 송신 큐를 닫고 이미 쌓인 메시지를 순서대로 전송 → ② 사유 통지(`sys.error`/`sys.kicked`) → ③ close 프레임 → ④ 피어의 close를 최대 1s 대기(깨끗한 TCP 종료). 쓰기는 `writer_send_timeout_ms` 하나의 예산을 공유하며, 막힌 피어는 포기한다.
  - 큐 플러시를 먼저 하는 이유: 통지가 그보다 먼저 큐에 들어간 메시지를 앞지르지 않게(순서 보장).
- **C8 — 제어 채널**: `Connection`에 `ctl: mpsc::Sender<CloseSpec>`(용량 1)을 추가. kick/replace/drain은 메시지 큐에 Close를 넣는 대신 `ctl`로 종료를 요청하고, 루프는 즉시 빠져나와 `close_session`을 실행한다. 피어가 close에 응답하지 않아도 최대 ~(writer timeout + 1s) 안에 끝난다(이전: idle 타임아웃까지 루프가 계속 돌며 메시지 처리 가능).
- **등록 해제 시점**: 종료가 끝날 때까지 세션은 레지스트리에 남는다. `main`의 drain 대기가 레지스트리를 보고 있어서, 먼저 해제하면 종료 핸드셰이크가 끝나기 전에 프로세스가 내려간다. 그 사이 라우팅된 메시지는 닫힌 큐가 거절한다(egress drop 카운트에 잡힘).
- **인증은 업그레이드 전**: 티켓 검증 실패 → HTTP **401** + `handshake_rejections{reason="auth_failed"}`. 인증 안 된 클라이언트에게 소켓 task를 할당하지 않는다.
  - 트레이드오프: 브라우저 `WebSocket` API는 실패한 핸드셰이크의 HTTP 상태를 읽을 수 없다(1006만 보임). 티켓은 보통 REST로 미리 발급받으므로 오류 처리는 그 단계에서 하는 게 일반적이라 판단.
  - 반면 **세션 한도 초과(deny)는 업그레이드 후 in-band**(`sys.error TOO_MANY_SESSIONS` + 1008): 이미 인증된 사용자에게 "다른 곳에서 접속 중" 같은 이유를 보여줄 수 있어야 하므로.
- **종료 코드**: 정상/idle 1000, drain 1001, 디코드 오류 1002, 정책·kick·replace·세션 한도 1008, 크기 초과 1009, 테넌트 용량 경합 1013. `docs/PROTOCOL.md`에 표로 정리.
- **C9는 별도**: 루프 내 응답이 여전히 `out_tx.send().await`를 쓴다. 다음 커밋에서 처리.

#### C2 결정 기록

- **원칙**: 레지스트리에서 세션 항목을 *실제로 제거한 쪽*이 게이지를 1 감소시킨다. kick 경로가 이미 제거·감소했으면 피해 세션의 RAII 정리(`SessionCleanup::drop`)는 `remove_session`이 `None`을 돌려주므로 감소하지 않는다.
- Presence 정리는 계속 무조건 수행한다. kick된 세션이 루프 종료 전에 다시 join했을 수 있기 때문(C8).
- 테스트 주의: 버그가 있어도 kick 직후엔 게이지가 1로 보인다. 이중 감소는 피해 세션의 정리가 돈 *뒤에* 일어나므로 테스트는 정리 시간을 기다린 후 판정한다.

### Phase 3 — 구조 / 테스트

- ✅ `run_session`(200줄+)을 테스트 가능한 단위로 분해(M0, ADR 0001·0005).
- ✅ 실제 WS 클라이언트로 붙는 통합 테스트 하네스(`crates/wsprism-gateway/tests/common`).

### Phase 4 — 기능

- ✅ 실제 인증(티켓·JWT·JWKS·connect 프록시·플러그인, ADR 0008·0012), ✅ `--config` CLI,
  ✅ 신뢰 프록시(`X-Forwarded-For`), ✅ 메트릭 단위 표준화(초, M6),
  ✅ README/PROTOCOL 문서와 실제 동작 정합(2026-10-21 전수 조사 반영).

### 배포 전 전체 리뷰 (2026-10-19)

코드 전체를 순회한 리뷰의 지적을 모두 반영했다(각 커밋에 테스트 포함).

| # | 결함 | 수정 |
|---|---|---|
| H1 | WS 클라이언트의 룸 이름 무검증 + 룸 상한 기본 무제한 → 인증된 클라이언트 하나가 presence 메모리를 무한히 키움(연결당 ~4 MB/s) | 이름 1-128바이트·제어 문자 금지(엔진 `join_room`·서버 API 공통), `max_rooms_per_user` 기본 64 |
| H2 | `/metrics`가 공개 포트에 노출(테넌트 이름·부하) | 내부 리스너로, `gateway.public_metrics`로만 공개 |
| H3 | SDK: 재접속 중 `getTicket` 실패 시 영구 정지 | `TICKET_FAILED` 이벤트 + 백오프 재시도 |
| M1 | RPC가 클라이언트가 보낸 `room`을 검증 없이 전달 | 입장한 룸만 전달(기본), 백엔드에 `joined` 제공 |
| M2 | `sid` 무검증(키·로그·응답에 그대로) | `[A-Za-z0-9_-]{1,64}`, 아니면 400 |
| M3 | 플러그인 인증기의 `user_id` 무검증 | 체인에서 일괄 검증. dev 인증기의 `user:` 접두사 제거 |
| M4 | `RealtimeCtx: Clone` → 동시 입장으로 presence 중복 멤버 | Clone 제거, `Publisher` 추가 |
| M5 | RPC 전역 동시 호출 상한 없음 | 테넌트별 `max_in_flight_total` |
| M6 | 인증이 용량 검사보다 먼저(티켓 소모·백엔드 호출 낭비) | 용량 먼저 |
| M7 | IPv6 /128 단위 핸드셰이크 제한(회피 쉬움), 방어기 기본 꺼짐 | /64 단위, 기본 켜짐 |
| D2 | SDK 오래된 프레임 판정 키에 svc 누락, 맵 무한 증가 | 키에 svc, 퇴장 시 정리 |
| D3 | Hot slot 키에 룸이 없어 서버 발행(sender 0)이 룸끼리 덮어씀, 퇴장 후 프레임 전송 | 키에 룸 id, 퇴장 시 purge |
| 추가 | dev 티켓 `dev:<id>`를 브라우저 서브프로토콜로 보낼 수 없음(`:`) | `dev.<id>` 형식 |
| 추가 | 공개 가이드(mdBook)가 존재하지 않는 설정 키를 문서화 | 정식 문서 링크로 재작성 |
| 저 | 세션 수 상한이 경쟁 시 둘 다 거절, 이그레스 카운터가 프로세스 전역 | CAS로 정확히, 인스턴스별 |

성능(P1-P6): 메시지당 메트릭 ~300 → ~11 ns, 조인한 룸 발행의 키 할당·권한 재검사 제거, 디스패처 무락, 테넌트 rate limit 무락 GCRA, 스위퍼를 블로킹 풀로, Hot 반향 끄기 옵션. 기능(D1): presence 이벤트(ADR 0010).

### M6 롤링 업데이트 검증 (2026-10-21)

카오스 테스트에 "노드를 차례로 SIGTERM·재시작"과 "Redis가 멈춘 순간의 정상 종료" 시나리오를 넣었다. 둘째가 결함을 찾았다.

| # | 결함 | 수정 |
|---|---|---|
| R6-1 | 정상 종료 때 `serve`가 끝나며 브로커 작업이 중단(abort)되어 송신 큐가 버려짐. Redis가 느린 순간이면 서버 API가 200으로 받은 발행과 닫힌 세션의 퇴장 기록이 사라짐(100개 중 30개 유실) | 드레인이 `drain_grace_ms` 안에서 큐가 넘어갈 때까지 기다림(`RealtimeCore::flush`). 드레인 중인 노드는 서버 API 발행에 503을 줘 백엔드가 다른 노드로 옮김. 회귀 테스트: 엔진 `flush_waits_for_accepted_frames`(처리 중 추적을 빼면 실패), 게이트웨이 `a_draining_node_refuses_publishes`, 카오스 2개 |

남은 점(문서화): Redis가 응답하지 않는 동안에는 노드가 시작하지 못하고 종료한다(오케스트레이터가 다시 띄움). `drain_grace_ms`(기본 2초) 안에 Redis가 돌아오지 않으면 남은 큐는 버려지고 경고 로그를 남긴다.

### CI fuzz job 첫 실행 실패 (2026-10-21)

`v/validation` 병합 때 fuzz job 5개가 모두 2초 만에 실패했다(다른 job은 통과). 이 job은 10-14에 추가됐지만 CI가 `main` push와 PR에서만 돌아 GitHub에서 처음 실행된 것이었다.

- **원인:** `taiki-e/install-action`이 설치하는 미리 빌드된 cargo-fuzz는 musl 바이너리다. cargo-fuzz는 `--target`이 없으면 자기가 빌드된 대상을 쓰므로 `x86_64-unknown-linux-musl`로 빌드했다. AddressSanitizer는 정적 libc와 함께 쓸 수 없고, musl 표준 라이브러리도 설치돼 있지 않았다. 로컬 cargo-fuzz는 `cargo install`로 받은 gnu판이라 재현되지 않았다.
- **수정:** `--target x86_64-unknown-linux-gnu`를 명시했다. 빌드와 실행을 다른 단계로 나눴다(환경 문제와 실제 크래시 구분). 실패하면 크래시 입력을 artifact로 남긴다.
- **재발 방지:** 작업 브랜치(`v/**`) push에도 CI가 돈다.
- 같이 정리: `actions/checkout@v5`(Node 20 종료 경고), `homepage`가 `repository`와 같아 생기는 cargo 경고(`homepage` 제거).

### 0.1.x: 무작위 장애 soak와 Redis TLS 실측 (2026-10-21)

| # | 결함 | 수정 |
|---|---|---|
| X1 | `rediss://`로 처음 연결하는 순간 rustls가 패닉: `wsprism-redis`의 의존성에 rustls 암호 공급자(ring·aws-lc-rs)가 하나도 켜져 있지 않거나, 앱이 다른 쪽을 켜면 둘 다라 기본 공급자를 고르지 못함. TLS를 실측한 적이 없어 드러나지 않았다(게이트웨이 빌드는 reqwest 덕에 ring만 켜져 동작) | `wsprism-redis`가 rustls `ring`에 직접 의존하고, TLS를 쓸 때 ring 공급자를 설치(이미 설치돼 있으면 그대로). 회귀 테스트: `wsprism-redis/tests/tls.rs`(공급자 설치를 빼면 패닉) |

- 함께 추가: `gateway.broker.tls`(사설 CA, 상호 TLS). 시험 환경 `bench/redis-tls-test.sh`(격리 네트워크·고정 IP, openssl로 만든 인증서, 클라이언트 인증서 필수인 TLS 전용 Redis)와 CI 단계.
- 무작위 장애 soak(`sdk/js/test/soak.mjs`): 판정기가 처음에는 너무 관대했다("수락 뒤 언제든 gap 알림이 있으면 유실 면제"). 그래서 되살린 롤백 결함을 놓쳤다. 면제 구간을 "수락부터 그 클라이언트가 다음 메시지로 넘어간 순간까지"로 좁힌 뒤에야 같은 결함을 클라이언트마다 67건 잡았다. 수정된 코드에서는 시드 1~3(각 3분, 장애 47~52회, 약 2만 1천 메시지) 모두 위반 0건. 15분 실행(시드 4): 장애 234회, 메시지 약 9만 5천, 재접속 261회, gap 알림 360회, 위반 0건.
- **미해결 관찰:** 같은 시드 4의 첫 15분 실행은 40분 제한에 걸려 출력 없이 끝났다. 그 실행 중에 게이트웨이 바이너리를 여러 번 다시 빌드하고 TLS 테스트를 동시에 돌렸으므로, 노드를 다시 띄우는 순간 바이너리가 교체 중이었을 가능성이 있으나 확인하지 못했다. soak에 감시 장치를 넣어(예상 시간 + 4분을 넘기면 노드·클라이언트 상태와 타임라인을 남기고 실패) 다시 돌렸고, 그 실행은 위 결과로 끝났다. 다시 멈추면 원인을 조사한다.

## 사각지대 (아직 검증하지 않은 것)

- (해소) 부하 테스트는 BENCHMARKS, 멀티 노드는 ADR 0011과 카오스 테스트, mdBook 잔여물은 문서 정리로 없어졌다.
- 남은 사각지대 (2026-10-21):
  - 클러스터 펜싱은 1차 조치뿐이다: 슬롯을 잃은 노드는 다음 heartbeat(`slot_ttl_s`/3)에서 알아채고 드레인 후 종료한다. 그 사이(기본 최대 약 10초)의 쓰기를 Redis가 거부하지는 않는다(ADR 0011 「출시 전 검토」).
  - 무작위 장애 soak는 프로세스·Redis 수준 장애만 섞는다. 노드와 Redis 사이의 네트워크 지연·비대칭 분할(tc/netem)은 아직 넣지 않았다.
  - 테넌트 간 공정성: 송신 큐가 테넌트 구분 없는 FIFO다. 한 테넌트가 몰아칠 때 다른 테넌트의 지연을 재지 않았다.
- **간헐 실패 (2026-10-19) — 원인 확정·수정:** `tests/jwt.rs::invalid_tokens_are_rejected`의 "bad signature" 토큰이 가끔 **통과**했다(두 번째 발생 때 메시지 보존). 제품 결함이 아니라 테스트 결함: 서명의 마지막 글자를 `'A'`로 바꿔 위조했는데, 이미 `'A'`였거나 마지막 글자의 하위 비트(base64url 패딩 비트, HS256 서명 43자의 마지막 2비트)만 달라 같은 서명으로 디코딩되는 경우가 있었다. 서명 가운데 글자를 확실히 다른 값으로 바꾸도록 수정. 첫 발생 때의 "메모리 압박 타임아웃" 가설은 틀렸다.
