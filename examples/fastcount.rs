//! Prototype: measure lopdf's xref-only metadata path (load_metadata_mem)
//! against pdq's current full-parse page_count, to size the win of an
//! xref-only bootstrap for LazyPdf. Not shipped; benchmark scaffolding only.

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: fastcount <pdf>");
    let buffer = std::fs::read(&path).expect("read pdf");

    let start = Instant::now();
    let metadata = lopdf::Document::load_metadata_mem(&buffer).expect("metadata");
    let metadata_elapsed = start.elapsed();
    eprintln!(
        "load_metadata_mem (xref chain + trusted /Count): {} pages in {:?}",
        metadata.page_count, metadata_elapsed
    );

    let start = Instant::now();
    let count = pdq::page_count(std::path::Path::new(&path)).expect("page_count");
    let full_elapsed = start.elapsed();
    eprintln!("pdq::page_count (full parse + validated walk): {count} pages in {full_elapsed:?}");
}
