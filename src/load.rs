use std::{fs::File, path::Path};

use lopdf::{Document, LoadOptions};
use memmap2::{Mmap, MmapOptions};

use crate::{range::PageRangeError, PdfOpsError, Result};

pub(crate) fn load_document(path: &Path) -> Result<Document> {
    let document = load_document_mmap(path)?;
    if document.is_encrypted() || document.was_encrypted() {
        return Err(PdfOpsError::Unsupported(format!(
            "encrypted PDFs are not supported: {}",
            path.display()
        )));
    }
    if document.page_iter().next().is_none() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    Ok(document)
}

fn load_document_mmap(path: &Path) -> Result<Document> {
    let mmap = map_file(path)?;
    Ok(Document::load_mem_with_options(
        &mmap,
        LoadOptions::default(),
    )?)
}

pub(crate) fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    // The mmap is read-only. Concurrent truncation of the input can still
    // SIGBUS the process, which is the standard memmap tradeoff.
    Ok(unsafe { MmapOptions::new().map(&file)? })
}
