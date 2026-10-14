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
| V6 MoQ | 🔄 진행 중 (아래) | — |

현재 판정상 ADR 0004 분기 **"G5만 실패 → 하이브리드"**. 확정은 G2·MoQ 후.

## 실험 재개 방법

`bench/run-v.sh`는 **재개 가능**하다: 출력 파일에 이미 있는 (transport, label)은 건너뛴다. 중단되면 같은 명령을 다시 실행한다. 처음부터 다시 하려면 결과 파일을 지운다.

```bash
# 판정 칸 (81회, 약 25분)
bench/run-v.sh v-judgment 3
python3 bench/analyze-v.py bench/results/v-judgment.jsonl
```

## V6 MoQ 실측 — 단계별 체크리스트

목표: 같은 판정 칸(20 ms, 60 Hz, 10×10, 손실 없음/균등 1%/버스트)에서 MoQ 경로의 AoI·끊김·CPU·메모리를 재고, 발행자 재접속 시 구독 유지 여부를 확인. 시간 상한 3일(2026-10-14 시작).

- [x] M-0 코드 조사: 한 세션 발행+구독 가능, datagram 중계 가능, CAT 인가, quinn 기반, README 일부 낡음 (BENCHMARKS 「V6」)
- [ ] M-1 `moq-relay-ietf` 빌드·실행 방법 확인(CLI, TLS), netlab에서 기동
- [ ] M-2 부하 생성기 `--transport moq`: 발행(참가자별 트랙, 업데이트마다 datagram 객체) + 구독(같은 룸의 다른 참가자 트랙)
- [ ] M-3 스모크(손실 없음, 2×5) → 커밋
- [ ] M-4 판정 칸 측정(재개 가능 스크립트로) → 커밋
- [ ] M-5 발행자 재접속 시 구독자 동작 확인
- [ ] M-6 판단 메모: 직접 구현 / MoQ 위 / MoQ 호환 → BENCHMARKS·ADR 0004 갱신

## 열린 항목

- **WT 연결 꼬리**: UDP가 열려 있어도 100명 동시 접속 시 연결 p99 약 1.13초(초기 패킷 손실 + 약 1초 PTO 추정, 가설: 서버 UDP 수신 버퍼). 폴백 예산 1.5초에 가까움 → T 과제.
- **소크 끊김 비율 6.5%**: 판정 칸보다 큼. 부하 생성기 포화 추정, 미검증.
- **G2 진행 방식**: Windows 브라우저는 릴레이를 WSL localhost에 띄워 확인(장치 안 트래픽). Mac Safari는 **Mac에서 릴레이를 직접 실행**해야 트래픽이 장치 밖으로 나가지 않는다 — 사용자 확인 대기.
