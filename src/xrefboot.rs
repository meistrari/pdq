//! Xref-only bootstrap for [`crate::lazy::LazyPdf`].
//!
//! `LazyPdf` only needs the cross-reference table and the trailer dictionary:
//! objects are parsed lazily from the mmapped buffer on demand. lopdf's
//! `load_mem_with_options` — even with a drop-everything filter — still
//! nom-parses every object and inflates every object stream before discarding
//! the results, which dominates `page-count`/`split-pages` startup on large
//! documents. This module walks the xref chain directly (classic tables, xref
//! streams, `/Prev` chains and hybrid `/XRefStm`) and builds a `Document` with
//! only `reference_table` and `trailer` populated.
//!
//! This is strictly a fast path: on ANY anomaly (missing `startxref`,
//! malformed table, indirect `/Length`, entry-count mismatch, …) it returns
//! `None` and the caller falls back to the full lopdf parse, so it can never
//! turn a loadable file into an error or vice versa in a way the slow path
//! would not. Encrypted files (trailer `/Encrypt`) are also punted to the slow
//! path so rejection semantics — including owner-password-only documents —
//! stay identical to today's behavior.

use std::collections::{BTreeMap, HashSet};

use lopdf::{
    xref::{Xref, XrefEntry, XrefType},
    Dictionary, Document, Object, Stream, StringFormat,
};

/// Upper bound on incremental-update sections we are willing to follow.
const MAX_CHAIN_SECTIONS: usize = 1024;
/// Maximum nesting depth for trailer-dictionary values.
const MAX_OBJECT_DEPTH: usize = 64;

/// Build a `Document` whose `reference_table` and `trailer` match what a full
/// lopdf load would produce, by parsing only the xref chain. Returns `None` on
/// any anomaly; the caller must then fall back to the full parse.
pub(crate) fn bootstrap_document(buffer: &[u8]) -> Option<Document> {
    // Xref offsets are relative to the `%PDF-` header. The `Reader` that
    // `LazyPdf` constructs resolves offsets against the full buffer, so only
    // fast-path the (overwhelmingly common) header-at-offset-zero layout.
    let version = parse_version(buffer)?;
    let xref_start = find_startxref(buffer)?;

    // Collect the sections of the xref chain in newest-first order.
    let mut sections: Vec<Section> = Vec::new();
    let mut visited = HashSet::new();
    let mut next_offset = Some(xref_start);

    while let Some(offset) = next_offset {
        if sections.len() >= MAX_CHAIN_SECTIONS || !visited.insert(offset) {
            return None;
        }
        let section = parse_section(buffer, offset)?;
        next_offset = optional_offset(&section.trailer, b"Prev")?;

        // Hybrid-reference file: a classic trailer can point at an xref
        // stream carrying the entries for object-stream-compressed objects.
        let hybrid = match optional_offset(&section.trailer, b"XRefStm")? {
            Some(stm_offset) => {
                if !visited.insert(stm_offset) {
                    return None;
                }
                Some(parse_section(buffer, stm_offset)?)
            }
            None => None,
        };
        sections.push(section);
        if let Some(hybrid) = hybrid {
            sections.push(hybrid);
        }
    }

    let kind = sections.first()?.kind;
    let mut trailer = std::mem::replace(&mut sections[0].trailer, Dictionary::new());

    // Assemble the reference table. The overwhelmingly common shape — a
    // single section whose ids are strictly ascending — bulk-builds the
    // `BTreeMap` from the already-sorted, duplicate-free vector, which is
    // several times faster than per-entry inserts on large documents.
    let entries: BTreeMap<u32, XrefEntry> = if sections.len() == 1 && sections[0].ascending {
        sections.pop()?.entries.into_iter().collect()
    } else {
        // Newest section first: the first insertion of an id wins, matching
        // lopdf's incremental-update merge semantics (`Xref::merge`).
        let mut map = BTreeMap::new();
        for section in sections {
            for (id, entry) in section.entries {
                map.entry(id).or_insert(entry);
            }
        }
        map
    };

    trailer.remove(b"Prev");
    trailer.remove(b"XRefStm");

    // Cheap sanity checks before trusting the fast path: the trailer must
    // name a catalog and declare a plausible size, and entries must exist.
    trailer.get(b"Root").ok()?.as_reference().ok()?;
    let declared_size = trailer.get(b"Size").ok()?.as_i64().ok()?;
    if declared_size <= 0 || entries.is_empty() {
        return None;
    }
    if trailer.has(b"Encrypt") {
        return None;
    }

    // Mirror `Reader::read`: the authoritative size is highest id + 1.
    let max_id = *entries.keys().next_back()?;
    let mut xref = Xref::new(max_id.checked_add(1)?, kind);
    xref.entries = entries;

    let mut document = Document::new();
    document.version = version;
    document.max_id = max_id;
    document.xref_start = xref_start;
    document.reference_table = xref;
    document.trailer = trailer;
    Some(document)
}

