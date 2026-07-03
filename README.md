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

Measured on 2026-07-03 with local PDFs identified only by page count. Each row
is a single run using `/usr/bin/time -l`; RSS is maximum resident set size.
Completed outputs were validated by page count and `qpdf --warning-exit-0
--check`. Split scenarios validated the first and last output file.

For rewrite and merge scenarios, qpdf used `--remove-unreferenced-resources=no`
to measure the comparable fast copy path. The market runner-up is the fastest
comparable non-qpdf CLI measured locally: Poppler for split workloads, MuPDF for
rewrite and merge workloads.

| Scenario | pdq | qpdf | Market runner-up | Notes |
| --- | ---: | ---: | ---: | --- |
| 12,732-page split into single-page PDFs | 2.04s / 245 MB | 2.96s / 1.45 GB | Poppler >60s timeout | Poppler produced 3 partial outputs before timeout. |
| 2,642-page split into single-page PDFs | 0.64s / 146 MB | 3.63s / 907 MB | Poppler >60s timeout | Poppler produced 58 partial outputs before timeout. |
| 12,732-page full rewrite | 0.99s / 1.03 GB | 1.53s / 417 MB | MuPDF 6.30s / 80 MB | pdq wins wall time; qpdf and MuPDF use less memory. |
| 2,642-page full rewrite | 1.16s / 254 MB | 0.21s / 98 MB | MuPDF 0.36s / 26 MB | qpdf wins this small rewrite path. |
| 15,374-page merge | 2.02s / 713 MB | 1.30s / 501 MB | MuPDF 9.06s / 384 MB | qpdf still wins merge wall time and RSS. |
