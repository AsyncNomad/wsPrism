# ADR 0009: 크레이트 분리와 공개 API 정책 (M2-9)

- 상태: **채택 (2026-10-16)** — 사용자 검토: 결정 1~4 승인("진행"), 5 배포 의향 있음. 단계별 구현(§7): S1 ✅ S2 ✅
- 관련: [ADR 0008](0008-m2-lite-integration.md) §9 M2-9, [ADR 0005](0005-session-transport-seam.md)(세션↔전송 경계), [ADR 0004](0004-product-direction.md)(쉬운 커스텀), [ROADMAP](../ROADMAP.md)

## 배경 (조사한 사실)

- 크레이트: `wsprism-core`(프로토콜 475줄), `wsprism-gateway`(나머지 전부 약 8,000줄, 라이브러리와 바이너리), `wsprism`(파사드: `pub use` 두 줄), `wsprism-loadgen`.
- 게이트웨이의 모듈 16개가 **전부 `pub mod`**다. 외부에서 쓰는 곳은 테스트·벤치·fuzz뿐이다. 실제로 쓰는 경로는 `AppState::new`, `config::load_from_str`, fuzz 대상 3개(`TrustedProxies`, `OriginPolicy`, `subprotocol`), 벤치(`RealtimeCore`, `Connection`, `Member`, `SessionSignals`, `TenantPolicyRuntime`, `GatewayMetrics`, `TenantLimits`)다.
- **확장할 방법이 없다.** 서비스 등록이 `AppState::new` 안에 하드코딩되어 있다(`RoomService`, `ChatService`, `EchoBinaryService(1)`). "쉬운 커스텀"(제품 목표)을 내걸었지만, 지금 사용자가 자기 Rust 서비스를 붙이려면 소스를 고쳐야 한다. 인증기도 `AuthConfig`가 닫힌 enum이라 마찬가지다.
- 모듈 의존을 측정했다. `realtime`이 바깥에 기대는 곳은 셋뿐이다: `auth::Permissions`, `config::schema::TenantLimits`(룸 상한 4개 필드), `sys`(메시지 빌더). `dispatch`·`services`는 `realtime`에만 의존한다. 반대로 `transport`(세션 루프 포함)는 `app_state`·`policy`·`obs`·`rpc` 전부에 의존한다.
- 세션 루프(`transport::session`)는 ADR 0005의 `SessionTx/SessionRx` 경계 덕에 WebSocket과 무관하다. 하지만 `AppState`를 통째로 받는다.
- 이름 문제: `EchoBinaryService`는 echo가 아니라 **룸 relay**다. `BinaryService`는 문서와 프로토콜이 쓰는 "Hot lane"과 용어가 다르다.
- release 프로필에 LTO 설정이 없다(`codegen-units` 기본 16). 아직 crates.io에 배포한 적이 없다(0.1.0). 그래서 이름을 바꾸는 비용은 지금이 가장 싸다.

## 목표와 비목표

**목표**
1. **계층을 컴파일러로 강제한다.** 엔진(룸·전달·서비스)이 설정 형식·HTTP·인증 구현을 import하는 순간 빌드가 깨지게 한다.
2. **약속하는 API를 작고 명시적으로 만든다.** 공개 항목은 목록(§4)에 있는 것만 둔다. 나머지는 바깥에서 보이지 않거나, 불안정하다고 명시한다.
3. **확장 경로를 연다.** 사용자가 소스를 고치지 않고 자기 서비스와 인증기를 등록한 바이너리를 만들 수 있게 한다.
4. **동작·성능을 바꾸지 않는다.** 모든 테스트, SDK·예제 E2E, `hotpath` 벤치가 그대로 통과해야 한다.

**비목표:** 세션 루프 일반화(§5-3), crates.io 배포 자체, WASM 플러그인, gRPC.

## 결정

### 1. 크레이트 4개 (안정성 경계로 나눈다)

