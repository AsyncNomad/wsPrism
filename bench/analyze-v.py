#!/usr/bin/env python3
"""Judge validation V gates G1 and G5 from bench/run-v.sh results.

Rules (ADR 0004, revision 2):
- Medians over runs; the min..max range is printed to show noise.
- Excess AoI p99 = AoI p99 at (transport, tick, loss) - AoI p99 at
  (transport, tick, no loss) with the same delay.
- Every comparison is between REAL configurations: WT at tick T against WSS at
  the SAME tick T (primary). Taking the best WSS value per metric from
  different ticks would compare against a WSS that does not exist.
  Also printed: WT at T against the single WSS tick with the best AoI.
- G1, under BOTH uniform and burst loss: excess AoI p99 <= 1/3 of WSS and
  stall ratio <= 1/3 (if WSS has no stalls, WT must have none). 0% control:
  WT AoI p50 and p99 within 10% of WSS.
- G5: WT CPU/message and memory/connection <= 2x WSS, worst ratio across
  loss conditions.
- Same-configuration rule: pass only if one tick satisfies G1 and G5.

The candidate is WT by default; --candidate moq judges the MoQ relay path
(bench/run-moq.sh) the same way. Several files may be given; their rows are
pooled, so a candidate measured in one file can be judged against WSS in the
same file (preferred: same session, CPU noise between sessions is ~20%).

Usage: bench/analyze-v.py [--candidate wt|moq] FILE...
"""
import argparse
import json
import statistics
from collections import defaultdict


def parse_label(label):
    return dict(kv.split("=", 1) for kv in label.split())


def load(paths):
    rows = defaultdict(list)
    for path in paths:
        for line in open(path):
            line = line.strip()
            if not line.startswith("{"):
                continue
            d = json.loads(line)
            lab = parse_label(d["label"])
            rows[(d["transport"], lab["loss"], int(lab["tick"]))].append(d)
    return rows


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def rng(xs):
    xs = [x for x in xs if x is not None]
    return (min(xs), max(xs)) if xs else (None, None)


def metrics(runs):
    return {
        "aoi_p50": med([r["aoi_ms"]["p50"] for r in runs]),
        "aoi_p99": med([r["aoi_ms"]["p99"] for r in runs]),
        "aoi_p99_rng": rng([r["aoi_ms"]["p99"] for r in runs]),
        "stall": med([r["stall_ratio"] for r in runs]),
        "stall_rng": rng([r["stall_ratio"] for r in runs]),
        "lat_p99": med([r["latency_ms"]["p99"] for r in runs]),
        "deliv": med([r["delivery_ratio"] for r in runs]),
        "cpu": med([r["server_cpu_us_per_msg"] for r in runs]),
        "cpu_rng": rng([r["server_cpu_us_per_msg"] for r in runs]),
        "mem": med([r["server_rss_kb_per_conn"] for r in runs]),
        "broken": sum(r["broken_links"] for r in runs),
        "n": len(runs),
    }


