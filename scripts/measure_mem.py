#!/usr/bin/env python3
"""Measure peak RSS for benchmark scenarios.

Reads a JSON list of scenarios on stdin:
  [{"scenario": "...", "tool": "...", "prepare": "<shell>", "cmd": ["argv", ...]}, ...]
Runs each command under /usr/bin/time (-l on macOS, -v on Linux), takes the
max resident set size across runs, and writes a JSON report.
"""
import argparse
import json
import re
import subprocess
import sys

MACOS_RE = re.compile(r"(\d+)\s+maximum resident set size")
LINUX_RE = re.compile(r"Maximum resident set size \(kbytes\): (\d+)")


def peak_rss_bytes(cmd):
    if sys.platform == "darwin":
        time_cmd, pattern, scale = ["/usr/bin/time", "-l"], MACOS_RE, 1
    else:
        time_cmd, pattern, scale = ["/usr/bin/time", "-v"], LINUX_RE, 1024
    proc = subprocess.run(time_cmd + cmd, stdout=subprocess.DEVNULL,
                          stderr=subprocess.PIPE, text=True)
    match = pattern.search(proc.stderr)
    if proc.returncode != 0 or not match:
        raise RuntimeError(f"command failed (exit {proc.returncode}): "
                           f"{' '.join(cmd)}\n{proc.stderr.strip()}")
    return int(match.group(1)) * scale


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    results = []
    for spec in json.load(sys.stdin):
        runs = []
        for _ in range(args.runs):
            if spec.get("prepare"):
                subprocess.run(spec["prepare"], shell=True, check=True)
            runs.append(peak_rss_bytes(spec["cmd"]))
        results.append({"scenario": spec["scenario"], "tool": spec["tool"],
                        "peak_rss_bytes": max(runs), "runs": runs})
        print(f"{spec['scenario']:<16} {spec['tool']:<8} "
              f"{max(runs) / 1e6:>10.1f} MB peak RSS")

    with open(args.output, "w") as f:
        json.dump(results, f, indent=2)
    print(f"memory report written to {args.output}")


if __name__ == "__main__":
    main()
