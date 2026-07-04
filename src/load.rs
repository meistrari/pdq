use std::{fs::File, path::Path};

use lopdf::{Document, FilterFunc, LoadOptions};
use memmap2::{Mmap, MmapOptions};

use crate::{range::PageRangeError, PdfOpsError, Result};

/// Load a whole document eagerly, transparently decrypting encrypted inputs.
///
/// Still the right source for whole-document copies: objects are parsed once
/// (in parallel) and then borrowed during the copy, which beats per-object
/// lazy fetches when the copy will touch every object anyway.
///
/// lopdf authenticates encrypted PDFs during the load: the empty user
/// password is tried first (covering the common owner-password-only files),
/// then `password` when provided. On success every object is decrypted in
/// memory, so the returned document — and anything saved from it — is
/// unencrypted.
pub(crate) fn load_document(path: &Path, password: Option<&str>) -> Result<Document> {
    let mmap = map_file(path)?;
    let document = Document::load_mem_with_options(&mmap, load_options(password, None))
        .map_err(|err| decorate_load_error(err, path))?;
    ensure_decrypted(&document, path)?;
    if document.page_iter().next().is_none() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    Ok(document)
}

pub(crate) fn load_options(password: Option<&str>, filter: Option<FilterFunc>) -> LoadOptions {
    LoadOptions {
        password: password.map(str::to_owned),
        filter,
        strict: false,
    }
}

/// Map a lopdf load failure to a user-facing error, giving wrong-password
/// failures a dedicated message.
pub(crate) fn decorate_load_error(err: lopdf::Error, path: &Path) -> PdfOpsError {
    match err {
        lopdf::Error::InvalidPassword => {
            PdfOpsError::Password(format!("invalid password for {}", path.display()))
        }
        err => PdfOpsError::Pdf(err),
    }
}

/// Reject documents lopdf could not decrypt during the load, which happens
/// when an input is encrypted with a non-empty user password and no (or an
/// empty) password was supplied.
pub(crate) fn ensure_decrypted(document: &Document, path: &Path) -> Result<()> {
    if document.is_encrypted() {
        return Err(PdfOpsError::Password(format!(
            "{} is encrypted and requires a password; retry with --password",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    // The mmap is read-only. Concurrent truncation of the input can still
    // SIGBUS the process, which is the standard memmap tradeoff.
    Ok(unsafe { MmapOptions::new().map(&file)? })
}
