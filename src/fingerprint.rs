//! Content fingerprints of pages (`pdq fingerprint`).
//!
//! A fingerprint is a SHA-256 over a canonical serialization of everything
//! that decides how a page looks, built so the same page copied into another
//! file hashes identically while any visible difference changes the hash.
//! The contract is one-sided on purpose: two pages that merely look alike may
//! hash differently (a false negative), but two pages with equal fingerprints
//! must draw the same thing. Callers use it to drop repeated pages, where a
//! false positive silently deletes content.
//!
//! What is independent of the file a page lives in:
//! - object numbers — references hash as the hash of their target (a Merkle
//!   hash), memoized per object, with cycles encoded as back-references;
//! - resource names — a content operand naming a resource (`/F1 12 Tf`,
//!   `/Im0 Do`) hashes as the resolved resource, so `/F1` and `/Xi236`
//!   pointing at the same font are equal, and only resources the content
//!   actually uses take part;
//! - content-stream layout — streams are tokenized and re-serialized, so
//!   whitespace, comments and how `/Contents` is split do not matter;
//! - Flate compression and predictors — non-content streams hash their
//!   inflated, un-predicted bytes.
//!
//! What never takes part: back-references (`/Parent`, an annotation's `/P`),
//! structure-tree indices, XMP metadata, modification dates, annotation names
//! and link destinations/actions. None of them is drawn.
//!
//! Volatile stamps: some court systems stamp every page of a download with
//! the downloading user and timestamp. A text-showing operand that matches
//! one of the known stamp shapes (see [`is_volatile_text`]) hashes as a fixed
//! placeholder, so two downloads of the same page still match. Changing that
//! list, or anything else in the serialization, changes every fingerprint and
//! must bump [`FINGERPRINT_VERSION`].

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Read},
    path::Path,
};

use lopdf::{
    content::{Content, Operation},
    Dictionary, Object, ObjectId, Stream,
};
use sha2::{Digest, Sha256};

use crate::{
    copy::ObjectSource,
    lazy::PdfSource,
    load::map_file,
    range::{dedupe_preserving_order, PageRangeGroup},
    repair::with_repair_retry,
    scan::{self, ResourceType},
    PdfOpsError, Result,
};

/// Version of the serialization below. Fingerprints are only comparable
/// within one version.
pub const FINGERPRINT_VERSION: &str = "pfp1";

/// Deepest object nesting followed before giving up on the page (matches the
/// page-tree and copy depth caps elsewhere in the crate).
const MAX_DEPTH: usize = 256;
/// Longest `/Parent` chain followed when resolving inherited attributes.
const MAX_CHAIN: usize = 256;

#[derive(Debug, Clone, Default)]
pub struct FingerprintOptions {
    /// Page ranges to fingerprint (same syntax as `render`); all pages when
    /// `None`.
    pub pages: Option<PageRangeGroup>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFingerprint {
    /// 1-based page number.
    pub page: usize,
    pub digest: [u8; 32],
}

impl PageFingerprint {
    pub fn hex(&self) -> String {
        use std::fmt::Write;

        self.digest
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
                hex
            })
    }
}

/// Fingerprint every selected page of `input`.
///
/// Uses the lazy, mmap-backed reader shared with `page-count`/`dimensions`,
/// so the page list matches what `split` resolves. Encrypted inputs with an
/// empty user password are decrypted transparently.
pub fn fingerprint_pages(
    input: &Path,
    options: &FingerprintOptions,
) -> Result<Vec<PageFingerprint>> {
    let mmap = map_file(input)?;
    with_repair_retry(&mmap, input, options.password.as_deref(), |source| {
        fingerprint_impl(source, options)
    })
}

/// Serialize fingerprints as the `pdq fingerprint` stdout JSON.
pub fn fingerprints_to_json(pages: &[PageFingerprint]) -> String {
    use std::fmt::Write;

    let mut json = format!("{{\"version\":\"{FINGERPRINT_VERSION}\",\"pages\":[");
    for (index, page) in pages.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        write!(
            json,
            "{{\"page\":{},\"fingerprint\":\"{}\"}}",
            page.page,
            page.hex()
        )
        .expect("writing to a String cannot fail");
    }
    json.push_str("]}");
    json
}

fn fingerprint_impl(
    source: &PdfSource,
    options: &FingerprintOptions,
) -> Result<Vec<PageFingerprint>> {
    let page_ids = source.page_ids()?;
    let selected = match &options.pages {
        Some(range) => dedupe_preserving_order(&range.resolve(page_ids.len())?),
        None => (1..=page_ids.len()).collect(),
    };
    let mut hasher = PageHasher::new(source, &page_ids);
    selected
        .into_iter()
        .map(|page| {
            Ok(PageFingerprint {
                page,
                digest: hasher.page(page_ids[page - 1])?,
            })
        })
        .collect()
}

