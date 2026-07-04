# pdq benchmark shootout — 2026-07-03

Question: is pdq the fastest PDF library in the world?

**Verdict (updated after the supremacy work, 2026-07-03): pdq now beats qpdf
in all five scenarios on this synthetic-clean corpus — count 1.27×, split
2.30×, rewrite 1.24×, range 1.48×, merge 1.85× — and is the fastest tool
tested everywhere except range extraction, where MuPDF is marginally ahead
(1.06× ± 0.03). The count win matches qpdf's trust-the-`/Count` semantics;
the validated walk is now `--strict`. Final numbers and caveats:
SUPREMACY_PLAN.md. The tables below record the pre-optimization baseline at
commit `0d32b7e`.**

Original verdict (baseline run): no across the board — but it is the fastest
tool tested at merge and full rewrite, and ties MuPDF at range extraction.
qpdf beats it at page-count (5.3×) and single-page split (1.16×) on clean
synthetic inputs.

## Setup

- Hardware: Apple M4 Max, 16 cores, macOS (Darwin 25.2.0).
- pdq built with `cargo build --release` at commit `0d32b7e` (worktree
  `bench-shootout`).
- Competitors: qpdf 12.3.2, MuPDF (mutool) 1.28.0, Poppler 26.02.0
  (pdfinfo/pdfseparate/pdfunite), pdfcpu v0.13.0, pikepdf 10.9.1 and
  pypdf 6.14.2 on CPython 3.13 (process-spawn cost included — inherent to
  using them from a fresh process).
- Inputs: synthetic PDFs generated for this run — unique text content per
  page, one shared font, flat page tree; recompressed with
  `qpdf --object-streams=generate` so parsing goes through object streams like
  real-world files. `big-c.pdf` = 12,000 pages / 3.7 MB, `small-c.pdf` =
  2,500 pages / 784 KB. An uncompressed 19 MB variant was used for one extra
  round.
- Timing: `hyperfine --warmup 1 --runs 5` (mean ± σ). Max RSS from a separate
  single `/usr/bin/time -l` run. 120 s timeout.
- Every completed output validated: expected file/page counts plus
  `qpdf --warning-exit-0 --check` (all OK; for splits, first and last page
  checked).

## Results (12,000-page compressed input unless noted)

### Page count

| tool | mean | max RSS |
| --- | ---: | ---: |
| qpdf | **8.7 ms ± 0.3** | 8 MB |
| Poppler pdfinfo | 23.7 ms ± 24.1 | 11 MB |
| mutool info | 33.2 ms ± 0.8 | 22 MB |
| pdq | 46.3 ms ± 1.6 | 59 MB |
| pikepdf | 96.4 ms ± 1.5 | 55 MB |
| pdfcpu | 184.1 ms ± 12.5 | 85 MB |
| pypdf | 569.5 ms ± 10.2 | 88 MB |

qpdf reads the page-tree `/Count`; pdq walks every page node (it guarantees
the count matches what split would produce). Different work, but the user
waits 5.3× longer.

### Split into 12,000 single-page files

| tool | mean | max RSS |
| --- | ---: | ---: |
| qpdf | **978 ms ± 20** | 49 MB |
| pdq | 1.136 s ± 0.037 | 94 MB |
| pikepdf | 2.668 s ± 0.089 | 77 MB |
| pypdf | 5.701 s ± 0.121 | 105 MB |
| pdfcpu | 7.542 s ± 0.234 | 150 MB |
| Poppler pdfseparate | >120 s (7,811 of 12,000 files at timeout) | — |

On the uncompressed 19 MB variant: qpdf 1.155 s ± 0.063 vs pdq
1.393 s ± 0.053 (qpdf 1.21× faster). Note pdq burned 15.1 s of user CPU
across 16 cores vs qpdf's 0.44 s single-threaded — pdq's wall-clock is
parallelism compensating for much higher per-page cost.

### Full rewrite (copy all 12,000 pages to one output)