fn parse_version(buffer: &[u8]) -> Option<String> {
    let rest = buffer.strip_prefix(b"%PDF-")?;
    let len = rest
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b'.')
        .count();
    if len == 0 {
        return None;
    }
    String::from_utf8(rest[..len].to_vec()).ok()
}

/// Locate the newest `startxref` offset with the same discovery rules as
/// lopdf's `Reader::get_xref_start`: last `%%EOF` within the final 512 bytes,
/// then the last `startxref` starting no more than 25 bytes before it.
fn find_startxref(buffer: &[u8]) -> Option<usize> {
    let seek_pos = buffer.len().saturating_sub(512);
    let eof_pos = search_last(buffer, b"%%EOF", seek_pos)?;
    if eof_pos <= 25 {
        return None;
    }
    let keyword_pos = search_last(&buffer[..eof_pos], b"startxref", eof_pos - 25)?;
    let mut lexer = Lexer {
        buffer,
        pos: keyword_pos + b"startxref".len(),
    };
    lexer.skip_whitespace();
    let offset = lexer.parse_unsigned::<u64>()?;
    let offset = usize::try_from(offset).ok()?;
    (offset < buffer.len()).then_some(offset)
}

fn search_last(buffer: &[u8], pattern: &[u8], start: usize) -> Option<usize> {
    buffer
        .get(start..)?
        .windows(pattern.len())
        .rposition(|window| window == pattern)
        .map(|pos| start + pos)
}

/// Read an optional file-offset entry (`/Prev`, `/XRefStm`). An absent key is
/// fine (`Some(None)`); a present-but-malformed value aborts the fast path.
fn optional_offset(trailer: &Dictionary, key: &[u8]) -> Option<Option<usize>> {
    match trailer.get(key) {
        Err(_) => Some(None),
        Ok(object) => {
            let offset = object.as_i64().ok()?;
            Some(Some(usize::try_from(offset).ok()?))
        }
    }
}

/// One section of the xref chain, with entries kept in file order so the
/// caller can bulk-build or merge them as appropriate.
struct Section {
    entries: Vec<(u32, XrefEntry)>,
    /// True when ids were pushed in strictly ascending order (hence unique),
    /// making an order-agnostic bulk `BTreeMap` build unambiguous.
    ascending: bool,
    trailer: Dictionary,
    kind: XrefType,
}

/// Entry collector that tracks whether ids stay strictly ascending.
struct EntrySink {
    entries: Vec<(u32, XrefEntry)>,
    ascending: bool,
}

impl EntrySink {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            ascending: true,
        }
    }

    fn push(&mut self, id: u32, entry: XrefEntry) {
        if let Some((last, _)) = self.entries.last() {
            if *last >= id {
                self.ascending = false;
            }
        }
        self.entries.push((id, entry));
    }
}

fn parse_section(buffer: &[u8], offset: usize) -> Option<Section> {
    if offset >= buffer.len() {
        return None;
    }
    let mut lexer = Lexer {
        buffer,
        pos: offset,
    };
    lexer.skip_whitespace();
    if lexer.try_keyword(b"xref") {
        parse_classic_section(lexer)
    } else {
        parse_stream_section(lexer)
    }
}

