# pdq

Rust-native PDF split and merge MVP.

Runtime constraints:

- does not invoke the `qpdf` binary;
- does not use a subprocess wrapper;
- does not link libqpdf through FFI.

The first implementation uses `lopdf` as a pure-Rust PDF object model and
writer. It focuses on valid split/merge outputs for ordinary, unencrypted PDFs.
Advanced qpdf behavior such as repair, encryption, linearization, forms,
outlines, and full compatibility with unusual historical PDFs is intentionally
out of scope for the current MVP.

## Commands

```sh
cargo test
cargo run --bin pdq -- split input.pdf --out 1-3 out-1.pdf --out 4-z out-2.pdf
cargo run --bin pdq -- split-pages --output 'page-%d.pdf' input.pdf
cargo run --bin pdq -- merge --output merged.pdf a.pdf b.pdf
```

Tests may use `qpdf` as a development validator when it is available on `PATH`.
The runtime implementation must remain qpdf-free.

## Benchmark Snapshot

Measured on 2026-07-03 with local PDFs identified only by page count. Wall time
is `hyperfine --warmup 1 --runs 5` mean plus standard deviation, except the
2,642-page split-pages row, which uses a focused `--warmup 2 --runs 7` rerun
because the full matrix had a filesystem outlier. RSS is maximum resident set
size from a separate single `/usr/bin/time -l` run, so use it as a memory-order
signal rather than a statistically averaged value.

Completed outputs were validated by page count and `qpdf --warning-exit-0
--check`. Split scenarios validated the first and last output file. qpdf used
`--remove-unreferenced-resources=no` for copy-like paths where applicable.

| Scenario | pdq | qpdf | Market runner-up | Notes |
| --- | ---: | ---: | ---: | --- |
| 12,732-page split into single-page PDFs | 2.19s +/- 0.28s / 234 MB | 4.65s +/- 0.32s / 207 MB | not measured | pdq still wins, but this run was noisier than the previous snapshot. |
| 2,642-page split into single-page PDFs | 0.58s +/- 0.05s / 142 MB | >60s timeout | not measured | qpdf produced 712 partial outputs before timeout in the full matrix. |
| 12,732-page split into two ranged PDFs | 0.49s +/- 0.01s | not measured | not measured | Control case for the `split()` LazyPdf migration. |
| 12,732-page full rewrite | 0.67s +/- 0.01s / 220 MB | 1.00s +/- 0.08s / 211 MB | MuPDF 0.55s +/- 0.02s / 68 MB | MuPDF still wins wall time; pdq now avoids the old 1 GB RSS path. |
| 2,642-page full rewrite | 0.12s +/- 0.00s / 40 MB | 0.17s +/- 0.00s / 56 MB | MuPDF 0.12s +/- 0.00s / 23 MB | `split 1-z` uses the streaming whole-document fast path. |
| 15,374-page merge | 0.72s +/- 0.02s / 219 MB | 1.38s +/- 0.02s / 472 MB | MuPDF 8.67s +/- 0.15s / 371 MB | pdq uses streaming output for whole-document merge. |

To reproduce the timing matrix:

```sh
PDQ_BIG_PDF=/path/to/12732-pages.pdf \
PDQ_SMALL_PDF=/path/to/2642-pages.pdf \
scripts/benchmark.sh
```
