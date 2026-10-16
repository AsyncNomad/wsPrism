# ADR 0008: M2-lite — 인증·권한·서버 API·백엔드 프록시·SDK

- 상태: **채택 (2026-10-16)**, M2-1~M2-8 완료 (**M2 완료 기준 충족**; M2-9 크레이트 분리 남음) — 사용자 결정: "상용 프로그램 수준으로 기능 확장을 먼저, 성능은 나중에". 단계별 구현(§9), 새 의존성은 해당 단계 착수 전에 확인(§8)
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
- 구현 결정(M2-4):
  - **실패한 사용도 티켓을 소멸시킨다**(다른 테넌트로 제시 등). 유출된 티켓을 올바른 테넌트로 재시도할 수 없게 하기 위해서. 기각: 테넌트가 맞을 때만 소멸 — 공격자가 테넌트를 바꿔가며 탐색할 여지를 남긴다.
  - 발급 시 테넌트 체인에 `ticket`이 없으면 400. 기각: 조용히 발급 — 절대 쓸 수 없는 티켓을 내주면 설정 실수가 런타임에야 드러난다.
  - 미결 티켓 상한 100만 개(초과 시 503), 만료분은 1초 스위퍼가 정리. 백엔드 폭주가 메모리를 무한히 늘리지 못하게.
  - 사용자 id는 `[A-Za-z0-9_-]{1,64}`(테넌트 id와 같은 규칙) — 내부 키 구분자 `::`가 섞이는 것을 막는다.
  - 티켓 신원의 `expires_at`은 비워 둔다(티켓 TTL은 *입장* 기한이지 세션 기한이 아니다). 세션 만료는 JWT(M2-5)부터.

### 5. JWT

- `auth: [{kind: jwt, ...}]`: `HS256`(공유 비밀) 또는 `RS256`/`ES256`(정적 공개키 PEM 또는 JWKS URL, 주기적 갱신). 필수 검증: 서명, `exp`, `alg` 고정(설정한 것만 — alg 혼동 공격 방지). 선택: `iss`, `aud`. `sub` → 사용자.
- 테넌트 결합: 클레임 `wsprism.tenant`가 있으면 접속 테넌트와 같아야 한다(다른 테넌트용 토큰 재사용 방지).
- 권한 클레임: `wsprism.subscribe`, `wsprism.publish`(문자열 배열).
- 구현 결정(M2-5):
  - `wsprism.*`는 **중첩 객체** `"wsprism": {tenant, subscribe, publish}`로 해석한다. 점이 들어간 평면 키(`"wsprism.tenant"`)는 기각 — 발급자 쪽 라이브러리마다 점 키를 다루는 방식이 달라 실수가 잦다.
  - 헤더의 `alg`가 이 링크의 알고리즘과 다르거나 `kid`(설정 시)가 다르면 `NotMine`(다음 링크로). 알고리즘은 링크마다 고정이므로 alg 혼동 공격 여지는 그대로 없고, 대신 키 교체(같은 알고리즘, `kid` 두 개)와 여러 발급자 체인이 가능해진다. 기각: 즉시 거절 — 링크를 둘 이상 둘 수 없다.
  - **jsonwebtoken 9는 `iss`/`aud` 클레임이 *없으면* 비교를 건너뛴다.** 그래서 설정한 경우 필수 클레임에 넣는다(돌연변이 검사로 확인: 빼면 `iss` 없는 토큰이 통과). `audience`를 설정하지 않으면 `aud` 검사를 끈다(아니면 `aud`가 있는 토큰이 모두 거절된다).
  - HMAC 비밀은 설정 파일에 직접 쓰지 못한다(`secret_file`/`secret_env`만). 길이는 해시 출력 이상(RFC 7518 §3.2). 키는 기동 시 읽어 `--check-config`가 잡는다.
  - 세션 만료 = `exp + leeway_s`. 연결마다 타이머 task를 두지 않고 기존 1초 스위퍼가 `SessionSignals`의 만료 시각(AtomicU64, 8바이트/연결)을 확인해 `auth_expired`(v2 4001, v1 1008)로 닫는다. 오차 최대 ~1초. 기각: 연결별 `sleep_until` — 세션 future 크기·타이머 휠 부담(P0-d에서 없앤 것과 같은 비용).
  - 토큰 갱신(재접속 없이 새 토큰 제시)은 아직 없다. 클라이언트는 4001을 받으면 새 토큰으로 재접속한다. Centrifugo의 `refresh`에 해당하는 것은 SDK(M2-7)와 함께 다시 판단.
  - 크레이트 버전: 최신은 11.x지만 9.3.1을 쓴다. 9.x는 lock에 이미 있는 `ring`을 쓰고 새 크레이트가 2개뿐이다. 10.x 이상은 백엔드로 `aws-lc-rs`(C 빌드) 또는 RustCrypto(`rsa` 크레이트 — RUSTSEC-2023-0071 Marvin 권고 대상)를 골라야 한다. 9.x 지원이 끊기면 다시 판단. 권고 검사: `cargo deny check`(로컬에 이미 설치된 0.20.2, CI와 같은 도구) — advisories·bans·licenses·sources 모두 ok(2026-10-16, 중복 버전 경고만).

