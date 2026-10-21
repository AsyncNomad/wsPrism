# ADR 0002: 전송 계층과 무관한 송신 프레임 `OutFrame`

- 상태: 채택 (2026-10-14, M0)
- 관련: [ROADMAP](../ROADMAP.md) "리뷰 제안에서 바꾼 점" 2번, M5

## 배경

axum의 `ws::Message`가 세션 레지스트리(`Connection.tx`), realtime 코어의 송신 경로(`deliver`, `PreparedMsg::to_ws_message`), 세션 큐까지 퍼져 있었다. M2에서 엔진 트레이트를 공개하면 이 타입이 공개 API가 되고, 전송 구현(tungstenite → axum 0.8 / fastwebsockets 등)을 바꿀 수 없게 된다. 또 팬아웃 때 발행자가 수신자마다 `String`/`Vec`을 복사했다.

## 결정

- `realtime::OutFrame { Text(Utf8Text), Binary(Bytes), Ping }`을 세션 큐의 원소 타입으로 쓴다. `Utf8Text`는 생성 시 한 번 검증한 UTF-8을 `Bytes`에 담는다(내부 필드 비공개로 불변식 보장).
- `OutFrame::from_payload`가 페이로드를 **한 번** 인코딩하고, 수신자마다 `clone()`(참조 카운트 증가)한다. `PreparedMsg`는 제거.
- 전송 타입으로의 변환은 `transport/session/wire.rs` 한 곳에서만 한다(수신 쪽은 기존 `transport/codec.rs`). 결과적으로 `axum::extract::ws`는 `transport/` 밖에서 쓰이지 않는다.

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| 지금 axum 0.8로 올려 `Bytes` 기반 `Message`를 그대로 사용 | 복사는 없어지지만 라이브러리 타입이 엔진 API에 박히는 문제는 그대로다. 버전 업은 전송 구현 평가(M1~M5)와 함께 결정한다 |
| 미리 인코딩한 WebSocket 프레임 바이트를 큐에 넣기 | 가장 큰 성능 레버지만 현재 라이브러리(tungstenite)가 사전 인코딩 프레임 쓰기를 지원하지 않는다. 전송 구현 선택 후 `OutFrame`에 변형을 추가하는 방식으로 확장 가능 |

## 결과

- 발행자의 팬아웃 비용이 수신자당 메모리 복사에서 참조 카운트 증가로 줄었다. 각 수신자의 복사는 그 수신자의 writer task(`wire::to_message`)로 옮겨졌다 — axum 0.7의 `String`/`Vec<u8>` 소유 타입 때문이며, 전송 교체 시 사라진다.
- `close_session`이 큐에서 `Close` 메시지를 걸러내던 분기가 사라졌다(종료는 제어 채널로만 요청, C8).
- **공개 API 변경**: `realtime::PreparedMsg` 제거, `OutFrame`·`Utf8Text` 추가, `Connection.tx`의 원소 타입 변경.
