# 검증 V 진행 상태 (재개용)

[ADR 0004](../adr/0004-product-direction.md)의 검증 단계 V를 **중단했다가 이어서** 할 수 있도록 남기는 살아 있는 문서. 단계가 끝날 때마다 갱신하고 커밋한다.
상세 결과는 [BENCHMARKS.md](../BENCHMARKS.md) 「검증 V」, 설계는 [hot-lane-over-webtransport.md](hot-lane-over-webtransport.md) §7.

- 브랜치: `v/validation` (push 안 함 — 로컬 커밋만)
- 마지막 갱신: 2026-10-16 (P1-a까지)

## 환경 준비 (새 셸·새 세션)

```bash
export PATH=$HOME/.cargo/bin:$PATH        # rustup으로 설치한 stable (nightly는 퍼징용)
cd ~/code/wsPrism && git switch v/validation
(cd spikes/wt-hotlane && cargo build --release)   # 버리는 스파이크 릴레이
cargo build --release -p wsprism-loadgen
```

- 모든 실험은 `bench/netlab.sh`(비특권 user namespace 안의 netns 두 개 + veth)에서만 돈다. **테스트 트래픽은 장치 밖으로 나가지 않는다**(기본 경로 없음, `10.77.0.0/24`만).
- root·sudo 불필요.
- 측정 중에는 CPU를 쓰는 다른 작업(컴파일 등)을 하지 않는다 — 비용 지표가 오염된다.

## 관문 현황

| 관문 | 상태 | 근거 (결과 파일) |
|---|---|---|
| G1 체감 성능 | ✅ **통과 (개정 4 사전 등록 기준, 시드 4~6 확인 측정)** — 즉시·60 Hz 모두. 개정 3 기준으로는 단절 칸 실패(공개) | `v2-judgment.jsonl`(탐색), `v2-confirm.jsonl`(판정) |
| G2 호환성 | ⏳ 미측정 — 사용자 필요 | — |
| G3 안정성 | ✅ 통과 | `v-soak*.{jsonl,log}` |
| G4 폴백 | ✅ 통과 | `v-fallback.jsonl` |
| G5 비용 | ❌ 실패 — 오프로드 켬 조건: 한계 메모리 2.8~2.9×, CPU 1.65~1.89×(기준 안) | `v2-cost-{10,30}`, `v-mem-*`, `v-cpu-scale` |
| V6 MoQ | ✅ 완료 — MoQ 위 구축 기각 (ADR 0004 개정 3) | `moq-*.jsonl`, `moq-reconnect-*.log` |

**분기 확정: "G5만 실패 → 하이브리드"** (2026-10-16). ADR 0004 확정은 G2 후. 다음 작업: T0 (아래 「T0 진행」).

## 실험 재개 방법

`bench/run-v.sh`는 **재개 가능**하다: 출력 파일에 이미 있는 (transport, label)은 건너뛴다. 중단되면 같은 명령을 다시 실행한다. 처음부터 다시 하려면 결과 파일을 지운다.

```bash
# 판정 칸 (81회, 약 25분)
bench/run-v.sh v-judgment 3
python3 bench/analyze-v.py bench/results/v-judgment.jsonl
```

```bash
# MoQ 판정 칸 (9회) — 재개 가능, 같은 명령 재실행
bench/run-moq.sh moq-judgment 3
```

## V6 MoQ 실측 — 단계별 체크리스트

목표: 같은 판정 칸(20 ms, 60 Hz, 10×10, 손실 없음/균등 1%/버스트)에서 MoQ 경로의 AoI·끊김·CPU·메모리를 재고, 발행자 재접속 시 구독 유지 여부를 확인. 시간 상한 3일(2026-10-14 시작).

- [x] M-0 코드 조사: 한 세션 발행+구독 가능, datagram 중계 가능, CAT 인가, quinn 기반, README 일부 낡음 (BENCHMARKS 「V6」)
- [x] M-1 `moq-relay-ietf` 0.7.29 설치(`cargo install moq-relay-ietf --version 0.7.29 --locked`), 실행마다 자가 서명 인증서(SAN IP 10.77.0.1)를 만들어 netlab에서 `--bind 10.77.0.1:4443`로 기동 — `bench/run-moq.sh`
- [x] M-2 버리는 프로브 `spikes/moq-probe` (제품 부하 생성기에 넣지 않음 — MoQ 의존성을 제품 워크스페이스에 들이지 않기 위해). AoI·프레임·비용 코드는 부하 생성기 파일을 `#[path]`로 그대로 써서 지표가 같다. 참가자별 네임스페이스 `r<room>/p<id>`, 트랙 `state`, 업데이트마다 datagram 객체 1개, 같은 룸의 다른 참가자 트랙 구독(발행 전이면 100 ms 간격 재시도)
- [x] M-3 스모크(20 ms, 손실 없음, 2×5, 5초): 구독 40/40, 전달률 1.0, 지연 p99 42.6 ms, AoI p99 51.2 ms → `moq-smoke.jsonl`
- [x] M-4 판정 칸 측정(재개 가능 스크립트로) → 커밋
  - [x] MoQ 9회 (`moq-judgment.jsonl`) — 중앙값: 초과 AoI 15.6~15.7 ms, 끊김 0, 지연 p99 46 ms, CPU 11.4~11.7 µs/msg, RSS 단순 평균 약 525 KB/conn
  - [x] 같은 세션 WSS·WT 기준(틱 0) 18회를 같은 파일에: `bench/run-v.sh moq-judgment 3 "none:20 uniform:1:20 burst:1:4:20" "0" "wss wt"` (재개 가능)
  - [x] 한계 메모리(연결 100 → 300, 약 477 KB/conn): `bench/run-moq.sh moq-mem-10 3 none:20` / `bench/run-moq.sh moq-mem-30 3 none:20 "--rooms 30 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 10"`
  - 판정: `python3 bench/analyze-v.py --candidate moq bench/results/moq-judgment.jsonl`
