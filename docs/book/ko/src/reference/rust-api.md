# Rust API

wsPrism은 바이너리로만 돌려도 되고, 그러면 Rust를 만질 일이 없습니다. 이 장은 게이트웨이 안에 코드를 넣고 싶을 때를 위한 것입니다. HTTP 왕복 대신 마이크로초 단위로 답하는 서비스, 사내 SSO용 인증기, 또는 더 큰 프로그램 안에 게이트웨이를 넣는 경우입니다.

생성된 API 문서는 이 문서와 함께 [`/api/`](../../api/wsprism/index.html)에 게시됩니다.

## 어느 크레이트를 쓰나

**`wsprism`** 하나에만 의존하세요. 다른 크레이트의 안정 API를 다시 내보냅니다.

| 크레이트 | 내용 | 직접 의존? |
|---|---|---|
| `wsprism` | 파사드: 안정 API와 `prelude` | 예 |
| `wsprism-gateway` | 서버: 설정, 핸드셰이크, 전송, 인증, 서버 API, 바이너리 | 아니요 |
| `wsprism-engine` | 룸, 전달, Hot 상태, 서비스 트레이트 | SPI 위에 서버나 전송을 직접 만들 때만 |
| `wsprism-protocol` | 와이어 형식 | 프로토콜을 쓰는 클라이언트나 도구를 만들 때만 |
| `wsprism-redis` | Redis 클러스터 브로커 | 아니요. `redis` 기능(기본으로 켜짐)으로 들어옵니다 |

모든 크레이트는 버전 번호 하나를 함께 쓰고 함께 릴리스합니다. `wsprism`에 `default-features = false`를 주면 Redis 없이 빌드합니다.

## 내 코드가 들어간 게이트웨이

```rust
use std::sync::Arc;
use wsprism::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    Gateway::builder()
        .config_file("wsprism.yaml")?
        .text_service(MyService::default())        // Ext 레인: svc "my"
        .hot_service(MyPhysics::new(7))             // Hot 레인: svc id 7
        .auth_plugin("corp_sso", |options| Ok(Arc::new(CorpSso::from_options(options)?)))
        .build()?                                   // 모두 검증. 아직 아무것도 바인딩하지 않음
        .serve()                                    // 리스너를 열고, SIGTERM이면 드레인
        .await
}
```

바이너리에는 `macros`, `rt-multi-thread` 기능을 켠 `tokio`도 필요합니다.

`build()`는 설정을 등록한 것과 대조합니다. 등록하지 않은 서비스를 적은 허용 목록, 모르는 플러그인을 가리키는 설정, 내장 이름(`room`, `chat`, `sys`, Hot ID `1`)으로 등록한 서비스는 실행 중이 아니라 여기서 실패합니다.

## 텍스트 서비스

```rust
use wsprism::prelude::*;
use wsprism::serde_json::json;

struct Echo;

#[async_trait]
impl TextService for Echo {
    fn svc(&self) -> &'static str { "echo" }

    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        ctx.reply(json!({"v": 1, "svc": "echo", "type": "said", "data": env.data}))
    }
}
```

`handle`은 세션 컨텍스트와 봉투를 받습니다. 서비스는 컨텍스트로 다음을 할 수 있습니다.

- `reply(...)`: 요청한 쪽에 답합니다(게이트웨이가 `reply_to`를 붙입니다).
- 룸에 발행합니다(`publish_room_reliable`, `publish_room_lossy`, `publish_room_others`). 사용자의 발행 권한을 따릅니다.
- 사용자나 세션 하나에 보냅니다(`send_to_user`, `send_to_session`).
- 세션을 대신해 룸에 들어가고 나옵니다(`join_room`, `leave_room`). 테넌트 한도를 따릅니다.
- 세션이 누구인지 읽습니다: `tenant()`, `user()`, `session_id()`, `trace_id()`, `is_in_room(room)`.
- 테넌트에 프레즌스 이벤트가 켜져 있으면 `room_members(room)`을 읽습니다.

`Err`를 돌려주면 클라이언트는 그에 맞는 코드의 `sys.error`를 받습니다(`WsPrismError::NotAllowed`는 `NOT_ALLOWED` 하는 식입니다).

