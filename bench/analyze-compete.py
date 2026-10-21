#!/usr/bin/env python3
"""Summarize bench/run-compete.sh results: medians over runs per
(scenario, target), and each competitor relative to wsPrism.

Usage: bench/analyze-compete.py bench/results/<name>.jsonl [BASELINE_TARGET]
BASELINE_TARGET defaults to wsprism-ext (JSON on the wire like the others).
"""
import json
import statistics
import sys
from collections import defaultdict


def load(path):
    rows = defaultdict(list)
    for line in open(path):
        if not line.startswith("{"):
            continue
        d = json.loads(line)
        lab = dict(kv.split("=", 1) for kv in d["label"].split())
        rows[(lab["scenario"], lab["target"])].append(d)
    return rows


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def summarize(runs):
    return {
        "n": len(runs),
        "p50": med([r["latency_us"]["p50"] / 1000 for r in runs]),
        "p99": med([r["latency_us"]["p99"] / 1000 for r in runs]),
        "p999": med([r["latency_us"]["p999"] / 1000 for r in runs]),
        "deliv": med([r["delivery_ratio"] for r in runs]),
        "rate": med([r["delivered_per_s"] for r in runs]),
        "echo": med([r["self_echo"] for r in runs]),
        "cpu": med([r["server_cpu_us_per_delivery"] for r in runs]),
        "rss": med([r["server_rss_kb_per_conn"] for r in runs]),
        "closed": sum(r["closed_early"] for r in runs),
        "lg": med([r.get("loadgen_cores") for r in runs]),
    }


def fmt(x, spec):
    return format(x, spec) if x is not None else "n/a"


def main(path, base):
    m = {k: summarize(v) for k, v in load(path).items()}
    for sc in sorted({k[0] for k in m}):
        print(f"== scenario {sc}")
        print(f"{'target':12} {'n':>2} | {'p50 ms':>7} {'p99':>7} {'p99.9':>7} | {'deliv':>6} {'deliv/s':>9} {'echo':>8} "
              f"| {'us CPU/deliv':>12} {'KB/conn':>8} {'lg cores':>8} | vs {base}: cpu, mem, p99")
        b = m.get((sc, base))
        for t in sorted(k[1] for k in m if k[0] == sc):
            x = m[(sc, t)]
            rel = ""
            if b and t != base:
                r = lambda a, c: fmt(a / c if a and c else None, ".2f") + "x"
                rel = f"{r(x['cpu'], b['cpu'])}, {r(x['rss'], b['rss'])}, {r(x['p99'], b['p99'])}"
            print(f"{t:12} {x['n']:>2} | {fmt(x['p50'], '7.2f')} {fmt(x['p99'], '7.2f')} {fmt(x['p999'], '7.2f')} | "
                  f"{fmt(x['deliv'], '6.3f')} {fmt(x['rate'], '9.0f')} {fmt(x['echo'], '8.0f')} | "
                  f"{fmt(x['cpu'], '12.2f')} {fmt(x['rss'], '8.1f')} {fmt(x['lg'], '8.2f')} | {rel}"
                  + (f"  closed_early={x['closed']}" if x["closed"] else ""))
        print()


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "wsprism-ext")
