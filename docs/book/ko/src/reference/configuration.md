# 설정

게이트웨이는 YAML 파일 하나를 읽습니다. 경로는 `--config`나 `WSPRISM_CONFIG`로 주며, 기본값은 `./wsprism.yaml`입니다. 이 장은 모든 설정 항목을 정리합니다. 자주 쓰는 항목에 주석을 단 예시는 저장소의 [`wsprism.yaml`](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml)에 있습니다.

```bash
wsprism-gateway --config wsprism.yaml --check-config
```

파서는 엄격합니다. 모르는 필드, 범위를 벗어난 값, 서로 맞지 않는 조합(핑 주기보다 짧은 유휴 시간, 존재하지 않는 서비스를 적은 허용 목록)은 시작할 때 문제 위치와 함께 실패합니다. `--check-config`는 서비스를 띄우지 않고 똑같은 검사를 합니다.

## 구조

```yaml
version: 1          # 필수. 지금은 1뿐입니다
gateway: { … }      # 프로세스 전체: 리스너, 시간 제한, 브로커
tenants: [ … ]      # 테넌트마다 하나씩
```

## 환경 변수에서 값 가져오기

어떤 값이든 `${NAME}`(변수가 없거나 비어 있으면 시작 실패), `${NAME:-default}`를 쓸 수 있고, `${`를 글자 그대로 쓰려면 `$${`라고 씁니다.

- 치환은 파싱 뒤에 일어나므로 값이 파일 구조를 바꿀 수 없습니다. 키는 치환하지 않습니다.
- 값 전체가 자리표시자 하나뿐이면, 치환 결과가 숫자나 불리언으로 읽힐 때 그 타입이 됩니다. 나머지는 문자열입니다.
- `{ }`나 `[ ]` 안에서는 자리표시자를 따옴표로 감싸세요(`"${QUEUE}"`). 따옴표가 타입에는 영향을 주지 않습니다.
- 비밀 값은 쓰는 곳에서 변수를 읽는 `*_env` 설정을 쓰는 편이 낫습니다.

## `gateway`

