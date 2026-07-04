//! Best-effort cross-reference reconstruction for damaged files.
//!
//! Scope decision from issue #14: pdq repairs files whose xref/trailer data is
//! damaged, using the same recovery strategy qpdf, Poppler, pdf.js and mutool
//! converge on — sweep the raw buffer for `N G obj` headers and rebuild the
//! cross-reference table from what is actually in the file, ignoring the
//! damaged table entirely.
//!
//! Two properties keep this free for well-formed files:
//!
//! * It never runs eagerly. Callers reach it only after the normal parse
//!   fails (load time) or after a fetch proves the xref lies about an offset
//!   (fetch time, see [`is_offset_damage`]) — error paths a healthy file
//!   never enters.
//! * Its output is the same metadata-only `Document` (reference table +
//!   trailer) that `crate::xrefboot` produces, wrapped in the same lazy
//!   reader. There is no separate "repaired" pipeline: downstream count,
//!   copy, split, merge, and write behavior is identical by construction.
//!
//! Encrypted files are never repaired: the lazy reader would hand out
//! still-encrypted bytes, so those inputs keep today's hard error and are
//! qpdf territory. Outputs produced from a repaired source are always full
//! rewrites — the merge byte-copy fast path refuses repaired sources so
//! damage is never copied through verbatim.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use lopdf::{
    xref::{Xref, XrefEntry, XrefType},
    Dictionary, Document, Object, ObjectId, Reader,
};
use memchr::memmem;

use crate::{
    lazy::PdfSource,
    xrefboot::{is_delimiter, is_whitespace, parse_version, Lexer},
    PdfOpsError, Result,
};

/// `XrefEntry::Compressed` indexes are u16, so an object stream can
/// contribute at most this many recovered entries.
const MAX_OBJSTM_ENTRIES: usize = u16::MAX as usize + 1;

/// True for fetch-time errors that prove the cross-reference table lies about
/// an offset: the recorded position holds a different object, or bytes that
/// are not an object at all. These — and only these — justify a repair retry.
/// `ObjectNotFound`/`MissingXrefEntry` must NOT trigger one: dangling
/// references are common in perfectly valid files and are already tolerated
/// as `Null` by the copy paths, so treating them as damage would run a
/// full-buffer scan on healthy inputs.
pub(crate) fn is_offset_damage(err: &PdfOpsError) -> bool {
    matches!(
        err,
        PdfOpsError::Pdf(lopdf::Error::ObjectIdMismatch | lopdf::Error::IndirectObject { .. })
    )
}

/// Run `operation` over a lazily opened source, retrying exactly once with a
/// force-reconstructed source when the first run fails with an error that
/// proves the xref lies (see [`is_offset_damage`]). This is the wrapper for
/// single-input operations: the whole operation re-runs against the repaired
/// source, so the recovered table replaces the damaged one wholesale (qpdf
/// semantics) instead of mixing entries from both.
///
/// Healthy files pay nothing: the retry branch sits on the error path, and a
/// successful first run returns untouched.
pub(crate) fn with_repair_retry<'a, T>(
    buffer: &'a [u8],
    path: &Path,
    password: Option<&str>,
    operation: impl Fn(&PdfSource<'a>) -> Result<T>,
) -> Result<T> {
    let source = PdfSource::open(buffer, path, password)?;
    match operation(&source) {
        Err(err) if is_offset_damage(&err) && !source.repaired() => {
            match PdfSource::open_repaired(buffer, path) {
                Some(repaired) => operation(&repaired),
                // Unrepairable: surface the original failure, not a new one.
                None => Err(err),
            }
        }
        result => result,
    }
}

