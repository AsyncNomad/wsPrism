# 운영

운영 환경에서 게이트웨이를 돌리는 분을 위한 장입니다. 띄우고 멈추는 법, 남기는 로그, 지켜볼 지표와 경보를 다룹니다.

## 명령줄

| 플래그 | 환경 변수 | 기본값 | |
|---|---|---|---|
| `--config <path>` | `WSPRISM_CONFIG` | `wsprism.yaml` | 설정 파일 |
| `--check-config` | | | 파일을 검사하고 종료 |
| `--log-format text\|json` | `WSPRISM_LOG_FORMAT` | `text` | 로그 형식 |
| | `RUST_LOG` | 오류만 | 수준. 예: `info`, `warn,wsprism_redis=info` |

`--check-config`는 CI와 매 배포 전에 돌리세요. 파일을 파싱하고, 모든 검증 규칙을 적용하고, 테넌트 정책을 전부 만들어 봅니다. JWT 키를 읽고 허용 목록에 적힌 서비스가 실제로 있는지도 확인합니다. 이 검사를 통과한 파일이면 실제로도 뜹니다.

## 파일 하나로 모든 환경을

설정 값은 어디서든 환경 변수에서 가져올 수 있습니다.

```yaml
gateway:
  listen: "0.0.0.0:${PORT:-8080}"
  broker: { kind: redis, url_env: REDIS_URL, prefix: "${CLUSTER:-wsp}" }
```

`${NAME}`은 변수가 없거나 비어 있으면 시작하지 못하고, `${NAME:-default}`는 기본값을 씁니다. 치환은 파싱 뒤에 하므로 값이 파일 구조를 바꿀 수는 없습니다. 비밀 값은 `*_env` 설정(`secret_env`, `url_env`)을 쓰는 편이 낫습니다. 실제로 쓰는 지점에서 변수를 읽으므로 오류 메시지에 값이 드러나지 않습니다.

설정을 실행 중에 다시 읽지는 않습니다. 바꾼 설정은 재시작해야 반영되며, 노드가 여럿이면 하나씩 재시작해서 중단 없이 반영할 수 있습니다.

## 로그

로그는 표준 오류로 나갑니다. `--log-format json`을 주면 한 줄에 객체 하나씩 써서 Loki, Elasticsearch, Cloud Logging 같은 곳에 바로 넣을 수 있습니다.

```json
{"ts":"2026-10-21T09:00:00.123456Z","level":"WARN","target":"wsprism_gateway::rpc",
 "msg":"rpc backend call failed","error":"Timeout",
 "url":"https://api.example.com/rpc?key=***",
 "span":{"trace_id":"5f0c…","t":"acme","u":"alice","s":"tab-1","ip":"203.0.113.7"}}
```

`span`은 그 이벤트가 어느 세션에서 났는지 알려 줍니다. `trace_id`, 테넌트(`t`), 사용자(`u`), 세션 ID(`s`), 클라이언트 주소(`ip`), 연결 ID(`c`)입니다. 같은 `trace_id`가 클라이언트에게 보내는 오류에도 실리므로, 사용자의 버그 제보를 게이트웨이 로그와 바로 맞춰 볼 수 있습니다.

티켓, 토큰, API 키, 백엔드 자격 증명, 메시지 본문은 절대 로그에 남기지 않습니다. 백엔드 URL은 사용자 정보를 빼고 쿼리 값을 `***`로 가려서 남깁니다.

### 개인 정보

사용자 ID, 세션 ID, IP 주소는 여러 법역에서 개인 정보입니다. 로그에 어떻게 나올지는 `gateway.log_identities`로 정합니다.

| 값 | 로그에 나오는 것 |
|---|---|
| `plain`(기본) | 값 그대로 |
| `hashed` | `h:` 뒤에 HMAC-SHA256 16진수 16자 |
| `off` | 없음(필드를 빼고 남김) |

`hashed`를 쓴다면 `gateway.log_identity_key_env`에 16바이트 이상의 키를 담은 변수를 지정하고, 모든 노드에 같은 키를 주세요. 그러면 한 사람은 어느 노드에서든, 재시작 뒤에도 같은 해시를 가집니다. 특정 사용자의 세션을 찾으려면 같은 키로 그 ID를 해시하면 됩니다. 키를 주지 않으면 프로세스마다 임의의 키를 씁니다.

## 멈추기와 업데이트

SIGTERM(또는 Ctrl-C)을 받으면 드레인을 시작합니다.

1. `/readyz`가 503이 되고 `wsprism_draining`이 1이 됩니다.
2. 새 연결과 서버 API 발행을 503으로 거절합니다.
3. 세션을 4103 `draining`(프로토콜 v1은 1001)으로 닫습니다. 클라이언트는 잠깐 기다렸다가 다른 노드로 재접속합니다.
4. 클러스터 노드는 큐에 있던 프레임을 Redis에 넘깁니다.
5. 늦어도 `gateway.drain_grace_ms`(기본 2초)가 지나면 종료합니다.

