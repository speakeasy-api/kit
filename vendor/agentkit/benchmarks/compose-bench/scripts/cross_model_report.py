#!/usr/bin/env python3
"""Merge per-model compose-bench runs.jsonl files into one cross-model report.

Usage: cross_model_report.py <sweep-dir>
where <sweep-dir> contains one subdirectory per model, each with a runs.jsonl
(as produced by `compose-bench --out <sweep-dir>/<model>`).
"""

import json
import sys
from collections import defaultdict
from pathlib import Path


def load_runs(sweep_dir: Path):
    runs = []
    for runs_file in sorted(sweep_dir.glob("*/runs.jsonl")):
        with runs_file.open() as fh:
            for line in fh:
                line = line.strip()
                if line:
                    runs.append(json.loads(line))
    return runs


def mean(values):
    values = list(values)
    return sum(values) / len(values) if values else 0.0


def total_tokens(run):
    return run["input_tokens"] + run["cached_input_tokens"] + run["output_tokens"]


def fmt_cost(runs):
    reported = [r["cost_usd"] for r in runs if r["cost_reported"]]
    return f"{sum(reported):.3f}" if reported else "—"


def main() -> int:
    sweep_dir = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    runs = load_runs(sweep_dir)
    if not runs:
        print(f"no runs found under {sweep_dir}", file=sys.stderr)
        return 1

    by_model = defaultdict(list)
    for run in runs:
        by_model[run["model"]].append(run)

    print("# compose-bench cross-model report\n")

    # Headline: one row per model, compose-arm adoption + compose-vs-granular deltas.
    print("## Per-model summary (compose arm vs granular arm, means across scenarios)\n")
    print("| model | scenarios | compose adoption | Δ wall | Δ model reqs | Δ tokens | Δ cost | acc granular | acc compose |")
    print("|---|---|---|---|---|---|---|---|---|")
    for model in sorted(by_model):
        mruns = by_model[model]
        granular = {r["scenario"]: r for r in mruns if r["arm"] == "granular"}
        compose = {r["scenario"]: r for r in mruns if r["arm"] == "compose"}
        # Pairs where either run was cut short by infrastructure (timeout,
        # provider error, request cap) would poison the deltas; keep them out
        # of the summary — they remain visible in the all-cells grid.
        paired = sorted(
            s
            for s in set(granular) & set(compose)
            if not granular[s].get("failure") and not compose[s].get("failure")
        )
        if not paired:
            continue
        adoption = sum(1 for s in paired if compose[s]["compose_calls"] > 0)

        def delta(metric):
            ratios = []
            for s in paired:
                old, new = metric(granular[s]), metric(compose[s])
                if old:
                    ratios.append((new - old) / old)
            return f"{mean(ratios) * 100:+.0f}%" if ratios else "—"

        cost_pairs = [
            s
            for s in paired
            if granular[s]["cost_reported"] and compose[s]["cost_reported"]
        ]
        cost_delta = (
            f"{mean((compose[s]['cost_usd'] - granular[s]['cost_usd']) / granular[s]['cost_usd'] for s in cost_pairs) * 100:+.0f}%"
            if cost_pairs
            else "—"
        )
        print(
            f"| {model} | {len(paired)} | {adoption}/{len(paired)} "
            f"| {delta(lambda r: r['wall_ms'])} "
            f"| {delta(lambda r: r['model_requests'])} "
            f"| {delta(total_tokens)} "
            f"| {cost_delta} "
            f"| {mean(granular[s]['accuracy'] for s in paired):.2f} "
            f"| {mean(compose[s]['accuracy'] for s in paired):.2f} |"
        )

    # Full grid for drill-down.
    print("\n## All cells\n")
    print("| model | scenario | arm | wall s | reqs | tool calls (compose) | tokens | cost $ | accuracy | failure |")
    print("|---|---|---|---|---|---|---|---|---|---|")
    for model in sorted(by_model):
        for run in sorted(by_model[model], key=lambda r: (r["scenario"], r["arm"])):
            cost = f"{run['cost_usd']:.4f}" if run["cost_reported"] else "—"
            failure = (run.get("failure") or "").replace("|", "/")[:60]
            print(
                f"| {model} | {run['scenario']} | {run['arm']} "
                f"| {run['wall_ms'] / 1000:.1f} | {run['model_requests']} "
                f"| {run['tool_calls']} ({run['compose_calls']}) "
                f"| {total_tokens(run)} | {cost} | {run['accuracy']:.2f} | {failure} |"
            )

    reported = [r["cost_usd"] for r in runs if r["cost_reported"]]
    print(f"\nTotal runs: {len(runs)}; total reported cost: ${sum(reported):.2f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
