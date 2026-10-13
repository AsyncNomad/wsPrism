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
| S4 | ☐ | 클라이언트 지정 `sid` 충돌 시 기존 세션 덮어쓰기 → 테넌트 카운터 누수, 기존 연결 고아화 | `realtime/core/session_registry.rs` |
| S5 | ☑ | 핸드셰이크 거절 메트릭 라벨에 미검증 `tenant` 사용 → 라벨 카디널리티 폭발(메모리 DoS) | `transport/ws.rs` |

#### S3 결정 기록

- **수정**: `chat.send`는 발신 *세션*이 해당 룸에 join한 경우에만 발행하고, 아니면 `NOT_ALLOWED`. `Presence::is_member` / `RealtimeCtx::is_in_room` 추가.
- **세션 단위 vs 유저 단위**: 세션 단위로 판정한다. 라우팅(Presence)이 세션 단위이고, 같은 유저의 다른 탭이 join했다고 이 탭이 그 룸에 말할 수 있게 하면 "받지 못하는 룸에 쓰기"가 가능해진다.
- Hot Lane(echo)은 join 성공 시에만 설정되는 `active_room`으로만 발행하므로 별도 검사 불필요. 단, 강제 종료(kick)된 세션이 루프를 계속 도는 동안 로컬 `active_room`으로 발행할 수 있는 문제는 C8로 별도 추적.

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
| C1 | ☐ | Reliable 발행이 발신 세션 루프를 최대 timeout×1 동안 블로킹 (head-of-line) | `services/chat.rs`, `realtime/core/realtime.rs` |
| C2 | ☑ | kick_oldest 시 `ws_active_sessions` 게이지 이중 감소 | `transport/ws.rs` |
| C3 | ☐ | "에러를 큐에 넣고 `break`"하는 모든 경로에서 에러가 클라이언트에 도달하지 않음: `on_exceed: deny`(writer 루프 시작 전 큐잉), policy `Close`, 디코드 오류, idle 타임아웃(큐잉 직후 루프 탈출 → out 큐 미플러시). 인증 실패도 업그레이드 *후* 판정 | `transport/ws.rs` |
| C4 | ☐ | `room:leave`가 active_room만 떠남 → 다른 룸 탈출 불가 | `transport/ws.rs` |
| C5 | ☐ | Presence `leave`의 empty 판정과 `remove` 사이 경쟁 → 신규 join 멤버십 소실 | `realtime/core/presence.rs` |
| C8 | ☐ | 강제 종료(kick/replace)된 세션의 루프가 클라이언트가 닫을 때까지 계속 돈다 → Presence에선 지워졌지만 로컬 `active_room`으로 Hot 발행·재join 가능. 서버 측 종료 신호 필요 | `transport/ws.rs`, `realtime/core/session_registry.rs` |
| C6 | ☐ | 경미: span guard를 await 너머로 유지, Ext `v` 미검증, ping 이중 응답, 토큰버킷 소수 시간 버림 | 여러 곳 |

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