- [x] M-5 발행자 재접속: `bench/run-moq-reconnect.sh graceful|abrupt` — 정상 종료는 재구독으로 약 1.1초 공백, 비정상 종료는 재발행 거절·13초 넘게 정지
- [x] M-6 판단: MoQ 위 구축 기각, 정면 경쟁 기각, 와이어 호환은 T 이후 재검토 → BENCHMARKS 「V6」, ADR 0004 개정 3

## 4차 리뷰 후 순서 (사용자 승인 2026-10-14)

1. **측정 수정 → G1·G5 재판정** (아래 R 체크리스트)
2. 경쟁 게이트웨이(Centrifugo, Sockudo)와 WebSocket 경로 직접 비교 (netlab 안) — ✅ 본 측정 60회 완료(BENCHMARKS 「경쟁 게이트웨이와의 WebSocket 비교」). 포화점 추가 측정(rooms:160·240, wsPrism Ext·Centrifugo, 각 3회)은 같은 파일에 재개 가능:
   ```bash
   bench/run-compete.sh compete-v1 3 "rooms:160 rooms:240" "wsprism-ext centrifugo"
   ```
   ✅ 완료: 216만 전달/s에서 wsPrism 포화(전달률 0.65~0.86), Centrifugo 유지 → T0 쓰기 묶음 후 재측정.
   본 측정 명령:
   ```bash
   bench/competitors/fetch.sh   # Centrifugo 6.9.7, Sockudo 5.1.0 (체크섬 검증)
   bench/run-compete.sh compete-v1 3 "rooms:20 rooms:60 rooms:120 broadcast:10 broadcast:50"
   python3 bench/analyze-compete.py bench/results/compete-v1.jsonl
   ```
3. ✅ 서버 피어 멤버 지정 송신을 설계 §4.6에 반영 (T0 경계 요건)
4. G2 브라우저 실증 (사용자 필요)
5. 수요 확인은 사용자 판단 영역

### R: 측정 수정 체크리스트

- [x] R-1 부하 생성기 `--phase random|aligned`(기본 random, `--seed`) — 클라이언트별 송신·렌더 위상 무작위. `aligned`는 이전 결과 재현용
- [x] R-2 끊김 기준 민감도: JSON에 `stall_ratio_at` (50/100/150/200/300 ms), 분석기 출력
- [x] R-3 netlab `--outage MEAN_PERIOD_MS:LEN_MS`(양방향 동시 단절, `NETLAB_SEED`) + run-v.sh 손실 조건 `outage:<period>:<len>:<delay>`
- [x] R-4 netlab `NETLAB_OFFLOAD=on` (손실 없는 비용 측정 전용)
- [x] R-5 스모크(`v2-smoke.jsonl`, n=1, 틱 0): 손실 0% AoI p50 49.4 / p99 57.8(계단 사라짐). 균등 1% 초과 AoI WT 9.8 vs WSS 53.8(5.5×), 단절 1000:50 초과 2.4×(기준 미달)·끊김 3.1% vs 13.2%
- [x] R-6 판정 칸 재측정: 손실 {없음, 균등 1%, 단절 1000:50, 단절 5000:250} × 틱 {0, 60, 30} × {ws, wss, wt} × 3회, 측정 20초 → `v2-judgment.jsonl` (108회, 약 1시간, 재개 가능):
  ```bash
  bench/run-v.sh v2-judgment 3 "none:20 uniform:1:20 outage:1000:50:20 outage:5000:250:20" "0 60 30" "ws wss wt" \
    "--rooms 10 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 20"
  python3 bench/analyze-v.py bench/results/v2-judgment.jsonl
  ```
- [x] R-7 비용 재측정(오프로드 켬, `v2-cost-{10,30}.jsonl`): 한계 메모리 WSS 30.7 / WT 85.5~90.1 KB/conn(**2.8~2.9×**, 실패 유지), CPU WT÷WSS 1.65~1.89×(기준 안, 오프로드 끈 측정 1.76×보다 약간 커짐)
- [x] R-8a 단절 칸 기준 사전 등록(ADR 0004 개정 4) + 판정 코드 `bench/analyze-g1-confirm.py`(탐색 데이터로 손계산 재현 확인) — 측정 전 커밋
- [x] R-8b 확인 측정(시드 4~6, 36회, 약 25분, 재개 가능):
  ```bash
  FIRST_RUN=4 bench/run-v.sh v2-confirm 3 "none:20 outage:1000:50:20 outage:5000:250:20" "0 60" "wss wt" \
    "--rooms 10 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 20"
  python3 bench/analyze-g1-confirm.py bench/results/v2-confirm.jsonl
  ```
