# ADR 0003: 티켓 전달 경로와 서브프로토콜 버전 협상

- 상태: 채택 (2026-10-14, M1)
- 관련: [PROTOCOL](../PROTOCOL.md) §1, [ROADMAP](../ROADMAP.md) M1 보안·프로토콜 v1

## 배경

티켓은 `?ticket=` 쿼리스트링으로만 받았다. URL은 리버스 프록시, 로드밸런서, 액세스 로그에 그대로 기록되므로 토큰이 유출된다. 그런데 브라우저의 `WebSocket` API는 업그레이드 요청에 임의 헤더(`Authorization`)를 붙일 수 없어서, 단순히 헤더로 옮기면 브라우저 클라이언트가 인증할 방법이 사라진다. 또 프로토콜 버전을 협상하는 수단이 없어 v2를 도입할 길이 없었다.

## 결정

1. **티켓 전달 경로를 테넌트별 설정**(`policy.ticket_sources`, 우선순위 순)으로 만든다.
   - `header`: `Authorization: Bearer <ticket>` — 네이티브 클라이언트.
   - `subprotocol`: `Sec-WebSocket-Protocol: wsprism.v1, wsprism.auth.<ticket>` — 브라우저. Kubernetes(`base64url.bearer.authorization.k8s.io.<token>`) 등이 쓰는 표준적 우회로.
   - `query`: `?ticket=` — 레거시. **기본값에서 제외**(`[header, subprotocol]`).
2. **서브프로토콜로 버전을 협상**한다. 클라이언트가 서브프로토콜을 제시하면 지원 버전(`wsprism.v1`)을 선택해 그것만 응답한다. 인증 항목은 절대 되돌려 보내지 않는다. 지원 버전 없이 제시하면 400(이유 명시). 헤더가 없으면 v1로 간주(기존 클라이언트 호환).
3. 구현 경계: 파싱은 `transport/subprotocol.rs`, 경로 선택은 `transport/credentials.rs`, 검증 호출은 admission의 `authenticate` 단계. M2의 `Authenticator` 트레이트는 "토큰 → 신원" 부분만 대체한다.
4. 거절 사유를 `auth_missing`(어느 경로에도 토큰 없음)과 `auth_failed`(토큰 무효)로 나눠 계측한다.

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| 쿼리 티켓 유지 + 로그 마스킹 권고 | 게이트웨이 앞단(LB, CDN)의 로그는 우리가 통제할 수 없다 |
| 쿠키 기반 인증 | 브라우저 외 클라이언트에 맞지 않고, CSWSH 위험을 키운다(쿠키는 자동 첨부됨) |
| 연결 직후 첫 메시지로 인증 | 업그레이드 전에 거절할 수 없어(C3의 401 원칙과 충돌) 미인증 연결이 소켓·task를 점유한다 |
| 인증 항목만 제시해도 허용 | 브라우저는 서버가 제시된 항목 중 하나를 응답하지 않으면 연결을 실패시키는데, 인증 항목을 응답하면 토큰이 응답 헤더로 노출된다 |

## 결과

- **BREAKING**: 기존 `?ticket=` 클라이언트는 테넌트에 `ticket_sources: [query]`(또는 목록에 추가)를 설정하지 않으면 401을 받는다. 테스트 장치·부하 생성기는 `Authorization: Bearer`로 전환했다.
- 서브프로토콜 토큰은 HTTP token 문자만 허용된다(base64url 무패딩·JWT 가능). 표준 base64(`+`, `/`, `=`)는 불가 — SDK가 base64url을 쓰도록 문서화해야 한다(M2 최소 TS SDK).
- "1회용 단기 티켓"은 실제 인증기(M2)가 있어야 하므로 이 ADR 범위 밖이다.
