# ADR 0008: M2-lite — 인증·권한·서버 API·백엔드 프록시·SDK

- 상태: **채택 (2026-10-16)** — 사용자 결정: "상용 프로그램 수준으로 기능 확장을 먼저, 성능은 나중에". 단계별 구현(§9), 새 의존성은 해당 단계 착수 전에 확인(§8)
- 관련: [ROADMAP](../ROADMAP.md) M2, [ADR 0003](0003-ticket-sources-and-subprotocol.md)(티켓 전달 경로), [ADR 0006](0006-protocol-v2.md)(프로토콜 v2), [ADR 0005](0005-session-transport-seam.md)

## 배경

wsPrism의 연결 앞단·정책·성능은 경쟁 제품과 견줄 수준이지만, 실서비스에 붙일 수 있는 기본 기능이 없다: 인증은 `ticket=dev` 스텁, 백엔드가 클라이언트에게 메시지를 보낼 방법(서버 발행 API)이 없고, 비즈니스 로직은 Rust로 같이 컴파일해야 하며, 클라이언트 SDK가 없다. Centrifugo·Sockudo·Ably는 모두 이것들을 갖고 있다. 이 ADR은 "갈아탈 때 막히지 않을 만큼"(ADR 0004 결정 5)의 범위와, 그것들이 끼워질 **경계**를 정한다.

## 결정

### 1. 인증: `Authenticator` 트레이트 + 테넌트별 체인

```rust
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Ok(identity) / Err(NotMine): 이 토큰 형식이 아님(다음으로) / Err(Rejected): 거절
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Identity, AuthError>;
}
pub struct AuthRequest<'a> { tenant: &'a str, token: &'a str, client_ip: IpAddr }
pub struct Identity {
    user_id: String,
    permissions: Permissions,          // §2
    expires_at: Option<SystemTime>,    // 토큰 만료 → 세션 종료(4001 auth_expired)
}
```

- 테넌트 설정 `auth: [ ... ]`은 **순서 있는 체인**이다. 각 인증기는 자기 형식이 아닌 토큰을 `NotMine`으로 넘긴다(1회용 티켓은 `wpt_` 접두사로 구별).
- 구현: `dev`(지금 스텁: `dev`, `dev:<id>`), `ticket`(§4), `jwt`(§5), `proxy`(§6).
- **기본값**: `auth`를 쓰지 않은 테넌트는 `[dev]` — 지금 설정과 테스트가 그대로 동작한다. 대신 기동 시와 `--check-config`에서 **경고**를 낸다("tenant X uses the dev authenticator; set tenants[].auth for production"). 기본값을 거부로 바꾸는 것은 M2 마지막에 다시 판단한다.
- admission의 인증 단계는 비동기가 된다(`admit`이 `async`). 순서(레이트리밋 → 드레이닝 → 테넌트 → 프로토콜 → Origin → **인증** → 용량)는 그대로.

### 2. 권한: 구독과 발행 분리 (ADR 0006 §5)

- `Permissions { subscribe: Vec<Pattern>, publish: Vec<Pattern> }`, 패턴은 룸 이름 glob(`*` 접미사만: `match:*`, `chat:lobby`). `None` = 제한 없음(지금 동작).
- 검사 지점: `room:join` → `subscribe`, `chat.send`·Hot 발행 → `publish`. 위반은 `sys.error NOT_ALLOWED`(닫지 않음).
- 테넌트 allowlist(어떤 `svc:type`이 가능한가)와 별개다: allowlist는 **테넌트 전체 규칙**, 권한은 **사용자별 규칙**.

### 3. 서버 API (백엔드 → 게이트웨이)

- **별도 리스너** `gateway.api.listen`(기본 꺼짐, 권장 `127.0.0.1` 또는 내부망). 공개 WebSocket 포트에 관리 API를 두지 않는다.
- 인증: 테넌트별 API 키 `tenants[].api_keys`(SHA-256 해시를 설정에 저장, 상수 시간 비교). 요청 헤더 `Authorization: Bearer <key>` → 키가 곧 테넌트를 정한다.
- 엔드포인트 (JSON):

| 메서드·경로 | 본문 | 동작 |
|---|---|---|
| `POST /api/v1/publish` | `{room, data}` 또는 `{room, data_b64, hot:true}`, `qos: reliable\|lossy` | 룸에 발행. 응답 `{recipients}` |
| `POST /api/v1/disconnect` | `{user}` 또는 `{user, sid}`, `reason?` | 해당 세션 종료(`sys.kicked{reason}` + v1 1008 / v2 4007 `disconnected`) |
| `POST /api/v1/tickets` | `{user, ttl_s?, permissions?}` | 1회용 티켓 발급(§4) |
| `GET /api/v1/rooms/{room}/presence` | — | 룸의 사용자 목록 |

- 발행 메시지 형식: Ext는 `{"v":1,"svc":"server","type":"publish","room":…,"data":…}`, Hot은 v1 페이로드 / v2 `sender = 0`(서버) 프레임.

### 4. 1회용 티켓

- `POST /api/v1/tickets`가 `wpt_<32바이트 무작위 base64url>`을 발급. 기본 TTL 30초(최대 300), **첫 사용 시 소멸**, 신원(사용자·권한)을 담는다.
- 용도: 브라우저(헤더를 못 붙임)·WebTransport(URL로만 전달)에서 장수명 토큰을 URL에 싣지 않기 위해(ADR 0003의 "쿼리 금지"는 장수명 토큰 기준). 저장소는 프로세스 메모리(단일 노드), M4에서 공유 저장소.
- 쿼리 전달은 `ticket_sources`에 `query`가 있어야만 허용(지금 규칙 그대로). 1회용 티켓이면 `query`를 켜도 위험이 작다고 문서화.

