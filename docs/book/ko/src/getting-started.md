# 빠르게 시작하기

내 컴퓨터에 게이트웨이를 띄우고, 클라이언트를 붙이고, 백엔드가 어디에 끼는지 살펴봅니다. 10분이면 충분합니다. Rust 툴체인(1.95 이상)과, 클라이언트를 돌릴 Node.js 22 이상이 필요합니다.

## 1. 게이트웨이 빌드

아직 crates.io에 올리지 않았으니 저장소에서 빌드합니다.

```bash
git clone https://github.com/AsyncNomad/wsPrism
cd wsPrism
cargo build --release -p wsprism-gateway
```

바이너리는 `target/release/wsprism-gateway`에 생깁니다. 저장소 루트의 `Dockerfile`로 컨테이너 이미지를 만들 수도 있습니다([운영](guide/operations.md) 참고).

## 2. 설정 파일 작성

다음 내용을 `quickstart.yaml`로 저장합니다.

```yaml
version: 1
gateway:
  listen: "127.0.0.1:8080"
  api:
    listen: "127.0.0.1:8081"      # 내부용: 서버 API, /metrics, 상태 확인
tenants:
  - id: demo
    auth:
      - kind: dev                 # 개발 전용: "dev.<사용자>"를 받아 줌
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]      # 내장 Hot 릴레이
```

짧은 파일이지만 눈여겨볼 점이 몇 가지 있습니다.

- 모든 것은 **테넌트**에 속합니다. 클라이언트는 접속할 때 테넌트를 밝히고, 인증·정책·한도는 테넌트마다 따로입니다.
- `dev` 인증기는 `dev.<이름>`을 내미는 사람을 누구든 들여보냅니다. 백엔드 없이 써 보라고 있는 것이니 배포에는 절대 쓰지 마세요.
- 허용 목록이 클라이언트가 보낼 수 있는 메시지의 전부입니다. `chat:*`가 없으면 아래에서 쓰는 `chat.send`는 `NOT_ALLOWED`로 거절됩니다.

실행 전에 파일을 검사합니다. 파서가 엄격해서 잘못된 곳이 있으면 해당 필드를 정확히 짚어 줍니다.

```bash
./target/release/wsprism-gateway --config quickstart.yaml --check-config
```

## 3. 실행

```bash
RUST_LOG=info ./target/release/wsprism-gateway --config quickstart.yaml
```

다른 터미널에서 확인합니다.

```bash
curl http://127.0.0.1:8080/healthz     # ok
curl http://127.0.0.1:8080/readyz      # ready
curl -s http://127.0.0.1:8081/metrics | head
```

메트릭은 내부 리스너에서만 나옵니다. 테넌트 이름과 부하가 드러나기 때문에, 따로 설정하지 않으면 공개 포트에는 내보내지 않습니다.

## 4. 클라이언트 연결

JavaScript SDK는 의존성 없는 파일 하나, `sdk/js/wsprism.js`입니다. 다음 코드를 저장소 루트에 `hello.mjs`로 저장하고 `node hello.mjs`로 실행합니다.

```js
import { WsPrismClient } from "./sdk/js/wsprism.js";

const client = new WsPrismClient({
  url: "ws://127.0.0.1:8080/v1/ws",
  tenant: "demo",
  getTicket: async () => "dev.alice",
});

client.on("message", (m) => console.log("message:", m.svc, m.type, m.data));

await client.connect();
await client.join("lobby");
client.send("chat", "send", { room: "lobby", data: { msg: "hello" } });
```

터미널 두 개에서 이름을 바꿔 가며 실행해 보세요. 서로의 메시지가 보입니다. `getTicket`은 접속을 시도할 때마다 불립니다. 실제 서비스에서는 이 함수가 백엔드에서 새 자격 증명을 받아 오기 때문에 문자열이 아니라 함수로 받습니다.

브라우저도 똑같이 쓰면 되는데, 한 가지가 더 필요합니다. 페이지의 Origin이 테넌트의 `policy.allowed_origins`에 있어야 하고, 없으면 핸드셰이크가 403으로 거절됩니다. Node 클라이언트는 `Origin` 헤더를 보내지 않으므로 이 검사를 받지 않습니다.

## 5. 백엔드 연결

`dev` 인증기는 원래 백엔드가 맡을 역할을 임시로 대신했을 뿐입니다. 실제 배포에서는 다음 가운데 하나로 바꿉니다.

- **1회용 티켓.** 백엔드가 평소대로 사용자를 인증한 뒤 게이트웨이에 수명이 짧은 티켓을 요청하고(`POST /api/v1/tickets`), 그 티켓을 클라이언트에 건넵니다.
- **JWT.** 이미 JWT를 발급하고 있다면 게이트웨이가 직접 검증합니다. 정적 키도, IdP의 JWKS 엔드포인트도 쓸 수 있습니다.
- **connect 프록시.** 게이트웨이가 토큰을 백엔드의 HTTP 엔드포인트로 넘기고 판단을 맡깁니다.

그다음부터 백엔드는 룸에 메시지를 보내고(`POST /api/v1/publish`), 사용자를 내보내고(`POST /api/v1/disconnect`), HTTP로 넘어온 클라이언트 요청에 답할 수 있습니다. 하나하나는 [백엔드 연동](guide/backend.md)에서 다룹니다.

## 완성된 예제

`examples/chat-and-state`는 Node 백엔드와 브라우저 페이지로 된 작은 앱입니다. 1회용 티켓과 룸 권한으로 로그인하고, 채팅하고, RPC 프록시를 거쳐 백엔드가 답하는 주사위를 굴리고, Hot 레인과 프레즌스 이벤트로 다른 사람의 커서를 실시간으로 봅니다.

```bash
examples/chat-and-state/run.sh
```

실행한 뒤 브라우저 창 두 개에서 <http://localhost:3000>을 엽니다.

## 다음 단계

- [핵심 개념](guide/concepts.md): 테넌트, 세션, 룸, 레인을 제대로 정의합니다.
- [설정](reference/configuration.md): 모든 설정 항목과 기본값, 허용 범위
- [클러스터 운영](guide/cluster.md): 노드를 하나 더 붙이고 Redis를 씁니다.
