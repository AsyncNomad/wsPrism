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
- **MSRV 미선언**: clippy 제안으로 `u32::is_multiple_of`(Rust 1.87+)를 쓰게 됐다. 현재 CI는 stable만 추적한다. MSRV가 필요해지면 `rust-version`을 선언하고 CI 매트릭스에 추가한다.

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
| C6 | ◐ | 경미: span guard를 await 너머로 유지, Ext `v` 미검증, ~~ping 이중 응답~~(C9에서 제거), 토큰버킷 소수 시간 버림 | 여러 곳 |

#### C1 결정 기록

- **재현 조건이 중요했다**: 읽기를 *완전히 멈춘* 수신자로는 재현되지 않는다(서버 writer 타임아웃 1.5s가 그 수신자를 끊어 금방 풀림). 문제는 **느리지만 꾸준히 읽는** 수신자(모바일 등): writer가 조금씩 진행해 타임아웃이 안 걸리는 동안 큐가 계속 차 있어, 발신자(및 룸 전체)가 가장 느린 멤버의 속도로 끌려간다. 50ms마다 1개씩 읽는 수신자로 3/3 재현(메시지 198개째에서 8s 초과).
- **기존 "Reliable"의 실제 의미**: 수신자 큐를 최대 1.5s 기다린 뒤 **조용히 버림**. 발신자를 막으면서도 신뢰성은 보장하지 못했다.
- **새 의미**: `QoS::Reliable` = *순서대로 빠짐없이 전달하거나, 불가능하면 수신자를 `slow_consumer`(1008)로 끊는다*. 발신자는 절대 기다리지 않는다. `QoS::Lossy`는 큐가 차면 그 수신자에게만 버린다. 끊긴 수신자는 재접속으로 상태를 다시 맞춘다.
  - 기각: *팬아웃을 별도 task로 spawn* — 발신자는 안 막히지만 같은 발신자의 연속 메시지 순서가 수신자별로 뒤섞일 수 있고, 느린 수신자 문제(무한 대기 task 누적)는 그대로.
  - 기각: *발신자별 순차 팬아웃 워커* — 순서는 지키지만 구조가 크고, 결국 느린 수신자에 대해 같은 선택(버림/끊음/대기)을 해야 한다.
- **통합**: 모든 송신 경로(`send_to_user`, `send_to_session`, `publish_room*`)가 `deliver` 하나를 거치며 `out.qos`를 따른다. (이전엔 `send_to_user`/`send_to_session`이 QoS를 무시하고 항상 lossy.) `publish_room_lossy`/`_reliable`은 QoS를 강제하는 래퍼로 유지.
- **API 변화**: `QoS::Reliable { timeout_ms }` → `QoS::Reliable`(단위 variant), `publish_room_reliable`은 더 이상 `async`가 아니다.
- **메트릭 변화**: `wsprism_egress_send_fail_total` 제거 → `wsprism_egress_slow_consumer_closes_total`(reliable 메시지 때문에 끊은 수신자 수). 대시보드가 있다면 갱신 필요.

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

- `run_session`(200줄+)을 테스트 가능한 단위로 분해.
- 실제 WS 클라이언트로 붙는 통합 테스트 하네스.

### Phase 4 — 기능

- 실제 인증(현재 `ticket == "dev"` 스텁), `--config` CLI, 신뢰 프록시(`X-Forwarded-For`),
  메트릭 단위 표준화(초), README/PROTOCOL 문서와 실제 동작 정합.

## 사각지대 (아직 검증하지 않은 것)

- 부하/동시성 테스트 없음. "best-effort" 한도 초과폭이 실제로 얼마인지 측정한 적 없다.
- 단일 노드 전제. 멀티 노드(룸 브로드캐스트 공유) 설계는 범위 밖.
- `docs/src/https:/...index.html`은 SUMMARY의 외부 링크 때문에 mdBook이 만든 잔여물로 보인다. 문서 정리(Phase 4)에서 처리.
