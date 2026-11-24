#!/usr/bin/env python3
"""Per-query latency for `opencrabs eval retrieval`, measured by slope.

WHY A SLOPE. One process pays a fixed cost — startup, embedder model load, and
(on the fixture leg) re-embedding the whole corpus. Reporting that process's wall
clock as "vector latency" measures the fixed cost, not the per-query cost. So the
same corpus is scored at several case counts: the fixed cost is constant and
cancels, leaving the slope of wall clock against case count as the per-query
cost.

WHY THE LIVE LEG. Its vector arm reads the profile's existing store, so there is
no corpus embedding and the only fixed cost is startup plus model load. On the
fixture leg the vector arm re-embeds the corpus inside every process and the
slope is unusable (it comes out negative: fixed-cost variance exceeds the
per-query signal).

Usage (from the repo root, with a profile that has vectors enabled):

    python3 src/eval/fixtures/measure_retrieval_latency.py --profile ops

The figures in src/eval/results/retrieval-v1.md come from exactly this script.
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time

DEFAULT_FIXTURE = "src/eval/fixtures/retrieval_v1_recall.json"
SIZES = [1, 24, 72]
REPS = 5
MODES = ["bm25", "vec"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="opencrabs")
    ap.add_argument("--profile", default="ops",
                    help="the default profile ships vector_enabled=false")
    ap.add_argument("--fixture", default=DEFAULT_FIXTURE)
    ap.add_argument("--reps", type=int, default=REPS)
    ap.add_argument("--out", default=None, help="write raw JSON here")
    args = ap.parse_args()

    cases = json.load(open(args.fixture))["cases"]
    tmp = tempfile.mkdtemp(prefix="retrieval-latency-")
    subsets = {}
    for n in SIZES:
        if n > len(cases):
            continue
        p = os.path.join(tmp, f"sub{n}.json")
        with open(p, "w") as f:
            json.dump({"name": f"lat-{n}", "cases": cases[:n]}, f)
        subsets[n] = p

    results = []
    for mode in MODES:
        for n, path in subsets.items():
            for rep in range(args.reps):
                cmd = [args.binary, "-p", args.profile, "eval", "retrieval",
                       "--live", "--dataset", path, "--modes", mode, "--json"]
                t0 = time.perf_counter()
                p = subprocess.run(cmd, capture_output=True, text=True)
                dt = time.perf_counter() - t0
                row = {"mode": mode, "n": n, "rep": rep,
                       "secs": round(dt, 4), "rc": p.returncode}
                if p.returncode != 0:
                    row["err"] = (p.stderr or p.stdout)[-300:]
                else:
                    try:
                        row["fingerprint"] = json.loads(p.stdout).get("fingerprint")
                    except Exception as e:
                        row["parse_err"] = str(e)
                results.append(row)
                print(json.dumps(row), flush=True)

    print("\n=== live medians ===")
    summary = {}
    for mode in MODES:
        pts = []
        for n in sorted(subsets):
            vals = [r["secs"] for r in results
                    if r["mode"] == mode and r["n"] == n and r["rc"] == 0]
            if vals:
                pts.append((n, statistics.median(vals), min(vals), max(vals), len(vals)))
        if not pts:
            continue
        print(f"{mode:5s} " + "  ".join(
            f"n={n}: med={m:.3f} [{lo:.3f},{hi:.3f}] k={c}" for n, m, lo, hi, c in pts))
        if len(pts) >= 2:
            xs = [p[0] for p in pts]
            ys = [p[1] for p in pts]
            mx, my = statistics.mean(xs), statistics.mean(ys)
            den = sum((x - mx) ** 2 for x in xs)
            slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den
            icept = my - slope * mx
            two_point = (pts[-1][1] - pts[0][1]) / (pts[-1][0] - pts[0][0])
            summary[mode] = {"slope_ms": round(slope * 1000, 2),
                             "two_point_ms": round(two_point * 1000, 2),
                             "fixed_s": round(icept, 2)}
            print(f"      -> slope={slope * 1000:.1f} ms/query "
                  f"(two-point {two_point * 1000:.1f} ms) fixed={icept:.2f}s")

    print("\n" + json.dumps(summary, indent=1))
    if args.out:
        with open(args.out, "w") as f:
            json.dump({"raw": results, "summary": summary}, f, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