```
wsprism-protocol   와이어 계약: Envelope, HotFrame(v1·v2 코덱), ProtocolVersion, ClientCode, 오류, 종료 코드 매핑
      ↑                (의존: serde, serde_json, bytes, thiserror — tokio 없음)
wsprism-engine     실시간 엔진: 룸·멤버·전달·Hot 슬롯·틱, RealtimeCtx, DeferredReply, 서비스 트레이트,
      ↑            Dispatcher, 권한 모델, 내장 서비스(room·chat·hot relay)
      |                (의존: protocol, tokio(sync·time·rt), dashmap, serde_json, tracing — axum·설정 형식·HTTP 클라이언트·암호 없음)
wsprism-gateway    서버: 설정(YAML 스키마), 핸드셰이크·admission, WebSocket 전송, 세션 루프, 정책 런타임,
      ↑            인증 구현(dev·ticket·jwt·proxy), 서버 API, RPC 프록시, 메트릭, GatewayBuilder, 바이너리
wsprism            파사드: 사용자가 의존하는 유일한 크레이트. 안정 API만 재수출 + prelude
```

- 분할 기준은 **크기가 아니라 바뀌는 속도와 이유**다.
  - protocol은 와이어가 바뀔 때만 바뀐다(가장 느림). 앞으로 나올 Rust 클라이언트 SDK·loadgen·fuzz는 이 크레이트만 쓰면 된다.
  - engine은 실시간 의미론(전달 등급, conflation)이 바뀔 때 바뀐다.
  - gateway는 운영 기능(인증 방식, API, 설정)이 바뀔 때 바뀐다(가장 빠름).
- `wsprism-core` → **`wsprism-protocol`으로 이름을 바꾼다**. 하는 일이 이름에 드러나게 하려는 것이다. 배포 전이라 비용이 거의 없다.
- 와이어 계약인 **v2 종료 코드 매핑**(`wire_close_code`)은 지금 gateway의 세션 코드에 있는데, protocol로 옮긴다. SDK·문서와 같은 표를 한 곳에서 관리하기 위해서다.

### 2. 경계에서 생기는 타입 이동

| 지금 | 결정 | 이유 |
|---|---|---|
| `config::schema::TenantLimits`(serde) | engine에는 serde 없는 `RoomLimits` 평범한 구조체를 두고, gateway 설정 타입이 `From`으로 변환한다 | 설정 YAML이 계약이고, 엔진 API는 따로 진화한다. 필드 4개라 중복 비용이 작다 |
| `auth::Permissions`, `RoomPattern` | engine `permissions` | 검사 지점이 `RealtimeCtx`(엔진)다. 인증 구현은 gateway에 남는다 |
| `sys` 빌더 | engine. 외부에는 §4-3 SPI로만 연다 | realtime이 쓰고, gateway(authed·error)도 쓴다 |
| `Authenticator` 트레이트·`Identity`·`AuthRequest`·`AuthError` | gateway `auth` (안정 API) | 인증은 admission(서버) 관심사이고, 엔진은 `Permissions`만 알면 된다 |
| `DeferredReply` | engine | 이미 `RealtimeCtx`의 일부다 |

### 3. 확장 경로: `GatewayBuilder` (이번 단계에 포함)

공개 API를 줄이기만 하면 "쉬운 커스텀"이 더 멀어진다. 그래서 줄이는 것과 **유일한 확장 입구를 만드는 것**을 같은 단계에서 한다.

```rust
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> wsprism::Result<()> {
    wsprism::Gateway::builder()
        .config_file("wsprism.yaml")?           // 또는 .config(GatewayConfig)
        .text_service(MatchService::new())      // 사용자 Ext 서비스
        .hot_service(PhysicsService::new(7))    // 사용자 Hot 서비스 (svc_id 7)
        .auth_plugin("corp_sso", |opts| Ok(Arc::new(CorpSso::from_options(opts)?)))
        .build()?                               // 설정 검증 + 정책 컴파일 + 키 로딩 (지금의 AppState::new)
        .serve()                                // 설정의 listen·api.listen 바인드, SIGTERM 드레이닝
        .await
}
```

- `build()`는 지금 `AppState::new`가 하는 일(검증·컴파일·allowlist↔서비스 대조)을 **사용자 서비스까지 포함해서** 한다. allowlist가 등록되지 않은 서비스를 가리키면 지금처럼 기동이 실패한다.
- **사용자 인증기 설정:** `- kind: plugin` + `name: corp_sso` + `options: {...}` 형태로 쓴다. 내장 `kind`는 계속 `deny_unknown_fields`로 엄격하게 검사한다. 그래서 `kind: jtw` 같은 오타는 여전히 기동 오류가 된다. `options`는 플러그인 팩토리에 넘기는 자유 형식 JSON 값이다. 등록되지 않은 `name`도 기동 오류다.
- **내장 서비스 이름은 예약한다**(`room`, `chat`, `sys`, Hot svc 1). 사용자 서비스가 같은 이름을 쓰면 `build()`가 실패한다. 조용히 덮어쓰는 것보다 낫다.
- **dogfooding:** 배포 바이너리 `main.rs`를 이 빌더만으로 다시 쓴다. 공개 API로 서버 하나를 끝까지 만들 수 없다면 공개 API가 부족하다는 뜻이다.
- 테스트 헬퍼(`tests/common`)도 빌더를 쓰도록 바꾼다. `AppState`는 `pub(crate)`가 된다.

