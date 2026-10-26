# ADR 0014: 공개 리스너에서 TLS 직접 종단 (`gateway.tls`)

- 상태: **채택, 구현 완료 (2026-10-26)** — 사용자 결정: "구현했을 때 좋아지는 거라면 당장". 판정 기준(아래 「측정」)을 충족했다.
- 관련: BENCHMARKS 「출시 전: TLS」, CONFIG.md 「TLS on the listener」, 외부 검토 #4 A2

## 문제

게이트웨이는 TLS를 종단하지 않았다. 운영 트래픽은 거의 모두 `wss://`이므로, 같은 서버에서 TLS를 끝내야 하는 배포는 앞에 프록시를 두어야 했다. 측정해 보니 이 구성에서 wsPrism이 진다.

| 64 B, 각 3회 | wsPrism + nginx | Centrifugo 자체 TLS |
|---|---|---|
| rooms:20, CPU µs/전달 | 14.5~15.7 | 7.2~8.7 |
| rooms:20, KB/연결 | 93 | 94~99 |
| broadcast:10, CPU | 17.6~18.9 | 10.8~12.0 |
| broadcast:10, KB/연결 | 65 | 44~48 |

비용 대부분은 암호화가 아니라 **중계 한 번**(소켓 한 쌍, 프록시 버퍼, 복사)에서 나왔다. Centrifugo가 자체 TLS에 치르는 비용은 평문 대비 약 +15 KB, +1 µs였다.

## 결정

1. `gateway.tls: { cert_file, key_file, handshake_timeout_ms }`. 없으면 지금처럼 평문.
2. **rustls**(ring), 기본값 그대로: TLS 1.2·1.3, 안전한 암호군, ALPN `http/1.1`, 클라이언트 인증서 없음.
3. 인증서는 **빌드 때** 읽는다. 없는 파일, 잘못된 PEM, 인증서와 맞지 않는 키는 시작과 `--check-config`에서 실패한다.
4. **재시작 없는 교체:** 인증서 선택기(`ResolvesServerCert`)가 `RwLock<Arc<CertifiedKey>>`를 들고, SIGHUP이나 `Gateway::reload_tls()`가 파일을 다시 읽어 바꾼다. 실패하면 오류를 남기고 기존 인증서를 그대로 쓴다. 열린 연결은 건드리지 않는다.
5. **평문 경로는 그대로** 둔다(axum `serve`). TLS일 때만 hyper로 직접 연결을 받는다: TCP_NODELAY, TLS 핸드셰이크, 요청에 `ConnectInfo`(주소별 핸드셰이크 제한기용)를 넣어 같은 라우터로, `with_upgrades`로 WebSocket. 종료 순서는 axum과 같다(드레인 future가 끝나면 받기를 멈추고 HTTP 연결을 정상 종료).
6. **핸드셰이크 보호:** TLS 핸드셰이크는 입장 검사(속도 제한, 테넌트, Origin)보다 먼저 CPU를 쓴다. 동시에 진행되는 핸드셰이크를 1,024개로 묶고(넘으면 즉시 닫음), `handshake_timeout_ms`(기본 10초) 안에 끝나지 않으면 닫는다.
7. 내부 리스너(`gateway.api.listen`)는 평문으로 둔다(루프백·사설망 전제).

## 기각한 대안

| 대안 | 이유 |
|---|---|
| 프록시 뒤 배포만 지원하고 문서화 | 같은 서버 종단에서 CPU 2배, 메모리 1~1.4배로 진다(측정). 핵심 주장(연결당 비용이 가장 낮음)이 흔한 배포에서 깨진다 |
| `axum-server` 크레이트 | 새 의존성과 그 크레이트의 종료·연결 정보 처리 방식을 떠안는다. 필요한 루프는 백 줄 남짓이고, 드레인 순서를 우리가 그대로 정할 수 있다 |
| axum 0.8로 올려 `Listener` 트레이트 사용 | ADR 0007에서 건너뛰기로 했다(최신 tungstenite의 연결당 128 KiB 읽기 버퍼 기본값 등). TLS 하나 때문에 바꿀 일이 아니다 |
| OpenSSL / native-tls | 시스템 라이브러리 의존(distroless 이미지에 맞지 않음). rustls는 이미 Redis TLS와 백엔드 HTTPS에 쓰고 있다 |
| 파일 변경 감시(inotify)로 자동 교체 | 플랫폼별 차이와 반쯤 쓰인 파일을 읽는 경쟁이 생긴다. SIGHUP은 운영자가 파일을 다 바꾼 뒤 보내는, nginx·haproxy와 같은 관례다 |
| mTLS(클라이언트 인증서), ACME 자동 발급, HTTP/2·3 | 지금 요구가 없다. 범위를 좁게 지킨다 |

## 측정 (`compete-tls-native.jsonl`, 같은 세션에서 세 구성 번갈아 3회, 64 B)

| 시나리오 | 구성 | p99 (ms) | CPU µs/전달 | KB/연결 |
|---|---|---|---|---|
| rooms:20 | **wsPrism `gateway.tls`** | 0.87~1.14 | **6.09~6.49** | **28.0~28.2** |
| | Centrifugo 자체 TLS | 1.05~1.40 | 7.96~9.65 | 93.8~95.0 |
| | wsPrism + nginx | 1.61~1.70 | 15.35~15.81 | 93.4~94.4 |
| broadcast:10 | **wsPrism `gateway.tls`** | 2.75~8.25 | **8.67~8.97** | **26.1** |
| | Centrifugo 자체 TLS | 5.24~6.33 | 11.04~12.11 | 47.2~47.9 |
| | wsPrism + nginx | 7.00~9.28 | 17.94~18.43 | 64.9~65.3 |

- 판정 기준("Centrifugo 자체 TLS보다 CPU·메모리가 의미 있게 앞선다")을 충족했다. CPU 1.25~1.5배, 메모리 1.8~3.4배 적다.
- 평문(compete-v2) 대비 TLS 비용: 전달당 약 1.2~1.7 µs, 연결당 약 7~8 KB.

## 결과 (지켜야 할 것, 새 비용)

- 평문 경로와 TLS 경로가 따로 있다. 드레인·연결 정보·소켓 옵션을 바꿀 때는 두 경로를 함께 바꾸고, `tests/tls_listener.rs`가 TLS 경로를 지킨다.
- 보안에 민감한 코드다. 출시 전 사람 리뷰 대상(Lua 스크립트, 펜싱, 세션 표)에 `server.rs`의 `serve_tls`와 `transport/tls.rs`를 더한다.
- 테스트용 인증서(`tests/fixtures/tls`, 100년, CA 개인 키는 버림)는 테스트 외에 쓰지 않는다.