def main(paths, cand):
    rows = load(paths)
    m = {k: metrics(v) for k, v in rows.items()}
    transports = sorted({k[0] for k in m})
    losses = sorted({k[1] for k in m})
    ticks = sorted({k[2] for k in m})
    base_loss = next((l for l in losses if l.startswith("none")), None)
    lossy = [l for l in losses if l != base_loss]

    def excess(t, loss, tick):
        a, b = m.get((t, loss, tick)), m.get((t, base_loss, tick))
        if not a or not b or a["aoi_p99"] is None or b["aoi_p99"] is None:
            return None
        return a["aoi_p99"] - b["aoi_p99"]

    print(f"{'transport':4} {'loss':14} {'tick':>4} {'n':>2} | {'AoI p50':>7} {'p99':>7} {'(range)':>15} {'excess':>7} "
          f"{'stall%':>7} | {'lat p99':>7} {'deliv':>6} | {'cpu us/msg':>10} {'mem KB/conn':>11} broken")
    for loss in losses:
        for tick in ticks:
            for t in transports:
                x = m.get((t, loss, tick))
                if not x:
                    continue
                ex = excess(t, loss, tick) if loss != base_loss else 0.0
                r0, r1 = x["aoi_p99_rng"]
                print(f"{t:4} {loss:14} {tick:>4} {x['n']:>2} | {x['aoi_p50']:7.1f} {x['aoi_p99']:7.1f} "
                      f"({r0:6.1f}-{r1:6.1f}) {ex if ex is not None else float('nan'):7.1f} {x['stall']*100:7.3f} | "
                      f"{x['lat_p99']:7.1f} {x['deliv']:6.3f} | {x['cpu'] or float('nan'):10.2f} "
                      f"{x['mem'] or float('nan'):11.1f} {x['broken']}")
        print()

    if cand not in transports or "wss" not in transports:
        return
    print(f"== Gates ({cand.upper()} vs WSS) ==")
    # Only ticks at which the candidate was measured (MoQ forwards immediately: tick 0).
    cticks = [tk for tk in ticks if all((cand, l, tk) in m and ("wss", l, tk) in m for l in losses)]

    def judge(tick, wss_tick):
        notes, g1 = [], True
        for loss in lossy:
            wt_ex, wss_ex = excess(cand, loss, tick), excess("wss", loss, wss_tick)
            wt_st, wss_st = m[(cand, loss, tick)]["stall"], m[("wss", loss, wss_tick)]["stall"]
            ex_ok = wt_ex is not None and wss_ex is not None and wt_ex <= wss_ex / 3
            st_ok = wt_st <= wss_st / 3 if wss_st > 0 else wt_st == 0
            g1 &= ex_ok and st_ok
            notes.append(f"{loss}: excess {wt_ex:.1f} vs {wss_ex:.1f} ms ({wss_ex / max(wt_ex, 1e-9):.1f}x) {'ok' if ex_ok else 'FAIL'}; "
                         f"stall {wt_st*100:.3f}% vs {wss_st*100:.3f}% {'ok' if st_ok else 'FAIL'}")
        c_wt, c_wss = m[(cand, base_loss, tick)], m[("wss", base_loss, wss_tick)]
        ctrl = c_wt["aoi_p50"] <= c_wss["aoi_p50"] * 1.1 and c_wt["aoi_p99"] <= c_wss["aoi_p99"] * 1.1
        g1 &= ctrl
        notes.append(f"0% control: AoI p50 {c_wt['aoi_p50']:.1f} vs {c_wss['aoi_p50']:.1f}, p99 {c_wt['aoi_p99']:.1f} vs {c_wss['aoi_p99']:.1f} {'ok' if ctrl else 'FAIL'}")
        cpu = [m[(cand, l, tick)]["cpu"] / m[("wss", l, wss_tick)]["cpu"] for l in losses]
        mem = [m[(cand, l, tick)]["mem"] / m[("wss", l, wss_tick)]["mem"] for l in losses]
        g5 = max(cpu) <= 2.0 and max(mem) <= 2.0
        notes.append(f"cost: cpu {min(cpu):.2f}-{max(cpu):.2f}x, mem {min(mem):.2f}-{max(mem):.2f}x")
        return g1, g5, notes

    any_pass = False
    for tick in cticks:
        g1, g5, notes = judge(tick, tick)
        any_pass |= g1 and g5
        print(f"{cand.upper()} tick={tick} vs WSS tick={tick}: G1 {'PASS' if g1 else 'FAIL'}, G5 {'PASS' if g5 else 'FAIL'}"
              f"{' -> BOTH' if g1 and g5 else ''}")
        for n in notes:
            print(f"    {n}")

    # Reference: against the single WSS configuration with the best AoI under loss.
    if not lossy:
        print("\n(no lossy conditions in this file: G1 cannot be judged; cost ratios above)")
        return
    wss_best = min(ticks, key=lambda tk: max(m[("wss", l, tk)]["aoi_p99"] for l in lossy))
    print(f"\nReference: {cand.upper()} vs the best-AoI WSS configuration (tick={wss_best})")
    for tick in cticks:
        g1, g5, notes = judge(tick, wss_best)
        print(f"  {cand.upper()} tick={tick}: G1 {'PASS' if g1 else 'FAIL'}, G5 {'PASS' if g5 else 'FAIL'}; {notes[-1]}")

    print(f"\nSame-configuration rule: {'PASS — a tick satisfies G1 and G5' if any_pass else 'FAIL — no tick satisfies both'}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--candidate", default="wt", help="transport judged against WSS (wt, moq)")
    ap.add_argument("files", nargs="*", default=["bench/results/v-judgment.jsonl"])
    a = ap.parse_args()
    main(a.files, a.candidate)