/// Rebuild a metadata-only `Document` (reference table + trailer) by scanning
/// `buffer` for object headers, ignoring the file's own xref data entirely.
/// Returns `None` when the buffer cannot be repaired — encrypted, no
/// recoverable objects, or no root that resolves to a catalog — in which case
/// callers surface their original error.
pub(crate) fn reconstruct_document(buffer: &[u8]) -> Option<Document> {
    // Encrypted inputs cannot be repaired: object streams and strings would
    // come out still encrypted. The token search is conservative (a literal
    // "/Encrypt" inside page content also refuses), but a false hit only
    // means keeping today's hard error instead of attempting repair.
    if memmem::find(buffer, b"/Encrypt").is_some() {
        return None;
    }

    let headers = scan_object_headers(buffer);
    if headers.is_empty() {
        return None;
    }

    // Classify the recovered objects by their header dictionary: catalogs
    // (root fallback), object streams (their members need Compressed
    // entries), and xref streams (whose dict doubles as a trailer).
    let mut catalogs: Vec<(u32, ObjectId)> = Vec::new();
    let mut object_streams: Vec<ObjStmCandidate> = Vec::new();
    let mut xref_trailers: Vec<(u32, Dictionary)> = Vec::new();
    for (&id, header) in &headers {
        let Some((dict, dict_end)) = parse_header_dict(buffer, header.offset) else {
            continue;
        };
        match dict.get(b"Type").ok().and_then(|t| t.as_name().ok()) {
            Some(b"Catalog") => catalogs.push((header.offset, (id, header.generation))),
            Some(b"ObjStm") => object_streams.push(ObjStmCandidate {
                container: id,
                dict,
                dict_end,
            }),
            Some(b"XRef") => xref_trailers.push((header.offset, dict)),
            _ => {}
        }
    }

    let trailer_source = recover_trailer_dict(buffer, &xref_trailers);

    // Root candidates in order of trust: the recovered trailer's /Root
    // (generation normalized to the scanned header when they disagree, since
    // lookups resolve strictly by (id, generation)), then the catalog object
    // that appears last in the file. When neither exists the catalog may
    // live compressed inside an object stream, so the expansion below is
    // asked to look for one among the members it decodes.
    let mut root_candidates: Vec<ObjectId> = Vec::new();
    if let Some(dict) = &trailer_source {
        if let Ok(root) = dict.get(b"Root").and_then(Object::as_reference) {
            root_candidates.push(match headers.get(&root.0) {
                Some(header) if header.generation != root.1 => (root.0, header.generation),
                _ => root,
            });
        }
    }
    if let Some(&(_, id)) = catalogs.iter().max_by_key(|(offset, _)| *offset) {
        if !root_candidates.contains(&id) {
            root_candidates.push(id);
        }
    }
    let scan_members_for_catalog = root_candidates.is_empty();

    let mut entries: BTreeMap<u32, XrefEntry> = headers
        .iter()
        .map(|(&id, header)| {
            (
                id,
                XrefEntry::Normal {
                    offset: header.offset,
                    generation: header.generation,
                },
            )
        })
        .collect();
    let mut member_catalogs: Vec<ObjectId> = Vec::new();
    for candidate in &object_streams {
        // Best effort: an object stream that cannot be decoded just
        // contributes nothing; its members stay unresolvable.
        let _ = expand_object_stream(
            buffer,
            &headers,
            candidate,
            &mut entries,
            scan_members_for_catalog.then_some(&mut member_catalogs),
        );
    }
    root_candidates.extend(member_catalogs);
    if root_candidates.is_empty() {
        return None;
    }

    let version = recover_version(buffer);
    let max_id = *entries.keys().next_back()?;
    let mut xref = Xref::new(max_id.checked_add(1)?, XrefType::CrossReferenceTable);
    xref.entries = entries;

    let mut document = Document::new();
    document.version = version;
    document.max_id = max_id;
    document.reference_table = xref;

    // The recovered table is only trusted when a root candidate actually
    // resolves to a catalog-shaped dictionary through it.
    for root in root_candidates {
        let mut trailer = Dictionary::new();
        trailer.set("Size", i64::from(max_id) + 1);
        trailer.set("Root", Object::Reference(root));
        if let Some(source) = &trailer_source {
            for key in [b"Info".as_slice(), b"ID".as_slice()] {
                if let Ok(value) = source.get(key) {
                    trailer.set(key, value.clone());
                }
            }
        }
        document.trailer = trailer;
        let (returned, is_catalog) = root_resolves_to_catalog(buffer, document, root);
        document = returned;
        if is_catalog {
            return Some(document);
        }
    }
    None
}