- [x] R-8b 결과: G1 통과(즉시·60 Hz), 가장 얇은 여유 60 Hz·50 ms 단절 끊김 4.71% (기준 5.34%)
- [x] R-8c G1·G5 최종 판정 → BENCHMARKS·ADR 0004(분기 확정)·ROADMAP·이 문서

## 열린 항목

- **wsPrism Hot 레인 p99가 Ext보다 나쁨**(compete 스모크: 1.60 vs 0.79 ms, 180k 전달/s) — 손실 허용 경로의 큐·플러시 방식 조사 필요.

- **WT 연결 꼬리**: UDP가 열려 있어도 100명 동시 접속 시 연결 p99 약 1.13초(초기 패킷 손실 + 약 1초 PTO 추정, 가설: 서버 UDP 수신 버퍼). 폴백 예산 1.5초에 가까움 → T 과제.
- **소크 끊김 비율 6.5%**: 판정 칸보다 큼. 부하 생성기 포화 추정, 미검증.
- **G2 진행 방식**: Windows 브라우저는 릴레이를 WSL localhost에 띄워 확인(장치 안 트래픽). Mac Safari는 **Mac에서 릴레이를 직접 실행**해야 트래픽이 장치 밖으로 나가지 않는다 — 사용자 확인 대기.

## T0 진행 (경계, WebSocket만 — 설계 §9)

브랜치: `v/validation`에서 이어서 한다(로컬 커밋만). 각 단계는 동작 변화 없는 리팩터링이거나, 동작 변화가 있으면 테스트를 먼저 쓴다. 게이트: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`.

- [x] T0-1 `SessionIo` 추출 → 읽기·쓰기 두 절반 `SessionTx`/`SessionRx`([ADR 0005](../adr/0005-session-transport-seam.md)): 세션 루프·종료 처리가 axum `WebSocket`(Split 타입) 대신 트레이트를 쓴다. WebSocket 구현 `WsIo`는 `wire`·`close`·`codec`을 감싼다. 동작 변화 없음
- [x] T0 쓰기 합치기(버퍼 없는 `feed`×N + `flush` 1회, 최대 64) — 216만/s 포화 해소, 저부하 회귀 없음(BENCHMARKS 「T0: 쓰기 합치기」)
- [ ] 성능 P0 (설계 [performance-architecture.md](performance-architecture.md) §11): ✅ P0-a 쓰기 합치기, ✅ P0-b ID 규칙·사용자 분리 벤치, ✅ P0-c 중첩 DashSet 제거, ✅ P0-g 쓰기 타임아웃, ✅ P0-d 종료 신호·스위퍼 (유휴 24.2 → 16.2 KB/연결). ✅ P0-f mimalloc 기각. ✅ P1-a 룸 멤버 핸들(팬아웃 수신자당 200 → 35 ns). 남은 것: P0-e 메트릭·GCRA(낮음), P1-b ID 인터닝(낮음). **다음 큰 레버: P2 전송 재작성(커널 송신 46%·프레이밍 14%) — ADR 필요, 1단계 axum 0.8은 의존성 업그레이드(네트워크) 확인 후.** 프로토콜 v2는 [ADR 0006](../adr/0006-protocol-v2.md) 채택(리뷰 반영) — v2-1 ✅(협상·종료 코드), 다음 v2-2(제어 봉투). P2는 [ADR 0007](../adr/0007-transport-own-framing.md) 제안(axum 0.8 건너뛰고 자체 프레이밍) — 사용자 승인 대기 프로파일: `bench/profile-gateway.sh <이름>` (perf 6.8 설치됨; 재부팅 시 `perf_event_paranoid=1`, `kptr_restrict=0` 재설정 필요)
- [ ] T0-2 전달 등급: `OutFrame::Hot { key, bytes }`, `RealtimeCore::deliver`의 Lossy 바이너리 → Hot 경로
- [ ] T0-3 Hot 슬롯 `(sender, svc, key)` 최신값 + 짧은 FIFO + 상한, 틱 묶음 전송(WebSocket) — 큐 적체·드롭 지표, 포화점 시나리오(`rooms:160·240`) 재측정
- [ ] T0-4 Hot 헤더 확장(KEY/SENDER), 연결별 `member_id`, `HotRelayService`, 크기 상한(PROTOCOL.md)
- [ ] T0-5 코어 전달 API를 `Audience`(룸·멤버 지정·세션·사용자)로 통합 — 서버 피어 멤버 지정 송신 경계(설계 §4.6)
- [ ] T0-6 완료 조건 확인: `SessionIo`·전달 등급이 브라우저 세션을 전제하지 않음