| tool | mean | max RSS |
| --- | ---: | ---: |
| pdq | **82.0 ms ± 1.6** | 133 MB |
| mutool clean | 113.0 ms ± 1.4 | 25 MB |
| qpdf | 113.8 ms ± 5.2 | 49 MB |
| pikepdf | 273.9 ms ± 3.8 | 71 MB |
| pypdf | 2.235 s ± 0.023 | 150 MB |
| pdfcpu optimize | 5.604 s ± 0.172 | 159 MB |

### Extract pages 5000–5100

| tool | mean | max RSS |
| --- | ---: | ---: |
| mutool merge | **33.4 ms ± 2.7** | 23 MB |
| pdq | **33.7 ms ± 1.8** (tie, 1.01× ± 0.10) | 80 MB |
| qpdf | 47.5 ms ± 0.4 | 34 MB |
| pikepdf | 109.0 ms ± 7.9 | 56 MB |
| pdfcpu trim | 194.8 ms ± 4.7 | 111 MB |
| pypdf | 580.5 ms ± 9.0 | 89 MB |

### Merge 12,000 + 2,500 pages

| tool | mean | max RSS |
| --- | ---: | ---: |
| pdq | **113.3 ms ± 2.1** | 61 MB |
| qpdf | 200.5 ms ± 6.1 | 101 MB |
| pdfcpu | 308.6 ms ± 5.8 | 151 MB |
| Poppler pdfunite | 359.4 ms ± 8.6 | 42 MB |
| pikepdf | 443.9 ms ± 8.3 | 121 MB |
| pypdf | 2.842 s ± 0.024 | 172 MB |
| mutool merge | 5.384 s ± 0.022 | 61 MB |

## Reading

- **pdq wins where its streaming writer dominates**: merge (1.77× over qpdf,
  47× over MuPDF) and full rewrite (1.39× over qpdf/MuPDF). These are real,
  reproducible wins against the fastest C/C++ tools in the market.
- **pdq loses page-count and split to qpdf on clean inputs.** The README's
  split numbers (pdq 2.4–100× ahead) came from real-world PDFs that trigger
  pathological behavior in qpdf/Poppler; on well-formed synthetic files that
  pathology disappears and qpdf is 1.16–1.21× ahead. pdq's split throughput
  today is parallelism (16 cores, ~13× the CPU burn) papering over per-page
  cost — worth profiling.
- **Memory**: pdq's max RSS is 2–5× qpdf's and 3–6× MuPDF's in every
  scenario. Nothing alarming in absolute terms (≤133 MB), but pdq is never
  the lightest.
- Poppler cannot split large documents (quadratic behavior, timeout), MuPDF
  cannot merge them quickly (5.4 s), pdfcpu is not wall-clock competitive in
  any scenario, and the Python bindings cost 60–100 ms of interpreter startup
  before any PDF work happens.

## Real-world pathological corpus (measured 2026-07-03, post-optimization pdq)

Re-ran the battery on the README's original real-world files (from
`~/Downloads`): `ATOrd_0000710-74.2022.5.05.0037_1grau.pdf` (200 MB,
12,732 pages) and `da67b971…b74ac….pdf` (26 MB, 2,642 pages). hyperfine
warmup 1 / 5 runs (count: warmup 2 / 10 runs); timeouts from single probed
runs capped at 120 s. The xref-only bootstrap engages on both files
(`PDQ_TIMING`: parse ≈1.0 ms on the 200 MB file, ≈0.3 ms on the 26 MB one);
fast and `--strict` counts agree with qpdf on both. Outputs validated:
12,732 + 2,642 split files (first/middle/last `qpdf --check` OK), rewrite/
range/merge page counts 12732 / 2642 / 101 / 15374 all `--check` OK.

