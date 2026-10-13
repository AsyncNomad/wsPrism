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
| S1 | ☐ | Hot Lane에 연결 단위 레이트리밋 미적용 (Text 분기에서만 검사) | `transport/ws.rs` |
| S2 | ☐ | WS `max_message_size` 미설정(axum 기본 64MB) + `check_len`이 디코드 *후* 실행 → 대용량 JSON 파싱 DoS | `transport/ws.rs`, `transport/codec.rs` |
| S3 | ☐ | `chat.send`가 발신자의 룸 멤버십을 검사하지 않음 | `services/chat.rs` |
| S4 | ☐ | 클라이언트 지정 `sid` 충돌 시 기존 세션 덮어쓰기 → 테넌트 카운터 누수, 기존 연결 고아화 | `realtime/core/session_registry.rs` |
| S5 | ☐ | 핸드셰이크 거절 메트릭 라벨에 미검증 `tenant` 사용 → 라벨 카디널리티 폭발(메모리 DoS) | `transport/ws.rs` |

### Phase 2 — 정확성

| ID | 상태 | 내용 | 위치 |
|---|---|---|---|
| C1 | ☐ | Reliable 발행이 발신 세션 루프를 최대 timeout×1 동안 블로킹 (head-of-line) | `services/chat.rs`, `realtime/core/realtime.rs` |
| C2 | ☐ | kick_oldest 시 `ws_active_sessions` 게이지 이중 감소 | `transport/ws.rs` |
| C3 | ☐ | `on_exceed: deny` 에러 메시지가 writer 루프 시작 전에 큐잉되어 유실 / 인증 실패가 업그레이드 후 판정 | `transport/ws.rs` |
| C4 | ☐ | `room:leave`가 active_room만 떠남 → 다른 룸 탈출 불가 | `transport/ws.rs` |
| C5 | ☐ | Presence `leave`의 empty 판정과 `remove` 사이 경쟁 → 신규 join 멤버십 소실 | `realtime/core/presence.rs` |
| C6 | ☐ | 경미: span guard를 await 너머로 유지, Ext `v` 미검증, ping 이중 응답, 토큰버킷 소수 시간 버림 | 여러 곳 |

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