### 6. 백엔드 프록시 (언어 무관 연동의 핵심)

- **connect 프록시** (`auth: [{kind: proxy, url}]`): `POST url {tenant, token, client_ip}` → 백엔드가 `200 {user, permissions?, expires_at?}` / `401·403`. 타임아웃(기본 2초) 초과는 거절(503 + `Retry-After`).
- **RPC 프록시** (`tenants[].rpc: {url, allow: ["game:*"], timeout_ms}`): allowlist를 통과하고 `rpc.allow`에 맞는 Ext 메시지를 `POST url {tenant, user, sid, svc, type, room, data}`로 넘기고, 응답 `{data}`를 `sys.reply`(요청 `seq`에 `reply_to`)로 돌려준다. 오류는 `sys.error`.
  - **세션 루프를 막지 않는다**(C1·C9 원칙): HTTP 호출은 별도 task, 응답은 세션 큐로. 세션당 동시 호출 상한(기본 16) 초과는 `RATE_LIMITED`.
- 구현 결정(M2-6):
  - **`DeferredReply`(새 경계)**: `RealtimeCtx::defer_reply(max)`가 지금 요청(`reply_to` 포함)에 *나중에* 다른 task에서 답하는 핸들을 준다. 세션당 미결 개수는 `SessionSignals`의 `AtomicU32`로 제한하고, 핸들이 drop되면 반납된다(돌연변이 검사로 확인). RPC 전용이 아니라 앞으로의 비동기 서비스(WASM 등)도 같은 경계를 쓴다. 기각: RPC 안에 세션별 `DashMap<session, count>` — 세션 종료 시 정리 누락 위험, 경계가 RPC에 갇힘.
  - 라우팅: 디스패처는 전역, RPC는 테넌트별이라 세션이 시작 시 자기 테넌트의 `RpcProxy`를 받아 `dispatch_text`에서 먼저 확인한다(내장 이름은 설정 검증에서 금지하므로 순서가 결과를 바꾸지 않는다). 메트릭 라벨은 `"rpc"`(정적, S5). 기동 시 allowlist↔서비스 검사는 RPC가 덮는 svc를 등록된 것으로 본다.
  - 응답 매핑: `200 {data}` → `sys.reply`, `200 {error:{code,msg}}` → `sys.error`(코드 `[A-Z0-9_]{1,32}` 검증, 메시지 512바이트 절단), 그 밖 → `UNAVAILABLE`(새 `ClientCode`). **백엔드 본문은 절대 그대로 전달하지 않는다**(스택 트레이스 유출 방지, 테스트로 확인).
  - connect 프록시: `401`/`403`만 거절, 나머지 실패는 503+`Retry-After`(장애를 "잘못된 자격 증명"으로 보고하면 클라이언트가 재로그인 루프에 빠진다). 동시 호출 상한(세마포어, 기본 256) — 접속 폭주가 백엔드에 무한정 쌓이지 않게. 응답의 `expires_at`은 JWT와 같은 세션 만료 경로(4001).
  - HTTP 클라이언트: 프로세스당 하나(풀), **리다이렉트 안 따름**(토큰이 설정에 없는 호스트로 가지 않게), `HTTP(S)_PROXY` 환경 변수 무시(`no_proxy`, 트래픽 경로는 설정이 정한다), 응답 1 MiB 상한, 루트 인증서는 **OS 저장소**(`rustls-native-certs`, 이미 lock에 있음). 처음엔 webpki-roots(빌드 시 번들)를 골랐으나 기각 — 라이선스 CDLA-Permissive-2.0이 `deny.toml` 허용 목록 밖이라 CI가 깨지고, 사내망 백엔드의 사설 CA를 신뢰할 수 없다. 대가: 최소 컨테이너 이미지는 `ca-certificates` 필요(문서화). `reqwest` 기본 기능은 끄고 rustls·http2만, rustls는 기존 0.23.46 재사용.
  - 백엔드 인증: `secret_env`의 값을 `Authorization: Bearer`로 보낸다(값은 기동 시 읽고 헤더는 sensitive 표시). mTLS·HMAC 서명은 수요가 확인되면.
  - 아직 없는 것: JWKS URL(이제 HTTP 클라이언트가 있으므로 다음 후보), RPC 재시도(멱등성을 게이트웨이가 알 수 없어 의도적으로 없음), 실패 로그 샘플링(지금은 호출마다 `warn` — 백엔드 장애 때 로그가 많아질 수 있음; 메트릭 정비(P0-e) 때 함께).