/// One recovered `N G obj` header. `offset` points at the first digit of the
/// object number, computed from the header's real position in the buffer —
/// which makes the recovered table immune to the junk-before-`%PDF-` and
/// offset-relative-to-header problems of the original xref.
struct ScannedHeader {
    offset: u32,
    generation: u16,
}

/// Sweep the buffer for `N G obj` headers. Later occurrences of an id win,
/// approximating incremental-update semantics: an appended replacement sits
/// after the object it supersedes.
fn scan_object_headers(buffer: &[u8]) -> BTreeMap<u32, ScannedHeader> {
    let mut headers = BTreeMap::new();
    for pos in memmem::find_iter(buffer, b"obj") {
        if let Some((id, header)) = parse_header_at(buffer, pos) {
            headers.insert(id, header);
        }
    }
    headers
}

/// Validate and parse the `N G obj` header whose `obj` keyword starts at
/// `keyword_pos`, walking backwards over the generation and object number.
fn parse_header_at(buffer: &[u8], keyword_pos: usize) -> Option<(u32, ScannedHeader)> {
    // `obj` must end at a token boundary (`objx` is not a keyword).
    if let Some(&byte) = buffer.get(keyword_pos + 3) {
        if !is_whitespace(byte) && !is_delimiter(byte) {
            return None;
        }
    }

    let gen_end = skip_whitespace_backwards(buffer, keyword_pos)?;
    let (gen_start, generation) = parse_digits_backwards(buffer, gen_end)?;
    let generation = u16::try_from(generation).ok()?;
    let id_end = skip_whitespace_backwards(buffer, gen_start)?;
    let (id_start, id) = parse_digits_backwards(buffer, id_end)?;
    let id = u32::try_from(id).ok().filter(|id| *id > 0)?;

    // The object number must start at a token boundary, so `/F12 0 obj`
    // inside a content stream does not register as object 12.
    if id_start > 0 {
        let byte = buffer[id_start - 1];
        if !is_whitespace(byte) && !is_delimiter(byte) {
            return None;
        }
    }

    // Plausibility: the bytes after `obj` must begin a PDF object (or an
    // empty object's `endobj`), which filters most `N G obj` lookalikes
    // inside string and stream payloads.
    let mut lexer = Lexer {
        buffer,
        pos: keyword_pos + 3,
    };
    lexer.skip_whitespace();
    if !matches!(
        lexer.peek(),
        Some(
            b'<' | b'[' | b'/' | b'(' | b'+' | b'-' | b'.' | b'0'
                ..=b'9' | b't' | b'f' | b'n' | b'e'
        )
    ) {
        return None;
    }

    let offset = u32::try_from(id_start).ok()?;
    Some((id, ScannedHeader { offset, generation }))
}

/// Step backwards over whitespace ending at `end`; at least one whitespace
/// byte is required (the tokens of a header cannot touch).
fn skip_whitespace_backwards(buffer: &[u8], end: usize) -> Option<usize> {
    let mut pos = end;
    while pos > 0 && is_whitespace(buffer[pos - 1]) {
        pos -= 1;
    }
    (pos < end).then_some(pos)
}

