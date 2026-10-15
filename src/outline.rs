//! Read the document outline (bookmarks), with each entry's page resolved.
//!
//! This is read-only metadata in the same family as `page-count` and
//! `dimensions`: the page-tree walk plus the outline tree, no content streams.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::OnceLock,
};

use lopdf::{Dictionary, Object, ObjectId, StringFormat};

use crate::{
    copy::ObjectSource, dimensions::dereference, json::escape_json, lazy::PdfSource,
    load::map_file, repair::with_repair_retry, Result,
};

/// Deepest outline nesting and name-tree depth followed. Matches the
/// page-tree depth cap in `lazy::walk_pages_until`.
const MAX_DEPTH: usize = 256;

/// A named destination whose value is another name is followed at most this
/// many times; anything longer is a loop.
const MAX_NAME_HOPS: usize = 8;

/// One outline entry. Entries come flat, in document order, with `depth`
/// recording the nesting, so consecutive entries turn straight into page
/// ranges and the tree is still recoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    /// The entry's title. Undecodable parts become U+FFFD and set
    /// [`OutlineEntry::degraded`]; they are never dropped.
    pub title: String,
    /// The 1-based page the entry points at, ready for a `--pages` or
    /// `--out` range. `None` when it points at no page of this document: a
    /// URI or remote action, an unresolvable name, or a stale destination.
    pub page: Option<usize>,
    /// Nesting depth; top-level entries are 0.
    pub depth: usize,
    /// True when the title could not be fully decoded: missing, not a
    /// string, or holding bytes its encoding does not define. A title is
    /// often the only label a downstream system has for a bundled document,
    /// so a damaged one is flagged rather than passed on as if it were right.
    pub degraded: bool,
}

/// A document's page count and its flattened outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub pages: usize,
    /// Empty when the document has no outline, which is not an error.
    pub entries: Vec<OutlineEntry>,
}

/// Read the outline of `input` with every entry's destination resolved to a
/// 1-based page number.
///
/// Destinations are resolved through `/Dest` and `/GoTo` actions, explicit
/// arrays, named destinations in the name tree (`/Names /Dests`) and the
/// older catalog `/Dests` dictionary. A destination that does not land on a
/// page of this document yields `page: None` instead of failing, so one stale
/// entry never costs the others. Cycles and runaway nesting in the
/// `/First`/`/Next` chains are cut off.
///
/// Encrypted PDFs with an empty user password are decrypted transparently;
/// files that need a real password require [`read_outline_with_password`].
pub fn read_outline(input: &Path) -> Result<Outline> {
    read_outline_with_password(input, None)
}

/// Like [`read_outline`], additionally decrypting encrypted inputs with
/// `password` when the empty user password does not authenticate.
pub fn read_outline_with_password(input: &Path, password: Option<&str>) -> Result<Outline> {
    let mmap = map_file(input)?;
    with_repair_retry(&mmap, input, password, outline_impl)
}

/// Serialize as `{"pages":N,"outline":[{"title":..,"page":N|null,"depth":N,"degraded":bool},..]}`.
pub fn outline_to_json(outline: &Outline) -> String {
    use std::fmt::Write;

    let mut json = format!("{{\"pages\":{},\"outline\":[", outline.pages);
    for (index, entry) in outline.entries.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        let page = entry
            .page
            .map_or_else(|| "null".to_string(), |page| page.to_string());
        write!(
            json,
            "{{\"title\":\"{}\",\"page\":{},\"depth\":{},\"degraded\":{}}}",
            escape_json(&entry.title),
            page,
            entry.depth,
            entry.degraded
        )
        .expect("writing to a String cannot fail");
    }
    json.push_str("]}");
    json
}

fn outline_impl(source: &PdfSource) -> Result<Outline> {
    let page_ids = source.page_ids()?;
    let mut page_numbers = HashMap::with_capacity(page_ids.len());
    for (index, id) in page_ids.iter().enumerate() {
        page_numbers.entry(*id).or_insert(index + 1);
    }
    let entries = match source
        .trailer_value(b"Root")
        .and_then(|root| dictionary(source, &root))
    {
        Some(catalog) => OutlineReader {
            source,
            catalog: &catalog,
            page_numbers: &page_numbers,
            page_count: page_ids.len(),
            named: None,
        }
        .entries(),
        None => Vec::new(),
    };
    Ok(Outline {
        pages: page_ids.len(),
        entries,
    })
}

