# 검증 V 진행 상태 (재개용)

[ADR 0004](../adr/0004-product-direction.md)의 검증 단계 V를 **중단했다가 이어서** 할 수 있도록 남기는 살아 있는 문서. 단계가 끝날 때마다 갱신하고 커밋한다.
상세 결과는 [BENCHMARKS.md](../BENCHMARKS.md) 「검증 V」, 설계는 [hot-lane-over-webtransport.md](hot-lane-over-webtransport.md) §7.

- 브랜치: `v/validation` (push 안 함 — 로컬 커밋만)
- 마지막 갱신: 2026-10-14

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
| G1 체감 성능 | ✅ 통과 (60 Hz 판정 칸) | `v-judgment.jsonl`, 20 Hz 참고 `v-20hz.jsonl` |
| G2 호환성 | ⏳ 미측정 — 사용자 필요 | — |
| G3 안정성 | ✅ 통과 | `v-soak*.{jsonl,log}` |
| G4 폴백 | ✅ 통과 | `v-fallback.jsonl` |
| G5 비용 | ❌ 실패 (메모리 2.6~2.9×, CPU는 틱 묶음 시 1.76×) | `v-mem-*`, `v-lean`, `v-cpu-scale` |
| V6 MoQ | 🔄 진행 중 (아래) | `moq-*.jsonl` |

현재 판정상 ADR 0004 분기 **"G5만 실패 → 하이브리드"**. 확정은 G2·MoQ 후.

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
- [ ] M-4 판정 칸 측정(재개 가능 스크립트로) → 커밋
  - [x] MoQ 9회 (`moq-judgment.jsonl`) — 중앙값: 초과 AoI 15.6~15.7 ms, 끊김 0, 지연 p99 46 ms, CPU 11.4~11.7 µs/msg, RSS 단순 평균 약 525 KB/conn
  - [ ] 같은 세션 WSS·WT 기준(틱 0) 18회를 같은 파일에: `bench/run-v.sh moq-judgment 3 "none:20 uniform:1:20 burst:1:4:20" "0" "wss wt"` (재개 가능)
  - [ ] 한계 메모리(연결 100 → 300): `bench/run-moq.sh moq-mem-10 3 none:20` / `bench/run-moq.sh moq-mem-30 3 none:20 "--rooms 30 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 10"`
  - 판정: `python3 bench/analyze-v.py --candidate moq bench/results/moq-judgment.jsonl`
- [ ] M-5 발행자 재접속 시 구독자 동작 확인 — 프로브 `pub`/`sub` 하위 명령(별도 프로세스, `kill -9` = 비정상 종료, `--graceful` = 세션 닫기) 작성·빌드 완료, 실행 전
- [ ] M-6 판단 메모: 직접 구현 / MoQ 위 / MoQ 호환 → BENCHMARKS·ADR 0004 갱신

## 열린 항목

- **WT 연결 꼬리**: UDP가 열려 있어도 100명 동시 접속 시 연결 p99 약 1.13초(초기 패킷 손실 + 약 1초 PTO 추정, 가설: 서버 UDP 수신 버퍼). 폴백 예산 1.5초에 가까움 → T 과제.
- **소크 끊김 비율 6.5%**: 판정 칸보다 큼. 부하 생성기 포화 추정, 미검증.
- **G2 진행 방식**: Windows 브라우저는 릴레이를 WSL localhost에 띄워 확인(장치 안 트래픽). Mac Safari는 **Mac에서 릴레이를 직접 실행**해야 트래픽이 장치 밖으로 나가지 않는다 — 사용자 확인 대기.
