use std::{collections::BTreeMap, fs::File, path::Path};

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use lopdf::{xref::XrefEntry, Document, FilterFunc, LoadOptions, Object, Stream};
use memmap2::{Mmap, MmapOptions};

use crate::{
    filter::normalize_filter_names_for_lopdf_load, range::PageRangeError, PdfOpsError, Result,
};

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

struct Aes256Repair {
    key: [u8; 32],
    encrypt_metadata: bool,
}

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
    let mut document = Document::load_mem_with_options(
        &mmap,
        load_options(password, Some(normalize_filter_names_for_lopdf_load)),
    )
    .map_err(|err| decorate_load_error(err, path))?;
    finalize_decrypted_document(&mut document, path, Some(&mmap))?;
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

/// Load failed AND reconstruction failed: name the real problem — damaged
/// cross-reference data — instead of lopdf's generic parse error, and point
/// at dedicated repair tooling (issue #14). Errors outside that class (bad
/// header, I/O, …) keep their original message.
pub(crate) fn upgrade_damaged_xref_error(err: lopdf::Error, path: &Path) -> PdfOpsError {
    let xref_class = match &err {
        lopdf::Error::Xref(_)
        | lopdf::Error::ObjectIdMismatch
        | lopdf::Error::IndirectObject { .. } => true,
        // `ParseError` is not re-exported by lopdf, so classify the inner
        // error by its message: trailer/xref parse failures and truncation
        // are xref damage, while e.g. "invalid file header" (not a PDF at
        // all) keeps its original message.
        lopdf::Error::Parse(inner) => {
            let message = inner.to_string();
            message.contains("trailer")
                || message.contains("cross reference")
                || message.contains("end of input")
        }
        _ => false,
    };
    if xref_class {
        PdfOpsError::InvalidStructure(format!(
            "{}: damaged cross-reference table or trailer ({err}); automatic repair \
             failed — a dedicated repair tool (e.g. `qpdf file.pdf repaired.pdf`) may \
             still recover it",
            path.display()
        ))
    } else {
        decorate_load_error(err, path)
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

pub(crate) fn finalize_decrypted_document(
    document: &mut Document,
    path: &Path,
    original: Option<&[u8]>,
) -> Result<()> {
    ensure_decrypted(document, path)?;
    repair_lingering_aes256_streams(document, original);
    Ok(())
}

fn repair_lingering_aes256_streams(document: &mut Document, original: Option<&[u8]>) {
    let Some(repair) = aes256_repair(document) else {
        return;
    };

    let original_streams = original.map(|buffer| {
        document
            .objects
            .iter()
            .filter_map(|(id, object)| {
                if !matches!(object, Object::Stream(_)) {
                    return None;
                }
                let XrefEntry::Normal { offset, .. } =
                    document.reference_table.entries.get(&id.0)?
                else {
                    return None;
                };
                let stream = object.as_stream().ok()?;
                let length = stream_content_length(stream)?;
                let content =
                    raw_stream_content_at(buffer, usize::try_from(*offset).ok()?, length)?;
                Some((*id, content.to_vec()))
            })
            .collect::<BTreeMap<_, _>>()
    });

    for (id, object) in document.objects.iter_mut() {
        repair_lingering_aes256_streams_in_object(
            object,
            &repair,
            original_streams
                .as_ref()
                .and_then(|streams| streams.get(id)),
        );
    }
}

fn aes256_repair(document: &Document) -> Option<Aes256Repair> {
    let state = document.encryption_state.as_ref()?;
    if state.revision() < 5 || state.default_stream_filter() == b"Identity" {
        return None;
    }

    let key = state.file_encryption_key();
    if key.len() != 32 {
        return None;
    }

    let mut fixed = [0u8; 32];
    fixed.copy_from_slice(key);
    Some(Aes256Repair {
        key: fixed,
        encrypt_metadata: state.encrypt_metadata(),
    })
}

fn repair_lingering_aes256_streams_in_object(
    object: &mut Object,
    repair: &Aes256Repair,
    original_content: Option<&Vec<u8>>,
) {
    match object {
        Object::Array(items) => {
            for item in items {
                repair_lingering_aes256_streams_in_object(item, repair, None);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter_mut() {
                repair_lingering_aes256_streams_in_object(value, repair, None);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                repair_lingering_aes256_streams_in_object(value, repair, None);
            }

            if stream.dict.has_type(b"XRef")
                || (stream.dict.has_type(b"Metadata") && !repair.encrypt_metadata)
                || stream_uses_identity_crypt_filter(stream)
            {
                return;
            }

            let ciphertext = original_content
                .map(Vec::as_slice)
                .unwrap_or(&stream.content);
            if !should_try_aes256_stream_repair(stream, ciphertext, original_content.is_some()) {
                return;
            }

            if let Some(plain) = decrypt_aes256_cbc_pkcs7(&repair.key, ciphertext) {
                if stream_content_is_raw_ciphertext(stream, ciphertext)
                    || !stream_content_decodes(stream)
                    || stream_content_decodes_with(stream, &plain)
                {
                    stream.set_content(plain);
                }
            }
        }
        _ => {}
    }
}

fn should_try_aes256_stream_repair(
    stream: &lopdf::Stream,
    ciphertext: &[u8],
    from_original_file: bool,
) -> bool {
    if ciphertext.len() <= 16 || !ciphertext.len().is_multiple_of(16) {
        return false;
    }

    if stream_content_is_raw_ciphertext(stream, ciphertext) {
        return true;
    }

    if from_original_file && stream_content_decodes(stream) {
        return false;
    }

    !stream_content_decodes(stream)
}

fn stream_content_is_raw_ciphertext(stream: &lopdf::Stream, ciphertext: &[u8]) -> bool {
    stream.content.as_slice() == ciphertext
}

fn stream_content_decodes(stream: &Stream) -> bool {
    stream.filters().is_ok()
        && matches!(stream.decompressed_content(), Ok(decoded) if !decoded.is_empty())
}

fn stream_content_decodes_with(stream: &Stream, content: &[u8]) -> bool {
    let mut candidate = stream.clone();
    candidate.set_content(content.to_vec());
    stream_content_decodes(&candidate)
}

fn stream_uses_identity_crypt_filter(stream: &lopdf::Stream) -> bool {
    let Ok(filters) = stream.filters() else {
        return false;
    };
    let Some(crypt_index) = filters.iter().position(|filter| *filter == b"Crypt") else {
        return false;
    };

    match stream.dict.get(b"DecodeParms") {
        Ok(Object::Dictionary(params)) => crypt_filter_name_is_identity(params),
        Ok(Object::Array(params)) => params
            .get(crypt_index)
            .and_then(|param| param.as_dict().ok())
            .is_some_and(crypt_filter_name_is_identity),
        _ => false,
    }
}

fn crypt_filter_name_is_identity(params: &lopdf::Dictionary) -> bool {
    matches!(params.get(b"Name"), Ok(Object::Name(name)) if name == b"Identity")
}

fn decrypt_aes256_cbc_pkcs7(key: &[u8; 32], ciphertext: &[u8]) -> Option<Vec<u8>> {
    if ciphertext.len() <= 16 || !ciphertext.len().is_multiple_of(16) {
        return None;
    }

    let iv: &[u8; 16] = ciphertext[..16].try_into().ok()?;
    let mut data = ciphertext[16..].to_vec();
    Aes256CbcDec::new(key.into(), iv.into())
        .decrypt_padded::<Pkcs7>(&mut data)
        .ok()
        .map(|plain| plain.to_vec())
}

fn stream_content_length(stream: &Stream) -> Option<usize> {
    let length = stream.dict.get(b"Length").ok()?.as_i64().ok()?;
    usize::try_from(length).ok()
}

fn raw_stream_content_at(buffer: &[u8], offset: usize, length: usize) -> Option<&[u8]> {
    let object = buffer.get(offset..)?;
    let mut search_from = 0usize;

    while let Some(relative_marker) = find_subslice(object.get(search_from..)?, b"stream") {
        let stream_marker = search_from + relative_marker;
        search_from = stream_marker + b"stream".len();
        let Some(start) = stream_content_start(object, stream_marker) else {
            continue;
        };
        let end = start.checked_add(length)?;
        if raw_stream_has_expected_end_marker(object, end) {
            return object.get(start..end);
        }
    }

    None
}

fn stream_content_start(object: &[u8], stream_marker: usize) -> Option<usize> {
    let mut start = stream_marker.checked_add(b"stream".len())?;
    match object.get(start..start + 2) {
        Some(b"\r\n") => start += 2,
        _ => match object.get(start) {
            Some(b'\n' | b'\r') => start += 1,
            _ => return None,
        },
    }
    Some(start)
}

fn raw_stream_has_expected_end_marker(object: &[u8], mut end: usize) -> bool {
    if object.get(end..end + b"endstream".len()) == Some(b"endstream") {
        return true;
    }
    if object.get(end..end + 2) == Some(b"\r\n") {
        end += 2;
    } else if matches!(object.get(end), Some(b'\n' | b'\r')) {
        end += 1;
    }
    object.get(end..end + b"endstream".len()) == Some(b"endstream")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path)?;
    // The mmap is read-only. Concurrent truncation of the input can still
    // SIGBUS the process, which is the standard memmap tradeoff.
    Ok(unsafe { MmapOptions::new().map(&file)? })
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Stream};

    use super::{raw_stream_content_at, stream_content_decodes};

    #[test]
    fn raw_stream_content_uses_length_not_first_stream_word() {
        let object = b"7 0 obj\n<< /Note (Download stream\ndata) /Length 19 >>\nstream\nabcendstreamxyz1234\nendstream\nendobj\n";

        assert_eq!(
            raw_stream_content_at(object, 0, 19),
            Some(b"abcendstreamxyz1234".as_slice())
        );
    }

    #[test]
    fn no_filter_stream_does_not_count_as_decoded() {
        let stream = Stream::new(dictionary! {}, b"raw ciphertext-ish bytes".to_vec());

        assert!(!stream_content_decodes(&stream));
    }
}