### 4. 공개 범위 3등급

**4-1. 안정 API** (`wsprism` 파사드에서 재수출. 0.x 동안 깨는 변경은 minor 버전에서만, CHANGELOG에 기록)

| 크레이트 | 항목 |
|---|---|
| protocol | `Envelope`, `HotFrame`, `ProtocolVersion`, `ClientCode`, `WsPrismError`, `Result` |
| engine | `TextService`, **`HotService`**(← `BinaryService`), `RealtimeCtx`(아래 메서드만), `Outgoing`, `Payload`, `QoS`, `DeferredReply`, `Permissions`, `RoomPattern`, `RoomLimits`, 내장 `RoomService`·`ChatService`·**`HotRelayService`**(← `EchoBinaryService`) |
| gateway | `Gateway`, `GatewayBuilder`, `GatewayConfig`와 `load_from_file`·`load_from_str`, `Authenticator`, `AuthRequest`, `AuthError`, `Identity`, `AuthPluginFactory` |

`RealtimeCtx`에서 공개하는 메서드는 `tenant`, `user`, `session_id`, `protocol`, `active_room`, `is_in_room`, `request_seq`, `trace_id`, `reply`, `send_to_session`, `publish_room_reliable`, `publish_room_lossy`, `join_room`, `leave_room`, `defer_reply`다. 지금 `pub`인 필드 `trace_id`는 메서드로 바꾸고, `session_key`·`user_key`(내부 키 형식)는 숨긴다.

**4-2. 불안정 SPI** (`wsprism_engine::spi`. gateway와 앞으로의 다른 전송이 쓰는 "서버 제작자용" 경계. 문서 첫 줄에 "semver 대상 아님"을 명시하고, 파사드에서는 재수출하지 않는다)

`RealtimeCore`(`sessions`·`presence` 필드는 메서드 뒤로 숨김), `SessionRegistry`, `Presence`, `Connection`, `SessionSignals`, `CloseRequest`, `CloseSpec`, `OutFrame`, `HotOut`, `HotSlot`, `CtxIdentity`, `Dispatcher`, `sys`, `now_ms`, 이그레스 카운터.

**4-3. 내부 전용** (`pub(crate)`. 벤치·fuzz는 `internals` cargo feature로만 접근)

gateway의 나머지 전부: `app_state`, `transport::*`, `policy::*`, `obs`, `api`, `rpc`, `backend`, 인증 구현체 4종, `server`, `router`, `ops`, `context`, `ident`.
- `internals` feature를 켜면 `#[doc(hidden)] pub mod __internal`이 fuzz·벤치가 쓰는 타입만 재수출한다. fuzz 크레이트와 `hotpath` 벤치는 `required-features`로 이 feature를 켠다.

**강제 장치**
- protocol·engine·gateway 모두 `#![warn(unreachable_pub)]`(CI에서 `-D warnings`라 사실상 오류)를 켠다. 안정 API 크레이트에는 `#![warn(missing_docs)]`도 켠다.
- 공개 enum과 커질 수 있는 공개 구조체에는 `#[non_exhaustive]`를 붙인다: `ClientCode`, `WsPrismError`, `QoS`, `Payload`, `ProtocolVersion`, `AuthError`, `GatewayConfig` 계열(설정 구조체는 역직렬화로만 만들어진다). 이번 세션에서 `ClientCode::Unavailable`을 추가한 것이 바로 이것이 필요한 사례다.

### 5. 호환성·버전 정책

1. **와이어 계약이 Rust API보다 우선이다.** 아래 계약은 **추가만 허용**하고, 바꾸려면 새 버전을 낸다.
   - 프로토콜 v2(봉투 필드, Hot 프레임, 종료 코드 범위)
   - 서버 API `/api/v1/*`, RPC·connect 프록시 계약
   - 설정 `version: 1`. 모르는 필드는 거부하므로, 새 필드는 기본값이 있는 선택 필드로만 추가한다.

   Rust API는 0.x 동안 minor 버전에서 깨질 수 있다. 와이어는 0.x라도 v2를 공개한 뒤에는 깨지 않는다.