- 내장 서비스(`chat`, `room`, echo)는 그대로 두고, 같은 `svc`가 RPC 대상이면 RPC가 우선하지 않는다 — 내장 서비스 이름은 RPC `allow`에 쓸 수 없게 설정 검증에서 막는다.

### 7. 클라이언트 SDK (JavaScript, 최소판)

- 의존성 없는 ESM(`sdk/js/wsprism.js`) + 손으로 쓴 타입 선언(`wsprism.d.ts`). 브라우저와 Node 22+(전역 `WebSocket`)에서 같은 코드.
- 기능: v2 협상, 티켓을 서브프로토콜로 전달, `request(svc, type, data)`(seq ↔ `reply_to` Promise), `join/leave`, `onMessage`, Hot `sendState(chan, key, bytes)` / `onState` + `(sender, key)`별 오래된 `seq` 폐기, **종료 코드 범위에 따른 재접속**(40xx 안 함, 41xx 지수 백오프 + 지터, 42xx 즉시), 재접속 후 룸 재입장.
- 테스트: Node 내장 `node:test`로 실제 게이트웨이(테스트 프로세스가 띄움)에 붙어 검증.
- 구현 결정(M2-7):
  - **v2 전용.** v1을 함께 지원하면 Hot 경로(활성 룸 암묵 라우팅)와 종료 코드 해석이 두 벌이 된다. 새 클라이언트는 v2로 시작하면 된다.
  - `getTicket()`을 **접속 시도마다** 호출한다(1회용 티켓은 재사용 불가, JWT는 새로 발급). 그래서 `4001 auth_expired`는 40xx지만 "즉시 재접속"으로 다룬다 — "그대로 재접속하지 말 것"의 "그대로"가 새 자격 증명으로 해소되기 때문. 이것이 토큰 갱신의 현재 형태다(대역 내 갱신은 없음; 필요해지면 `sys.refresh` 설계).
  - `send`(대기 없음)와 `request`(답을 기다림)를 나눈다. `chat.send`처럼 성공 시 답이 없는 연산을 `request`로 보내면 타임아웃까지 매달리기 때문. 모든 송신에 `seq`를 붙여 `send`의 오류도 `error` 이벤트로 식별 가능.
  - Hot 수신의 오래된 `seq` 폐기는 `(chan, sender, key)`별, u32 순환 비교(RFC 1982식). 재접속 시 기록을 지운다(룸 id·member id가 바뀐다).
  - 재접속 백오프: `min(30s, 1s·2^(n-1))`의 [½, 1) 구간 지터. "즉시"도 연속 실패 2회째부터는 백오프(인증 서버 장애 시 폭주 방지).
  - 타입은 손으로 쓴 `.d.ts`(빌드 단계·의존성 없음). `test/types-check.ts`가 앱처럼 API를 써 보고(`@ts-expect-error`로 틀린 사용도 확인) `tsc -p sdk/js`(strict)로 검사한다. typescript는 리포 의존성이 아니라 `npx`로만 받는다(로컬 scratchpad, CI `sdk-js`). 일부러 틀린 타입을 넣으면 실패하는 것을 확인.
  - Node 지원 정책: engines `>=22`(전역 `WebSocket`이 기본 활성인 첫 LTS), CI는 지원 중인 LTS 전부(지금 22·24), 개발 기본은 `.nvmrc` = 24. 22는 2027-04 EOL — 그때 매트릭스에서 빼고 26을 넣는다.
  - 개발 환경 메모: 이 WSL의 기본 `node`는 v18(전역 WebSocket 없음). `run-tests.sh`는 22 미만이면 nvm의 v22+를 찾는다.
  - 발견한 게이트웨이 쪽 어긋남: allowlist에 없는 `svc:type`(그리고 빈 allowlist)은 `BAD_REQUEST`로 답했는데, PROTOCOL.md 정의상 `NOT_ALLOWED`(정책상 금지)가 맞다. **v1·v2 모두 `NOT_ALLOWED`로 수정**(사용자 결정 2026-10-16). 0.x·배포된 클라이언트 없음 → 호환 깨짐을 감수하고, 버전별 분기를 영구히 남기지 않는다. 기각: v2에서만 변경 — 오류 코드가 프로토콜 버전마다 달라지는 분기가 남는다. 이 코드를 검사하는 테스트가 없었다(변경해도 아무것도 안 깨졌다) → `reply_correlation`에 단언 추가.

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
| M2-2 ✅ | 권한 모델(구독/발행) + 검사 지점(`RealtimeCtx`의 입장·발행 — 모든 서비스가 거침). 통합 테스트는 1회용 티켓과 함께(`tests/tickets.rs`) | 없음 |
| M2-3 ✅ | 서버 API 리스너 + API 키 + `publish`·`disconnect`·`presence` | `ring`·`base64`(오프라인) |
| M2-4 ✅ | 1회용 티켓 발급 + `ticket` 인증기 | 없음 |
| M2-5 ✅ | JWT 인증기 (정적 키; JWKS는 M2-6 HTTP 클라이언트 이후) | `jsonwebtoken` 9.3.1 (사용자 승인 2026-10-16) |
| M2-6 ✅ | connect·RPC 프록시 | `reqwest` 0.12 (rustls, 사용자 승인 2026-10-16) |
| M2-7 ✅ | JS SDK + 통합 테스트 (`sdk/js`, CI `sdk-js`) | 없음 |
| M2-8 ✅ | 예제 앱(Node 백엔드 + SDK 채팅·상태 동기화) — **M2 완료 기준** (`examples/chat-and-state`, CI E2E) | 없음 |
| M2-9 | engine/server 크레이트 분리, 공개 API 최소화 | 없음 |

