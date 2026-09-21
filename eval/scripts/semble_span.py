"""Score semble on `question<TAB>path<TAB>anchor` probes: file rank and span rank."""
import json, subprocess, sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

root = Path(sys.argv[1]); limit = 20

def run(probe):
    question, expected, anchor = probe
    out = subprocess.run(["semble", "search", question, str(root), "--top-k", str(limit * 3), "--max-snippet-lines", "0"],
                         capture_output=True, text=True, encoding="utf-8").stdout
    try:
        results = json.loads(out)["results"]
    except Exception:
        return None, None
    def rel(p):
        p = p.replace("\\", "/")
        r = str(root).replace("\\", "/")
        return p[len(r) + 1:] if p.startswith(r) else p
    seen = []
    for r in results:
        p = rel(r["file_path"])
        if p not in seen:
            seen.append(p)
    file_rank = next((i for i, p in enumerate(seen[:limit]) if expected in p), None)
    lines = (root / expected).read_text(encoding="utf-8", errors="replace").splitlines()
    span_rank = None
    for i, r in enumerate(results[:limit]):
        if rel(r["file_path"]) == expected and any(anchor in l for l in lines[r["start_line"] - 1:r["end_line"]]):
            span_rank = i
            break
    return file_rank, span_rank

def report(label, ranks):
    n = len(ranks) or 1
    rec = lambda k: sum(1 for r in ranks if r is not None and r < k) / n
    mrr = sum(0 if r is None else 1 / (r + 1) for r in ranks) / n
    print(f"  {label}  R@1 {rec(1):.3f}  R@3 {rec(3):.3f}  R@5 {rec(5):.3f}  R@10 {rec(10):.3f}  R@20 {rec(20):.3f}  MRR {mrr:.3f}")

for f in sys.argv[2:]:
    probes = [l.split("\t") for l in Path(f).read_text(encoding="utf-8").splitlines() if l.strip()]
    probes = [(p[0], p[1].strip(), p[2].strip()) for p in probes if len(p) >= 3]
    with ThreadPoolExecutor(4) as pool:
        got = list(pool.map(run, probes))
    print(f"\n=== semble {f} ({len(probes)} probes)")
    report("file", [g[0] for g in got]); report("span", [g[1] for g in got])
