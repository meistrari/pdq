use std::path::Path;

use crate::{lazy::LazyPdf, load::map_file, Result};

/// Count the pages in a PDF by walking the page tree (validated count).
///
/// Uses the same lazy, mmap-backed reader as `split-pages` and counts via the
/// shared page-tree walk (`count_pages`), so it stays cheap on very large
/// documents — O(1) memory, no per-page id allocation — and can never disagree
/// with the pages `split`/`split-pages` would resolve. Encrypted PDFs are
/// rejected, consistent with the rest of the MVP.
///
/// Returns `0` for a structurally valid PDF that declares no pages.
pub fn page_count(input: &Path) -> Result<usize> {
    count_impl(input, true)
}

/// Count the pages in a PDF by trusting the root `/Pages` `/Count` (fast count).
///
/// Market semantics, matching `qpdf --show-npages` and lopdf's metadata load:
/// the trailer `/Root` is resolved to the catalog, its `/Pages` node is read,
/// and a plausible `/Count` is returned as-is — the page tree is NOT walked,
/// so a lying-but-plausible `/Count` is trusted. When `/Count` is missing, not
/// a direct non-negative integer, or larger than the xref size, this falls
/// back to the validated walk ([`page_count`]) and still returns the true
/// count. Encrypted PDFs are rejected exactly as in [`page_count`].
pub fn page_count_fast(input: &Path) -> Result<usize> {
    count_impl(input, false)
}

fn count_impl(input: &Path, strict: bool) -> Result<usize> {
    let timing = std::env::var_os("PDQ_TIMING").is_some();
    let start = std::time::Instant::now();
    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    if timing {
        eprintln!("phase parse: {:?}", start.elapsed());
    }
    let count_start = std::time::Instant::now();
    let count = if strict {
        source.count_pages()
    } else {
        source.count_pages_fast()
    };
    if timing {
        let phase = if strict { "walk" } else { "count" };
        eprintln!("phase {phase}: {:?}", count_start.elapsed());
    }
    count
}
