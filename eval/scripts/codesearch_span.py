"""Score flupkede/codesearch on `question<TAB>path<TAB>anchor` probes: file rank and span rank.

The repository must already be indexed (`codesearch index add` inside it).
Usage: python -I codesearch_span.py <codesearch-exe> <indexed-root> <probes.tsv>...
"""
import json
import subprocess
import sys
import time
from pathlib import Path

exe, root = sys.argv[1], Path(sys.argv[2]).resolve()
limit = 20
prefix = str(root).replace("\\", "/").lower() + "/"


def relative(path):
    path = path.replace("\\", "/")
    return path[len(prefix):] if path.lower().startswith(prefix) else path.lstrip("./")


def run(probe):
    question, expected, anchor = probe
    started = time.perf_counter()
    out = subprocess.run([exe, "search", question, "-m", str(limit * 3), "--json"], cwd=root,
                         capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    spent = time.perf_counter() - started
    try:
        results = json.loads(out[out.index("{"):])["results"]
    except (ValueError, KeyError):
        return None, None, spent
    seen = []
    for r in results:
        p = relative(r["path"])
        if p not in seen:
            seen.append(p)
    file_rank = next((i for i, p in enumerate(seen[:limit]) if p == expected), None)
    lines = (root / expected).read_text(encoding="utf-8", errors="replace").splitlines()
    span_rank = None
    for i, r in enumerate(results[:limit]):
        # Lenient about whether lines are counted from zero or one.
        body = lines[max(r["start_line"] - 1, 0):r["end_line"] + 1]
        if relative(r["path"]) == expected and any(anchor in l for l in body):
            span_rank = i
            break
    return file_rank, span_rank, spent


def report(label, ranks):
    n = len(ranks) or 1
    rec = lambda k: sum(1 for r in ranks if r is not None and r < k) / n
    mrr = sum(0 if r is None else 1 / (r + 1) for r in ranks) / n
    # "found" is what an agent sees in eight answers, as omega's --hits 8 reports it.
    print(f"  {label}  R@1 {rec(1):.3f}  R@3 {rec(3):.3f}  R@5 {rec(5):.3f}  found-in-8 {rec(8):.3f}  R@20 {rec(20):.3f}  MRR {mrr:.3f}")


for f in sys.argv[3:]:
    probes = [l.split("\t") for l in Path(f).read_text(encoding="utf-8").splitlines() if l.strip()]
    probes = [(p[0], p[1].strip(), p[2].strip()) for p in probes if len(p) >= 3]
    got = [run(p) for p in probes]
    print(f"\n=== codesearch {f} ({len(probes)} probes, {sum(g[2] for g in got) / len(got) * 1000:.0f} ms/query incl. process start)")
    report("file", [g[0] for g in got])
    report("span", [g[1] for g in got])