struct OutlineReader<'s, 'a> {
    source: &'s PdfSource<'a>,
    catalog: &'s Dictionary,
    page_numbers: &'s HashMap<ObjectId, usize>,
    page_count: usize,
    /// Built on the first named destination; most outlines use none.
    named: Option<NamedDestinations>,
}

impl OutlineReader<'_, '_> {
    /// Walk the outline depth-first: an entry, then its children, then its
    /// next sibling. Every indirect entry is visited once, so a `/Next` or
    /// `/First` pointing back up the tree ends that chain instead of looping.
    fn entries(&mut self) -> Vec<OutlineEntry> {
        let mut entries = Vec::new();
        let Ok(root) = self.catalog.get(b"Outlines") else {
            return entries;
        };
        let mut visited = HashSet::new();
        if let Object::Reference(id) = root {
            visited.insert(*id);
        }
        let Some(first) =
            dictionary(self.source, root).and_then(|root| root.get(b"First").ok().cloned())
        else {
            return entries;
        };

        let mut pending = vec![(first, 0usize)];
        while let Some((item, depth)) = pending.pop() {
            if let Object::Reference(id) = item {
                if !visited.insert(id) {
                    continue;
                }
            }
            let Some(item) = dictionary(self.source, &item) else {
                continue;
            };
            let (title, degraded) = self.title(&item);
            let page = self.item_page(&item);
            entries.push(OutlineEntry {
                title,
                page,
                depth,
                degraded,
            });
            // Last in, first out: push the sibling before the children so
            // the whole subtree comes out before the next sibling.
            if let Ok(next) = item.get(b"Next") {
                pending.push((next.clone(), depth));
            }
            if depth + 1 < MAX_DEPTH {
                if let Ok(child) = item.get(b"First") {
                    pending.push((child.clone(), depth + 1));
                }
            }
        }
        entries
    }

    fn title(&self, item: &Dictionary) -> (String, bool) {
        match item
            .get(b"Title")
            .ok()
            .and_then(|title| dereference(self.source, title))
        {
            Some(Object::String(bytes, _)) => decode_text(&bytes),
            _ => (String::new(), true),
        }
    }

    /// The page an entry points at, through `/Dest` or a `/GoTo` action.
    /// Other actions (`/URI`, `/GoToR`, `/Launch`, ...) point at no page of
    /// this document.
    fn item_page(&mut self, item: &Dictionary) -> Option<usize> {
        if let Ok(dest) = item.get(b"Dest") {
            return self.destination_page(dest, 0);
        }
        let action = item
            .get(b"A")
            .ok()
            .and_then(|action| dictionary(self.source, action))?;
        match action
            .get(b"S")
            .ok()
            .and_then(|kind| dereference(self.source, kind))?
        {
            Object::Name(kind) if kind == b"GoTo" => {
                self.destination_page(action.get(b"D").ok()?, 0)
            }
            _ => None,
        }
    }

    fn destination_page(&mut self, dest: &Object, hops: usize) -> Option<usize> {
        if hops > MAX_NAME_HOPS {
            return None;
        }
        match dereference(self.source, dest)? {
            Object::Array(array) => self.explicit_page(&array),
            Object::Name(name) => {
                let target = self.named(&name, NameKind::Name)?;
                self.destination_page(&target, hops + 1)
            }
            Object::String(name, _) => {
                let target = self.named(&name, NameKind::String)?;
                self.destination_page(&target, hops + 1)
            }
            // A named destination's value may be a dictionary whose /D holds
            // the destination.
            Object::Dictionary(dict) => self.destination_page(dict.get(b"D").ok()?, hops + 1),
            _ => None,
        }
    }

    /// `[page /XYZ ...]`, `[page /Fit]` and the other explicit forms: only the
    /// page matters here.
    fn explicit_page(&self, array: &[Object]) -> Option<usize> {
        match array.first()? {
            Object::Reference(id) => self.page_numbers.get(id).copied(),
            // The spec reserves integer pages for remote destinations, but
            // some writers use a 0-based page index in local ones too. Accept
            // it when it names a page of this document, as pdf.js does.
            Object::Integer(index) => usize::try_from(*index)
                .ok()
                .filter(|index| *index < self.page_count)
                .map(|index| index + 1),
            _ => None,
        }
    }