// Serialization tags. Every item starts with one tag byte; variable-length
// data is length-prefixed and containers carry their element count, so no
// two different structures can serialize to the same byte sequence.
const TAG_NULL: u8 = b'n';
const TAG_FALSE: u8 = b'f';
const TAG_TRUE: u8 = b't';
const TAG_INTEGER: u8 = b'i';
const TAG_REAL: u8 = b'r';
const TAG_NAME: u8 = b'N';
const TAG_STRING: u8 = b'S';
const TAG_ARRAY: u8 = b'A';
const TAG_DICTIONARY: u8 = b'D';
const TAG_STREAM: u8 = b'T';
const TAG_REFERENCE: u8 = b'R';
const TAG_PAGE_REFERENCE: u8 = b'P';
const TAG_ORPHAN_PAGE: u8 = b'O';
const TAG_BACK_REFERENCE: u8 = b'C';
const TAG_MISSING: u8 = b'X';
const TAG_ABSENT: u8 = b'-';
const TAG_CONTENT: u8 = b'B';
const TAG_CONTENT_RAW: u8 = b'W';
const TAG_OPERATION: u8 = b'Q';
const TAG_RESOURCE: u8 = b'K';
const TAG_UNRESOLVED: u8 = b'U';
const TAG_VOLATILE: u8 = b'V';
const TAG_BYTES_INFLATED: u8 = b'z';
const TAG_BYTES_RAW: u8 = b'w';

/// Dictionary keys that are never drawn and vary between copies of the same
/// page: back-references, structure indices, metadata, edit bookkeeping, and
/// link destinations/actions — a link into the document points at a page
/// NUMBER, which shifts whenever the page sits at another position.
const SKIPPED_KEYS: [&[u8]; 12] = [
    b"Parent",
    b"StructParent",
    b"StructParents",
    b"Metadata",
    b"PieceInfo",
    b"LastModified",
    b"NM",
    b"M",
    b"Dest",
    b"A",
    b"AA",
    b"PA",
];

/// Colour-space operands that name a built-in family rather than a
/// `/ColorSpace` resource (inline-image abbreviations included).
const DEVICE_COLOR_SPACES: [&[u8]; 9] = [
    b"DeviceGray",
    b"DeviceRGB",
    b"DeviceCMYK",
    b"Pattern",
    b"Indexed",
    b"G",
    b"RGB",
    b"CMYK",
    b"I",
];

/// What a hashed subtree depends on beyond its own bytes. It bubbles up to
/// decide memoization and which document-level state the page must include.
#[derive(Debug, Clone, Copy)]
struct Flags {
    /// Smallest stack index a back-reference in the subtree points at;
    /// `usize::MAX` when there is none. A subtree referring above itself
    /// hashes differently depending on the path that reached it.
    shallowest: usize,
    /// Hashed against resources inherited from the caller (a form XObject
    /// without its own `/Resources`), so not reusable elsewhere.
    contextual: bool,
    /// A resource name did not resolve; viewers may fall back to the
    /// AcroForm default resources, so the page includes them.
    unresolved: bool,
    /// Uses optional content, whose visibility is set at document level.
    optional_content: bool,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            shallowest: usize::MAX,
            contextual: false,
            unresolved: false,
            optional_content: false,
        }
    }
}

impl Flags {
    fn merge(&mut self, other: Flags) {
        self.shallowest = self.shallowest.min(other.shallowest);
        self.contextual |= other.contextual;
        self.unresolved |= other.unresolved;
        self.optional_content |= other.optional_content;
    }
}

#[derive(Debug, Clone, Copy)]
struct Memo {
    digest: [u8; 32],
    unresolved: bool,
    optional_content: bool,
}

struct PageHasher<'s, S: ObjectSource> {
    source: &'s S,
    /// Page object id → 1-based page number: a reference to another page
    /// hashes as its position instead of pulling that whole page in.
    page_numbers: HashMap<ObjectId, usize>,
    memo: HashMap<ObjectId, Memo>,
    /// Objects currently being hashed, for cycle detection.
    stack: Vec<ObjectId>,
}

impl<'s, S: ObjectSource> PageHasher<'s, S> {
    fn new(source: &'s S, page_ids: &[ObjectId]) -> Self {
        Self {
            source,
            page_numbers: page_ids
                .iter()
                .enumerate()
                .map(|(index, id)| (*id, index + 1))
                .collect(),
            memo: HashMap::new(),
            stack: Vec::new(),
        }
    }

    fn page(&mut self, page_id: ObjectId) -> Result<[u8; 32]> {
        let source = self.source;
        let page_object = source.get_object_value(page_id)?;
        let page = page_object.as_dict().map_err(|_| {
            PdfOpsError::InvalidStructure(format!("page {page_id:?} is not a dictionary"))
        })?;
        let inherited = self.inherited_attributes(page)?;

        let mut hasher = Sha256::new();
        hasher.update(FINGERPRINT_VERSION.as_bytes());
        let mut flags = Flags::default();

        // Geometry. An absent CropBox defaults to the MediaBox, and /Rotate
        // is normalized, so equivalent spellings hash equally.
        let media_box = inherited.media_box.clone();
        let crop_box = inherited.crop_box.clone().or_else(|| media_box.clone());
        for value in [&media_box, &crop_box] {
            flags.merge(self.feed_optional(&mut hasher, value.as_ref())?);
        }
        let rotate = match inherited
            .rotate
            .as_ref()
            .map(|value| self.dereference(value))
        {
            Some(Ok(Some(Object::Integer(value)))) => value.rem_euclid(360),
            Some(Ok(Some(Object::Real(value)))) => (value as i64).rem_euclid(360),
            Some(Err(err)) => return Err(err),
            _ => 0,
        };
        feed_integer(&mut hasher, rotate);
        for key in [b"UserUnit".as_slice(), b"Group"] {
            flags.merge(self.feed_optional(&mut hasher, page.get(key).ok())?);
        }

        let resources = match &inherited.resources {
            Some(value) => self.resolve_dictionary(value)?,
            None => None,
        };
        flags.merge(self.feed_page_content(&mut hasher, page, resources.as_ref())?);
        flags.merge(self.feed_optional(&mut hasher, page.get(b"Annots").ok())?);

        // Document-level state the drawing depends on.
        if flags.unresolved {
            hasher.update(b"acroform-dr");
            let default_resources = self.catalog_entry(&[b"AcroForm", b"DR"])?;
            self.feed_optional(&mut hasher, default_resources.as_ref())?;
        }
        if flags.optional_content {
            hasher.update(b"ocproperties");
            let properties = self.catalog_entry(&[b"OCProperties"])?;
            self.feed_optional(&mut hasher, properties.as_ref())?;
        }

        Ok(hasher.finalize().into())
    }

