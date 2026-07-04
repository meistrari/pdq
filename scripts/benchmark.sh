#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIG_PDF=${PDQ_BIG_PDF:?set PDQ_BIG_PDF to the 12,732-page benchmark PDF}
SMALL_PDF=${PDQ_SMALL_PDF:?set PDQ_SMALL_PDF to the 2,642-page benchmark PDF}
RUNS=${BENCH_RUNS:-5}
WARMUP=${BENCH_WARMUP:-1}
TIMEOUT_SECONDS=${BENCH_TIMEOUT_SECONDS:-60}
BENCH_DIR=${BENCH_DIR:-"$(mktemp -d /tmp/pdq-bench.XXXXXX)"}

mkdir -p "$BENCH_DIR/json" "$BENCH_DIR/out"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-json "$BENCH_DIR/json/split-big.json" \
  --prepare "rm -rf '$BENCH_DIR/out/split-big' && mkdir -p '$BENCH_DIR/out/split-big/pdq' '$BENCH_DIR/out/split-big/qpdf'" \
  -n pdq "$ROOT/target/release/pdq split-pages --output '$BENCH_DIR/out/split-big/pdq/page-%d.pdf' '$BIG_PDF'" \
  -n qpdf "qpdf --remove-unreferenced-resources=no --split-pages '$BIG_PDF' '$BENCH_DIR/out/split-big/qpdf/page-%d.pdf'"

hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-json "$BENCH_DIR/json/split-small-pdq.json" \
  --prepare "rm -rf '$BENCH_DIR/out/split-small-pdq' && mkdir -p '$BENCH_DIR/out/split-small-pdq'" \
  -n pdq "$ROOT/target/release/pdq split-pages --output '$BENCH_DIR/out/split-small-pdq/page-%d.pdf' '$SMALL_PDF'"

python3 - <<'PY' "$BENCH_DIR/out/split-small-qpdf-timeout" "$SMALL_PDF" "$BENCH_DIR/qpdf-split-small-timeout.txt" "$TIMEOUT_SECONDS"
import pathlib, shutil, subprocess, sys, time

outdir = pathlib.Path(sys.argv[1])
small = sys.argv[2]
log = pathlib.Path(sys.argv[3])
timeout = int(sys.argv[4])
shutil.rmtree(outdir, ignore_errors=True)
outdir.mkdir(parents=True)
cmd = ["qpdf", "--remove-unreferenced-resources=no", "--split-pages", small, str(outdir / "page-%d.pdf")]
start = time.perf_counter()
try:
    subprocess.run(cmd, timeout=timeout, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    status = "completed"
except subprocess.TimeoutExpired:
    status = "timeout"
end = time.perf_counter()
count = len(list(outdir.glob("*.pdf")))
log.write_text(f"status={status}\nelapsed={end-start:.3f}\noutputs={count}\ncmd={' '.join(cmd)}\n")
print(log.read_text(), end="")
PY

hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-json "$BENCH_DIR/json/rewrite-big.json" \
  --prepare "rm -f '$BENCH_DIR/out/rewrite-big-'*.pdf" \
  -n pdq "$ROOT/target/release/pdq split '$BIG_PDF' --out 1-z '$BENCH_DIR/out/rewrite-big-pdq.pdf'" \
  -n qpdf "qpdf --remove-unreferenced-resources=no '$BIG_PDF' '$BENCH_DIR/out/rewrite-big-qpdf.pdf'" \
  -n mutool "mutool clean '$BIG_PDF' '$BENCH_DIR/out/rewrite-big-mutool.pdf'"

hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-json "$BENCH_DIR/json/rewrite-small.json" \
  --prepare "rm -f '$BENCH_DIR/out/rewrite-small-'*.pdf" \
  -n pdq "$ROOT/target/release/pdq split '$SMALL_PDF' --out 1-z '$BENCH_DIR/out/rewrite-small-pdq.pdf'" \
  -n qpdf "qpdf --remove-unreferenced-resources=no '$SMALL_PDF' '$BENCH_DIR/out/rewrite-small-qpdf.pdf'" \
  -n mutool "mutool clean '$SMALL_PDF' '$BENCH_DIR/out/rewrite-small-mutool.pdf'"

hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-json "$BENCH_DIR/json/merge.json" \
  --prepare "rm -f '$BENCH_DIR/out/merge-'*.pdf" \
  -n pdq "$ROOT/target/release/pdq merge --output '$BENCH_DIR/out/merge-pdq.pdf' '$BIG_PDF' '$SMALL_PDF'" \
  -n qpdf "qpdf --empty --remove-unreferenced-resources=no --pages '$BIG_PDF' '$SMALL_PDF' -- '$BENCH_DIR/out/merge-qpdf.pdf'" \
  -n mutool "mutool merge -o '$BENCH_DIR/out/merge-mutool.pdf' '$BIG_PDF' '$SMALL_PDF'"

cat <<EOF
Benchmark data written to:
$BENCH_DIR
EOF