    /// Look a named destination up in the name tree and in the catalog
    /// `/Dests` dictionary. Names (PDF 1.1) belong to `/Dests` and strings
    /// (PDF 1.2+) to the name tree; each falls back to the other because
    /// writers mix them up.
    fn named(&mut self, key: &[u8], kind: NameKind) -> Option<Object> {
        let (source, catalog) = (self.source, self.catalog);
        let named = self
            .named
            .get_or_insert_with(|| NamedDestinations::collect(source, catalog));
        let from_dests = || {
            named
                .dests
                .as_ref()
                .and_then(|dests| dests.get(key).ok())
                .cloned()
        };
        let from_tree = || named.tree.get(key).cloned();
        match kind {
            NameKind::Name => from_dests().or_else(from_tree),
            NameKind::String => from_tree().or_else(from_dests),
        }
    }
}

#[derive(Clone, Copy)]
enum NameKind {
    Name,
    String,
}

struct NamedDestinations {
    /// `/Names /Dests`, flattened. The first entry wins on duplicate keys.
    tree: HashMap<Vec<u8>, Object>,
    /// The catalog `/Dests` dictionary.
    dests: Option<Dictionary>,
}

impl NamedDestinations {
    fn collect(source: &PdfSource, catalog: &Dictionary) -> Self {
        let dests = catalog
            .get(b"Dests")
            .ok()
            .and_then(|dests| dictionary(source, dests));
        let mut tree = HashMap::new();
        if let Some(root) = catalog
            .get(b"Names")
            .ok()
            .and_then(|names| dictionary(source, names))
            .and_then(|names| names.get(b"Dests").ok().cloned())
        {
            collect_name_tree(source, root, &mut tree);
        }
        Self { tree, dests }
    }
}

