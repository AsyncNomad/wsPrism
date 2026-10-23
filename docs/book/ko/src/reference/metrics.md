# 메트릭

내부 리스너의 `GET /metrics`는 Prometheus 텍스트 형식을 돌려줍니다. 레이블 값은 설정이나 정해진 집합에서만 오고 클라이언트 입력에서는 오지 않으므로, 시계열 수는 테넌트 수를 넘지 않습니다.

## 연결

| 메트릭 | 종류 | 레이블 | 의미 |
|---|---|---|---|
| `wsprism_ws_upgrades_total` | counter | `tenant`, `status` | 받아들인 WebSocket 업그레이드 |
| `wsprism_ws_sessions_active` | gauge | `tenant` | 지금 열려 있는 세션 |
| `wsprism_handshake_rejections_total` | counter | `tenant`, `reason` | 업그레이드 전에 거절한 연결 |
| `wsprism_draining` | gauge | | 종료를 위해 드레인하는 동안 1 |

요청이 설정에 없는 테넌트를 가리키면 `tenant`는 `_unknown`입니다. `reason`은 다음 가운데 하나입니다.

| 사유 | HTTP | 원인 |
|---|---|---|
| `rate_limit` | 429 | 핸드셰이크 속도 제한 |
| `bad_sid` | 400 | 잘못된 `sid` |
| `draining` | 503 | 노드가 종료 중 |
| `cluster_busy` | 503 | Redis가 너무 느리거나 대기 중인 세션 등록이 너무 많음 |
| `unknown_tenant` | 400 | 없는 테넌트 |
| `unsupported_protocol` | 400 | 지원하는 서브프로토콜이 없음 |
| `origin_not_allowed` | 403 | 허락되지 않은 브라우저 Origin |
| `tenant_capacity` | 503 | `max_sessions_total` 도달 |
| `auth_missing` | 401 | 테넌트가 보는 위치에 자격 증명이 없음 |
| `auth_failed` | 401 | 자격 증명 거절 |
| `auth_unavailable` | 503 | 인증 백엔드나 JWKS에 닿지 않음 |

## 메시지

| 메트릭 | 종류 | 레이블 | 의미 |
|---|---|---|---|
| `wsprism_policy_decisions_total` | counter | `tenant`, `lane`, `decision`, `reason` | 테넌트 정책이 거절하거나 제한한 메시지와 세션 |
| `wsprism_dispatch_duration_seconds` | histogram | `tenant`, `lane` | 클라이언트 메시지 하나의 처리 시간(버킷 100 µs~1 s. Hot 레인은 1024건 중 1건 표본) |
| `wsprism_decode_errors_total` | counter | `tenant` | 디코딩하지 못한 클라이언트 프레임 |
| `wsprism_service_errors_total` | counter | `tenant`, `lane`, `svc` | 서비스가 오류로 답한 클라이언트 메시지 |

## 전달

| 메트릭 | 종류 | 의미 |
|---|---|---|
| `wsprism_egress_drop_total` | counter | 수신자의 큐가 가득 차서 버린 손실 허용 프레임 |
| `wsprism_egress_slow_consumer_closes_total` | counter | 신뢰 프레임이 큐에 들어가지 못해 닫은 세션 |
| `wsprism_writer_timeouts_total`(`tenant`) | counter | 소켓 쓰기가 `writer_send_timeout_ms`를 넘겨 닫은 세션 |
| `wsprism_hot_slot_overwritten_total` | counter | 보내기 전에 새 값으로 바뀐 키 있는 Hot 프레임. 정상이며, 값이 크면 수신자가 밀리고 있다는 뜻 |
| `wsprism_hot_slot_full_drops_total` | counter | 수신자의 슬롯 표가 가득 차서 버린 키 있는 Hot 프레임 |

## 클러스터

| 메트릭 | 종류 | 의미 |
|---|---|---|
| `wsprism_broker_outbox_refused_total` | counter | Redis로 가는 큐가 가득 찼거나 너무 오래돼 거절한 프레임. 발행자는 `UNAVAILABLE`을 받음 |
| `wsprism_broker_retries_total` | counter | Redis가 느리거나 없어서 다시 시도한 브로커 호출 |
| `wsprism_broker_failed_total` | counter | Redis가 거부한 프레임이나 명령. 버려지므로 원인을 찾아야 함 |

## 읽는 법

- `wsprism_broker_retries_total`이 늘어도 거절이 0이면, Redis가 잠깐 멈칫한 것을 게이트웨이가 흡수한 것입니다. 거절이 생기면 큐가 포기했고 발행자도 그 사실을 들었다는 뜻입니다.
- `wsprism_broker_failed_total`은 0이어야 합니다. 대개 ACL 권한이 빠졌거나, `noeviction` 아래에서 Redis 메모리가 바닥난 경우입니다.
- `wsprism_egress_drop_total`은 느린 수신자가 있는 바쁜 Hot 룸에서는 평소에도 늘어납니다. Ext만 쓰는 테넌트에서 예상치 않게 늘어난다면 서버 API 발행을 `lossy`로 보내고 있지 않은지 보세요.
- `wsprism_handshake_rejections_total{reason="auth_failed"}`와 `auth_unavailable`을 나란히 보세요. 앞의 것은 사용자 문제, 뒤의 것은 인프라 문제입니다.
