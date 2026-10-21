# ADR 0001: 세션 컨텍스트를 서비스에 `&mut`로 빌려주고, 룸 기능을 일반 서비스로

- 상태: 채택 (2026-10-14, M0)
- 관련: [ROADMAP](../ROADMAP.md) M0·M2, HARDENING C4/S5

## 배경

`room:join`/`room:leave`는 세션 루프 안에서 `if svc == "room"`으로 특수 처리되고 있었다. 이유는 룸 기능이 세션 상태(active room, Hot Lane 대상)를 바꿔야 하는데, 서비스 인터페이스(`TextService::handle(ctx: RealtimeCtx, ...)`)는 메시지마다 새로 만든 컨텍스트 **사본**을 받아 세션 상태를 바꿀 방법이 없었기 때문이다. 그 결과:

- 세션 루프가 특정 서비스를 알고 있었다(새 "세션 상태를 바꾸는 서비스"를 추가하려면 루프를 고쳐야 함).
- `app_state`의 allowlist 검사에 `"room"` 예외가 필요했다.
- 메시지마다 `RealtimeCtx::new`가 `format!`과 문자열 복제를 했다(ROADMAP M5의 낭비 항목).

## 결정

1. **세션당 컨텍스트 하나.** 세션 task가 `RealtimeCtx`를 하나 소유하고, 서비스에는 `&mut RealtimeCtx`로 빌려준다.
   - `TextService::handle(&self, ctx: &mut RealtimeCtx, env)`, `BinaryService::handle_binary(&self, ctx: &mut RealtimeCtx, frame)`.
   - 세션 상태 변경 API: `set_active_room`. 테넌트 한도는 컨텍스트가 보유(`join_room(room)`).
2. **룸 기능은 `RoomService`** (`services/room.rs`)로 chat과 똑같이 디스패처에 등록한다. 세션 루프에는 서비스 이름별 분기가 없다.
3. **서비스의 응답은 `ctx.send_to_session(QoS::Reliable)`** 로 보낸다. 큐가 가득 차면 slow consumer로 끊는 동작은 세션 자체 응답 경로와 같다.
4. **`sys.*` 생성기는 crate 공용 모듈** (`crate::sys`)로 옮겨 서비스도 쓴다.
5. **서비스 오류 메트릭에 `svc` 라벨**: 값은 디스패처에 등록된 정적 이름(`Dispatcher::text_service_name`)이고, 미등록이면 `unknown`. 클라이언트 문자열은 라벨로 쓰지 않는다(S5).

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| 세션 상태를 `Arc<Mutex<..>>`로 공유해 사본 컨텍스트에서도 수정 | Hot Lane이 프레임마다 active room을 읽는다. 락이 핫패스에 들어가고, 소유권이 흐려진다 |
| 서비스가 "효과(effect)" 목록을 반환하고 세션이 적용 | 깔끔하지만 효과 종류마다 enum을 늘려야 해 결국 코어를 고치게 된다. `&mut`가 같은 일을 더 단순하게 한다 |
| 특수 처리를 유지하고 M2/M3에서 한 번에 재설계 | 같은 패턴(세션 상태를 바꾸는 기능)이 채널 구독·토큰 갱신에서 반복될 것이 확실하다. 경계를 지금 만든다 |

## 결과

- 서비스는 세션 task 안에서 순차 실행되므로 `&mut`가 안전하다. 서비스가 일을 다른 task로 넘기려면 `ctx.clone()`을 쓴다. 이때 **사본의 세션 상태 변경은 세션에 반영되지 않는다**(문서화됨).
- 메시지마다 컨텍스트를 만들지 않으므로 할당이 줄었다.
- **공개 API 변경**: 서비스 트레이트 시그니처, `RealtimeCtx::new` 인자(`active_room` 대신 `limits`), `join_room_with_limits` → `join_room`.
- **메트릭 변경**: `wsprism_service_errors_total{svc="room",type="join_failed"}` → `{lane="ext",svc="room"}`. 다른 Ext 서비스 오류에도 `svc` 라벨이 붙는다.
- **에러 메시지 문구**: 룸 오류가 서비스 에러 경로를 거치므로 `msg`에 `bad request: ` 같은 접두어가 붙는다. 에러 `code`는 동일. (에러 메시지 정리는 M1 "내부 정보 노출 제거"에서 다룬다.)
- M2에서 엔진 트레이트를 정의할 때 이 컨텍스트가 서비스 확장 지점의 기반이 된다. 채널 구독(M3)도 같은 방식(`&mut ctx`)으로 세션 상태를 다룬다.