    fn feed_page_content(
        &mut self,
        hasher: &mut Sha256,
        page: &Dictionary,
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        if let Some(data) = self.page_content(page)? {
            return self.feed_content(hasher, &data, resources, false);
        }
        // Undecodable /Contents: hash the objects as they are plus every
        // resource, so names still resolve to the same objects.
        let mut flags = Flags::default();
        hasher.update([TAG_CONTENT_RAW]);
        flags.merge(self.feed_optional(hasher, page.get(b"Contents").ok())?);
        flags.merge(self.feed_resources_whole(hasher, resources)?);
        Ok(flags)
    }

    /// A page's decoded `/Contents`, parts joined with `\n` (a part boundary is
    /// white-space). Unlike the pruning scan, an indirect array is followed.
    /// `None` when any part is missing or cannot be decoded.
    fn page_content(&self, page: &Dictionary) -> Result<Option<Vec<u8>>> {
        let Ok(contents) = page.get(b"Contents") else {
            return Ok(Some(Vec::new()));
        };
        let parts = match self.dereference(contents)? {
            Some(Object::Array(items)) => items,
            Some(stream @ Object::Stream(_)) => vec![stream],
            _ => return Ok(None),
        };
        let mut data = Vec::new();
        for part in &parts {
            let Some(Object::Stream(stream)) = self.dereference(part)? else {
                return Ok(None);
            };
            let Ok(decoded) = crate::filter::decode_stream_content(&stream) else {
                return Ok(None);
            };
            data.extend(decoded);
            data.push(b'\n');
        }
        Ok(Some(data))
    }

    /// Canonical serialization of a content stream: every operation with its
    /// operands, resource names replaced by the resources they resolve to and
    /// volatile stamps replaced by a placeholder. Falls back to the raw bytes
    /// plus every resource when the stream does not parse cleanly.
    fn feed_content(
        &mut self,
        hasher: &mut Sha256,
        data: &[u8],
        resources: Option<&Dictionary>,
        contextual_resources: bool,
    ) -> Result<Flags> {
        let stripped = scan::strip_comments(data);
        let operations = match Content::decode_strict(&stripped) {
            // lopdf drops an inline image it cannot size (unknown colour
            // space) and keeps an empty `BI`; its bytes would be lost.
            Ok(content)
                if !content
                    .operations
                    .iter()
                    .any(|op| op.operator == "BI" && op.operands.is_empty()) =>
            {
                content.operations
            }
            _ => {
                let mut flags = Flags::default();
                feed_bytes(hasher, TAG_CONTENT_RAW, data);
                flags.merge(self.feed_resources_whole(hasher, resources)?);
                flags.contextual |= contextual_resources;
                return Ok(flags);
            }
        };

        let mut flags = Flags::default();
        hasher.update([TAG_CONTENT]);
        feed_len(hasher, operations.len());
        for operation in &operations {
            feed_bytes(hasher, TAG_OPERATION, operation.operator.as_bytes());
            feed_len(hasher, operation.operands.len());
            let resource_slot = resource_operand(operation);
            let volatile_slot = volatile_operand(operation);
            for (index, operand) in operation.operands.iter().enumerate() {
                if volatile_slot == Some(index) {
                    hasher.update([TAG_VOLATILE]);
                    continue;
                }
                match (resource_slot, operand) {
                    (Some((slot, resource_type)), Object::Name(name)) if slot == index => {
                        flags.merge(self.feed_resource(hasher, resource_type, name, resources)?);
                    }
                    (_, Object::Stream(image)) if operation.operator == "BI" => {
                        flags.merge(self.feed_inline_image(hasher, image, resources)?);
                    }
                    _ => flags.merge(self.feed_object(hasher, operand)?),
                }
            }
        }
        // Names resolved against the caller's resources: not reusable.
        flags.contextual |= contextual_resources;
        Ok(flags)
    }

