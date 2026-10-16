# ADR 0005: 세션 전송 경계 — `SessionTx` / `SessionRx`

- 상태: 채택 (2026-10-16, T0-1)
- 관련: [ADR 0004](0004-product-direction.md) 결정 6(경계를 먼저 연다), [설계 §4.1](../design/hot-lane-over-webtransport.md), [ADR 0002](0002-transport-agnostic-out-frame.md)

## 배경

세션 루프(`transport/session`)가 axum의 `SplitSink<WebSocket, Message>` / `SplitStream<WebSocket>`을 직접 받았다. 디코드(`codec::decode(Message)`), 크기 초과 판별(tungstenite 에러 downcast), 종료 처리가 모두 그 타입에 묶여 있어서 WebTransport(ADR 0004 하이브리드 분기)나 서버 피어(설계 §4.6)를 붙이려면 세션 코드를 고쳐야 했다. M2에서 엔진을 공개 API로 굳히기 전에 이 경계를 열어야 한다.

## 결정

1. 전송을 **두 절반의 트레이트**로 나눈다(`transport/session/io.rs`).
   - `SessionTx`: `send(OutFrame)`(순서 보장 신뢰 채널), `send_close(code, reason)`, `finish_peer_close()`, `too_big_linger()`.
   - `SessionRx`: `recv() -> Option<Result<Inbound, RecvError>>`, `await_peer_close()`.
   - `RecvError { TooBig, Broken, Decode(WsPrismError) }` — 전송별 에러를 세션이 다루는 세 가지 사유로 분류한다.
2. 메서드는 `impl Future<Output = …> + Send`를 반환한다(구현은 `async fn`). 세션 future가 `on_upgrade`/`tokio::spawn`에서 `Send`여야 하기 때문.
3. 세션 루프·종료 처리(`run`, `event_loop`, `close::finish`, `close::close_session`)는 `Tx: SessionTx, Rx: SessionRx`에 대해 제네릭이다. axum 타입은 `ws_io.rs`(WebSocket 구현)와 `codec.rs`(디코드)에만 남는다. 기존 `wire.rs`는 `ws_io.rs`로 흡수했다.
4. 비신뢰 datagram 채널은 **이번에 넣지 않는다.** Hot 슬롯(T0-3)과 함께 `send_unreliable` 같은 능력으로 추가한다. 쓰는 곳이 없는 메서드를 미리 넣으면 시그니처를 실제 요구 없이 굳히게 된다.

## 기각한 대안

| 대안 | 기각 이유 |
|---|---|
| `&mut self` 메서드를 가진 단일 `SessionIo` 트레이트(설계 §4.1 초안) | 이벤트 루프는 `select!`에서 읽기와 쓰기를 동시에 기다린다. 한 객체의 `&mut` 메서드 둘을 동시에 빌릴 수 없다 |
| `dyn` 트레이트 객체(`Box<dyn SessionIo>`) | async 메서드마다 future를 박싱해야 한다(`async_trait`). 메시지마다 할당이 생기고, 전송 종류는 연결 시점에 정해지므로 정적 디스패치로 충분하다 |
| `async fn`을 트레이트에 직접 선언 | 반환 future에 `Send`를 강제할 수 없어 제네릭 세션 future의 `Send` 증명이 구현 타입 추론에 의존한다. 명시적 `+ Send`가 의도를 문서화한다 |
| 디코드를 세션에 두고 전송은 원시 바이트만 넘기기 | 전송마다 프레이밍이 다르다(WS 메시지 / WT 제어 스트림 길이 접두 / datagram). 디코드까지가 전송의 책임이어야 세션이 프레이밍을 모른다 |

## 결과

- 동작 변화 없음: 기존 테스트 전부 통과(게이트웨이 72개, 5회 반복).
- 새 전송은 `SessionTx`/`SessionRx` 구현 + 핸들러에서 `session::run`을 호출하면 된다(WebTransport `WtIo`, 서버 피어).
- `close_session`은 이제 큐에 쌓인 `OutFrame`과 통지를 전송의 `send`로, 종료는 `send_close`로 보낸다. WebSocket에서는 이전과 같은 프레임 순서다.
