use std::{fs, fs::File, os::unix::fs::MetadataExt, path::Path};

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

pub(crate) fn same_file(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::metadata(left)?;
    let right = match fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}