오케스트레이터의 종료 대기 시간은 `drain_grace_ms`보다 길게 잡으세요. 노드가 여럿이면 [롤링 업데이트](cluster.md#롤링-업데이트)를 참고하세요.

## 컨테이너

저장소 루트의 `Dockerfile`은 distroless 이미지를 만듭니다. 루트가 아닌 사용자로 돌고, 셸이 없고, `WSPRISM_LOG_FORMAT=json`이 설정돼 있습니다. 설정 파일은 `/etc/wsprism/wsprism.yaml`에 마운트하고, `gateway.listen`과 `gateway.api.listen`은 `0.0.0.0`에 바인딩하세요. 루프백 주소는 컨테이너 밖에서 닿지 않습니다.

```bash
docker build -t wsprism-gateway .
docker run -v $PWD/my.yaml:/etc/wsprism/wsprism.yaml:ro -p 8080:8080 wsprism-gateway
```

이미지에 `curl`이 없으니 오케스트레이터의 HTTP 프로브를 쓰세요. CA 인증서는 들어 있습니다. 게이트웨이가 `https` 백엔드나 JWKS URL을 부를 때 필요하니, 최소 이미지를 직접 만든다면 꼭 넣으세요.

## 상태 확인

| 엔드포인트 | 응답 | 위치 |
|---|---|---|
| `/healthz` | 프로세스가 살아 있으면 200 | 공개·내부 리스너 |
| `/readyz` | 드레인 중이거나 클러스터 노드가 슬롯을 잃으면 503 | 공개·내부 리스너 |
| `/metrics` | Prometheus 텍스트 형식 | 내부 리스너. `gateway.public_metrics: true`일 때만 공개 리스너에도 |

## 메트릭

종류와 레이블을 포함한 전체 목록은 [메트릭](../reference/metrics.md)에 있습니다. 레이블 값은 설정이나 정해진 집합에서만 오고 클라이언트 입력에서는 오지 않으므로, 시계열 수는 테넌트 수를 넘지 않습니다.

먼저 볼 지표는 이렇습니다.

- 테넌트별 `wsprism_ws_sessions_active`: 갑자기 떨어지면 클라이언트가 끊기고 있다는 뜻입니다.
- 사유별 `wsprism_handshake_rejections_total`: `rate_limit`, `tenant_capacity`, `auth_failed`, `auth_unavailable`, `origin_not_allowed`, `cluster_busy` 등으로 사람들이 왜 못 들어오는지 알 수 있습니다.
- `wsprism_egress_slow_consumer_closes_total`: 신뢰 트래픽을 따라가지 못하는 클라이언트입니다. 늘고 있다면 큐가 작거나, 클라이언트가 읽을 수 있는 것보다 많이 발행하는 룸이 있다는 뜻입니다.
- `wsprism_dispatch_duration_seconds`: 클라이언트 메시지 하나를 처리하는 데 걸리는 시간
- 클러스터라면 `wsprism_broker_outbox_refused_total`과 `wsprism_broker_failed_total`: Redis가 포화돼 거절한 발행, Redis가 거부한 명령

## 경보 예시

```yaml
- alert: WsPrismSessionsDropped
  expr: sum(wsprism_ws_sessions_active) < 0.5 * sum(wsprism_ws_sessions_active offset 10m)
- alert: WsPrismBrokerRefusing
  expr: rate(wsprism_broker_outbox_refused_total[5m]) > 0
- alert: WsPrismBrokerFailures
  expr: rate(wsprism_broker_failed_total[5m]) > 0
- alert: WsPrismSlowConsumers
  expr: rate(wsprism_egress_slow_consumer_closes_total[10m]) > 1
```

노드마다 `/readyz` 프로브 경보도 걸어 두세요.

## 규모 잡기

데스크톱 CPU에서 게이트웨이에 코어 8개를 주고 쟀습니다([벤치마크](../project/benchmarks.md) 참고).

- **메모리:** 연결당 프로세스 메모리 약 18~38 KB에 커널 소켓 버퍼가 더해집니다. 큰 쪽은 모든 룸에 이력을 켰을 때입니다. 이력 룸은 하나당 최대 `max_bytes`만큼 더 듭니다.
- **CPU:** 룸 형태에 따라 전달 하나에 약 3~8 µs입니다. 쓰기를 묶어 보내기 때문에 부하가 클수록 줄어듭니다. 초당 200만 건을 전달할 때도 p99 지연은 10 ms 아래였습니다.
- **접속 폭주:** 클러스터에서 노드당 초당 약 3만 세션

숫자를 약속하기 전에 실제 트래픽으로 재 보세요. 메시지 크기, 룸 크기, Hot과 Ext의 비율에 따라 값이 꽤 달라집니다.

## 보안 점검 목록

- TLS는 게이트웨이 앞에서 종단하세요. 게이트웨이 자체는 평문 HTTP와 WebSocket을 씁니다.
- `dev` 인증기를 바꾸세요. `auth`가 없는 테넌트는 시작할 때 경고를 남깁니다.
- 서버 API는 루프백이나 사설망에 두고, 백엔드마다 키를 따로 주세요.
- `allowed_origins`에는 정확한 Origin을 적으세요.
- 로드밸런서 뒤라면 `trusted_proxies`를 설정해 클라이언트별 속도 제한이 진짜 주소를 보게 하세요.
- 레거시 클라이언트가 없다면 `ticket_sources`는 기본값으로 두세요.
- 테넌트마다 `limits`를 정하세요. 프레임 크기, 세션 수, 룸 수입니다.
- 클러스터라면 [클러스터 운영](cluster.md)의 ACL을 준 전용 Redis 사용자, TLS, `noeviction`을 쓰세요.
- `log_identities: hashed`도 고려해 보세요.