/// Parse a classic `xref` table (subsections of 20-byte entries) followed by
/// its `trailer` dictionary.
fn parse_classic_section(mut lexer: Lexer) -> Option<Section> {
    let mut sink = EntrySink::new();
    loop {
        lexer.skip_whitespace();
        if lexer.try_keyword(b"trailer") {
            break;
        }
        let start = lexer.parse_unsigned::<u32>()?;
        lexer.skip_whitespace();
        let count = usize::try_from(lexer.parse_unsigned::<u32>()?).ok()?;
        // A conforming entry is 20 bytes; a subsection that cannot possibly
        // fit its declared entry count is malformed. This also bounds the
        // reserve below by the buffer length.
        if count.checked_mul(18)? > lexer.remaining() {
            return None;
        }
        sink.entries.reserve(count);
        for index in 0..count {
            lexer.skip_whitespace();
            let entry_offset = lexer.parse_unsigned::<u64>()?;
            lexer.skip_whitespace();
            let generation = lexer.parse_unsigned::<u32>()?;
            lexer.skip_whitespace();
            let id = start.checked_add(u32::try_from(index).ok()?)?;
            match lexer.next_byte()? {
                b'n' => {
                    let entry_offset = u32::try_from(entry_offset).ok()?;
                    // Match lopdf: normal entries whose generation overflows
                    // u16 are dropped rather than failing the parse.
                    if let Ok(generation) = u16::try_from(generation) {
                        sink.push(
                            id,
                            XrefEntry::Normal {
                                offset: entry_offset,
                                generation,
                            },
                        );
                    }
                }
                // Free entries are never materialized, matching lopdf's
                // classic-table parser (so they cannot shadow older entries).
                b'f' => {}
                _ => return None,
            }
        }
    }
    let Object::Dictionary(trailer) = lexer.parse_object(0)? else {
        return None;
    };
    Some(Section {
        entries: sink.entries,
        ascending: sink.ascending,
        trailer,
        kind: XrefType::CrossReferenceTable,
    })
}

/// Parse an xref STREAM: the indirect object header and its stream dictionary
/// via the minimal object parser, the raw content via a direct `/Length`,
/// then decode the `/W`/`/Index` entry rows over `decompressed_content()`
/// (lopdf handles FlateDecode and DecodeParms/PNG predictors).
fn parse_stream_section(mut lexer: Lexer) -> Option<Section> {
    lexer.skip_whitespace();
    lexer.parse_unsigned::<u32>()?; // object number
    lexer.skip_whitespace();
    lexer.parse_unsigned::<u16>()?; // generation
    lexer.skip_whitespace();
    if !lexer.try_keyword(b"obj") {
        return None;
    }
    let Object::Dictionary(dict) = lexer.parse_object(0)? else {
        return None;
    };
    // Only direct `/Length` integers: resolving an indirect length would need
    // the very xref we are still building.
    let length = dict.get(b"Length").ok()?.as_i64().ok()?;
    let length = usize::try_from(length).ok()?;
    lexer.skip_whitespace();
    if !lexer.try_keyword(b"stream") {
        return None;
    }
    lexer.consume_stream_eol()?;
    let raw_content = lexer.take(length)?;
    lexer.skip_whitespace();
    // `endstream` right after `/Length` bytes double-checks the length.
    if !lexer.try_keyword(b"endstream") {
        return None;
    }

    let stream = Stream::new(dict, raw_content.to_vec());
    let content = stream.decompressed_content().ok()?;
    let mut trailer = stream.dict;

    let sink = decode_stream_entries(&content, &trailer)?;

    // Normalize the stream dict into a trailer exactly like lopdf's
    // `decode_xref_stream` + `Stream::decompress` do.
    trailer.remove(b"Length");
    trailer.remove(b"W");
    trailer.remove(b"Index");
    trailer.remove(b"Filter");
    trailer.remove(b"DecodeParms");

    Some(Section {
        entries: sink.entries,
        ascending: sink.ascending,
        trailer,
        kind: XrefType::CrossReferenceStream,
    })
}