/// Flatten a name tree into `out`, visiting each node once and stopping at
/// [`MAX_DEPTH`], so a malformed `/Kids` loop cannot hang the walk.
fn collect_name_tree(source: &PdfSource, root: Object, out: &mut HashMap<Vec<u8>, Object>) {
    let mut visited = HashSet::new();
    let mut pending = vec![(root, 0usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        if let Object::Reference(id) = node {
            if !visited.insert(id) {
                continue;
            }
        }
        let Some(node) = dictionary(source, &node) else {
            continue;
        };
        if let Some(Object::Array(pairs)) = node
            .get(b"Names")
            .ok()
            .and_then(|names| dereference(source, names))
        {
            let (pairs, _) = pairs.as_chunks::<2>();
            for [key, value] in pairs {
                if let Some(Object::String(key, _) | Object::Name(key)) = dereference(source, key) {
                    out.entry(key).or_insert_with(|| value.clone());
                }
            }
        }
        if let Some(Object::Array(kids)) = node
            .get(b"Kids")
            .ok()
            .and_then(|kids| dereference(source, kids))
        {
            pending.extend(kids.into_iter().rev().map(|kid| (kid, depth + 1)));
        }
    }
}

fn dictionary(source: &PdfSource, object: &Object) -> Option<Dictionary> {
    match dereference(source, object)? {
        Object::Dictionary(dictionary) => Some(dictionary),
        _ => None,
    }
}

/// Decode a PDF text string: UTF-16BE (or UTF-8) behind its byte-order mark,
/// PDFDocEncoding otherwise. Returns the text and whether any part of it could
/// not be decoded. Undecodable parts become U+FFFD instead of disappearing, so
/// a damaged title is flagged, never silently shortened. Trailing NULs, which
/// many writers append, carry no text and are dropped.
pub(crate) fn decode_text(bytes: &[u8]) -> (String, bool) {
    let (mut text, degraded) = if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        decode_utf16(rest, u16::from_be_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        // Little-endian UTF-16 is not allowed by the spec, but some writers
        // produce it; pdf.js reads it the same way.
        decode_utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        match std::str::from_utf8(rest) {
            Ok(text) => (text.to_owned(), false),
            Err(_) => (String::from_utf8_lossy(rest).into_owned(), true),
        }
    } else {
        // PDFDocEncoding leaves 0x00 undefined, so trailing NUL padding would
        // decode as U+FFFD and flag the title. Strip it before decoding; a NUL
        // inside the title is still flagged.
        let end = bytes
            .iter()
            .rposition(|&byte| byte != 0)
            .map_or(0, |last| last + 1);
        decode_pdf_doc(&bytes[..end])
    };
    let kept = text.trim_end_matches('\0').len();
    text.truncate(kept);
    (text, degraded)
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> (String, bool) {
    let (pairs, odd) = bytes.as_chunks::<2>();
    let mut degraded = !odd.is_empty();
    let mut text = String::with_capacity(bytes.len() / 2);
    for decoded in char::decode_utf16(pairs.iter().map(|pair| unit(*pair))) {
        match decoded {
            Ok(character) => text.push(character),
            Err(_) => {
                text.push(char::REPLACEMENT_CHARACTER);
                degraded = true;
            }
        }
    }
    if !odd.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    (text, degraded)
}

fn decode_pdf_doc(bytes: &[u8]) -> (String, bool) {
    let table = pdf_doc_encoding();
    let mut degraded = false;
    let mut text = String::with_capacity(bytes.len());
    for byte in bytes {
        match table[usize::from(*byte)] {
            Some(character) => text.push(character),
            None => {
                text.push(char::REPLACEMENT_CHARACTER);
                degraded = true;
            }
        }
    }
    (text, degraded)
}

/// PDFDocEncoding, taken byte by byte from lopdf's own table so the two never
/// disagree. lopdf leaves every control code undefined and silently drops
/// undefined bytes; the spec defines tab, line feed and carriage return, so
/// those are kept here.
fn pdf_doc_encoding() -> &'static [Option<char>; 256] {
    static TABLE: OnceLock<[Option<char>; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [None; 256];
        for (byte, slot) in (0..=u8::MAX).zip(table.iter_mut()) {
            let single = Object::String(vec![byte], StringFormat::Literal);
            *slot = lopdf::decode_text_string(&single)
                .ok()
                .and_then(|text| text.chars().next());
        }
        for control in *b"\t\n\r" {
            table[usize::from(control)] = Some(char::from(control));
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_doc_encoding_maps_the_specials_and_flags_undefined_bytes() {
        assert_eq!(decode_text(b"Cap\xedtulo"), ("Capítulo".to_string(), false));
        assert_eq!(
            decode_text(b"text\x8b"),
            ("text\u{2030}".to_string(), false)
        );
        assert_eq!(decode_text(b"a\tb\nc"), ("a\tb\nc".to_string(), false));
        assert_eq!(decode_text(b"bad\x01"), ("bad\u{FFFD}".to_string(), true));
    }

    #[test]
    fn utf16_titles_decode_and_damage_is_flagged() {
        assert_eq!(
            decode_text(b"\xfe\xff\x00C\x00a\x00p\x00\xed\x00t\x00u\x00l\x00o"),
            ("Capítulo".to_string(), false)
        );
        // A lone high surrogate and a dangling odd byte both survive as U+FFFD.
        assert_eq!(
            decode_text(b"\xfe\xff\x00A\xd8\x00\x00B"),
            ("A\u{FFFD}B".to_string(), true)
        );
        assert_eq!(
            decode_text(b"\xfe\xff\x00A\x00"),
            ("A\u{FFFD}".to_string(), true)
        );
        assert_eq!(decode_text(b"\xff\xfeA\x00"), ("A".to_string(), false));
    }

    #[test]
    fn trailing_nuls_are_dropped_and_utf8_is_read() {
        assert_eq!(
            decode_text(b"\xfe\xff\x00A\x00\x00"),
            ("A".to_string(), false)
        );
        assert_eq!(
            decode_text("\u{FEFF}Seção".as_bytes()),
            ("Seção".to_string(), false)
        );
        assert_eq!(
            decode_text(b"\xef\xbb\xbf\xff"),
            ("\u{FFFD}".to_string(), true)
        );
    }

    #[test]
    fn pdf_doc_titles_drop_trailing_nuls_but_flag_inner_ones() {
        assert_eq!(decode_text(b"Cover\x00"), ("Cover".to_string(), false));
        assert_eq!(decode_text(b"Cover\x00\x00"), ("Cover".to_string(), false));
        assert_eq!(
            decode_text(b"Co\x00ver"),
            ("Co\u{FFFD}ver".to_string(), true)
        );
    }

    #[test]
    fn json_keeps_null_pages_and_escapes_titles() {
        let outline = Outline {
            pages: 3,
            entries: vec![
                OutlineEntry {
                    title: "Cover \"A\"".to_string(),
                    page: Some(1),
                    depth: 0,
                    degraded: false,
                },
                OutlineEntry {
                    title: String::new(),
                    page: None,
                    depth: 1,
                    degraded: true,
                },
            ],
        };
        assert_eq!(
            outline_to_json(&outline),
            "{\"pages\":3,\"outline\":[{\"title\":\"Cover \\\"A\\\"\",\"page\":1,\"depth\":0,\"degraded\":false},\
             {\"title\":\"\",\"page\":null,\"depth\":1,\"degraded\":true}]}"
        );
    }
}