    fn feed_resource(
        &mut self,
        hasher: &mut Sha256,
        resource_type: ResourceType,
        name: &[u8],
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        let category = resource_type.dictionary_key();
        let dictionary = match resources.and_then(|r| r.get(category).ok()) {
            Some(value) => self.resolve_dictionary(value)?,
            None => None,
        };
        let Some(value) = dictionary.as_ref().and_then(|d| d.get(name).ok()) else {
            feed_bytes(hasher, TAG_UNRESOLVED, category);
            feed_bytes(hasher, TAG_NAME, name);
            let builtin =
                resource_type == ResourceType::ColorSpace && DEVICE_COLOR_SPACES.contains(&name);
            return Ok(Flags {
                unresolved: !builtin,
                ..Flags::default()
            });
        };
        feed_bytes(hasher, TAG_RESOURCE, category);

        let mut flags = Flags::default();
        match resource_type {
            ResourceType::XObject => {
                // A form without its own /Resources draws with the caller's.
                if let Some(flags) = self.feed_inheriting_form(
                    hasher,
                    value,
                    resources.unwrap_or(&Dictionary::new()),
                )? {
                    return Ok(flags);
                }
            }
            ResourceType::Font => {
                // A Type3 font without /Resources draws its glyphs with the
                // caller's resources.
                if let Some(Object::Dictionary(font)) = self.dereference(value)? {
                    if font.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Type3")
                        && !font.has(b"Resources")
                    {
                        flags.merge(self.feed_resources_whole(hasher, resources)?);
                        flags.contextual = true;
                    }
                }
            }
            ResourceType::Properties => flags.optional_content = true,
            _ => {}
        }
        flags.merge(self.feed_object(hasher, value)?);
        Ok(flags)
    }

