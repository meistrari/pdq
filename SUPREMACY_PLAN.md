# pdq supremacy investigation — 2026-07-03

Goal: understand why pdq loses page-count (5×) and split (1.16×) to qpdf on
clean inputs (see BENCH_REPORT.md), and map the route to being fastest across
the board. Method: macOS `sample` profiles of `page-count` and `split-pages`,
phase timing (env `PDQ_TIMING=1`), lopdf 0.43 source review, and one
implemented prototype.

## Root causes (profiled, not guessed)

### 1. Parse-and-drop: the lazy paths full-parse the document anyway

`LazyPdf::parse` calls `Document::load_mem_with_options` with a filter that
discards every object. lopdf still nom-parses **every** normal object (in
parallel via its `rayon` feature) and **inflates + parses every object
stream** (`load_objects_raw` in lopdf's reader.rs), then throws the results
away. The lazy walk afterwards re-inflates and re-parses the same containers
through `LazyPdf`'s cache. Costs measured:

- Thread-pool storm: `pdq page-count` on 12k pages spends ~290 ms of
  user+sys CPU inside a 42 ms wall — the profile's top entries are
  `swtch_pri` (1291 samples) and `__psynch_cvwait` (1268), i.e. rayon workers
  contending, not parsing.
- RSS: 292 MB at 100k pages vs qpdf's 42 MB — that's the discarded objects
  map plus per-thread allocator arenas.
- For `split-pages` of the raw 19 MB file, the discarded pre-parse is roughly
  6 of the 15 CPU-seconds burned.

### 2. The page-tree walk was clone-heavy (partially fixed here)

`walk_pages` cloned every node dict out of the object-stream cache
(`get_owned` → `Cow::into_owned`), taking a mutex and doing an O(cache)
LRU shuffle per node. Phase timing on 100k pages: parse 28 ms, walk 280 ms —
the walk, not the parse, dominates validated counting.

**Implemented in this worktree**: `classify_page_node` borrows nodes from the
current container's `Arc` (consecutive pages share containers) instead of
cloning. Walk: 32→23 ms @12k, 280→207 ms @100k; split-pages raw wall
1.39→1.22 s. All 24 tests pass. Remaining walk cost is `visited` BTreeSet +
xref BTreeMap lookups (~2 µs/node) — see fixes below.

### 3. Split re-parses shared objects once per output

The split profile is dominated by nom parser frames (~40% of CPU). For
non-compressed inputs every `get_object_value` re-parses from the mmap — the
shared font dict is parsed 12,000 times, once per output. There is a cache
for object-stream containers but none for normal objects. On top of that,
each single-page output builds a fresh lopdf `Document` and serializes via
`Document::save` (allocation-heavy writer), plus the unavoidable 12k×
open/write/close syscall floor (~2 s of the profile).

### 4. Resource pruning was NOT the bottleneck — on these files

`should_prune_resources` only engages above 6 font/xobject names, so the
synthetic corpus never ran the `Content::decode_strict` scan. On real-world
files with rich resource dicts the full content tokenization per page will
bite; a cheap name-token scanner or a resources-identity cache would cap it.

### 5. Semantics gap on page-count

qpdf's `--show-npages` (and lopdf's own `load_metadata_mem`) trusts the root
`/Pages /Count`; pdq walks every leaf to guarantee count == what split would
produce. pdq is paying for a stronger guarantee the market leader doesn't
offer. Supremacy claim on count requires offering the same trust level as an
option (or default), keeping validation as `--strict`.

## The prize, quantified

lopdf already ships the xref-only path (`Reader::read_metadata`): measured
`load_metadata_mem` at **7.5 ms** on 12k-objstm (pdq today: 99 ms in-process),
**24.5 ms** on 100k (pdq: 332 ms), **2.8 ms** on 12k-raw (pdq: 58 ms). That is
the floor an xref-only bootstrap unlocks. (`examples/fastcount.rs` reproduces
this.)

## Roadmap to supreme

Ordered by leverage/effort:

1. **Xref-only bootstrap for `LazyPdf`** — parse `startxref` + xref chain
   (tables, streams, `/Prev`, `/XRefStm`) into `reference_table` + trailer,
   skip all object parsing. lopdf's `parser::xref_and_trailer` is private, so
   either (a) upstream a lopdf PR exposing an xref-only constructor (clean;
   `read_metadata` shows the internals already exist), or (b) hand-roll the
   xref parser in pdq (~300–500 lines incl. FlateDecode + PNG predictors;
   flate2 already in-tree). Wins: count wall −80%, split CPU −40%, RSS ~10×,
   kills the thread-pool storm. This one change flips count and split.
