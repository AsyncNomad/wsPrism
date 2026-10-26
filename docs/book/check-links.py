#!/usr/bin/env python3
"""Check the three books: same pages in every language, relative links that
resolve, and #anchors that match a heading (mdBook's id rules).

    python3 docs/book/check-links.py
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
LANGS = ["en", "ko", "zh"]


def heading_id(text):
    text = re.sub(r"`|\*", "", text).strip().lower()
    return "".join(c if (c.isalnum() or c in "_-") else ("-" if c == " " else "") for c in text)


def anchors(path):
    ids, seen, code = set(), {}, False
    for line in open(path, encoding="utf-8"):
        if line.startswith("```"):
            code = not code
            continue
        m = None if code else re.match(r"^#+\s+(.*)", line)
        if m:
            i = heading_id(m.group(1))
            n = seen.get(i, 0)
            seen[i] = n + 1
            ids.add(i if n == 0 else f"{i}-{n}")
    return ids


def pages(src):
    return sorted(
        os.path.relpath(os.path.join(d, f), src)
        for d, _, fs in os.walk(src)
        for f in fs
        if f.endswith(".md")
    )


problems = []
base = pages(os.path.join(ROOT, "en", "src"))
for lang in LANGS:
    src = os.path.join(ROOT, lang, "src")
    files = pages(src)
    if files != base:
        problems.append(f"{lang}: pages differ from en: {sorted(set(files) ^ set(base))}")
    for f in files:
        p = os.path.join(src, f)
        text = re.sub(r"```.*?```", "", open(p, encoding="utf-8").read(), flags=re.S)
        for m in re.finditer(r"\]\(([^)\s]+)\)", text):
            target = m.group(1)
            if re.match(r"https?:|mailto:", target) or "/api/" in target:
                continue
            path, _, frag = target.partition("#")
            dest = os.path.normpath(os.path.join(os.path.dirname(p), path)) if path else p
            if not os.path.exists(dest):
                problems.append(f"{lang}/{f}: missing {target}")
            elif frag and dest.endswith(".md") and frag not in anchors(dest):
                problems.append(f"{lang}/{f}: no heading for {target}")

print("\n".join(problems) or "books OK")
sys.exit(1 if problems else 0)