/// Decode xref-stream entry rows, mirroring lopdf's `decode_xref_stream`
/// semantics: `/W` must hold at least three non-negative widths, a malformed
/// `/Index` silently falls back to `[0 Size]`, type-0 (free) rows are
/// skipped, and unknown row types are ignored.
fn decode_stream_entries(content: &[u8], dict: &Dictionary) -> Option<EntrySink> {
    let size = dict.get(b"Size").ok()?.as_i64().ok()?;
    let widths = integer_array(dict.get(b"W").ok()?)?;
    if widths.len() < 3 {
        return None;
    }
    let mut field_widths = [0usize; 3];
    for (slot, width) in field_widths.iter_mut().zip(&widths) {
        // Cap widths at 8 bytes so field values fit u64; wider fields would
        // wrap in lopdf and are pathological anyway — fall back instead.
        *slot = usize::try_from(*width).ok().filter(|width| *width <= 8)?;
    }
    let row_width = field_widths.iter().sum::<usize>();
    if row_width == 0 {
        return None;
    }

    let index = dict
        .get(b"Index")
        .ok()
        .and_then(integer_array)
        .unwrap_or_else(|| vec![0, size]);

    let mut sink = EntrySink::new();
    let mut pos = 0usize;
    for range in index.chunks_exact(2) {
        let start = u32::try_from(range[0]).ok()?;
        let count = usize::try_from(range[1]).ok()?;
        // Every declared row must actually be present in the content.
        if content.len().checked_sub(pos)? / row_width < count {
            return None;
        }
        sink.entries.reserve(count);
        for row in 0..count {
            let id = start.checked_add(u32::try_from(row).ok()?)?;
            let row_type = if field_widths[0] == 0 {
                1
            } else {
                read_big_endian(&content[pos..pos + field_widths[0]])
            };
            let second = read_big_endian(
                &content[pos + field_widths[0]..pos + field_widths[0] + field_widths[1]],
            );
            let third =
                read_big_endian(&content[pos + row_width - field_widths[2]..pos + row_width]);
            pos += row_width;
            match row_type {
                // Free rows are never materialized (match lopdf).
                0 => {}
                1 => sink.push(
                    id,
                    XrefEntry::Normal {
                        offset: u32::try_from(second).ok()?,
                        generation: third as u16,
                    },
                ),
                2 => sink.push(
                    id,
                    XrefEntry::Compressed {
                        container: u32::try_from(second).ok()?,
                        index: third as u16,
                    },
                ),
                // Unknown row types are ignored (match lopdf).
                _ => {}
            }
        }
    }
    Some(sink)
}

fn integer_array(object: &Object) -> Option<Vec<i64>> {
    let array = object.as_array().ok()?;
    array.iter().map(|item| item.as_i64().ok()).collect()
}

fn read_big_endian(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |value, &byte| (value << 8) | u64::from(byte))
}