    /// Hash a form XObject that lacks `/Resources` against `inherited`.
    /// Returns `None` when `value` is anything else (the caller hashes it
    /// normally, memoized).
    fn feed_inheriting_form(
        &mut self,
        hasher: &mut Sha256,
        value: &Object,
        inherited: &Dictionary,
    ) -> Result<Option<Flags>> {
        let Object::Reference(id) = value else {
            return Ok(None);
        };
        let source = self.source;
        let object = match source.get_object_value(*id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let Ok(stream) = object.as_stream() else {
            return Ok(None);
        };
        if !is_content_stream(stream) || stream.dict.has(b"Resources") {
            return Ok(None);
        }
        if let Some(position) = self.stack.iter().position(|entry| entry == id) {
            hasher.update([TAG_BACK_REFERENCE]);
            feed_len(hasher, self.stack.len() - position);
            return Ok(Some(Flags {
                shallowest: position,
                ..Flags::default()
            }));
        }
        self.enter(*id)?;
        let mut child = Sha256::new();
        let result = self.feed_stream(&mut child, stream, Some(inherited));
        self.stack.pop();
        let mut flags = result?;
        flags.contextual = true;
        hasher.update([TAG_REFERENCE]);
        hasher.update(child.finalize());
        Ok(Some(flags))
    }

    fn feed_inline_image(
        &mut self,
        hasher: &mut Sha256,
        image: &Stream,
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        let mut flags = Flags::default();
        hasher.update([TAG_STREAM]);
        let mut entries: Vec<_> = image.dict.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        hasher.update([TAG_DICTIONARY]);
        feed_len(hasher, entries.len());
        for (key, value) in entries {
            feed_bytes(hasher, TAG_NAME, key);
            if key.as_slice() == b"CS" || key.as_slice() == b"ColorSpace" {
                flags.merge(self.feed_inline_color_space(hasher, value, resources)?);
            } else {
                flags.merge(self.feed_object(hasher, value)?);
            }
        }
        feed_bytes(hasher, TAG_BYTES_RAW, &image.content);
        Ok(flags)
    }

    fn feed_inline_color_space(
        &mut self,
        hasher: &mut Sha256,
        value: &Object,
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        match value {
            Object::Name(name) => {
                self.feed_resource(hasher, ResourceType::ColorSpace, name, resources)
            }
            Object::Array(items) => {
                let mut flags = Flags::default();
                hasher.update([TAG_ARRAY]);
                feed_len(hasher, items.len());
                for item in items {
                    flags.merge(self.feed_inline_color_space(hasher, item, resources)?);
                }
                Ok(flags)
            }
            other => self.feed_object(hasher, other),
        }
    }

    /// Every entry of a resource dictionary, for paths that cannot tell which
    /// resources the content uses.
    fn feed_resources_whole(
        &mut self,
        hasher: &mut Sha256,
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        match resources {
            Some(resources) => self.feed_dictionary(hasher, resources, None),
            None => {
                hasher.update([TAG_ABSENT]);
                Ok(Flags::default())
            }
        }
    }

    fn feed_optional(&mut self, hasher: &mut Sha256, value: Option<&Object>) -> Result<Flags> {
        match value {
            Some(value) => self.feed_object(hasher, value),
            None => {
                hasher.update([TAG_ABSENT]);
                Ok(Flags::default())
            }
        }
    }

    fn feed_object(&mut self, hasher: &mut Sha256, object: &Object) -> Result<Flags> {
        match object {
            Object::Null => hasher.update([TAG_NULL]),
            Object::Boolean(false) => hasher.update([TAG_FALSE]),
            Object::Boolean(true) => hasher.update([TAG_TRUE]),
            Object::Integer(value) => feed_integer(hasher, *value),
            Object::Real(value) => feed_real(hasher, *value),
            Object::Name(name) => feed_bytes(hasher, TAG_NAME, name),
            Object::String(bytes, _) => feed_bytes(hasher, TAG_STRING, bytes),
            Object::Array(items) => {
                let mut flags = Flags::default();
                hasher.update([TAG_ARRAY]);
                feed_len(hasher, items.len());
                for item in items {
                    flags.merge(self.feed_object(hasher, item)?);
                }
                return Ok(flags);
            }
            Object::Dictionary(dictionary) => {
                return self.feed_dictionary(hasher, dictionary, None);
            }
            Object::Stream(stream) => return self.feed_stream(hasher, stream, None),
            Object::Reference(id) => return self.feed_reference(hasher, *id),
        }
        Ok(Flags::default())
    }

    fn feed_reference(&mut self, hasher: &mut Sha256, id: ObjectId) -> Result<Flags> {
        if let Some(page) = self.page_numbers.get(&id) {
            hasher.update([TAG_PAGE_REFERENCE]);
            feed_len(hasher, *page);
            return Ok(Flags::default());
        }
        if let Some(memo) = self.memo.get(&id) {
            hasher.update([TAG_REFERENCE]);
            hasher.update(memo.digest);
            return Ok(Flags {
                unresolved: memo.unresolved,
                optional_content: memo.optional_content,
                ..Flags::default()
            });
        }
        if let Some(position) = self.stack.iter().position(|entry| *entry == id) {
            hasher.update([TAG_BACK_REFERENCE]);
            feed_len(hasher, self.stack.len() - position);
            return Ok(Flags {
                shallowest: position,
                ..Flags::default()
            });
        }

        let source = self.source;
        let object = match source.get_object_value(id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => {
                hasher.update([TAG_MISSING]);
                return Ok(Flags::default());
            }
            Err(err) => return Err(err.into()),
        };
        if let Ok(dictionary) = object.as_dict() {
            if dictionary.get(b"Type").and_then(Object::as_name).ok() == Some(b"Page") {
                // A page outside the page tree: its identity is unknowable
                // and its content is not part of this page.
                hasher.update([TAG_ORPHAN_PAGE]);
                return Ok(Flags::default());
            }
        }

        let position = self.stack.len();
        self.enter(id)?;
        let mut child = Sha256::new();
        let result = self.feed_object(&mut child, &object);
        self.stack.pop();
        let mut flags = result?;
        let digest: [u8; 32] = child.finalize().into();

        if flags.shallowest >= position {
            flags.shallowest = usize::MAX;
            if !flags.contextual {
                self.memo.insert(
                    id,
                    Memo {
                        digest,
                        unresolved: flags.unresolved,
                        optional_content: flags.optional_content,
                    },
                );
            }
        }
        hasher.update([TAG_REFERENCE]);
        hasher.update(digest);
        Ok(flags)
    }

    fn feed_dictionary(
        &mut self,
        hasher: &mut Sha256,
        dictionary: &Dictionary,
        skip_stream_keys: Option<&[&[u8]]>,
    ) -> Result<Flags> {
        let mut entries: Vec<_> = dictionary
            .iter()
            .filter(|(key, value)| !self.skips_entry(key, value))
            .filter(|(key, _)| {
                skip_stream_keys.is_none_or(|skipped| !skipped.contains(&key.as_slice()))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        let mut flags = Flags::default();
        if dictionary.has(b"OC")
            || matches!(
                dictionary.get(b"Type").and_then(Object::as_name).ok(),
                Some(b"OCG" | b"OCMD")
            )
        {
            flags.optional_content = true;
        }
        hasher.update([TAG_DICTIONARY]);
        feed_len(hasher, entries.len());
        for (key, value) in entries {
            feed_bytes(hasher, TAG_NAME, key);
            flags.merge(self.feed_object(hasher, value)?);
        }
        Ok(flags)
    }

    fn skips_entry(&self, key: &[u8], value: &Object) -> bool {
        if SKIPPED_KEYS.contains(&key) {
            return true;
        }
        // An annotation's /P points back at the page it sits on.
        key == b"P" && matches!(value, Object::Reference(id) if self.page_numbers.contains_key(id))
    }

    /// Streams. Content streams (forms, tiling patterns) are canonicalized
    /// against their resources; everything else hashes its inflated bytes
    /// when the only filter is Flate, and its raw bytes plus the filter chain
    /// otherwise.
    fn feed_stream(
        &mut self,
        hasher: &mut Sha256,
        stream: &Stream,
        inherited: Option<&Dictionary>,
    ) -> Result<Flags> {
        hasher.update([TAG_STREAM]);

        if is_content_stream(stream) {
            if let Ok(data) = crate::filter::decode_stream_content(stream) {
                let own = match stream.dict.get(b"Resources") {
                    Ok(value) => self.resolve_dictionary(value)?,
                    Err(_) => None,
                };
                let mut flags = self.feed_dictionary(
                    hasher,
                    &stream.dict,
                    Some(&[b"Length", b"Filter", b"DecodeParms", b"DL", b"Resources"]),
                )?;
                let (resources, contextual) = match (&own, inherited) {
                    (Some(own), _) => (Some(own), false),
                    (None, Some(inherited)) => (Some(inherited), true),
                    (None, None) => (None, false),
                };
                flags.merge(self.feed_content(hasher, &data, resources, contextual)?);
                return Ok(flags);
            }
        }

        if is_flate_only(stream) {
            let predictor = match self.decode_params(stream)? {
                Some(params) => Predictor::from_params(params.as_ref()),
                None => None,
            };
            if let Some(digest) = predictor.and_then(|p| inflate_digest(&stream.content, p)) {
                let flags = self.feed_dictionary(
                    hasher,
                    &stream.dict,
                    Some(&[b"Length", b"Filter", b"DecodeParms", b"DL"]),
                )?;
                hasher.update([TAG_BYTES_INFLATED]);
                hasher.update(digest);
                return Ok(flags);
            }
        }
        if !stream.dict.has(b"Filter") {
            let flags = self.feed_dictionary(hasher, &stream.dict, Some(&[b"Length", b"DL"]))?;
            hasher.update([TAG_BYTES_INFLATED]);
            hasher.update(Sha256::digest(&stream.content));
            return Ok(flags);
        }
        let flags = self.feed_dictionary(hasher, &stream.dict, Some(&[b"Length"]))?;
        feed_bytes(hasher, TAG_BYTES_RAW, &stream.content);
        Ok(flags)
    }

    /// The single filter's `/DecodeParms`: `Some(None)` when there are none,
    /// `None` when they are present but unusable.
    fn decode_params(&self, stream: &Stream) -> Result<Option<Option<Dictionary>>> {
        let Ok(value) = stream.dict.get(b"DecodeParms") else {
            return Ok(Some(None));
        };
        let value = match self.dereference(value)? {
            Some(Object::Array(items)) if items.len() == 1 => self.dereference(&items[0])?,
            other => other,
        };
        Ok(match value {
            Some(Object::Dictionary(params)) => Some(Some(params)),
            Some(Object::Null) => Some(None),
            _ => None,
        })
    }

    fn enter(&mut self, id: ObjectId) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(PdfOpsError::InvalidStructure(format!(
                "object nesting deeper than {MAX_DEPTH} while fingerprinting"
            )));
        }
        self.stack.push(id);
        Ok(())
    }

    fn dereference(&self, object: &Object) -> Result<Option<Object>> {
        let mut object = Cow::Borrowed(object);
        let mut hops = 0usize;
        while let Object::Reference(id) = object.as_ref() {
            hops += 1;
            if hops > MAX_CHAIN {
                return Ok(None);
            }
            object = match self.source.get_object_value(*id) {
                Ok(next) => Cow::Owned(next.into_owned()),
                Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
        }
        Ok(Some(object.into_owned()))
    }

    fn resolve_dictionary(&self, value: &Object) -> Result<Option<Dictionary>> {
        Ok(match self.dereference(value)? {
            Some(Object::Dictionary(dictionary)) => Some(dictionary),
            _ => None,
        })
    }

    fn catalog_entry(&self, path: &[&[u8]]) -> Result<Option<Object>> {
        let Some(mut current) = self.source.trailer_value(b"Root") else {
            return Ok(None);
        };
        for key in path {
            let Some(dictionary) = self.resolve_dictionary(&current)? else {
                return Ok(None);
            };
            let Ok(value) = dictionary.get(key) else {
                return Ok(None);
            };
            current = value.clone();
        }
        Ok(Some(current))
    }

    fn inherited_attributes(&self, page: &Dictionary) -> Result<InheritedAttributes> {
        let mut attributes = InheritedAttributes {
            resources: page.get(b"Resources").ok().cloned(),
            media_box: page.get(b"MediaBox").ok().cloned(),
            crop_box: page.get(b"CropBox").ok().cloned(),
            rotate: page.get(b"Rotate").ok().cloned(),
        };
        let mut parent = page.get(b"Parent").ok().cloned();
        let mut hops = 0usize;
        while !attributes.complete() {
            let Some(Object::Reference(id)) = parent else {
                break;
            };
            hops += 1;
            if hops > MAX_CHAIN {
                break;
            }
            let node = match self.source.get_object_value(id) {
                Ok(node) => node,
                Err(lopdf::Error::ObjectNotFound(_)) => break,
                Err(err) => return Err(err.into()),
            };
            let Ok(node) = node.as_dict() else {
                break;
            };
            for (slot, key) in [
                (&mut attributes.resources, b"Resources".as_slice()),
                (&mut attributes.media_box, b"MediaBox"),
                (&mut attributes.crop_box, b"CropBox"),
                (&mut attributes.rotate, b"Rotate"),
            ] {
                if slot.is_none() {
                    *slot = node.get(key).ok().cloned();
                }
            }
            parent = node.get(b"Parent").ok().cloned();
        }
        Ok(attributes)
    }
}

struct InheritedAttributes {
    resources: Option<Object>,
    media_box: Option<Object>,
    crop_box: Option<Object>,
    rotate: Option<Object>,
}

impl InheritedAttributes {
    fn complete(&self) -> bool {
        self.resources.is_some()
            && self.media_box.is_some()
            && self.crop_box.is_some()
            && self.rotate.is_some()
    }
}

/// Which operand of `operation` names a resource, and of which type.
fn resource_operand(operation: &Operation) -> Option<(usize, ResourceType)> {
    let resource_type = scan::resource_type_for_operator(&operation.operator)?;
    let index = match operation.operator.as_str() {
        "Tf" => 0,
        // `/Tag /Props BDC`: the tag is literal, only the second operand is
        // a /Properties name (it may be an inline dictionary instead).
        "BDC" | "DP" => 1,
        _ => operation.operands.len().checked_sub(1)?,
    };
    matches!(operation.operands.get(index), Some(Object::Name(_))).then_some((index, resource_type))
}

/// Which operand of a text-showing `operation` is a known volatile stamp.
fn volatile_operand(operation: &Operation) -> Option<usize> {
    let index = match operation.operator.as_str() {
        "Tj" | "'" => 0,
        "\"" => 2,
        "TJ" => 0,
        _ => return None,
    };
    let text: Cow<'_, [u8]> = match operation.operands.get(index)? {
        Object::String(bytes, _) => Cow::Borrowed(bytes),
        Object::Array(items) => Cow::Owned(
            items
                .iter()
                .filter_map(|item| match item {
                    Object::String(bytes, _) => Some(bytes.as_slice()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .concat(),
        ),
        _ => return None,
    };
    is_volatile_text(&text).then_some(index)
}

/// Known per-download stamps. Each rule must match the WHOLE string and be
/// specific enough that no page content could plausibly match it: masking
/// hides a difference, so an over-broad rule is a false positive.
///
/// - PJe: `Este documento foi gerado pelo usuário <user> em dd/mm/yyyy hh:mm:ss`,
///   written by the download stamp in WinAnsi (`á` = 0xE1) or UTF-8.
pub(crate) fn is_volatile_text(text: &[u8]) -> bool {
    is_pje_generated_by(text)
}

fn is_pje_generated_by(text: &[u8]) -> bool {
    const PREFIX: &[u8] = b"Este documento foi gerado pelo usu";
    // " em dd/mm/yyyy hh:mm:ss"
    const SUFFIX_SHAPE: &[u8] = b" em 00/00/0000 00:00:00";

    let Some(rest) = text.strip_prefix(PREFIX) else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix(b"\xe1rio ".as_slice())
        .or_else(|| rest.strip_prefix("ário ".as_bytes()))
    else {
        return false;
    };
    if rest.len() <= SUFFIX_SHAPE.len() {
        return false;
    }
    let (user, suffix) = rest.split_at(rest.len() - SUFFIX_SHAPE.len());
    !user.trim_ascii().is_empty()
        && suffix
            .iter()
            .zip(SUFFIX_SHAPE)
            .all(|(byte, shape)| match shape {
                b'0' => byte.is_ascii_digit(),
                _ => byte == shape,
            })
}

fn is_content_stream(stream: &Stream) -> bool {
    stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Form")
        || matches!(stream.dict.get(b"PatternType"), Ok(Object::Integer(1)))
}

fn is_flate_only(stream: &Stream) -> bool {
    let flate = |name: &[u8]| crate::filter::canonical_filter_name(name) == b"FlateDecode";
    match stream.dict.get(b"Filter") {
        Ok(Object::Name(name)) => flate(name),
        Ok(Object::Array(filters)) => {
            matches!(filters.as_slice(), [Object::Name(name)] if flate(name))
        }
        _ => false,
    }
}

/// Largest predictor row accepted; a bogus `/Columns` must not turn into a
/// huge allocation.
const MAX_PREDICTOR_ROW: usize = 16 * 1024 * 1024;

/// A Flate stream's predictor (ISO 32000-1 §7.4.4.4), undone while hashing so
/// a producer that drops or changes the predictor still yields the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Predictor {
    None,
    /// PNG predictors (10–15): every row starts with its own filter-type byte.
    Png {
        row: usize,
        bpp: usize,
    },
    /// TIFF predictor 2, 8 bits per component only.
    Tiff {
        row: usize,
        colors: usize,
    },
}

impl Predictor {
    /// `None` for parameters this hasher cannot undo exactly (the caller then
    /// hashes the raw bytes).
    fn from_params(params: Option<&Dictionary>) -> Option<Self> {
        let Some(params) = params else {
            return Some(Self::None);
        };
        let integer = |key: &[u8], default: i64| match params.get(key) {
            Err(_) => Some(default),
            Ok(Object::Integer(value)) => Some(*value),
            Ok(_) => None,
        };
        let predictor = integer(b"Predictor", 1)?;
        if predictor == 1 {
            return Some(Self::None);
        }
        let colors = usize::try_from(integer(b"Colors", 1)?)
            .ok()
            .filter(|v| *v > 0)?;
        let bits = usize::try_from(integer(b"BitsPerComponent", 8)?).ok()?;
        let columns = usize::try_from(integer(b"Columns", 1)?)
            .ok()
            .filter(|v| *v > 0)?;
        if !matches!(bits, 1 | 2 | 4 | 8 | 16) {
            return None;
        }
        let row = colors.checked_mul(bits)?.checked_mul(columns)?.div_ceil(8);
        if row > MAX_PREDICTOR_ROW {
            return None;
        }
        match predictor {
            2 if bits == 8 => Some(Self::Tiff { row, colors }),
            10..=15 => Some(Self::Png {
                row,
                bpp: (colors * bits).div_ceil(8).max(1),
            }),
            _ => None,
        }
    }
}

/// SHA-256 of the inflated, un-predicted `data`, streamed row by row so a
/// large image never sits decoded in memory. `None` when the data does not
/// decode cleanly (bad zlib data, an unknown PNG filter type, a truncated
/// row); the caller then hashes the raw bytes — a mismatch at worst, never a
/// collision.
fn inflate_digest(data: &[u8], predictor: Predictor) -> Option<[u8; 32]> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut hasher = Sha256::new();
    let (row_len, prefix) = match predictor {
        Predictor::None => (0, 0),
        Predictor::Png { row, .. } => (row, 1),
        Predictor::Tiff { row, .. } => (row, 0),
    };
    let mut previous = vec![0u8; row_len];
    let mut current = vec![0u8; row_len + prefix];
    let mut filled = 0usize;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match decoder.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if predictor == Predictor::None {
            hasher.update(&buffer[..read]);
            continue;
        }
        let mut chunk = &buffer[..read];
        while !chunk.is_empty() {
            let take = (current.len() - filled).min(chunk.len());
            current[filled..filled + take].copy_from_slice(&chunk[..take]);
            filled += take;
            chunk = &chunk[take..];
            if filled == current.len() {
                unpredict_row(predictor, &mut current, &previous)?;
                hasher.update(&current[prefix..]);
                previous.copy_from_slice(&current[prefix..]);
                filled = 0;
            }
        }
    }
    // A truncated last row cannot be un-predicted the way another producer
    // would have; refuse rather than guess.
    (filled == 0).then(|| hasher.finalize().into())
}

/// Undo one row's prediction in place. `row` includes the PNG filter-type
/// byte when there is one; `previous` is the already decoded row above.
fn unpredict_row(predictor: Predictor, row: &mut [u8], previous: &[u8]) -> Option<()> {
    match predictor {
        Predictor::None => {}
        Predictor::Tiff { colors, .. } => {
            for index in colors..row.len() {
                row[index] = row[index].wrapping_add(row[index - colors]);
            }
        }
        Predictor::Png { bpp, .. } => {
            let (filter, data) = row.split_first_mut()?;
            match *filter {
                0 => {}
                1 => {
                    for index in bpp..data.len() {
                        data[index] = data[index].wrapping_add(data[index - bpp]);
                    }
                }
                2 => {
                    for (byte, up) in data.iter_mut().zip(previous) {
                        *byte = byte.wrapping_add(*up);
                    }
                }
                3 => {
                    for index in 0..data.len() {
                        let left = if index >= bpp { data[index - bpp] } else { 0 };
                        let average = ((u16::from(left) + u16::from(previous[index])) / 2) as u8;
                        data[index] = data[index].wrapping_add(average);
                    }
                }
                4 => {
                    for index in 0..data.len() {
                        let (left, up_left) = if index >= bpp {
                            (data[index - bpp], previous[index - bpp])
                        } else {
                            (0, 0)
                        };
                        data[index] =
                            data[index].wrapping_add(paeth(left, previous[index], up_left));
                    }
                }
                _ => return None,
            }
            // The filter-type byte is not image data; zero it so the caller
            // can hash from `prefix` without branching.
            *filter = 0;
        }
    }
    Some(())
}

fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(up_left);
    let distance_left = (estimate - i16::from(left)).abs();
    let distance_up = (estimate - i16::from(up)).abs();
    let distance_up_left = (estimate - i16::from(up_left)).abs();
    if distance_left <= distance_up && distance_left <= distance_up_left {
        left
    } else if distance_up <= distance_up_left {
        up
    } else {
        up_left
    }
}