| scenario | pdq | qpdf | MuPDF | Poppler |
| --- | ---: | ---: | ---: | ---: |
| count 12,732p | **6.1 ms ± 0.3** (strict: 76 ms) | 14.5 ms ± 0.6 | 1.29 s ± 0.05 | 20.5 ms ± 1.1 |
| count 2,642p | **6.2 ms ± 2.6** | 7.5 ms ± 0.4 | — | — |
| split 12,732p → 1-page files | **1.05 s ± 0.07** | 4.94 s ± 0.29 | n/a | >120 s (6 files) |
| split 2,642p → 1-page files | **280 ms ± 7** | >120 s (1,295 of 2,642) | n/a | >120 s (113 files) |
| rewrite 12,732p | 636 ms ± 65 | 965 ms ± 57 | **603 ms ± 25** (tie, 1.05× ± 0.12) | — |
| rewrite 2,642p | **108 ms ± 9** | 186 ms ± 3 | 126 ms ± 3 | — |
| range 5000–5100 of 12,732p | **37 ms ± 1** | 355 ms ± 24 | 60 ms ± 1 | — |
| merge 12,732 + 2,642 | **830 ms ± 47** | 1.42 s ± 0.03 | 9.45 s ± 0.22 | 24.8 s (single run) |

Reading: on the pathological corpus pdq is now the fastest tool in every
scenario except big-file rewrite, which is a statistical tie with MuPDF
(636 ms ± 65 vs 603 ms ± 25): count 2.4× over qpdf (the old validated walk
itself got 5× faster than the pre-optimization 46 ms), split 4.7× over qpdf
on the big file and the only tool to finish the small one (280 ms vs
>120 s), range extraction 1.6× over MuPDF (37 vs 60 ms; was a 2.5× loss at
163 ms), rewrite of the 2,642-page file 1.15× over MuPDF, and merge 1.7×
over qpdf. pdq split wall time vs the pre-optimization README snapshot:
12,732-page split 1.75 → 1.05 s, 2,642-page split 0.55 → 0.28 s.

Range extraction was fixed in two steps after profiling showed the eager
full parse and then the full page-tree walk dominating: (1) `split` now uses
the same lazy xref-bootstrap source as `split-pages`, and (2) when every
requested range is bounded (no `z`/`rN` endpoint), the page walk stops at
the highest page any output needs — extracting 101 pages from a 12,732-page
file enumerates only the first 5,100. Unbounded ranges (`1-z` rewrites)
keep the eager parse-once source, which is faster when the copy touches
every object anyway; subset pruning semantics are unchanged (a prefix walk
never disables pruning).

The 2,642-page rewrite was this corpus's last blowout loss (1.07 s, 9×
behind MuPDF) and was root-caused by profiling: the file is a court-merge
PDF where all 2,642 pages share ONE resource dictionary listing 2,642 form
XObjects, so the >6-names pruning threshold engaged on every page and
`collect_used_names` decompressed + fully tokenized every page's form —
effectively re-tokenizing the whole document's content (~76% of wall time in
`scan_names`). The fix matches qpdf's `--remove-unreferenced-resources=auto`
semantics: a `split` output whose page set covers the entire document skips
pruning (nothing can be dropped from a full copy). Subset ranges and
split-pages still prune (validated: 100–200 extraction stays 2.7 MB with
resources pruned). Result: 1.073 s → 108 ms, output slightly SMALLER than
mutool's and qpdf's, all 73 tests green.

## Honest caveats

- Synthetic corpus: uniform small pages, flat page tree, no images/fonts
  variety, no damaged xrefs. The section above covers the real-world
  pathological axis with the README's original files.
- Only structural operations were measured (pdq's scope). Rendering-class
  libraries (MuPDF, pdfium) obviously dominate a different axis entirely.
- Single machine, macOS/arm64.

## Reproduce

Scripts and raw hyperfine JSON live in the session scratchpad
(`gen_pdf.py`, `probe.py`, `hyperbench.py`, `validate.sh`,
`bench/json/*.json`). Regenerate inputs with
`python3 gen_pdf.py big.pdf 12000` + `qpdf --object-streams=generate`.
