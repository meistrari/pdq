#!/usr/bin/env python3
"""Sustained-throughput benchmark for a fixed time window.

Runs `--concurrency` workers, each looping the command until `--duration`
seconds elapse, and reports completed ops/min, per-op latency percentiles,
and failures (an OOM-killed child exits 137 and counts as a failure).

The command template may use {out} (unique scratch output path, deleted
after each op) and {worker} (worker index). Example:

  throughput_bench.py --duration 60 --concurrency 4 --label pdq-rewrite \
    -- pdq split big.pdf --out 1-z {out}
"""
import argparse
import json
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time


def worker(index, args, deadline, results, lock):
    while time.monotonic() < deadline:
        with tempfile.NamedTemporaryFile(
            dir=args.scratch, suffix=".pdf", delete=False
        ) as handle:
            out = handle.name
        cmd = [
            part.replace("{out}", out).replace("{worker}", str(index))
            for part in args.command
        ]
        start = time.monotonic()
        proc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        elapsed = time.monotonic() - start
        subprocess.run(["rm", "-f", out])
        with lock:
            if proc.returncode == 0:
                results["latencies"].append(elapsed)
            else:
                results["failures"].append(
                    {"exit": proc.returncode, "stderr": proc.stderr[-200:].decode(errors="replace")}
                )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--label", default="run")
    parser.add_argument("--scratch", default=tempfile.gettempdir())
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()

    results = {"latencies": [], "failures": []}
    lock = threading.Lock()
    deadline = time.monotonic() + args.duration
    threads = [
        threading.Thread(target=worker, args=(i, args, deadline, results, lock))
        for i in range(args.concurrency)
    ]
    start = time.monotonic()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    window = time.monotonic() - start

    latencies = sorted(results["latencies"])
    ops = len(latencies)
    report = {
        "label": args.label,
        "concurrency": args.concurrency,
        "window_s": round(window, 1),
        "ops": ops,
        "ops_per_min": round(ops / window * 60, 1),
        "failures": len(results["failures"]),
        "p50_s": round(statistics.median(latencies), 2) if ops else None,
        "p95_s": round(latencies[max(0, int(ops * 0.95) - 1)], 2) if ops else None,
        "cmd": shlex.join(args.command),
    }
    if results["failures"]:
        report["failure_sample"] = results["failures"][0]
    print(json.dumps(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
