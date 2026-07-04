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

## Current standing after this worktree's prototype

| scenario (12k objstm) | pdq before | pdq now | qpdf | after #1–#3 (est.) |
| --- | ---: | ---: | ---: | ---: |
| page-count | 46 ms | 42 ms | 9 ms | **~5–8 ms** |
| split-pages | 1.14 s | 1.09 s | 0.98 s | **~0.6–0.8 s** |
| rewrite | 82 ms (wins) | — | 114 ms | — |
| range | 34 ms (ties MuPDF) | — | 48 ms | — |
| merge | 113 ms (wins) | — | 200 ms | — |

pdq already leads merge/rewrite/range. Items #1–#3 are enough to take count
and split on clean inputs; #4–#5 turn split into the same kind of blowout the
README shows on pathological real-world files.
