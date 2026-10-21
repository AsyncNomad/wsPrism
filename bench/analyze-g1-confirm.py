#!/usr/bin/env python3
"""Pre-registered G1 judgment for link outages (ADR 0004, revision 4).

Written and committed BEFORE the confirmatory measurement it judges. The rule
must not be edited after the confirmatory results exist; a change needs a new
ADR revision and a new measurement with unused seeds.

Why a new rule: the revision 2/3 bar ("excess AoI p99 <= 1/3 of WSS") cannot
be met by ANY transport under an outage of length L, because L itself is a
floor of the excess (nothing arrives while the link is down). Revision 4
judges what a transport can influence: the delay it adds AFTER the link
comes back.

Rule, per outage cell (length L ms) and tick T, candidate (WT) vs WSS at the
same tick, medians over runs:
  excess    = AoI p99(outage) - AoI p99(no loss)        (same transport, tick)
  recovery  = max(0, excess - L)
  recovery  : WT <= WSS / 3  AND  WSS - WT >= one 60 Hz frame (16.7 ms)
  stall     : only where it is physically achievable (L < 100 ms):
              WT stall ratio (AoI > 100 ms) <= WSS / 3; for L >= 100 reported only
  0% control: WT AoI p50 and p99 within 10% of WSS
Uniform 1% loss keeps the revision 2 rule (excess <= 1/3 and stall <= 1/3),
judged from the uniform file (already measured).
G1 passes if one tick in {0, 60} passes every cell (uniform + both outages).

Usage:
  bench/analyze-g1-confirm.py CONFIRM_FILE [--uniform FILE]
"""
import argparse
import json
import statistics
from collections import defaultdict

FRAME_MS = 1000 / 60
TICKS = (0, 60)


def load(path):
    rows = defaultdict(list)
    for line in open(path):
        line = line.strip()
        if not line.startswith("{"):
            continue
        d = json.loads(line)
        lab = dict(kv.split("=", 1) for kv in d["label"].split())
        rows[(d["transport"], lab["loss"], int(lab["tick"]))].append(d)
    return rows


def med(rows, f):
    return statistics.median(f(r) for r in rows)


def cell(rows, t, loss, tick):
    rs = rows.get((t, loss, tick))
    if not rs:
        return None
    return {
        "p50": med(rs, lambda r: r["aoi_ms"]["p50"]),
        "p99": med(rs, lambda r: r["aoi_ms"]["p99"]),
        "stall": med(rs, lambda r: r["stall_ratio"]),
        "seeds": sorted(int(dict(kv.split("=", 1) for kv in r["label"].split())["run"]) for r in rs),
    }


def base_loss(rows):
    return next(l for (_, l, _) in rows if l.startswith("none"))


def main(confirm_path, uniform_path):
    rows = load(confirm_path)
    base = base_loss(rows)
    outages = sorted({l for (_, l, _) in rows if l.startswith("outage")},
                     key=lambda l: int(l.split(":")[2]))
    urows = load(uniform_path) if uniform_path else None

    verdicts = {}
    for tick in TICKS:
        ok_tick, notes = True, []
        b_wt, b_wss = cell(rows, "wt", base, tick), cell(rows, "wss", base, tick)
        if not b_wt or not b_wss:
            print(f"tick={tick}: missing no-loss baseline, skipped")
            continue
        ctrl = b_wt["p50"] <= b_wss["p50"] * 1.1 and b_wt["p99"] <= b_wss["p99"] * 1.1
        ok_tick &= ctrl
        notes.append(f"0% control: p50 {b_wt['p50']:.1f}/{b_wss['p50']:.1f}, p99 {b_wt['p99']:.1f}/{b_wss['p99']:.1f} "
                     f"(WT/WSS) {'ok' if ctrl else 'FAIL'}  seeds {b_wt['seeds']}")

        for loss in outages:
            L = int(loss.split(":")[2])
            c_wt, c_wss = cell(rows, "wt", loss, tick), cell(rows, "wss", loss, tick)
            if not c_wt or not c_wss:
                ok_tick = False
                notes.append(f"{loss}: missing runs -> FAIL")
                continue
            rec_wt = max(0.0, c_wt["p99"] - b_wt["p99"] - L)
            rec_wss = max(0.0, c_wss["p99"] - b_wss["p99"] - L)
            rec_ok = rec_wt <= rec_wss / 3 and rec_wss - rec_wt >= FRAME_MS
            if L < 100:
                st_ok = c_wt["stall"] <= c_wss["stall"] / 3 if c_wss["stall"] > 0 else c_wt["stall"] == 0
                st_txt = f"stall {c_wt['stall']*100:.2f}% vs {c_wss['stall']*100:.2f}% {'ok' if st_ok else 'FAIL'}"
            else:
                st_ok = True
                st_txt = f"stall {c_wt['stall']*100:.2f}% vs {c_wss['stall']*100:.2f}% (reported only, L >= 100 ms)"
            ok_tick &= rec_ok and st_ok
            notes.append(f"{loss}: recovery {rec_wt:.1f} vs {rec_wss:.1f} ms "
                         f"(diff {rec_wss - rec_wt:.1f}) {'ok' if rec_ok else 'FAIL'}; {st_txt}  seeds {c_wt['seeds']}")

        if urows:
            ub = base_loss(urows)
            for loss in sorted({l for (_, l, _) in urows if l.startswith("uniform")}):
                u = {t: cell(urows, t, loss, tick) for t in ("wt", "wss")}
                b = {t: cell(urows, t, ub, tick) for t in ("wt", "wss")}
                if not all(u.values()) or not all(b.values()):
                    continue
                ex = {t: u[t]["p99"] - b[t]["p99"] for t in u}
                ex_ok = ex["wt"] <= ex["wss"] / 3
                st_ok = u["wt"]["stall"] <= u["wss"]["stall"] / 3 if u["wss"]["stall"] > 0 else u["wt"]["stall"] == 0
                ok_tick &= ex_ok and st_ok
                notes.append(f"{loss} (rev 2 rule, {uniform_path.split('/')[-1]}): excess {ex['wt']:.1f} vs {ex['wss']:.1f} ms "
                             f"{'ok' if ex_ok else 'FAIL'}; stall {u['wt']['stall']*100:.2f}% vs {u['wss']['stall']*100:.2f}% "
                             f"{'ok' if st_ok else 'FAIL'}")
        verdicts[tick] = ok_tick
        print(f"WT tick={tick} vs WSS tick={tick}: {'PASS' if ok_tick else 'FAIL'}")
        for n in notes:
            print(f"    {n}")

    g1 = any(verdicts.values())
    print(f"\nG1 (revision 4): {'PASS' if g1 else 'FAIL'}"
          f" — passing ticks: {[t for t, v in verdicts.items() if v] or 'none'}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("confirm")
    ap.add_argument("--uniform", default="bench/results/v2-judgment.jsonl")
    a = ap.parse_args()
    main(a.confirm, a.uniform)
