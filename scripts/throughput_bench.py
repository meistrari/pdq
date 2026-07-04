#!/usr/bin/env python3
"""Sustained-throughput benchmark for a fixed time window.

Runs `--concurrency` workers, each looping the command until `--duration`
seconds elapse, and reports completed ops/min, per-op latency percentiles,
and failures (an OOM-killed child exits 137 and counts as a failure).

The command template may use {out} (unique scratch output path, deleted
after each op) and {worker} (worker index). Example:

  throughput_bench.py --duration 60 --concurrency 4 --label pdq-rewrite \
    -- pdq split big.pdf --out 1-z {out}

Mixed traffic: pass --mix mix.json instead of a command, where mix.json is
a list of {"op": str, "weight": number, "cmd": [str, ...]} entries. Each
worker draws ops by weight (seeded per worker, so runs are reproducible)
and the report breaks ops/min and latency out per op.
"""
import argparse
import json
import math
import random
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time


def run_one(entry, index, scratch, results, lock):
    with tempfile.NamedTemporaryFile(dir=scratch, suffix=".pdf", delete=False) as handle:
        out = handle.name
    cmd = [
        part.replace("{out}", out).replace("{worker}", str(index))
        for part in entry["cmd"]
    ]
    start = time.monotonic()
    proc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    elapsed = time.monotonic() - start
    subprocess.run(["rm", "-f", out])
    with lock:
        if proc.returncode == 0:
            results["latencies"].setdefault(entry["op"], []).append(elapsed)
        else:
            results["failures"].append(
                {
                    "op": entry["op"],
                    "exit": proc.returncode,
                    "stderr": proc.stderr[-200:].decode(errors="replace"),
                }
            )


def worker(index, mix, args, deadline, results, lock):
    rng = random.Random(index)
    weights = [entry["weight"] for entry in mix]
    while time.monotonic() < deadline:
        entry = rng.choices(mix, weights=weights)[0]
        run_one(entry, index, args.scratch, results, lock)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--label", default="run")
    parser.add_argument("--scratch", default=tempfile.gettempdir())
    parser.add_argument("--mix", help="JSON file of weighted {op, weight, cmd} entries")
    parser.add_argument("command", nargs="*")
    args = parser.parse_args()

    if args.mix:
        mix = json.load(open(args.mix))
    elif args.command:
        mix = [{"op": args.label, "weight": 1, "cmd": args.command}]
    else:
        parser.error("pass a command or --mix")

    results = {"latencies": {}, "failures": []}
    lock = threading.Lock()
    deadline = time.monotonic() + args.duration
    threads = [
        threading.Thread(target=worker, args=(i, mix, args, deadline, results, lock))
        for i in range(args.concurrency)
    ]
    start = time.monotonic()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    window = time.monotonic() - start

    def stats(latencies):
        latencies = sorted(latencies)
        n = len(latencies)
        return {
            "ops": n,
            "ops_per_min": round(n / window * 60, 1),
            "p50_s": round(statistics.median(latencies), 2) if n else None,
            "p95_s": round(latencies[max(0, math.ceil(n * 0.95) - 1)], 2) if n else None,
        }

    all_latencies = [l for per_op in results["latencies"].values() for l in per_op]
    report = {
        "label": args.label,
        "concurrency": args.concurrency,
        "window_s": round(window, 1),
        **stats(all_latencies),
        "failures": len(results["failures"]),
        "per_op": {op: stats(l) for op, l in sorted(results["latencies"].items())},
    }
    if results["failures"]:
        fails_by_op = {}
        for failure in results["failures"]:
            fails_by_op[failure["op"]] = fails_by_op.get(failure["op"], 0) + 1
        report["failures_by_op"] = fails_by_op
        report["failure_sample"] = results["failures"][0]
    print(json.dumps(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
