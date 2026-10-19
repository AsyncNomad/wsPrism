# ADR 0010: 멤버 입장·퇴장 이벤트 (presence events)

- 상태: **채택 (2026-10-19)** — 배포 전 리뷰(D1)에서 "클라이언트가 누가 나갔는지 알 수 없다"(예제 앱의 커서가 사라지지 않음)는 공백으로 지적, 사용자 결정("배포 전에 다 해결")
- 관련: [ADR 0006](0006-protocol-v2.md)(sys 메시지·member_id), [ADR 0008](0008-m2-lite-integration.md)(서버 API presence 조회), [PROTOCOL.md](../PROTOCOL.md)

## 결정

1. **테넌트별 옵트인** `policy.presence_events: true` (기본 `false`). 입장 한 번이 룸 전체로 브로드캐스트(O(N))되므로, 큰 룸(관전·방송)에서 기본으로 켜면 비용이 크다. Centrifugo도 namespace별 옵트인이다.
2. **다른 멤버에게 알림**(본인 제외, Reliable):
   - `{"v":1,"svc":"sys","type":"member_joined","room":R,"data":{"user":U,"member_id":M}}`
   - `{"v":1,"svc":"sys","type":"member_left","room":R,"data":{"user":U,"member_id":M}}`
   - `member_id`는 v2 Hot 프레임의 `sender`와 같은 값이라, 클라이언트는 퇴장한 멤버의 상태(커서 등)를 지울 수 있다. 같은 사용자의 세션이 여럿이면 세션(멤버십)마다 이벤트가 나간다.
   - Reliable인 이유: 퇴장 알림을 잃으면 "유령 멤버"가 남는다. 대가로 큐가 가득 찬 수신자는 slow consumer로 닫힌다(재접속 시 스냅숏으로 복구).
3. **스냅숏:** 이 옵션이 켜진 테넌트의 `sys.joined`에 `members: [{user, member_id}]`(본인 제외)가 실린다. 최대 1000명, 넘으면 `members_truncated: true`(나머지는 이후 이벤트로만 알 수 있음; 전체 목록은 서버 API presence로).
4. **퇴장의 모든 경로**가 `member_left`를 낸다: 명시적 `room.leave`, 연결 종료(세션 정리), 다른 세션에 의한 축출(sid 교체·세션 수 초과), 서버 API `disconnect`. 정리 경로가 하나(`RealtimeCore::remove_session_presence`)로 모여 있어 경로별 누락이 없다.
5. 같은 룸에 다시 `join`해도(멱등) 이벤트는 나가지 않는다.

## 기각한 대안

| 대안 | 이유 |
|---|---|
| 기본으로 켜기 | 1만 명 룸에서 입장 한 번이 1만 건 전송. 필요한 테넌트만 켠다 |
| Lossy로 보내기 | 퇴장 알림 유실 = 영구적인 유령 멤버. 상태가 어긋나는 쪽이 끊고 재동기화보다 나쁘다 |
| 스냅숏을 별도 요청(`room.members`)으로 | 입장과 조회 사이에 이벤트가 끼어 경쟁이 생긴다. 입장 응답에 같이 주면 그 시점 기준으로 일관된다 |
| 사용자 단위 이벤트(첫 세션 입장·마지막 세션 퇴장) | Hot `sender`가 멤버십 단위라 클라이언트가 상태를 지우려면 멤버십 단위가 필요하다. 사용자 단위 집계는 클라이언트가 할 수 있다 |
