# Architecture Decision Records

설계 결정 하나당 문서 하나. 무엇을 정했는지와 함께 **왜**, **기각한 대안**, **결과(지켜야 할 것·새로 생긴 비용)** 를 남긴다.
결함 수정의 결정 기록은 [`../HARDENING.md`](../HARDENING.md)에 있다.

| # | 제목 | 상태 |
|---|---|---|
| [0001](0001-session-context-and-room-service.md) | 세션 컨텍스트를 서비스에 `&mut`로 빌려주고, 룸 기능을 일반 서비스로 | 채택 (M0) |
| [0002](0002-transport-agnostic-out-frame.md) | 전송 계층과 무관한 송신 프레임 `OutFrame` | 채택 (M0) |
| [0003](0003-ticket-sources-and-subprotocol.md) | 티켓 전달 경로와 서브프로토콜 버전 협상 | 채택 (M1) |
| [0004](0004-product-direction.md) | 제품 방향: 실시간 상태 동기화 (개정 2) | 조건부 채택 (검증 V 후 확정) |
| [0005](0005-session-transport-seam.md) | 세션 전송 경계 — `SessionTx` / `SessionRx` | 채택 (T0-1) |
| [0006](0006-protocol-v2.md) | 프로토콜 v2 — 인코딩·전달 의미 분리, 명시적 채널, 재접속 의미를 가진 종료 코드 | 채택 (리뷰 반영, 단계별 구현) |
| [0007](0007-transport-own-framing.md) | P2 전송: axum 0.8을 건너뛰고 서버 측 WebSocket 프레이밍 직접 구현 | 보류 (단계 1 기준 미달, 성능 단계 재개 시 재판단) |
| [0008](0008-m2-lite-integration.md) | M2-lite — 인증·권한·서버 API·백엔드 프록시·SDK | 채택 (단계별) |
| [0009](0009-crate-split-and-public-api.md) | 크레이트 분리(protocol·engine·gateway·파사드)와 공개 API 정책 | 제안 (검토 대기) |

새 ADR은 다음 번호로 만들고, 결정이 바뀌면 기존 ADR을 고치지 말고 새 ADR에서 "대체함"을 밝힌다.