2. **Trusted-`/Count` fast path for `page-count`** (matching qpdf/lopdf
   semantics), validated walk behind `--strict`. With #1: ~3–8 ms vs qpdf's
   9 ms → supremacy; without #1 it's pointless (parse dominates).
3. **Arc cache for parsed normal objects** (analog of the container cache) —
   ends the 12k× font re-parse in split. Est. −20–40% split CPU.
4. **Per-output shared-subgraph template** — resolve the objects shared by
   all pages once, then per output copy only page-specific objects onto the
   template. Combined with #3 this attacks the ~1.2 ms/output cost directly
   (qpdf does ~37 µs/output single-threaded).
5. **Streaming single-page writer** — pdq already streams whole-document
   merges (`write.rs`); reusing that for split outputs bypasses per-output
   `Document` construction + `save`.
6. **Walk micro-costs** (after #1): replace `visited: BTreeSet` with a
   HashSet/bitmap keyed by object number, batch xref lookups per container.
   Target ≤1 µs/node validated.
7. **Real-world pruning guard** (independent): fast name-token scanner or
   skip-scan when the page's resource dict is document-global; keeps the
   README's real-PDF wins while avoiding decode_strict on hot paths.

## Final standing (measured 2026-07-03, after items #1–#3 + lean split outputs)

Final shootout on `big-c.pdf` (12,000 pages, object streams; merge adds
`small-c.pdf`, 2,500 pages). hyperfine mean ± σ, warmup 1 / 5 runs (count:
warmup 2 / 10 runs). All outputs validated: expected page counts plus
`qpdf --warning-exit-0 --check` on the rewrite/range/merge outputs and split
pages 1 / 6000 / 12000.

| scenario (12k objstm) | pdq before | pdq after | qpdf | vs qpdf |
| --- | ---: | ---: | ---: | ---: |
| page-count | 46.3 ms | **6.8 ms ± 0.9** | 8.7 ms ± 0.6 | 1.27× ± 0.19 faster |
| page-count `--strict` (validated walk) | 46.3 ms | 30.0 ms ± 0.7 | n/a (qpdf trusts `/Count`) | — |
| split-pages (12k files) | 1.136 s | **403 ms ± 42** | 929 ms ± 15 | 2.30× ± 0.24 faster |
| rewrite (copy all pages) | 82.0 ms | **85.5 ms ± 5.1** | 106.2 ms ± 3.0 | 1.24× ± 0.08 faster |
| range 5000–5100 | 33.7 ms | **31.3 ms ± 0.9** | 46.3 ms ± 2.1 | 1.48× ± 0.11 faster |
| merge 12k + 2.5k | 113.3 ms | **109.8 ms ± 5.0** | 203.3 ms ± 13.4 | 1.85× ± 0.15 faster |

MuPDF (mutool), where it competes: count 29.9 ms ± 0.7, rewrite (clean)
98.0 ms ± 1.4, range (merge) **29.6 ms ± 0.5**, merge 5.15 s ± 0.08.

## Outcome

**Met** — pdq beats qpdf in all five scenarios on this corpus:

- **count** flipped from a 5.3× loss to a 1.27× win. The win comes from the
  xref-only bootstrap (no discarded full parse) plus matching qpdf's
  semantics: the default now trusts the root `/Pages /Count` exactly like
  `qpdf --show-npages`. The old validated walk survives as `--strict`
  (30 ms) and kicks in automatically when `/Count` is missing/implausible.
- **split-pages** flipped from a 1.16× loss to a 2.30× win (xref-only
  bootstrap + Arc cache for normal objects + lean per-output writing).
- **rewrite, range, merge** kept their pre-existing wins over qpdf (no
  regression outside noise: rewrite 82.0→85.5 ms is within run-to-run σ and
  still ahead of both qpdf and mutool).

**Not met / caveats**:

- **Range vs MuPDF**: mutool merge measured 1.06× ± 0.03 ahead of pdq in the
  final run (29.6 vs 31.3 ms; previously a statistical tie at 1.01× ± 0.10).
  pdq is not the fastest tool tested at range extraction — only faster than
  qpdf there.
- **Strict counting** (the guarantee pdq used to give by default) is 30 ms —
  qpdf offers no equivalent validated mode, so there is no direct comparison,
  but users who opt into `--strict` wait longer than qpdf's trusting answer.
- **Synthetic-clean corpus only**: uniform pages, flat tree, no damaged
  xrefs. The README's original headline wins came from real-world messy PDFs
  where qpdf/Poppler degrade; that axis was not re-tested here (no such files
  in the repo). These numbers prove clean-input parity+lead, not universal
  supremacy.
- Single machine (Apple M4 Max, macOS/arm64), qpdf 12.3.2, mutool 1.28.0.
  RSS was not re-measured in the final round.
