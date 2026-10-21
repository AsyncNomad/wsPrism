#!/usr/bin/env python3
"""Summarize a folded perf profile of the gateway.

1. Self time by leaf frame (top 40).
2. Exclusive categories: every sample is attributed to exactly ONE category —
   the first rule (in priority order) that matches any frame of its stack, so
   the shares add up to 100%. Rules are ordered from specific to general.

Usage: bench/profile-top.py FILE.folded
"""
import re
import sys
from collections import Counter

# (category, regex on a frame). First matching rule wins for the whole stack.
RULES = [
    ("kernel: TCP send (incl. veth peer receive in netlab)", r"__sys_sendto|tcp_sendmsg|__x64_sys_sendto|sock_sendmsg"),
    ("kernel: TCP receive", r"__sys_recvfrom|tcp_recvmsg|__x64_sys_recvfrom"),
    ("kernel: epoll / futex / sched", r"epoll_wait|do_epoll|futex|__schedule|schedule_hrtimeout"),
    ("utf8 re-validation (to_message)", r"from_utf8_lossy|Utf8Chunks"),
    ("ws framing (tungstenite)", r"tungstenite"),
    ("split-socket BiLock", r"bilock|BiLock"),
    ("per-write timeout timer", r"time.*timeout.*Timeout|tokio4time5sleep|Sleep.*poll|TimerEntry|time.*entry"),
    ("presence / registry (dashmap)", r"dashmap|session_regi|presence|Presence"),
    ("publish_room / deliver", r"publish_room|deliver"),
    ("outbound queue (mpsc)", r"mpsc"),
    ("waker register / wake", r"atomic_waker|AtomicWaker|wake_by_ref|clone_waker|waker"),
    ("json (serde)", r"serde_json"),
    ("metrics", r"obs.*metrics|CounterVec|HistogramVec|GaugeVec"),
    ("allocator", r"^malloc$|^free$|^cfree$|realloc|_int_malloc|_int_free|kmem_cache"),
    ("tokio runtime (scheduler, io driver)", r"tokio.*runtime|scheduled_io|poll_readiness"),
    ("session loop / dispatch (ours)", r"wsprism_gateway"),
    ("libc (unresolved)", r"libc\.so"),
]
COMPILED = [(name, re.compile(rx)) for name, rx in RULES]


def main(path):
    self_t, cat = Counter(), Counter()
    total = 0
    for line in open(path):
        stack, _, n = line.rstrip().rpartition(" ")
        n = int(n)
        total += n
        frames = stack.split(";")
        self_t[frames[-1]] += n
        for name, rx in COMPILED:
            if any(rx.search(f) for f in frames):
                cat[name] += n
                break
        else:
            cat["other"] += n

    def pct(x):
        return f"{100 * x / total:5.1f}%"

    print(f"samples (weighted): {total}\n\n== exclusive categories (sum = 100%) ==")
    for name, n in cat.most_common():
        print(f"{pct(n)}  {name}")
    print("\n== top self (leaf frame) ==")
    for f, n in self_t.most_common(40):
        print(f"{pct(n)}  {f[:150]}")


if __name__ == "__main__":
    main(sys.argv[1])
