# Architecture Decision Records

설계 결정 하나당 문서 하나. 무엇을 정했는지와 함께 **왜**, **기각한 대안**, **결과(지켜야 할 것·새로 생긴 비용)** 를 남긴다.
결함 수정의 결정 기록은 [`../HARDENING.md`](../HARDENING.md)에 있다.

| # | 제목 | 상태 |
|---|---|---|
| [0001](0001-session-context-and-room-service.md) | 세션 컨텍스트를 서비스에 `&mut`로 빌려주고, 룸 기능을 일반 서비스로 | 채택 (M0) |

새 ADR은 다음 번호로 만들고, 결정이 바뀌면 기존 ADR을 고치지 말고 새 ADR에서 "대체함"을 밝힌다.
