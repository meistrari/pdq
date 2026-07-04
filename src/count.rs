use std::path::Path;

use crate::{lazy::LazyPdf, load::map_file, Result};

/// Count the pages in a PDF.
///
/// Uses the same lazy, mmap-backed reader as `split-pages` and counts via the
/// shared page-tree walk (`count_pages`), so it stays cheap on very large
/// documents — O(1) memory, no per-page id allocation — and can never disagree
/// with the pages `split`/`split-pages` would resolve. Encrypted PDFs are
/// rejected, consistent with the rest of the MVP.
///
/// Returns `0` for a structurally valid PDF that declares no pages.
pub fn page_count(input: &Path) -> Result<usize> {
    let timing = std::env::var_os("PDQ_TIMING").is_some();
    let start = std::time::Instant::now();
    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    if timing {
        eprintln!("phase parse: {:?}", start.elapsed());
    }
    let walk_start = std::time::Instant::now();
    let count = source.count_pages();
    if timing {
        eprintln!("phase walk:  {:?}", walk_start.elapsed());
    }
    count
}