2. **버전은 모든 크레이트가 같이 움직인다.** 워크스페이스 `version` 하나로 함께 배포한다. 크레이트별 semver를 따로 관리하는 비용을 얻는 것이 없다. 사용자는 `wsprism`만 의존한다.
3. **세션 루프는 아직 engine으로 옮기지 않는다.** 두 번째 전송(WebTransport, 단계 T)이 생길 때 옮긴다(rule of two). 지금 일반화하면 `SessionEnv`(정책·디스패치·메트릭·RPC 묶음)의 모양을 전송 하나만 보고 추측하게 된다. 그동안 세션 루프는 gateway에 있고, engine에는 SPI로만 닿는다.
4. **MSRV**: `rust-version`을 명시하고 CI에 그 버전 작업을 추가한다. 규칙은 "적어도 6개월 된 stable"(tokio 관례)이다. 정확한 숫자는 구현 때 해당 툴체인으로 실제 빌드해 보고 정한다. 툴체인 다운로드가 필요해서, 그때 승인을 받는다.
5. **폐기(deprecation):** 1.0 이후에는 `#[deprecated]`로 한 minor 버전을 거친 뒤에 제거한다. 0.x에서는 CHANGELOG 기록만 한다.
6. `cargo-semver-checks` CI 도입은 첫 crates.io 배포 직후에 한다. 배포본이 있어야 비교 기준이 생긴다.

### 6. 성능 가드

- 크레이트 경계를 넘는, 제네릭이 아닌 작은 함수는 LTO가 없으면 인라인되지 않을 수 있다. 전달 경로(`deliver`, `HotSlot::put`, 서명·카운터)가 경계를 넘는다.
- **통과 기준:** 분리 전후로 `hotpath` criterion을 같은 세션에서 번갈아 측정해, 노이즈 범위 안이어야 한다. 넘으면 해당 함수에 `#[inline]`을 붙이고 다시 측정한다.
- **S2 측정 결과 (2026-10-16~19):**
  - `hotpath`를 분리 전 커밋(worktree)과 같은 세션에서 번갈아 3회씩 측정했다. policy·metrics는 변화 없음. **`publish_room` 팬아웃은 +8~12%**(10명 363→399 ns, 100명 3.32→3.74 µs, 1000명 36.0→38.9 µs, 수신자당 약 3 ns)로 일관되게 회귀했다.
  - 원인(perf): 우리 코드가 아니라 **tokio `mpsc` 송신 경로(제네릭)의 인라인 결정이 codegen unit 분할에 따라 달라졌다.** 분리 전에는 한 심볼에 합쳐져 있던 `chan::Tx` 송신이, 분리 후에는 `list::Tx::push`가 따로 떨어지고 `deliver` 비중이 1.96%→6.63%로 커졌다.
  - `deliver`에 `#[inline]` → 효과 없음(되돌림). `codegen-units = 1`(양쪽) → 팬아웃 차이 1~3%로 사라짐. 그러나 release에 cgu=1을 넣으면 끊기기 전 2회 측정에서 policy·metrics가 7~10% 느려지는 신호가 있었고, 측정 중 다른 프로세스(Dart LSP 11 GB)로 메모리가 꽉 차 있었다(빌드 OOM 1회) → 근거가 엇갈려 **빌드 프로필은 바꾸지 않았다.**
  - **결정: 회귀를 기록하고 성능 단계로 넘긴다.** 이것은 크레이트 구조가 아니라 빌드 설정(cgu·LTO·PGO)의 문제이고, cgu=1로 해소되는 것을 확인했다. 성능 단계에서 cgu·`lto`를 깨끗한 환경에서 전체 벤치 + 실부하(`run-ab-gateway.sh`)로 정한다. 그 전까지 S3 이후 단계의 벤치 기준선은 S2 이후 값으로 한다.
- release 프로필에 `lto = "thin"`을 넣을지는 이번에 정하지 않는다. 성능 작업 단계(사용자 결정: "성능은 나중에")에서 측정과 함께 정한다. 이번 단계에서는 "바뀌지 않았음"만 증명한다.

### 7. 작업 단계 (단계마다 커밋, 동작 변화 없음, 전체 게이트)