fn feed_len(hasher: &mut Sha256, len: usize) {
    hasher.update((len as u64).to_le_bytes());
}

fn feed_bytes(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hasher.update([tag]);
    feed_len(hasher, bytes.len());
    hasher.update(bytes);
}

fn feed_integer(hasher: &mut Sha256, value: i64) {
    hasher.update([TAG_INTEGER]);
    hasher.update(value.to_le_bytes());
}

/// Integral reals hash as integers (`1` and `1.0` draw the same); `-0.0`
/// folds into `0` on the same path.
fn feed_real(hasher: &mut Sha256, value: f32) {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < (1u64 << 53) as f32 {
        feed_integer(hasher, value as i64);
    } else {
        hasher.update([TAG_REAL]);
        hasher.update(value.to_bits().to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::is_volatile_text;

    #[test]
    fn matches_pje_download_stamp_in_winansi_and_utf8() {
        assert!(is_volatile_text(
            b"Este documento foi gerado pelo usu\xe1rio 569.***.***-04 em 17/08/2026 13:43:50"
        ));
        assert!(is_volatile_text(
            "Este documento foi gerado pelo usuário 123.***.***-00 em 01/01/2025 00:00:00"
                .as_bytes()
        ));
    }

    #[test]
    fn rejects_near_misses() {
        // Missing user, trailing text, wrong date shape, other stamp lines.
        for text in [
            b"Este documento foi gerado pelo usu\xe1rio  em 17/08/2026 13:43:50".as_slice(),
            b"Este documento foi gerado pelo usu\xe1rio x em 17/08/2026 13:43:50 ",
            b"Este documento foi gerado pelo usu\xe1rio x em 17/08/26 13:43:50",
            b"Este documento foi gerado pelo usu\xe1rio x em 17/08/2026 13h43",
            b"Num. 39526322 - P\xe1g. 1",
            b"Assinado eletronicamente por: FULANO - 16/07/2026 13:52:52",
        ] {
            assert!(!is_volatile_text(text), "{}", String::from_utf8_lossy(text));
        }
    }
}