컨텍스트는 가변 참조로 빌려주며 `Clone`이 아닙니다. 룸 멤버십은 세션 자신의 태스크에서만 바뀌기 때문이고, 그 덕에 핫 패스에서 멤버십에 락이 필요 없습니다. 타이머나 외부 호출 뒤에 나중에 답하려면 `defer_reply(...)` 핸들을, 다른 태스크에서 발행하려면 `publisher()`를 받아 두세요.

서비스 인스턴스 하나가 모든 테넌트를 처리합니다. 상태를 들고 있다면 `ctx.tenant()`를 키에 넣으세요.

바로 돌려 볼 수 있는 완전한 예제가 저장소의 `crates/wsprism/examples/room_counter.rs`에 있습니다.

```bash
cargo run -p wsprism --example room_counter -- my.yaml
```

## Hot 서비스

```rust
#[async_trait]
impl HotService for MyPhysics {
    fn svc_id(&self) -> u8 { self.id }

    async fn handle_hot(&self, ctx: &mut RealtimeCtx, frame: HotFrame) -> Result<()> {
        // 검증하고, 시뮬레이션하고, 권위 있는 상태를 발행합니다
        Ok(())
    }
}
```

Hot 서비스는 자기 ID로 향하고 테넌트의 `hot_allowlist`를 통과한 바이너리 프레임을 모두 봅니다. 보낸 세션의 태스크에서 돌기 때문에 빨라야 합니다.

## 인증 플러그인

`auth_plugin(name, factory)`로 등록하면 테넌트의 `auth`에서 `kind: plugin`, `name: <name>`을 쓸 수 있습니다. 팩토리는 게이트웨이를 빌드할 때 그 항목의 `options`를 받으므로, 잘못된 옵션은 `build()`와 `--check-config`에서 실패합니다.

인증기는 다음 가운데 하나를 돌려줍니다.

| 결과 | 효과 |
|---|---|
| `Ok(Identity)` | 받아들임. 사용자 ID와, 있으면 권한과 만료 시각 |
| `Err(AuthError::NotMine)` | 이 인증기가 다루는 토큰이 아님. 다음 인증기가 판단 |
| `Err(AuthError::Rejected(_))` | 401 |
| `Err(AuthError::Unavailable(_))` | 503과 `Retry-After` |

## 다른 프로그램에 넣기

`Gateway::serve_on(listener, api_listener, shutdown)`은 직접 바인딩한 리스너로 서비스하고, 넘겨준 종료 future가 끝나면 멈춥니다. `Gateway`를 복제한 어느 인스턴스에서든 `metrics_text()`, `session_count()`, `drain()`을 쓸 수 있습니다.

## 안정성

| 수준 | 대상 | 약속 |
|---|---|---|
| 안정 | `wsprism`이 다시 내보내는 모든 것 | 0.x 동안에는 마이너 릴리스(0.1 → 0.2)에서만 호환성을 깨며, 변경 기록에 남깁니다. 1.0부터는 일반 semver |
| SPI | `wsprism_engine::spi`, 문서에서 숨긴 컨텍스트 메서드 | 엔진 위에 서버나 전송을 만들 때 씁니다. 0.x에서는 약속이 없습니다 |
| 내부 | `wsprism_gateway::__internal`(`internals` 기능) | 저장소 자체의 테스트와 벤치마크용. API가 아닙니다 |

오류 타입과 설정 타입은 `#[non_exhaustive]`이므로 와일드카드 갈래와 함께 매칭하세요. 설정 구조체는 YAML을 파싱해서만 만들어집니다.

와이어 프로토콜, 서버 API, 프록시 요청 형식, 설정 형식에는 Rust API보다 엄격한 기준이 적용됩니다. 늘어나기만 합니다.

## 빌드 프로필

Cargo는 최상위 워크스페이스의 빌드 프로필을 쓰므로, wsPrism의 릴리스 프로필은 내 바이너리에 적용되지 않습니다. 공식 빌드와 같은 팬아웃 성능을 내려면 다음을 넣으세요.

```toml
[profile.release]
codegen-units = 1
```

이 설정이 없으면 룸 팬아웃 마이크로벤치마크가 약 10% 느려집니다. 종단 간으로 보면 차이는 0.1% 안팎입니다.

## 최소 지원 Rust 버전

Rust **1.95**입니다. 정책은 나온 지 6개월이 지난 안정 릴리스입니다. 0.x에서 이 버전을 올리는 것은 호환성을 깨는 변경으로 치지 않지만, 변경 기록에는 남깁니다.
