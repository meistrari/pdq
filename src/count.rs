use std::path::Path;

use crate::{lazy::LazyPdf, load::map_file, Result};

/// Count the pages in a PDF.
///
/// Uses the same lazy, mmap-backed reader as `split-pages`, so it stays cheap on
/// very large documents (it walks the page tree without materializing page
/// content). Encrypted PDFs are rejected, consistent with the rest of the MVP.
///
/// Returns `0` for a structurally valid PDF that declares no pages.
pub fn page_count(input: &Path) -> Result<usize> {
    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    Ok(source.page_ids()?.len())
}