/// Minimal hand-rolled tokenizer/object parser: just enough of the PDF object
/// grammar for trailer dictionaries and xref-stream headers (names, numbers,
/// booleans, null, references, arrays, dictionaries, hex/literal strings).
struct Lexer<'a> {
    buffer: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.buffer.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.buffer.get(self.pos + ahead).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn remaining(&self) -> usize {
        self.buffer.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        if end > self.buffer.len() {
            return None;
        }
        let slice = &self.buffer[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) {
                self.pos += 1;
            } else if byte == b'%' {
                // Comments run to the end of the line.
                while let Some(byte) = self.peek() {
                    if byte == b'\r' || byte == b'\n' {
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// Consume `keyword` if it is present at the cursor and ends at a token
    /// boundary (whitespace, delimiter, or end of input).
    fn try_keyword(&mut self, keyword: &[u8]) -> bool {
        let end = self.pos + keyword.len();
        if self.buffer.len() < end || &self.buffer[self.pos..end] != keyword {
            return false;
        }
        match self.buffer.get(end) {
            Some(&byte) if !is_whitespace(byte) && !is_delimiter(byte) => false,
            _ => {
                self.pos = end;
                true
            }
        }
    }

    fn parse_unsigned<T: TryFrom<u64>>(&mut self) -> Option<T> {
        let start = self.pos;
        let mut value: u64 = 0;
        while let Some(byte) = self.peek() {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        T::try_from(value).ok()
    }

    /// The EOL that separates the `stream` keyword from the stream data:
    /// CRLF or LF per spec, plus lone CR for robustness.
    fn consume_stream_eol(&mut self) -> Option<()> {
        match self.next_byte()? {
            b'\n' => Some(()),
            b'\r' => {
                if self.peek() == Some(b'\n') {
                    self.pos += 1;
                }
                Some(())
            }
            _ => None,
        }
    }

    fn parse_object(&mut self, depth: usize) -> Option<Object> {
        if depth > MAX_OBJECT_DEPTH {
            return None;
        }
        self.skip_whitespace();
        match self.peek()? {
            b'<' => {
                if self.peek_at(1) == Some(b'<') {
                    self.parse_dictionary(depth).map(Object::Dictionary)
                } else {
                    self.parse_hex_string()
                }
            }
            b'/' => self.parse_name().map(Object::Name),
            b'(' => self.parse_literal_string(),
            b'[' => self.parse_array(depth),
            b't' | b'f' | b'n' => self.parse_keyword_object(),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.parse_number_or_reference(),
            _ => None,
        }
    }

    fn parse_dictionary(&mut self, depth: usize) -> Option<Dictionary> {
        self.take(2)?; // <<
        let mut dict = Dictionary::new();
        loop {
            self.skip_whitespace();
            match self.peek()? {
                b'>' => {
                    if self.peek_at(1) == Some(b'>') {
                        self.take(2)?;
                        return Some(dict);
                    }
                    return None;
                }
                b'/' => {
                    let key = self.parse_name()?;
                    let value = self.parse_object(depth + 1)?;
                    dict.set(key, value);
                }
                _ => return None,
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Option<Object> {
        self.next_byte()?; // [
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek()? == b']' {
                self.pos += 1;
                return Some(Object::Array(items));
            }
            items.push(self.parse_object(depth + 1)?);
        }
    }

    fn parse_name(&mut self) -> Option<Vec<u8>> {
        self.next_byte()?; // '/'
        let mut name = Vec::new();
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) || is_delimiter(byte) {
                break;
            }
            self.pos += 1;
            if byte == b'#' {
                let high = hex_digit(self.next_byte()?)?;
                let low = hex_digit(self.next_byte()?)?;
                name.push(high * 16 + low);
            } else {
                name.push(byte);
            }
        }
        Some(name)
    }

    fn parse_hex_string(&mut self) -> Option<Object> {
        self.next_byte()?; // '<'
        let mut bytes = Vec::new();
        let mut pending: Option<u8> = None;
        loop {
            let byte = self.next_byte()?;
            if byte == b'>' {
                break;
            }
            if is_whitespace(byte) {
                continue;
            }
            let digit = hex_digit(byte)?;
            match pending.take() {
                Some(high) => bytes.push(high * 16 + digit),
                None => pending = Some(digit),
            }
        }
        if let Some(high) = pending {
            // Odd digit count: the final digit is the high nibble.
            bytes.push(high * 16);
        }
        Some(Object::String(bytes, StringFormat::Hexadecimal))
    }

    fn parse_literal_string(&mut self) -> Option<Object> {
        self.next_byte()?; // '('
        let mut bytes = Vec::new();
        let mut depth = 1usize;
        loop {
            let byte = self.next_byte()?;
            match byte {
                b'(' => {
                    depth += 1;
                    bytes.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    bytes.push(byte);
                }
                b'\\' => match self.next_byte()? {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'b' => bytes.push(0x08),
                    b'f' => bytes.push(0x0c),
                    escaped @ (b'(' | b')' | b'\\') => bytes.push(escaped),
                    // Escaped EOL is a line continuation: emit nothing.
                    b'\r' => {
                        if self.peek() == Some(b'\n') {
                            self.pos += 1;
                        }
                    }
                    b'\n' => {}
                    digit @ b'0'..=b'7' => {
                        let mut value = u16::from(digit - b'0');
                        for _ in 0..2 {
                            match self.peek() {
                                Some(byte @ b'0'..=b'7') => {
                                    value = value * 8 + u16::from(byte - b'0');
                                    self.pos += 1;
                                }
                                _ => break,
                            }
                        }
                        bytes.push(value as u8);
                    }
                    other => bytes.push(other),
                },
                _ => bytes.push(byte),
            }
        }
        Some(Object::String(bytes, StringFormat::Literal))
    }

    fn parse_keyword_object(&mut self) -> Option<Object> {
        if self.try_keyword(b"true") {
            return Some(Object::Boolean(true));
        }
        if self.try_keyword(b"false") {
            return Some(Object::Boolean(false));
        }
        if self.try_keyword(b"null") {
            return Some(Object::Null);
        }
        None
    }

    fn parse_number_or_reference(&mut self) -> Option<Object> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let mut is_real = false;
        while let Some(byte) = self.peek() {
            match byte {
                b'0'..=b'9' => self.pos += 1,
                b'.' => {
                    is_real = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let token = &self.buffer[start..self.pos];
        if token.is_empty() || matches!(token, b"+" | b"-" | b".") {
            return None;
        }
        let text = std::str::from_utf8(token).ok()?;
        if is_real {
            return Some(Object::Real(text.parse().ok()?));
        }
        let value: i64 = text.parse().ok()?;

        // `N G R` lookahead: a reference is two non-negative integers
        // followed by a lone `R`; rewind on any mismatch.
        if let Ok(id) = u32::try_from(value) {
            let saved = self.pos;
            if let Some(reference) = self.try_reference_tail(id) {
                return Some(Object::Reference(reference));
            }
            self.pos = saved;
        }
        Some(Object::Integer(value))
    }

    fn try_reference_tail(&mut self, id: u32) -> Option<(u32, u16)> {
        self.skip_whitespace();
        if !self.peek()?.is_ascii_digit() {
            return None;
        }
        let generation = self.parse_unsigned::<u16>()?;
        self.skip_whitespace();
        self.try_keyword(b"R").then_some((id, generation))
    }
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{LoadOptions, Object, ObjectId};

    fn drop_object(_: ObjectId, _: &mut Object) -> Option<(ObjectId, Object)> {
        None
    }

    fn classic_pdf() -> (Vec<u8>, usize) {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let xref_offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n0 3\n0000000000 65535 f \n0000000015 00000 n \n0000000100 00007 n \n\
              trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n",
        );
        pdf.extend_from_slice(xref_offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        (pdf, xref_offset)
    }

    #[test]
    fn classic_table_bootstraps() {
        let (pdf, _) = classic_pdf();
        let document = bootstrap_document(&pdf).expect("classic table should bootstrap");
        assert_eq!(document.version, "1.4");
        assert_eq!(document.reference_table.size, 3);
        assert!(
            document.reference_table.get(0).is_none(),
            "free entry materialized"
        );
        assert!(matches!(
            document.reference_table.get(1),
            Some(XrefEntry::Normal {
                offset: 15,
                generation: 0
            })
        ));
        assert!(matches!(
            document.reference_table.get(2),
            Some(XrefEntry::Normal {
                offset: 100,
                generation: 7
            })
        ));
        let root = document
            .trailer
            .get(b"Root")
            .unwrap()
            .as_reference()
            .unwrap();
        assert_eq!(root, (1, 0));
    }

    #[test]
    fn xref_stream_bootstraps() {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        let obj_offset = pdf.len();
        pdf.extend_from_slice(
            b"7 0 obj\n<< /Type /XRef /Size 3 /W [1 2 1] /Index [1 2] /Root 1 0 R /Length 8 >>\nstream\n",
        );
        // Two entries, W = [1 2 1]:
        //   id 1: type 1 (normal), offset 15, generation 0
        //   id 2: type 2 (compressed), container 5, index 3
        pdf.extend_from_slice(&[1, 0, 15, 0, 2, 0, 5, 3]);
        pdf.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
        pdf.extend_from_slice(obj_offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");

        let document = bootstrap_document(&pdf).expect("xref stream should bootstrap");
        assert!(matches!(
            document.reference_table.get(1),
            Some(XrefEntry::Normal {
                offset: 15,
                generation: 0
            })
        ));
        assert!(matches!(
            document.reference_table.get(2),
            Some(XrefEntry::Compressed {
                container: 5,
                index: 3
            })
        ));
        assert_eq!(document.reference_table.size, 3);
        assert!(
            !document.trailer.has(b"W") && !document.trailer.has(b"Length"),
            "stream bookkeeping keys must not leak into the trailer"
        );
    }

    #[test]
    fn prev_chain_newest_wins() {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let old_offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 2\n0000000500 00000 n \n0000000600 00000 n \n\
              trailer\n<< /Size 3 /Root 1 0 R >>\n",
        );
        let new_offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 1\n0000000015 00000 n \ntrailer\n<< /Size 3 /Root 1 0 R /Prev ",
        );
        pdf.extend_from_slice(old_offset.to_string().as_bytes());
        pdf.extend_from_slice(b" >>\nstartxref\n");
        pdf.extend_from_slice(new_offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");

        let document = bootstrap_document(&pdf).expect("prev chain should bootstrap");
        // id 1 exists in both sections: the newest one (offset 15) must win.
        assert!(matches!(
            document.reference_table.get(1),
            Some(XrefEntry::Normal {
                offset: 15,
                generation: 0
            })
        ));
        // id 2 only exists in the older section and must be merged in.
        assert!(matches!(
            document.reference_table.get(2),
            Some(XrefEntry::Normal {
                offset: 600,
                generation: 0
            })
        ));
        assert!(
            !document.trailer.has(b"Prev"),
            "Prev must not leak into the trailer"
        );
    }

    #[test]
    fn hybrid_xrefstm_entries_are_merged() {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        let stm_offset = pdf.len();
        pdf.extend_from_slice(
            b"9 0 obj\n<< /Type /XRef /Size 3 /W [1 2 1] /Index [2 1] /Root 1 0 R /Length 4 >>\nstream\n",
        );
        pdf.extend_from_slice(&[2, 0, 5, 3]); // id 2: compressed, container 5, index 3
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let table_offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 1\n0000000015 00000 n \ntrailer\n<< /Size 3 /Root 1 0 R /XRefStm ",
        );
        pdf.extend_from_slice(stm_offset.to_string().as_bytes());
        pdf.extend_from_slice(b" >>\nstartxref\n");
        pdf.extend_from_slice(table_offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");

        let document = bootstrap_document(&pdf).expect("hybrid file should bootstrap");
        assert!(matches!(
            document.reference_table.get(1),
            Some(XrefEntry::Normal {
                offset: 15,
                generation: 0
            })
        ));
        assert!(matches!(
            document.reference_table.get(2),
            Some(XrefEntry::Compressed {
                container: 5,
                index: 3
            })
        ));
        assert!(!document.trailer.has(b"XRefStm"));
    }

    #[test]
    fn malformed_inputs_fall_back() {
        // Not a PDF header.
        assert!(bootstrap_document(b"not a pdf at all").is_none());

        // No startxref.
        assert!(bootstrap_document(b"%PDF-1.4\nxref\n0 0\ntrailer\n<< >>\n%%EOF\n").is_none());

        // startxref beyond EOF.
        assert!(bootstrap_document(b"%PDF-1.4\nstartxref\n99999\n%%EOF\n").is_none());

        // Declared entry count larger than the actual table.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n0 5\n0000000000 65535 f \n0000000015 00000 n \n\
              trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n",
        );
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "entry count mismatch must fall back"
        );

        // Xref stream whose rows overrun the stream content.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"7 0 obj\n<< /Type /XRef /Size 9 /W [1 2 1] /Index [0 9] /Root 1 0 R /Length 8 >>\nstream\n",
        );
        pdf.extend_from_slice(&[1, 0, 15, 0, 2, 0, 5, 3]);
        pdf.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "row overrun must fall back"
        );

        // Indirect /Length on an xref stream.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.5\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"7 0 obj\n<< /Type /XRef /Size 3 /W [1 2 1] /Root 1 0 R /Length 8 0 R >>\nstream\n",
        );
        pdf.extend_from_slice(&[1, 0, 15, 0, 2, 0, 5, 3]);
        pdf.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "indirect /Length must fall back"
        );

        // Trailer without /Root.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 1\n0000000015 00000 n \ntrailer\n<< /Size 2 >>\nstartxref\n",
        );
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "missing /Root must fall back"
        );

        // Encrypted trailer punts to the slow path.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 1\n0000000015 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Encrypt 5 0 R >>\nstartxref\n",
        );
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "encrypted files must fall back"
        );

        // /Prev cycle.
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let offset = pdf.len();
        pdf.extend_from_slice(
            b"xref\n1 1\n0000000015 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R /Prev ",
        );
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b" >>\nstartxref\n");
        pdf.extend_from_slice(offset.to_string().as_bytes());
        pdf.extend_from_slice(b"\n%%EOF\n");
        assert!(
            bootstrap_document(&pdf).is_none(),
            "Prev cycle must fall back"
        );
    }

    #[test]
    fn matches_full_lopdf_parse_on_fixtures() {
        for fixture in ["11-pages.pdf", "11-pages-objstm.pdf"] {
            let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
            let bytes = std::fs::read(&path).unwrap();
            let fast = bootstrap_document(&bytes)
                .unwrap_or_else(|| panic!("{fixture} should take the fast path"));
            let slow =
                Document::load_mem_with_options(&bytes, LoadOptions::with_filter(drop_object))
                    .unwrap();
            assert_eq!(
                format!("{:?}", fast.reference_table.entries),
                format!("{:?}", slow.reference_table.entries),
                "{fixture}: reference tables diverge"
            );
            assert_eq!(
                fast.reference_table.size, slow.reference_table.size,
                "{fixture}: xref size diverges"
            );
            // Key order may differ (lopdf's Dictionary::remove reorders), so
            // compare the trailers as key -> value maps.
            let as_map = |dict: &Dictionary| -> std::collections::BTreeMap<Vec<u8>, String> {
                dict.iter()
                    .map(|(key, value)| (key.clone(), format!("{value:?}")))
                    .collect()
            };
            assert_eq!(
                as_map(&fast.trailer),
                as_map(&slow.trailer),
                "{fixture}: trailers diverge"
            );
        }
    }

    #[test]
    fn encrypted_fixtures_fall_back() {
        for fixture in ["user-password.pdf", "owner-only.pdf"] {
            let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                bootstrap_document(&bytes).is_none(),
                "{fixture} must be punted to the slow path"
            );
        }
    }
}

/// Manual perf probe: `PDQ_PROBE_FILE=/path/to.pdf cargo test --release \
/// time_bootstrap -- --ignored --nocapture`.
#[cfg(test)]
mod perf_probe {
    #[test]
    #[ignore]
    fn time_bootstrap() {
        let Ok(path) = std::env::var("PDQ_PROBE_FILE") else {
            eprintln!("PDQ_PROBE_FILE not set; skipping");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        for round in 0..5 {
            let start = std::time::Instant::now();
            let doc = super::bootstrap_document(&bytes).unwrap();
            eprintln!(
                "round {round}: {:?} (entries {})",
                start.elapsed(),
                doc.reference_table.entries.len()
            );
        }
    }
}
