#!/usr/bin/env python3
"""Compare `katago-ws bench` reports: throughput, and what it cost in quality.

    scripts/bench_compare.py BASE.json RUN.json [RUN.json ...]

Throughput columns come straight from each report's summary. The quality
columns compare every move's fingerprint against BASE (same file, round 0):

  dWR   mean |win-rate difference| after the played move (0..1)
  dSL   mean |score-lead difference| in points
  best  share of positions where the engine's preferred move agrees

MCTS is not deterministic (nnRandomize, thread timing), so two runs of the
*same* config already differ. Benchmark BASE twice and compare the pair to get
the noise floor; a tuning change is quality-neutral when it stays near it.
"""

import json
import sys


def load(path):
    with open(path) as fh:
        return json.load(fh)


def fingerprints(report):
    return {g["file"]: g["fingerprint"] for g in report["games"] if g["round"] == 0}


def quality(base, run):
    base_fp, run_fp = fingerprints(base), fingerprints(run)
    dwr = dsl = agree = n = 0
    for name, moves in run_fp.items():
        ref = base_fp.get(name)
        if ref is None or len(ref) != len(moves):
            continue
        for lhs, rhs in zip(ref, moves):
            dwr += abs(lhs["wr"] - rhs["wr"])
            dsl += abs(lhs["sl"] - rhs["sl"])
            agree += lhs["best"] == rhs["best"]
            n += 1
    if n == 0:
        return None
    return dwr / n, dsl / n, agree / n


def main(paths):
    if len(paths) < 2:
        sys.exit(__doc__)
    base = load(paths[0])
    base_pps = base["summary"]["positions_per_sec"]
    header = f"{'label':<34} {'conc':>4} {'games/s':>8} {'pos/s':>8} {'x base':>7} {'p50 s':>7} {'p95 s':>7} {'gpu%':>5} {'dWR':>6} {'dSL':>6} {'best':>6}"
    print(header)
    print("-" * len(header))
    for path in paths:
        rep = load(path)
        s = rep["summary"]
        q = quality(base, rep) if path != paths[0] else None
        gpu = f"{s['gpu_busy_mean']:.0f}" if s.get("gpu_busy_mean") is not None else "-"
        qcols = f"{q[0]:6.3f} {q[1]:6.2f} {q[2]:6.1%}" if q else f"{'-':>6} {'-':>6} {'-':>6}"
        label = rep.get("label") or path
        print(
            f"{label[:34]:<34} {rep['concurrency']:>4} {s['games_per_sec']:8.3f} {s['positions_per_sec']:8.1f} "
            f"{s['positions_per_sec'] / base_pps:7.2f} {s['latency_p50_secs']:7.1f} {s['latency_p95_secs']:7.1f} {gpu:>5} {qcols}"
        )


if __name__ == "__main__":
    main(sys.argv[1:])