/// Parse the digit run ending at `end`, returning its start and value.
fn parse_digits_backwards(buffer: &[u8], end: usize) -> Option<(usize, u64)> {
    let mut start = end;
    while start > 0 && buffer[start - 1].is_ascii_digit() {
        start -= 1;
    }
    // At most 10 digits: enough for any u32 id and keeps the value in u64.
    if start == end || end - start > 10 {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in &buffer[start..end] {
        value = value * 10 + u64::from(byte - b'0');
    }
    Some((start, value))
}

/// Parse the dictionary of the object whose header starts at `offset`,
/// returning it with the buffer position just past the closing `>>`.
/// Non-dictionary objects return `None` cheaply (first byte check).
fn parse_header_dict(buffer: &[u8], offset: u32) -> Option<(Dictionary, usize)> {
    let mut lexer = Lexer {
        buffer,
        pos: offset as usize,
    };
    lexer.skip_whitespace();
    lexer.parse_unsigned::<u32>()?;
    lexer.skip_whitespace();
    lexer.parse_unsigned::<u16>()?;
    lexer.skip_whitespace();
    if !lexer.try_keyword(b"obj") {
        return None;
    }
    lexer.skip_whitespace();
    if lexer.peek() != Some(b'<') {
        return None;
    }
    match lexer.parse_object(0)? {
        Object::Dictionary(dict) => Some((dict, lexer.pos)),
        _ => None,
    }
}

/// Recover a trailer dictionary: the last parseable `trailer` dict carrying a
/// /Root reference wins; xref-stream files (no `trailer` keyword) fall back
/// to the last recovered /Type /XRef dictionary, which doubles as a trailer.
fn recover_trailer_dict(buffer: &[u8], xref_trailers: &[(u32, Dictionary)]) -> Option<Dictionary> {
    let mut best: Option<Dictionary> = None;
    for pos in memmem::find_iter(buffer, b"trailer") {
        if pos > 0 {
            let prev = buffer[pos - 1];
            if !is_whitespace(prev) && !is_delimiter(prev) {
                continue;
            }
        }
        let mut lexer = Lexer { buffer, pos };
        if !lexer.try_keyword(b"trailer") {
            continue;
        }
        let Some(Object::Dictionary(dict)) = lexer.parse_object(0) else {
            continue;
        };
        if dict
            .get(b"Root")
            .map(|root| root.as_reference().is_ok())
            .unwrap_or(false)
        {
            best = Some(dict);
        }
    }
    best.or_else(|| {
        xref_trailers
            .iter()
            .filter(|(_, dict)| {
                dict.get(b"Root")
                    .map(|root| root.as_reference().is_ok())
                    .unwrap_or(false)
            })
            .max_by_key(|(offset, _)| *offset)
            .map(|(_, dict)| dict.clone())
    })
}

/// An object whose recovered header dictionary is `/Type /ObjStm`.
struct ObjStmCandidate {
    container: u32,
    dict: Dictionary,
    /// Buffer position just past the dictionary's closing `>>`.
    dict_end: usize,
}

/// Register `XrefEntry::Compressed` entries for the members of an object
/// stream. A top-level `N G obj` header always beats membership here: it is
/// direct evidence in the file, and incremental updates that replace a
/// compressed object append exactly such a header.
///
/// When `member_catalogs` is given (no root candidate was found elsewhere),
/// each member's dictionary is also inspected so a catalog compressed into an
/// object stream can still serve as the root.
fn expand_object_stream(
    buffer: &[u8],
    headers: &BTreeMap<u32, ScannedHeader>,
    candidate: &ObjStmCandidate,
    entries: &mut BTreeMap<u32, XrefEntry>,
    mut member_catalogs: Option<&mut Vec<ObjectId>>,
) -> Option<()> {
    let count = usize::try_from(candidate.dict.get(b"N").ok()?.as_i64().ok()?).ok()?;
    let content = stream_content(buffer, candidate, headers)?;
    let stream = lopdf::Stream::new(candidate.dict.clone(), content.to_vec());
    // An undecodable stream contributes nothing; an unfiltered one is its own
    // decoded content.
    let decoded = stream
        .decompressed_content()
        .unwrap_or_else(|_| content.to_vec());
    let first = candidate
        .dict
        .get(b"First")
        .ok()
        .and_then(|first| first.as_i64().ok())
        .and_then(|first| usize::try_from(first).ok());

    let mut lexer = Lexer {
        buffer: &decoded,
        pos: 0,
    };
    for index in 0..count.min(MAX_OBJSTM_ENTRIES) {
        lexer.skip_whitespace();
        let id = lexer.parse_unsigned::<u32>()?;
        lexer.skip_whitespace();
        let member_offset = lexer.parse_unsigned::<u64>()?;
        if id == 0 {
            continue;
        }
        entries.entry(id).or_insert(XrefEntry::Compressed {
            container: candidate.container,
            index: index as u16,
        });
        if let (Some(catalogs), Some(first)) = (member_catalogs.as_deref_mut(), first) {
            let start = first.checked_add(member_offset as usize);
            if let Some(start) = start.filter(|start| *start < decoded.len()) {
                let mut member = Lexer {
                    buffer: &decoded,
                    pos: start,
                };
                if let Some(Object::Dictionary(dict)) = member.parse_object(0) {
                    // Compressed objects always have generation 0.
                    if dict.has_type(b"Catalog") {
                        catalogs.push((id, 0));
                    }
                }
            }
        }
    }
    Some(())
}

/// Slice the raw stream content following an object's dictionary, but only
/// when the declared /Length (direct, or indirect through the recovered
/// headers) verifies — `endstream` must sit right behind it. A lied-about
/// length makes the container unreadable for the runtime reader too (lopdf
/// trusts /Length), so registering its members would trade "missing object,
/// tolerated as null" for a hard error mid-copy; skipping the container is
/// the better degradation.
fn stream_content<'a>(
    buffer: &'a [u8],
    candidate: &ObjStmCandidate,
    headers: &BTreeMap<u32, ScannedHeader>,
) -> Option<&'a [u8]> {
    let mut lexer = Lexer {
        buffer,
        pos: candidate.dict_end,
    };
    lexer.skip_whitespace();
    if !lexer.try_keyword(b"stream") {
        return None;
    }
    lexer.consume_stream_eol()?;
    let start = lexer.pos;

    let length = resolve_stream_length(&candidate.dict, buffer, headers)?;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= buffer.len())?;
    let mut tail = Lexer { buffer, pos: end };
    tail.skip_whitespace();
    tail.try_keyword(b"endstream").then(|| &buffer[start..end])
}

