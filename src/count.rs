use std::path::Path;

use crate::{load::map_file, repair::with_repair_retry, Result};

const MEMORY_INPUT_LABEL: &str = "<memory>";

/// Count the pages in a PDF by walking the page tree (validated count).
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
    count_impl(&mmap, input, password, true)
}

/// Like [`page_count`], but takes an in-memory PDF instead of a file path.
pub fn page_count_from_bytes(input: &[u8]) -> Result<usize> {
    page_count_from_bytes_with_password(input, None)
}

/// Like [`page_count_with_password`], but takes an in-memory PDF instead of a
/// file path.
pub fn page_count_from_bytes_with_password(input: &[u8], password: Option<&str>) -> Result<usize> {
    count_impl(input, Path::new(MEMORY_INPUT_LABEL), password, true)
}

/// Count the pages in a PDF by trusting the root `/Pages` `/Count` (fast count).
///
/// Market semantics, matching `qpdf --show-npages` and lopdf's metadata load:
/// the trailer `/Root` is resolved to the catalog, its `/Pages` node is read,
/// and a plausible `/Count` is returned as-is — the page tree is NOT walked,
/// so a lying-but-plausible `/Count` is trusted. When `/Count` is missing, not
/// a direct non-negative integer, or larger than the xref size, this falls
/// back to the validated walk ([`page_count`]) and still returns the true
/// count. Encryption is handled exactly as in [`page_count`].
pub fn page_count_fast(input: &Path) -> Result<usize> {
    page_count_fast_with_password(input, None)
}

/// Like [`page_count_fast`], additionally decrypting encrypted inputs with
/// `password` when the empty user password does not authenticate.
pub fn page_count_fast_with_password(input: &Path, password: Option<&str>) -> Result<usize> {
    let mmap = map_file(input)?;
    count_impl(&mmap, input, password, false)
}

/// Like [`page_count_fast`], but takes an in-memory PDF instead of a file
/// path.
pub fn page_count_fast_from_bytes(input: &[u8]) -> Result<usize> {
    page_count_fast_from_bytes_with_password(input, None)
}

/// Like [`page_count_fast_with_password`], but takes an in-memory PDF instead
/// of a file path.
pub fn page_count_fast_from_bytes_with_password(
    input: &[u8],
    password: Option<&str>,
) -> Result<usize> {
    count_impl(input, Path::new(MEMORY_INPUT_LABEL), password, false)
}

fn count_impl(input: &[u8], label: &Path, password: Option<&str>, strict: bool) -> Result<usize> {
    let timing = std::env::var_os("PDQ_TIMING").is_some();
    let start = std::time::Instant::now();
    // Damaged inputs whose xref lies about offsets get one retry against a
    // reconstructed table; the closure re-runs (and re-logs) in that case.
    with_repair_retry(input, label, password, |source| {
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
    })
}
