use std::path::Path;

use crate::{lazy::PdfSource, load::map_file, Result};

/// Count the pages in a PDF.
///
/// Uses the same lazy, mmap-backed reader as `split-pages` and counts via the
/// shared page-tree walk (`count_pages`), so it stays cheap on very large
/// documents — O(1) memory, no per-page id allocation — and can never disagree
/// with the pages `split`/`split-pages` would resolve. Encrypted PDFs with an
/// empty user password are decrypted transparently; files that need a real
/// password require [`page_count_with_password`].
///
/// Returns `0` for a structurally valid PDF that declares no pages.
pub fn page_count(input: &Path) -> Result<usize> {
    page_count_with_password(input, None)
}

/// Like [`page_count`], additionally decrypting encrypted inputs with
/// `password` when the empty user password does not authenticate.
pub fn page_count_with_password(input: &Path, password: Option<&str>) -> Result<usize> {
    let mmap = map_file(input)?;
    let source = PdfSource::open(&mmap, input, password)?;
    source.count_pages()
}