### 5. JWT

- `auth: [{kind: jwt, ...}]`: `HS256`(공유 비밀) 또는 `RS256`/`ES256`(정적 공개키 PEM 또는 JWKS URL, 주기적 갱신). 필수 검증: 서명, `exp`, `alg` 고정(설정한 것만 — alg 혼동 공격 방지). 선택: `iss`, `aud`. `sub` → 사용자.
- 테넌트 결합: 클레임 `wsprism.tenant`가 있으면 접속 테넌트와 같아야 한다(다른 테넌트용 토큰 재사용 방지).
- 권한 클레임: `wsprism.subscribe`, `wsprism.publish`(문자열 배열).

### 6. 백엔드 프록시 (언어 무관 연동의 핵심)

- **connect 프록시** (`auth: [{kind: proxy, url}]`): `POST url {tenant, token, client_ip}` → 백엔드가 `200 {user, permissions?, expires_at?}` / `401·403`. 타임아웃(기본 2초) 초과는 거절(503 + `Retry-After`).
- **RPC 프록시** (`tenants[].rpc: {url, allow: ["game:*"], timeout_ms}`): allowlist를 통과하고 `rpc.allow`에 맞는 Ext 메시지를 `POST url {tenant, user, sid, svc, type, room, data}`로 넘기고, 응답 `{data}`를 `sys.reply`(요청 `seq`에 `reply_to`)로 돌려준다. 오류는 `sys.error`.
  - **세션 루프를 막지 않는다**(C1·C9 원칙): HTTP 호출은 별도 task, 응답은 세션 큐로. 세션당 동시 호출 상한(기본 16) 초과는 `RATE_LIMITED`.
- 내장 서비스(`chat`, `room`, echo)는 그대로 두고, 같은 `svc`가 RPC 대상이면 RPC가 우선하지 않는다 — 내장 서비스 이름은 RPC `allow`에 쓸 수 없게 설정 검증에서 막는다.

### 7. 클라이언트 SDK (JavaScript, 최소판)

- 의존성 없는 ESM(`sdk/js/wsprism.js`) + 손으로 쓴 타입 선언(`wsprism.d.ts`). 브라우저와 Node 22+(전역 `WebSocket`)에서 같은 코드.
- 기능: v2 협상, 티켓을 서브프로토콜로 전달, `request(svc, type, data)`(seq ↔ `reply_to` Promise), `join/leave`, `onMessage`, Hot `sendState(chan, key, bytes)` / `onState` + `(sender, key)`별 오래된 `seq` 폐기, **종료 코드 범위에 따른 재접속**(40xx 안 함, 41xx 지수 백오프 + 지터, 42xx 즉시), 재접속 후 룸 재입장.
- 테스트: Node 내장 `node:test`로 실제 게이트웨이(테스트 프로세스가 띄움)에 붙어 검증.

### 8. 의존성

| 용도 | 후보 | 상태 |
|---|---|---|
| API 키 해시, 티켓 무작위값 | `ring` (lock에 있음) | 오프라인 가능 |
| JWT 검증 | `jsonwebtoken` | **내려받기 필요** — 직접 구현(ring)은 보안상 피한다 |
| 백엔드 HTTP 클라이언트 | `reqwest`(rustls) | **내려받기 필요** — 대안 `hyper-util` legacy client + `tokio-rustls`(오프라인)는 연결 풀·리다이렉트·TLS 설정을 직접 맞춰야 해서 유지보수 비용이 크다 |
| SDK | 없음 (Node 내장 `WebSocket`, `node:test`) | 오프라인 |

### 9. 단계

| 단계 | 내용 | 새 의존성 |
|---|---|---|
| M2-1 ✅ | `Authenticator` 트레이트·체인, admission 비동기화, `dev` 구현 이전(동작 변화 없음), 기동 경고 | 없음 |
| M2-2 | 권한 모델(구독/발행) + 검사 지점 | 없음 |
| M2-3 | 서버 API 리스너 + API 키 + `publish`·`disconnect`·`presence` | `ring`(오프라인) |
| M2-4 | 1회용 티켓 발급 + `ticket` 인증기 | 없음 |
| M2-5 | JWT 인증기 | `jsonwebtoken` |
| M2-6 | connect·RPC 프록시 | `reqwest` |
| M2-7 | JS SDK + 통합 테스트 | 없음 |
| M2-8 | 예제 앱(Node 백엔드 + SDK 채팅·상태 동기화) — **M2 완료 기준** | 없음 |
| M2-9 | engine/server 크레이트 분리, 공개 API 최소화 | 없음 |

## 기각한 대안

| 대안 | 이유 |
|---|---|
| 서버 API를 WebSocket과 같은 포트에 | 관리 API가 공개 인터넷에 노출된다. 내부망·로컬 리스너 분리가 기본이어야 한다 |
| API 키를 평문으로 설정에 | 설정 파일 유출이 곧 키 유출. 해시 저장 + 상수 시간 비교 |
| 인증기 하나만(체인 없음) | 브라우저는 1회용 티켓, 네이티브는 JWT처럼 경로가 섞이는 것이 일반적 |
| RPC 호출을 세션 루프 안에서 기다림 | 느린 백엔드 하나가 세션의 송수신 전체를 멈춘다(C1과 같은 문제) |
| JWT 검증 직접 구현 | alg 혼동·`kid` 처리·시간 검증 같은 실수 여지가 크다. 검증된 크레이트를 쓴다 |