| 항목 | 기본값 | 범위 | 의미 |
|---|---|---|---|
| `listen` | `0.0.0.0:8080` | | 공개 WebSocket 리스너 |
| `api.listen` | 꺼짐 | | 내부 리스너: 서버 API, `/metrics`, 상태 확인. 루프백이나 사설망에 바인딩 |
| `public_metrics` | `false` | | 공개 리스너에서도 `/metrics` 제공 |
| `tls` | 꺼짐 | | `listen`에서 TLS를 직접 종단. [`gateway.tls`](#gatewaytls) 참고 |
| `ping_interval_ms` | 20000 | 5000~120000 | WebSocket 핑 주기 |
| `idle_timeout_ms` | 60000 | 10000~600000, 핑 주기보다 커야 함 | 이 시간 동안 들어오는 것이 없으면 연결을 닫음 |
| `writer_send_timeout_ms` | 1500 | 50~60000 | 소켓 쓰기가 이 시간 이상 막히면 세션을 닫음 |
| `drain_grace_ms` | 2000 | 600000 이하 | 정상 종료에 주는 시간 |
| `session_queue_capacity` | 1024 | 1~1048576 | 세션당 송신 큐 크기(메시지 수) |
| `socket.tcp_nodelay` | `true` | | Nagle 끄기. 켜 두세요. Nagle과 지연 ACK가 겹치면 작은 프레임이 수십 ms씩 기다립니다 |
| `trusted_proxies` | `[]` | IP 또는 CIDR | `X-Forwarded-For`로 클라이언트를 알려 줄 수 있는 프록시 |
| `log_identities` | `plain` | `plain`, `hashed`, `off` | 로그에 사용자 ID, 세션 ID, IP를 남기는 방식 |
| `log_identity_key_env` | 프로세스마다 임의 | | `hashed`용 HMAC 키를 담은 변수(16바이트 이상) |
| `broker` | `kind: memory` | | [클러스터 브로커](#클러스터-브로커) 참고 |

### `gateway.tls`

공개 리스너의 TLS입니다(rustls: TLS 1.2·1.3, 안전한 암호군, ALPN `http/1.1`, 클라이언트 인증서 없음).

| 항목 | 기본값 | 범위 | 의미 |
|---|---|---|---|
| `cert_file` | 필수 | | PEM 인증서 체인. 리프 인증서가 맨 앞 |
| `key_file` | 필수 | | PEM 개인 키(PKCS#8, PKCS#1, SEC1) |
| `handshake_timeout_ms` | 10000 | 100~60000 | 이 시간 안에 TLS 핸드셰이크를 끝내지 않은 연결을 닫음 |

파일은 시작할 때 읽습니다. 파일이 없거나, 형식이 틀렸거나, 키가 인증서와 맞지 않으면 시작과 `--check-config`가 실패합니다. SIGHUP(또는 `Gateway::reload_tls()`)을 받으면 다시 읽습니다. 새 핸드셰이크부터 새 인증서를 쓰고, 읽기에 실패하면 기존 인증서를 그대로 씁니다. 동시에 진행되는 핸드셰이크는 최대 1,024개입니다. 내부 리스너는 평문 그대로입니다.

### `gateway.handshake_limit`

WebSocket 업그레이드 전에 적용하는 토큰 버킷입니다. 거절된 시도는 429와 `Retry-After`를 받습니다.

| 항목 | 기본값 | 의미 |
|---|---|---|
| `enabled` | `true` | 한 주소에서 부하 시험을 할 때만 끄세요 |
| `global_rps` / `global_burst` | 100 / 200 | 모든 클라이언트를 합친 초당 시도 수 |
| `per_ip_rps` / `per_ip_burst` | 10 / 50 | 클라이언트 주소당 초당 시도 수 |
| `ipv6_prefix_len` | 64 (32~128) | IPv6 클라이언트를 이 접두사 길이로 묶음 |
| `max_ip_entries` | 50000 | 추적하는 주소 수. 넘으면 전체 버킷만 적용 |

## `tenants[]`

| 항목 | 의미 |
|---|---|
| `id` | `[A-Za-z0-9_-]` 1~64자, 겹치면 안 됨 |
| `limits` | 자원 한도(아래) |
| `policy` | 메시지·세션 정책(아래) |
| `auth` | 인증 사슬. 빠지면 개발용 인증기를 쓰고 경고를 남김 |
| `api_keys` | 서버 API 키: `{name, sha256}` |
| `rpc` | 백엔드로 가는 RPC 프록시 |
| `history` | 룸 이력 규칙 |

### `limits`

| 항목 | 기본값 | 의미 |
|---|---|---|
| `max_frame_bytes` | 4096 | 클라이언트 프레임 최대 크기. WebSocket 코덱에서 막습니다(종료 1009) |
| `max_sessions_total` | 0(무제한) | 테넌트의 동시 세션 수, 노드별 |
| `max_rooms_total` | 0(무제한) | 멤버가 한 명 이상인 룸 수, 노드별 |
| `max_users_per_room` | 0(무제한) | 룸당 멤버 수, 노드별 |
| `max_rooms_per_user` | 64 | 한 사용자가 동시에 들어갈 수 있는 룸 수. 0이면 무제한 |

### `policy`

| 항목 | 기본값 | 의미 |
|---|---|---|
| `ext_allowlist` | `["room:join", "room:leave"]` | 클라이언트가 보낼 수 있는 Ext 메시지: `svc:type`, `svc:*` |
| `hot_allowlist` | `[]`(Hot 레인 꺼짐) | 클라이언트가 보낼 수 있는 Hot 프레임: `svc:op`, `svc:*` |
| `rate_limit_rps` / `rate_limit_burst` | 200 / 400 | 들어오는 메시지의 초당 한도 |
| `rate_limit_scope` | `connection` | `connection`, `tenant`(공유), `both` |
| `sessions.mode` | `multi` | `single`이면 `max_sessions_per_user: 1`이어야 함 |
| `sessions.max_sessions_per_user` | 4 | 사용자당 동시 세션 수(클러스터에서는 노드 전체 기준) |
| `sessions.on_exceed` | `deny` | 새 세션을 `deny`(거절)하거나 `kick_oldest`(가장 오래된 세션 종료) |
| `allowed_origins` | `[]`(브라우저 차단) | 정확한 Origin, `https://*.example.com`, 또는 `*` |
| `ticket_sources` | `[header, subprotocol]` | 자격 증명을 실을 수 있는 위치, 우선순위 순. `query`는 직접 켜야 함 |
| `hot_error_mode` | `sys_error` | `silent`면 거절한 Hot 프레임을 클라이언트에게 알리지 않고 버림 |
| `hot_requires_active_room` | `true` | 프로토콜 v1: Hot 프레임을 보내려면 들어가 있는 룸이 있어야 함 |
| `hot_echo_to_sender` | `true` | 세션이 보낸 Hot 프레임을 자신에게도 되돌림 |
| `hot_tick_hz` | 0(즉시) | 1~240: 세션마다 키 있는 Hot 상태를 초당 이 횟수까지만 내보냄 |
| `presence_events` | `false` | `member_joined`/`member_left`와 멤버 목록을 보냄 |

허용 목록에 적힌 서비스는 모두 실제로 있어야 합니다. 내장 서비스(`room`, `chat`, Hot `1`)이거나, `Gateway::builder`로 등록했거나, `rpc.allow`로 라우팅한 서비스여야 합니다.

### `auth[]`

항목마다 `kind`가 있습니다.

**`dev`**: 개발 전용입니다. `dev`(사용자 `dev`)와 `dev.<id>` 또는 `dev:<id>`(사용자 `<id>`)를 받습니다.

**`ticket`**: `POST /api/v1/tickets`로 발급한 1회용 티켓. 옵션은 없습니다.

**`jwt`**:

| 항목 | 기본값 | 의미 |
|---|---|---|
| `algorithm` | 필수 | `HS256/384/512`, `RS256/384/512`, `PS256/384/512`, `ES256/384`, `EdDSA` |
| `secret_file` / `secret_env` | | HMAC 비밀(HS*). 해시 길이 이상 |
| `public_key_file` | | PEM 공개 키(RS*, PS*, ES*, EdDSA) |
| `jwks_url` | | JWKS 엔드포인트. `https`(평문 `http`는 루프백만) |
| `jwks_refresh_s` | 300 (60~86400) | 백그라운드 갱신 주기 |
| `jwks_min_refetch_s` | 30 (1~3600) | 모르는 `kid` 때문에 다시 가져오는 최소 간격 |
| `jwks_max_stale_s` | 86400 (1~604800) | 갱신이 계속 실패할 때 이보다 오래된 키는 믿지 않음 |
| `issuer` / `audience` | | 지정하면 `iss`/`aud`가 필수이고 대조함 |
| `leeway_s` | 30 (300 이하) | `exp`, `nbf`에 허용하는 시계 오차 |
| `require_tenant` | `false` | `wsprism.tenant` 클레임 필수 |
| `kid` | | 이 키 ID를 단 토큰만 봄 |

**`proxy`**:

| 항목 | 기본값 | 의미 |
|---|---|---|
| `url` | 필수 | 백엔드의 connect 엔드포인트 |
| `timeout_ms` | 2000 (1~60000) | 호출당 시간 제한 |
| `max_in_flight` | 256 | 동시 호출 수. 넘으면 503 |
| `secret_env` | | `Authorization: Bearer <값>`으로 보냄 |

**`plugin`**: `name`(`Gateway::builder().auth_plugin`으로 등록한 이름)과 `options`(팩토리에 넘김).

### `api_keys[]`

`{ name: "backend", sha256: "<16진수 64자>" }`. 해시는 `printf %s "$KEY" | sha256sum`으로 구합니다.

### `rpc`

| 항목 | 기본값 | 의미 |
|---|---|---|
| `url` | 필수 | 백엔드의 RPC 엔드포인트 |
| `allow` | 필수 | 넘길 `svc:type`/`svc:*` 패턴. 내장 서비스는 넘길 수 없음 |
| `timeout_ms` | 5000 (1~60000) | 호출당 시간 제한 |
| `max_in_flight` | 16 | 연결당 대기 호출 수. 넘으면 `RATE_LIMITED` |
| `max_in_flight_total` | 1024 | 테넌트 전체 대기 호출 수. 넘으면 `RESOURCE_EXHAUSTED` |
| `secret_env` | | `Authorization: Bearer <값>`으로 보냄 |
| `require_joined_room` | `true` | 세션이 들어가 있지 않은 룸을 적은 요청은 거절 |

### `history[]`

| 항목 | 기본값 | 범위 | 의미 |
|---|---|---|---|
| `rooms` | 필수 | 이름 또는 `prefix*` | 규칙이 적용되는 룸. 처음 맞는 규칙이 적용됨 |
| `size` | 200 | 1~10000 | 룸마다 남길 메시지 수 |
| `ttl_s` | 600 | 1~86400 | 메시지를 남겨 두는 시간(초) |
| `max_bytes` | 1048576 | 1024~67108864 | 룸마다 남길 바이트 수 |

## 클러스터 브로커

`gateway.broker`를 `kind: redis`로 두면 노드들이 Redis로 연결됩니다. [클러스터 운영](../guide/cluster.md)을 참고하세요.

| 항목 | 기본값 | 범위 | 의미 |
|---|---|---|---|
| `url` / `url_env` | 둘 중 하나 필수 | | `redis://`나 `rediss://` URL, 또는 그 URL을 담은 변수 |
| `prefix` | `wsp` | `[A-Za-z0-9_.-]` 1~64자 | 모든 키와 채널의 접두사. 여러 클러스터가 Redis 하나를 나눠 쓸 수 있음 |
| `pubsub_connections` | 4 | 1~64 | 구독용 연결 수 |
| `slot_ttl_s` | 30 | 3~300 | 마지막 하트비트 후 노드 슬롯이 만료되는 시간 |
| `outbox_capacity` | 65536 | | Redis로 보낼 프레임 큐 크기, 전체 테넌트 합 |
| `outbox_per_tenant` | 16384 | 1~capacity | 테넌트 하나가 쌓을 수 있는 프레임 수 |
| `outbox_max_wait_ms` | 1000 | 10~60000 | 가장 오래된 프레임이 이만큼 기다리는 동안 새 프레임을 거절 |
| `tls.ca_file` | 시스템 저장소 | | Redis 인증서를 검증할 CA |
| `tls.client_cert_file` / `tls.client_key_file` | | | 상호 TLS용 클라이언트 인증서 |
| `sentinel.addresses` | | | Sentinel URL(Sentinel과 TLS로 연결하려면 `rediss://`) |
| `sentinel.master_name` | | | Sentinel에 등록된 프라이머리 이름 |
| `replica_ack.replicas` | Sentinel이면 1, 아니면 꺼짐 | 0~16 | 로그인이 성공하기 전에 티켓 소비 기록을 받아야 하는 레플리카 수 |
| `replica_ack.timeout_ms` | 500 | 1~10000 | 최대 대기 시간. 넘으면 로그인이 503으로 실패 |
| `admission.max_latency_ms` | 500 | 0(끔) 또는 10~60000 | Redis 응답이 이보다 느린 동안 새 연결을 거절 |
| `admission.max_pending_claims` | 10000 | 1~1000000 | Redis를 기다리는 세션 등록이 이보다 많은 동안 새 연결을 거절 |

## 다시 읽기

설정은 시작할 때 한 번만 읽습니다. 바꾸려면 노드를 재시작하고, 노드가 여럿이면 하나씩 재시작하세요.
