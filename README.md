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
is `hyperfine --warmup 1 --runs 5` mean plus standard deviation. RSS is maximum
resident set size from a separate single `/usr/bin/time -l` run, so use it as a
memory-order signal rather than a statistically averaged value.

Completed outputs were validated by page count and `qpdf --warning-exit-0
--check`. Split scenarios validated the first and last output file. qpdf used
`--remove-unreferenced-resources=no` for copy-like paths where applicable.

| Scenario | pdq | qpdf | Market runner-up | Notes |
| --- | ---: | ---: | ---: | --- |
| 12,732-page split into single-page PDFs | 1.75s +/- 0.05s / 241 MB | 4.27s +/- 0.07s / 217 MB | Poppler >60s timeout | Poppler produced 3 partial outputs before timeout. |
| 2,642-page split into single-page PDFs | 0.55s +/- 0.03s / 146 MB | >60s timeout | Poppler >60s timeout | qpdf produced 727 partial outputs; Poppler produced 59. |
| 12,732-page full rewrite | 0.56s +/- 0.04s / 1.03 GB | 0.91s +/- 0.03s / 221 MB | MuPDF 0.54s +/- 0.02s / 71 MB | MuPDF narrowly wins wall time; pdq is close but uses more memory. |
| 2,642-page full rewrite | 1.07s +/- 0.01s / 261 MB | 0.17s +/- 0.00s / 57 MB | MuPDF 0.11s +/- 0.00s / 24 MB | Small rewrite remains a qpdf/MuPDF win. |
| 15,374-page merge | 0.71s +/- 0.01s / 230 MB | 1.36s +/- 0.04s / 495 MB | MuPDF 8.64s +/- 0.22s / 390 MB | pdq uses streaming output for whole-document merge. |

To reproduce the timing matrix:

```sh
PDQ_BIG_PDF=/path/to/12732-pages.pdf \
PDQ_SMALL_PDF=/path/to/2642-pages.pdf \
scripts/benchmark.sh
```
