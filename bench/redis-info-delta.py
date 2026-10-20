#!/usr/bin/env python3
"""Redis INFO before/after a run -> one JSON line (bench/run-cluster.sh).

Usage: redis-info-delta.py BEFORE AFTER LABEL
CPU in seconds, commands as counts, memory in bytes (after the run).
"""
import json
import sys


def parse(path):
    d = {}
    for line in open(path):
        if ":" not in line:
            continue
        k, v = line.strip().split(":", 1)
        if k.startswith("cmdstat_"):
            # calls=N,usec=...,...
            fields = dict(x.split("=", 1) for x in v.split(","))
            d[k + ".calls"] = int(fields.get("calls", 0))
            d[k + ".usec"] = int(fields.get("usec", 0))
        else:
            try:
                d[k] = float(v)
            except ValueError:
                pass
    return d


b, a, label = parse(sys.argv[1]), parse(sys.argv[2]), sys.argv[3]
delta = lambda k: a.get(k, 0) - b.get(k, 0)
print(json.dumps({
    "label": label,
    "redis": {
        "cpu_s": round(delta("used_cpu_sys") + delta("used_cpu_user"), 3),
        "commands": int(delta("total_commands_processed")),
        "publish_calls": int(delta("cmdstat_publish.calls")),
        "evalsha_calls": int(delta("cmdstat_evalsha.calls")),
        "publish_usec": int(delta("cmdstat_publish.usec")),
        "evalsha_usec": int(delta("cmdstat_evalsha.usec")),
        "used_memory": int(a.get("used_memory", 0)),
    },
}))