### 10. 예제 앱 (M2-8)

- `examples/chat-and-state`: 로그인→1회용 티켓(권한: lobby만)→SDK 접속, 내장 채팅, RPC(주사위 — 백엔드가 답하고 서버 API로 룸에 발행), Hot 커서(30 Hz 틱). 백엔드는 의존성 없는 Node(`backend.mjs`) — "사용자가 쓰는 부분"이 한 파일에 보이도록.
- 완료 기준 검증: `test/example.test.mjs`가 예제의 `gateway.yaml`을 포트만 바꿔 실제 게이트웨이와 `backend.mjs`를 띄우고, SDK 클라이언트 둘이 페이지와 같은 흐름을 밟는다(CI `sdk-js`).
- 결정: Spring 예제는 만들지 않음 — 연동 계약(HTTP JSON)이 언어 무관이라 하나로 충분하고, 두 번째 언어는 유지 비용만 든다. 수요가 확인되면 추가.
- 사각지대: `index.html`은 **실제 브라우저로 자동 검증하지 않았다**(이 환경에 브라우저 없음; Origin 검사·DOM 렌더링은 수동 확인 필요). E2E는 Node의 WebSocket으로 같은 SDK·같은 흐름을 검증한다. 브라우저 자동화(Playwright)는 G2 브라우저 측정과 함께.
- 개발 메모: 실행 스크립트가 공유하는 `sdk/js/find-node.sh`(`$NODE` → `node` → nvm 최신 순으로 22+를 찾음).

## 기각한 대안

| 대안 | 이유 |
|---|---|
| 서버 API를 WebSocket과 같은 포트에 | 관리 API가 공개 인터넷에 노출된다. 내부망·로컬 리스너 분리가 기본이어야 한다 |
| API 키를 평문으로 설정에 | 설정 파일 유출이 곧 키 유출. 해시 저장 + 상수 시간 비교 |
| 인증기 하나만(체인 없음) | 브라우저는 1회용 티켓, 네이티브는 JWT처럼 경로가 섞이는 것이 일반적 |
| RPC 호출을 세션 루프 안에서 기다림 | 느린 백엔드 하나가 세션의 송수신 전체를 멈춘다(C1과 같은 문제) |
| JWT 검증 직접 구현 | alg 혼동·`kid` 처리·시간 검증 같은 실수 여지가 크다. 검증된 크레이트를 쓴다 |