| 단계 | 내용 |
|---|---|
| S1 ✅ | `wsprism-core` → `wsprism-protocol` 이름 변경, 종료 코드 매핑을 protocol로 이동 (지난 기록 문서·SVG 프로파일의 옛 이름은 역사 기록이라 그대로 둔다) |
| S2 ✅ | `wsprism-engine` 생성: realtime·dispatch·services·permissions·sys 이동, `RoomLimits` 분리(테넌트별 `Arc` 공유 — 세션마다 하던 `TenantLimits` 복제·할당 제거), 이름 변경(`HotService`·`handle_hot`, `HotRelayService`). 엔진 의존성에 axum·hyper·reqwest·ring·jwt·YAML·rustls 없음(`cargo tree` 확인). **벤치: §6 측정 결과 참고 — 팬아웃 +8~12% 미해결** |
| S3 | 공개 범위 정리: `unreachable_pub`, `non_exhaustive`, `spi`, `internals` feature, fuzz·벤치 경로 정리 |
| S4 | `GatewayBuilder` + `kind: plugin` + 예약 이름 검사. `main.rs`와 테스트 헬퍼를 빌더로 전환 |
| S5 | 파사드 `wsprism`(prelude), `docs/API.md`(공개 목록과 등급), 호환성 정책 문서, 사용자 서비스 예제(`examples/custom-service`: 빌더 + Ext 서비스 하나 + 테스트) |
| S6 | MSRV 확정 + CI 작업 (툴체인 다운로드 승인 후) |

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| 크레이트 하나로 두고 `pub(crate)`만 정리 | 계층 위반을 컴파일러가 못 막는다(지금도 realtime이 config를 import한다). 엔진만 쓰려는 사람(사용자 전송, 시뮬레이터)도 axum·reqwest·jsonwebtoken을 끌고 온다 |
| 더 잘게 나누기(auth·transport·api 각각 크레이트) | 따로 쓰려는 소비자가 없는데, 경계마다 `pub`이 새고 버전 관리 비용이 커진다. 경계는 "따로 쓰일 이유"가 있을 때만 긋는다 |
| 세션 루프를 지금 engine으로 이동 | 전송이 하나뿐이라 추상화 모양을 추측하게 된다. WebTransport 때 두 구현을 보고 옮긴다 |
| 내부 항목을 `#[doc(hidden)] pub`으로만 숨김 | 문서에 안 보일 뿐 누구나 의존할 수 있어, 모르는 사이에 계약이 된다. SPI는 이름 있는 모듈 + "불안정" 명시, 벤치·fuzz용은 명시적 feature로 한다 |
| 크레이트별 독립 semver | 내부 크레이트 조합 호환표를 관리해야 한다. 사용자는 파사드만 쓰므로 얻는 것이 없다 |
| 사용자 인증기를 `kind: <아무 이름>`으로 | 내장 kind의 오타를 잡지 못하게 된다. `kind: plugin`으로 열어 두는 곳을 하나로 모은다 |
| 빌더 없이 `AppState::new` 공개 | 등록 순서·검증·예약 이름 규칙이 사용자 코드에 흩어진다. 진입점 하나가 불변식을 지킨다 |

## 검토해 주셨으면 하는 결정

1. **크레이트 이름 변경**(`wsprism-core` → `wsprism-protocol`, `BinaryService` → `HotService`, `EchoBinaryService` → `HotRelayService`). 배포 전이라 지금이 가장 싸다.
2. **`GatewayBuilder`를 M2-9 범위에 넣는 것.** 범위가 커지지만, 이것 없이 공개 API만 줄이면 확장할 방법이 없다.
3. **`kind: plugin` 방식의 사용자 인증기 설정.**
4. **세션 루프 이동 보류**(WebTransport 때).
5. **crates.io 배포 의향.** 배포할 계획이면 이름(`wsprism`, `wsprism-*`)을 쓸 수 있는지 확인해야 한다(네트워크 필요). 배포하지 않으면 §5-6은 빠진다.
   - **결정: 배포 의향 있음.** 2026-10-16 crates.io API 조회 결과 `wsprism`, `wsprism-protocol`, `wsprism-engine`, `wsprism-gateway`, `wsprism-loadgen` 모두 미등록(404). 선점 규칙이 없으므로 배포 직전에 다시 확인한다. §5-6(`cargo-semver-checks`)은 첫 배포 뒤 도입한다. 배포 전 할 일(별도 단계): 각 크레이트 `description`·`readme`·`keywords`·`categories`, `publish = false`(loadgen·fuzz·spikes), 파사드 문서.