/// Resolve a stream dictionary's /Length: a direct integer, or an indirect
/// reference resolved through the recovered headers.
fn resolve_stream_length(
    dict: &Dictionary,
    buffer: &[u8],
    headers: &BTreeMap<u32, ScannedHeader>,
) -> Option<usize> {
    match dict.get(b"Length").ok()? {
        Object::Integer(length) => usize::try_from(*length).ok(),
        Object::Reference((id, generation)) => {
            let header = headers.get(id)?;
            if header.generation != *generation {
                return None;
            }
            let mut lexer = Lexer {
                buffer,
                pos: header.offset as usize,
            };
            lexer.skip_whitespace();
            lexer.parse_unsigned::<u32>()?;
            lexer.skip_whitespace();
            lexer.parse_unsigned::<u16>()?;
            lexer.skip_whitespace();
            if !lexer.try_keyword(b"obj") {
                return None;
            }
            match lexer.parse_object(0)? {
                Object::Integer(length) => usize::try_from(length).ok(),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Version from the `%PDF-` header, tolerating leading junk; a mangled header
/// falls back to 1.4, which only affects the version stamp of rewrites.
fn recover_version(buffer: &[u8]) -> String {
    let window = &buffer[..buffer.len().min(1024)];
    memmem::find(window, b"%PDF-")
        .and_then(|pos| parse_version(&buffer[pos..]))
        .unwrap_or_else(|| "1.4".to_string())
}

/// Check that `root` resolves through the recovered table to a dictionary
/// that looks like a catalog. The document moves through the `Reader` and
/// back so the caller can keep it without cloning the reference table.
fn root_resolves_to_catalog(buffer: &[u8], document: Document, root: ObjectId) -> (Document, bool) {
    let reader = Reader {
        buffer,
        document,
        encryption_state: None,
        raw_objects: BTreeMap::new(),
        password: None,
        strict: false,
    };
    let is_catalog = reader
        .get_object(root, &mut HashSet::new())
        .ok()
        .and_then(|object| {
            object
                .as_dict()
                .map(|dict| dict.has_type(b"Catalog") || dict.has(b"Pages"))
                .ok()
        })
        .unwrap_or(false);
    (reader.document, is_catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned_ids(buffer: &[u8]) -> Vec<u32> {
        scan_object_headers(buffer).into_keys().collect()
    }

    #[test]
    fn header_scan_finds_headers_across_whitespace_styles() {
        let headers = scan_object_headers(
            b"%PDF-1.4\n1 0 obj\n<< >>\nendobj\r\n2 0 obj<< >>endobj\n3\t0\tobj\n[1 2]\nendobj\n",
        );
        assert_eq!(headers.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(headers[&1].offset, 9);
        assert_eq!(headers[&1].generation, 0);
    }

    #[test]
    fn header_scan_rejects_lookalikes() {
        // Digits glued to a name are not an object number.
        assert!(scanned_ids(b"/F12 0 obj << >>").is_empty());
        // `obj` followed by a delimiter that cannot start an object is
        // string/stream payload, not a header.
        assert!(scanned_ids(b"(see 1 0 obj)").is_empty());
        // Generation beyond u16 is implausible.
        assert!(scanned_ids(b" 1 99999999 obj << >>").is_empty());
        // Object number 0 is always free.
        assert!(scanned_ids(b" 0 0 obj << >>").is_empty());
        // Missing whitespace between the tokens is not a header.
        assert!(scanned_ids(b" 10obj << >>").is_empty());
    }

    #[test]
    fn header_scan_later_duplicate_wins() {
        let buffer = b" 5 0 obj\n<< /A 1 >>\nendobj\n 5 0 obj\n<< /A 2 >>\nendobj\n";
        let headers = scan_object_headers(buffer);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[&5].offset, 28);
    }

    /// A three-object document whose xref/trailer bytes are replaced by
    /// `tail`. Offsets shift with the length of `prefix`, which the scan must
    /// not care about.
    fn damaged_pdf(prefix: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut pdf = prefix.to_vec();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        pdf.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>\nendobj\n",
        );
        pdf.extend_from_slice(tail);
        pdf
    }

    #[test]
    fn reconstructs_without_any_trailer() {
        let pdf = damaged_pdf(b"", b"xref\ngarbage that is not a table\n%%EOF\n");
        let document = reconstruct_document(&pdf).expect("catalog fallback should recover");
        assert_eq!(
            document
                .trailer
                .get(b"Root")
                .unwrap()
                .as_reference()
                .unwrap(),
            (1, 0)
        );
        assert_eq!(document.trailer.get(b"Size").unwrap().as_i64().unwrap(), 4);
        for id in 1..=3u32 {
            assert!(
                matches!(
                    document.reference_table.get(id),
                    Some(XrefEntry::Normal { .. })
                ),
                "object {id} missing from the recovered table"
            );
        }
    }

    #[test]
    fn reconstructs_with_junk_before_header() {
        // Mail-mangled files carry junk before `%PDF-`, shifting every stored
        // offset; recovered offsets come from real positions, so the page
        // walk must still resolve.
        let pdf = damaged_pdf(b"From mail-gateway garbage line\n", b"%%EOF\n");
        let document = reconstruct_document(&pdf).expect("junk prefix should not matter");
        let Some(XrefEntry::Normal { offset, .. }) = document.reference_table.get(1) else {
            panic!("object 1 missing");
        };
        assert_eq!(&pdf[*offset as usize..*offset as usize + 7], b"1 0 obj");
    }

    #[test]
    fn trailer_root_wins_over_catalog_fallback() {
        // A parseable trailer names the root even when its /Size is junk and
        // startxref points nowhere.
        let pdf = damaged_pdf(
            b"",
            b"trailer\n<< /Root 1 0 R /Size -7 >>\nstartxref\n999999\n%%EOF\n",
        );
        let document = reconstruct_document(&pdf).expect("trailer should recover");
        assert_eq!(
            document
                .trailer
                .get(b"Root")
                .unwrap()
                .as_reference()
                .unwrap(),
            (1, 0)
        );
    }

    #[test]
    fn refuses_encrypted_and_unrecoverable_buffers() {
        // Encrypted: the lazy reader cannot decrypt, so repair must refuse.
        let pdf = damaged_pdf(b"", b"trailer\n<< /Root 1 0 R /Encrypt 9 0 R >>\n%%EOF\n");
        assert!(reconstruct_document(&pdf).is_none());
        // No object headers at all.
        assert!(reconstruct_document(b"not a pdf, no objects here").is_none());
        // Headers but no catalog and no trailer: nothing to trust as root.
        assert!(
            reconstruct_document(b"%PDF-1.4\n1 0 obj\n<< /Type /Font >>\nendobj\n%%EOF\n")
                .is_none()
        );
    }

    #[test]
    fn expands_object_stream_members() {
        // Catalog (5) and pages (6) live inside an uncompressed object
        // stream; page 7 is top-level. The trailer is destroyed, so the root
        // must be found through the recovered compressed entries.
        let members =
            b"<< /Type /Catalog /Pages 6 0 R >> << /Type /Pages /Kids [7 0 R] /Count 1 >>";
        let pairs = "5 0 6 34 ";
        let first = pairs.len();
        let content = format!("{pairs}{}", String::from_utf8_lossy(members));
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N 2 /First {first} /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
                content.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(
            b"7 0 obj\n<< /Type /Page /Parent 6 0 R /MediaBox [0 0 10 10] >>\nendobj\n",
        );
        pdf.extend_from_slice(b"garbage instead of an xref stream\n%%EOF\n");

        let document = reconstruct_document(&pdf).expect("object stream members should recover");
        assert_eq!(
            document
                .trailer
                .get(b"Root")
                .unwrap()
                .as_reference()
                .unwrap(),
            (5, 0)
        );
        assert!(matches!(
            document.reference_table.get(5),
            Some(XrefEntry::Compressed {
                container: 4,
                index: 0
            })
        ));
        assert!(matches!(
            document.reference_table.get(6),
            Some(XrefEntry::Compressed {
                container: 4,
                index: 1
            })
        ));
        // Top-level headers always beat object-stream membership.
        assert!(matches!(
            document.reference_table.get(7),
            Some(XrefEntry::Normal { .. })
        ));
    }

    #[test]
    fn top_level_header_beats_object_stream_membership() {
        // Object 6 is both a member of the stream and (later) a top-level
        // object, as an incremental update would leave it.
        let members = b"<< /Type /Catalog /Pages 6 0 R >> << /Ignored true >>";
        let content = format!("5 0 6 34 {}", String::from_utf8_lossy(members));
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N 2 /First 9 /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
                content.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(
            b"6 0 obj\n<< /Type /Pages /Kids [7 0 R] /Count 1 >>\nendobj\n\
              7 0 obj\n<< /Type /Page /Parent 6 0 R /MediaBox [0 0 10 10] >>\nendobj\n%%EOF\n",
        );

        let document = reconstruct_document(&pdf).expect("should recover");
        assert!(matches!(
            document.reference_table.get(6),
            Some(XrefEntry::Normal { .. })
        ));
    }

    #[test]
    fn skips_object_streams_whose_length_lies() {
        // /Length claims 5 bytes, so the runtime reader (which trusts
        // /Length) could never decode this container. Its members must NOT
        // be registered — a missing object degrades to null during copies,
        // while a registered-but-unreadable one would be a hard error.
        let content = "5 0 6 34 << /Ignored true >> << /Ignored true >>";
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N 2 /First 9 /Length 5 >>\nstream\n{content}\nendstream\nendobj\n"
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
              2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n%%EOF\n",
        );

        let document = reconstruct_document(&pdf).expect("top-level objects still recover");
        assert!(
            document.reference_table.get(5).is_none(),
            "members of an unreadable container must not be registered"
        );
        assert!(matches!(
            document.reference_table.get(4),
            Some(XrefEntry::Normal { .. })
        ));
        assert_eq!(
            document
                .trailer
                .get(b"Root")
                .unwrap()
                .as_reference()
                .unwrap(),
            (1, 0)
        );
    }
}
