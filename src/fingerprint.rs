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
//! Bookkeeping is excluded only in recognized dictionary types: widget and
//! annotation back-references, structure-tree indices, XMP metadata, edit
//! dates, annotation names and link destinations/actions. The same keys in
//! glyph and resource maps always count. A widget's `/Parent` field is
//! the exception in substance: the value and appearance defaults a viewer
//! draws from it are hashed explicitly (see [`INHERITED_FIELD_KEYS`]).
//!
//! What is document-relative on purpose: an optional content group hashes
//! with its position in the catalog's `/OCGs` list, since two groups with
//! equal dictionaries can still be switched on and off separately.
//!
//! Volatile stamps: some court systems stamp every page of a download with
//! the downloading user and timestamp. Such a line hashes as a fixed
//! placeholder, but only where it is drawn as the stamp (see
//! [`volatile_operands`]), so two downloads of the same page still match while
//! page text quoting the sentence is hashed as it is. Changing those rules,
//! or anything else in the serialization, changes every fingerprint and must
//! bump [`FINGERPRINT_VERSION`].
//!
//! Work is bounded: decoded content is capped per page and per form, and
//! inflated stream data per stream. Past a cap the bytes are hashed undecoded,
//! which can only make equal pages differ, never different pages equal.

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
/// Decoded content-stream bytes canonicalized per page (all `/Contents` parts
/// together) and per form XObject or tiling pattern.
const MAX_CONTENT_BYTES: usize = 128 * 1024 * 1024;
/// Inflated bytes hashed per non-content stream (images, fonts, ICC).
const MAX_INFLATED_BYTES: usize = 512 * 1024 * 1024;

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
const TAG_FIELD: u8 = b'F';
const TAG_OCG: u8 = b'G';

/// Keys skipped only in annotation dictionaries: the annotation name,
/// modification date, and link destinations/actions — a link into the
/// document points at a page NUMBER, which shifts whenever the page sits at
/// another position. Elsewhere the same names are drawn content: a Type3
/// `/CharProcs` glyph or a resource entry may be called `/A` or `/M`.
const SKIPPED_ANNOTATION_KEYS: [&[u8]; 6] = [b"NM", b"M", b"Dest", b"A", b"AA", b"PA"];

/// Colour-space resources that silently replace the device spaces `g`, `rg`,
/// `k` and inline images select (ISO 32000-1 §8.6.5.6). No operand names
/// them, so they take part in every content hash whenever they exist.
const DEFAULT_COLOR_SPACES: [&[u8]; 3] = [b"DefaultCMYK", b"DefaultGray", b"DefaultRGB"];

/// Field attributes a widget annotation inherits through `/Parent` (ISO
/// 32000-1 §12.7.3) that decide what a viewer draws when it (re)generates the
/// widget's appearance: the value, its defaults and the text layout. `/Parent`
/// itself stays out of the hash — it leads to `/Kids` and every sibling widget.
const INHERITED_FIELD_KEYS: [&[u8]; 12] = [
    b"FT", b"V", b"DV", b"DA", b"DS", b"RV", b"Ff", b"Q", b"Opt", b"MaxLen", b"TI", b"I",
];

/// AcroForm entries a widget's appearance falls back to.
const ACROFORM_DEFAULT_KEYS: [&[u8]; 4] = [b"DA", b"Q", b"DR", b"NeedAppearances"];

/// `/Resources` categories a content stream can name an entry of.
const RESOURCE_TYPES: [ResourceType; 7] = [
    ResourceType::ColorSpace,
    ResourceType::ExtGState,
    ResourceType::Font,
    ResourceType::Pattern,
    ResourceType::Properties,
    ResourceType::Shading,
    ResourceType::XObject,
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
    /// Has a form-field widget, whose appearance falls back to AcroForm
    /// defaults.
    form_fields: bool,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            shallowest: usize::MAX,
            contextual: false,
            unresolved: false,
            optional_content: false,
            form_fields: false,
        }
    }
}

impl Flags {
    fn merge(&mut self, other: Flags) {
        self.shallowest = self.shallowest.min(other.shallowest);
        self.contextual |= other.contextual;
        self.unresolved |= other.unresolved;
        self.optional_content |= other.optional_content;
        self.form_fields |= other.form_fields;
    }
}

#[derive(Debug, Clone, Copy)]
struct Memo {
    digest: [u8; 32],
    unresolved: bool,
    optional_content: bool,
    form_fields: bool,
}

struct PageHasher<'s, S: ObjectSource> {
    source: &'s S,
    /// Page object id → 1-based page number: a reference to another page
    /// hashes as its position instead of pulling that whole page in.
    page_numbers: HashMap<ObjectId, usize>,
    memo: HashMap<ObjectId, Memo>,
    /// Objects currently being hashed, for cycle detection.
    stack: Vec<ObjectId>,
    /// The catalog's `/OCProperties /OCGs` order, loaded on first use. Two
    /// groups with equal dictionaries are still distinct groups — one may be
    /// switched off — so a group hashes with its position in this list.
    ocg_order: Option<Vec<ObjectId>>,
    /// Attributes inherited from a page-tree node and its ancestors, by node
    /// id, so a shared root is distilled once instead of loaded per page.
    ancestors: HashMap<ObjectId, InheritedAttributes>,
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
            ocg_order: None,
            ancestors: HashMap::new(),
        }
    }

    /// Position of `id` in the catalog's `/OCGs` array, if listed.
    fn ocg_index(&mut self, id: ObjectId) -> Result<Option<usize>> {
        if self.ocg_order.is_none() {
            let order = match self.catalog_entry(&[b"OCProperties", b"OCGs"])? {
                Some(value) => match self.dereference(&value)? {
                    Some(Object::Array(items)) => items
                        .iter()
                        .filter_map(|item| match item {
                            Object::Reference(id) => Some(*id),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                },
                None => Vec::new(),
            };
            self.ocg_order = Some(order);
        }
        Ok(self
            .ocg_order
            .as_ref()
            .and_then(|order| order.iter().position(|entry| *entry == id)))
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
        if flags.form_fields {
            hasher.update(b"acroform-defaults");
            let acroform = match self.catalog_entry(&[b"AcroForm"])? {
                Some(value) => self.resolve_dictionary(&value)?,
                None => None,
            };
            for key in ACROFORM_DEFAULT_KEYS {
                let value = acroform.as_ref().and_then(|form| form.get(key).ok());
                self.feed_optional(&mut hasher, value)?;
            }
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
    /// `None` when any part is missing, cannot be decoded, or the parts decode
    /// past [`MAX_CONTENT_BYTES`].
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
            let budget = MAX_CONTENT_BYTES.saturating_sub(data.len());
            let Some(decoded) = decode_content(&stream, budget) else {
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
        let mut flags = self.feed_default_color_spaces(hasher, resources)?;
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
                feed_bytes(hasher, TAG_CONTENT_RAW, data);
                flags.merge(self.feed_resources_named(hasher, resources, data)?);
                flags.contextual |= contextual_resources;
                return Ok(flags);
            }
        };

        let volatile = volatile_operands(&operations);
        hasher.update([TAG_CONTENT]);
        feed_len(hasher, operations.len());
        for (position, operation) in operations.iter().enumerate() {
            feed_bytes(hasher, TAG_OPERATION, operation.operator.as_bytes());
            feed_len(hasher, operation.operands.len());
            let resource_slot = resource_operand(operation);
            let volatile_slot = volatile.get(&position).copied();
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

    /// The [`DEFAULT_COLOR_SPACES`] entries of `resources`, present or not,
    /// ahead of the content that draws with them.
    fn feed_default_color_spaces(
        &mut self,
        hasher: &mut Sha256,
        resources: Option<&Dictionary>,
    ) -> Result<Flags> {
        let color_spaces = match resources.and_then(|r| r.get(b"ColorSpace").ok()) {
            Some(value) => self.resolve_dictionary(value)?,
            None => None,
        };
        let mut flags = Flags::default();
        for name in DEFAULT_COLOR_SPACES {
            let value = color_spaces.as_ref().and_then(|d| d.get(name).ok());
            flags.merge(self.feed_optional(hasher, value)?);
        }
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

    /// The resources raw `content` can name, for content that did not parse
    /// cleanly: every entry of a resource category whose name appears as a
    /// name token anywhere in the bytes (`#xx` escapes decoded). That is a
    /// superset of what the content uses — a stray `/Name` inside inline-image
    /// data only adds an entry — so it still pins every drawn resource, while
    /// entries the content never mentions (the ones `split` prunes, or a
    /// document-wide template catalog shared by every page) stay out.
    fn feed_resources_named(
        &mut self,
        hasher: &mut Sha256,
        resources: Option<&Dictionary>,
        content: &[u8],
    ) -> Result<Flags> {
        let Some(resources) = resources else {
            hasher.update([TAG_ABSENT]);
            return Ok(Flags::default());
        };
        let names = name_tokens(content);
        let mut named: Vec<(ResourceType, Vec<Vec<u8>>)> = Vec::new();
        for resource_type in RESOURCE_TYPES {
            let Some(entries) = (match resources.get(resource_type.dictionary_key()) {
                Ok(value) => self.resolve_dictionary(value)?,
                Err(_) => None,
            }) else {
                continue;
            };
            let mut used: Vec<_> = entries
                .iter()
                .filter(|(name, _)| names.contains(name.as_slice()))
                .map(|(name, _)| name.clone())
                .collect();
            // A category none of whose entries is named draws nothing.
            if used.is_empty() {
                continue;
            }
            used.sort();
            named.push((resource_type, used));
        }

        // Each entry hashes the way a content operand naming it would, so a
        // form without /Resources still draws with this dictionary.
        let mut flags = Flags::default();
        hasher.update([TAG_DICTIONARY]);
        feed_len(hasher, named.len());
        for (resource_type, used) in named {
            feed_bytes(hasher, TAG_NAME, resource_type.dictionary_key());
            hasher.update([TAG_DICTIONARY]);
            feed_len(hasher, used.len());
            for name in &used {
                feed_bytes(hasher, TAG_NAME, name);
                flags.merge(self.feed_resource(hasher, resource_type, name, Some(resources))?);
            }
        }
        Ok(flags)
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
                form_fields: memo.form_fields,
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
        let mut optional_content_group = false;
        if let Ok(dictionary) = object.as_dict() {
            match dictionary.get(b"Type").and_then(Object::as_name).ok() {
                Some(b"Page") => {
                    // A page outside the page tree: its identity is
                    // unknowable and its content is not part of this page.
                    hasher.update([TAG_ORPHAN_PAGE]);
                    return Ok(Flags::default());
                }
                Some(b"OCG") => optional_content_group = true,
                _ => {}
            }
        }

        let position = self.stack.len();
        self.enter(id)?;
        let mut child = Sha256::new();
        if optional_content_group {
            match self.ocg_index(id)? {
                Some(index) => {
                    child.update([TAG_OCG]);
                    feed_len(&mut child, index);
                }
                None => child.update([TAG_MISSING]),
            }
        }
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
                        form_fields: flags.form_fields,
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
        // Validate the values too: a /CharProcs map can have glyphs named
        // /Subtype and /Rect, whose values are glyph streams, not a subtype
        // name and a rectangle. /Type is optional on annotations.
        let annotation = dictionary.get(b"Subtype").and_then(Object::as_name).is_ok()
            && dictionary
                .get(b"Rect")
                .and_then(Object::as_array)
                .is_ok_and(|rect| {
                    rect.len() == 4
                        && rect
                            .iter()
                            .all(|v| matches!(v, Object::Integer(_) | Object::Real(_)))
                });
        let mut entries: Vec<_> = dictionary
            .iter()
            .filter(|(key, value)| {
                !self.skips_entry(
                    dictionary,
                    key,
                    value,
                    annotation,
                    skip_stream_keys.is_some(),
                )
            })
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
        if annotation
            && dictionary.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Widget")
        {
            flags.merge(self.feed_inherited_field(hasher, dictionary)?);
        }
        Ok(flags)
    }

    /// The [`INHERITED_FIELD_KEYS`] a widget does not set itself, resolved up
    /// its `/Parent` chain (nearest ancestor wins), appended after the widget
    /// dictionary. A widget without an appearance stream is drawn from these,
    /// so two widgets differing only in their parent field's value must differ.
    fn feed_inherited_field(&mut self, hasher: &mut Sha256, widget: &Dictionary) -> Result<Flags> {
        let mut inherited: Vec<(&[u8], Object)> = Vec::new();
        let mut parent = widget.get(b"Parent").ok().cloned();
        let mut hops = 0usize;
        while let Some(link @ Object::Reference(_)) = parent {
            hops += 1;
            if hops > MAX_CHAIN {
                break;
            }
            let Some(field) = self.resolve_dictionary(&link)? else {
                break;
            };
            for key in INHERITED_FIELD_KEYS {
                if !widget.has(key) && !inherited.iter().any(|(seen, _)| *seen == key) {
                    if let Ok(value) = field.get(key) {
                        inherited.push((key, value.clone()));
                    }
                }
            }
            parent = field.get(b"Parent").ok().cloned();
        }
        inherited.sort_by(|a, b| a.0.cmp(b.0));

        let mut flags = Flags {
            form_fields: true,
            ..Flags::default()
        };
        hasher.update([TAG_FIELD]);
        feed_len(hasher, inherited.len());
        for (key, value) in &inherited {
            feed_bytes(hasher, TAG_NAME, key);
            flags.merge(self.feed_object(hasher, value)?);
        }
        Ok(flags)
    }

    fn skips_entry(
        &self,
        dictionary: &Dictionary,
        key: &[u8],
        value: &Object,
        annotation: bool,
        stream: bool,
    ) -> bool {
        let subtype = dictionary.get(b"Subtype").and_then(Object::as_name).ok();
        if annotation {
            return SKIPPED_ANNOTATION_KEYS.contains(&key)
                || key == b"StructParent"
                || (key == b"Parent" && subtype == Some(b"Widget"))
                || (key == b"P"
                    && matches!(value, Object::Reference(id) if self.page_numbers.contains_key(id)));
        }
        // XObjects may omit /Type. Only stream dictionaries can be
        // recognized this way; arbitrary resource and glyph maps retain
        // every entry, including names that happen to be bookkeeping here.
        if stream && matches!(subtype, Some(b"Form" | b"Image")) {
            return key == b"Metadata"
                || key == b"StructParent"
                || key == b"StructParents"
                || (subtype == Some(b"Form") && matches!(key, b"PieceInfo" | b"LastModified"));
        }
        key == b"Metadata"
            && matches!(
                dictionary.get(b"Type").and_then(Object::as_name).ok(),
                Some(b"Font" | b"FontDescriptor")
            )
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
            if let Some(data) = decode_content(stream, MAX_CONTENT_BYTES) {
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

    fn inherited_attributes(&mut self, page: &Dictionary) -> Result<InheritedAttributes> {
        let mut attributes = InheritedAttributes::own(page);
        if !attributes.complete() {
            if let Ok(Object::Reference(parent)) = page.get(b"Parent") {
                attributes.fill_from(&self.ancestor_attributes(*parent)?);
            }
        }
        Ok(attributes)
    }

    /// The inheritable attributes a child of page-tree node `id` sees:
    /// the node's own, with gaps filled from its ancestors. Walks up
    /// iteratively to the nearest cached node, then caches every node passed.
    fn ancestor_attributes(&mut self, id: ObjectId) -> Result<InheritedAttributes> {
        let mut chain: Vec<(ObjectId, InheritedAttributes)> = Vec::new();
        let mut next = Some(id);
        let mut inherited = InheritedAttributes::default();
        while let Some(id) = next {
            if let Some(cached) = self.ancestors.get(&id) {
                inherited = cached.clone();
                break;
            }
            if chain.len() >= MAX_CHAIN || chain.iter().any(|(seen, _)| *seen == id) {
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
            let own = InheritedAttributes::own(node);
            next = match node.get(b"Parent") {
                Ok(Object::Reference(parent)) if !own.complete() => Some(*parent),
                _ => None,
            };
            chain.push((id, own));
        }
        for (id, mut own) in chain.into_iter().rev() {
            own.fill_from(&inherited);
            self.ancestors.insert(id, own.clone());
            inherited = own;
        }
        Ok(inherited)
    }
}

#[derive(Debug, Clone, Default)]
struct InheritedAttributes {
    resources: Option<Object>,
    media_box: Option<Object>,
    crop_box: Option<Object>,
    rotate: Option<Object>,
}

impl InheritedAttributes {
    fn own(node: &Dictionary) -> Self {
        Self {
            resources: node.get(b"Resources").ok().cloned(),
            media_box: node.get(b"MediaBox").ok().cloned(),
            crop_box: node.get(b"CropBox").ok().cloned(),
            rotate: node.get(b"Rotate").ok().cloned(),
        }
    }

    fn complete(&self) -> bool {
        self.resources.is_some()
            && self.media_box.is_some()
            && self.crop_box.is_some()
            && self.rotate.is_some()
    }

    /// Fill each missing attribute from `ancestor`.
    fn fill_from(&mut self, ancestor: &Self) {
        for (slot, value) in [
            (&mut self.resources, &ancestor.resources),
            (&mut self.media_box, &ancestor.media_box),
            (&mut self.crop_box, &ancestor.crop_box),
            (&mut self.rotate, &ancestor.rotate),
        ] {
            if slot.is_none() {
                *slot = value.clone();
            }
        }
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

/// The text a text-showing operation draws (`Tj`, `'`, `"`, `TJ` strings
/// concatenated) and the index of the operand holding it.
fn shown_text(operation: &Operation) -> Option<(usize, Cow<'_, [u8]>)> {
    let index = match operation.operator.as_str() {
        "Tj" | "'" | "TJ" => 0,
        "\"" => 2,
        _ => return None,
    };
    let text = match operation.operands.get(index)? {
        Object::String(bytes, _) => Cow::Borrowed(bytes.as_slice()),
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
    Some((index, text))
}

/// Operands to mask as volatile stamps, keyed by operation index.
///
/// Recognize only the known PJe footer layout: consecutive single-show text
/// objects with the same explicit 7-point font and identity text matrices,
/// at (70, -18) for the download line and (70, -28) for the document number.
/// A page/form transform can place this strip, but no operations may change
/// the graphics state between its lines. Other layouts remain unmasked.
///
/// Content order alone does not locate text: a body quotation may be emitted
/// immediately before the footer. Multiple possible pairings are ambiguous
/// too, so neither line in such a pairing is masked.
fn volatile_operands(operations: &[Operation]) -> HashMap<usize, usize> {
    struct Line<'a> {
        font: &'a [u8],
        stamp: bool,
        operand: usize,
    }

    fn footer_line(operations: &[Operation]) -> Option<Line<'_>> {
        let [begin, font, matrix, show, end] = operations else {
            return None;
        };
        if begin.operator != "BT"
            || !begin.operands.is_empty()
            || font.operator != "Tf"
            || matrix.operator != "Tm"
            || !matches!(show.operator.as_str(), "Tj" | "TJ")
            || show.operands.len() != 1
            || end.operator != "ET"
            || !end.operands.is_empty()
        {
            return None;
        }
        let [Object::Name(font), size] = font.operands.as_slice() else {
            return None;
        };
        if size.as_float().ok() != Some(7.0) {
            return None;
        }
        let (operand, text) = shown_text(show)?;
        let stamp = is_pje_generated_by(&text);
        let y = if stamp {
            -18.0
        } else if is_pje_document_number(&text) {
            -28.0
        } else {
            return None;
        };
        let expected = [1.0, 0.0, 0.0, 1.0, 70.0, y];
        if matrix.operands.len() != expected.len()
            || !matrix
                .operands
                .iter()
                .zip(expected)
                .all(|(value, expected)| value.as_float().ok() == Some(expected))
        {
            return None;
        }
        Some(Line {
            font,
            stamp,
            operand,
        })
    }

    let mut pairs = Vec::new();
    let mut uses = HashMap::<usize, usize>::new();
    for (start, pair) in operations.windows(10).enumerate() {
        let (Some(first), Some(second)) = (footer_line(&pair[..5]), footer_line(&pair[5..])) else {
            continue;
        };
        if first.font != second.font || first.stamp == second.stamp {
            continue;
        }
        let (stamp, number, operand) = if first.stamp {
            (start + 3, start + 8, first.operand)
        } else {
            (start + 8, start + 3, second.operand)
        };
        pairs.push((stamp, number, operand));
        *uses.entry(stamp).or_default() += 1;
        *uses.entry(number).or_default() += 1;
    }
    pairs
        .into_iter()
        .filter(|(stamp, number, _)| uses[stamp] == 1 && uses[number] == 1)
        .map(|(stamp, _, operand)| (stamp, operand))
        .collect()
}

/// PJe stamp: `Este documento foi gerado pelo usuário <user> em dd/mm/yyyy
/// hh:mm:ss`, in WinAnsi (`á` = 0xE1) or UTF-8. The whole string must match.
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

/// PJe stamp: `Número do documento: <digits>`, in WinAnsi (`ú` = 0xFA) or
/// UTF-8. The whole string must match.
fn is_pje_document_number(text: &[u8]) -> bool {
    let Some(digits) = text
        .strip_prefix(b"N\xfamero do documento: ".as_slice())
        .or_else(|| text.strip_prefix("Número do documento: ".as_bytes()))
    else {
        return false;
    };
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

/// Every name token in raw content bytes, with `#xx` escapes decoded (ISO
/// 32000-1 §7.3.5), so `/F#31` counts as `F1`.
fn name_tokens(content: &[u8]) -> std::collections::HashSet<Vec<u8>> {
    const DELIMITERS: &[u8] = b"()<>[]{}/%";
    let mut names = std::collections::HashSet::new();
    let mut index = 0;
    while let Some(offset) = memchr::memchr(b'/', &content[index..]) {
        let mut position = index + offset + 1;
        let mut name = Vec::new();
        while let Some(&byte) = content.get(position) {
            if byte.is_ascii_whitespace() || byte == 0 || DELIMITERS.contains(&byte) {
                break;
            }
            let escaped = (byte == b'#')
                .then(|| content.get(position + 1..position + 3))
                .flatten()
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            match escaped {
                Some(decoded) => {
                    name.push(decoded);
                    position += 3;
                }
                None => {
                    name.push(byte);
                    position += 1;
                }
            }
        }
        if !name.is_empty() {
            names.insert(name);
        }
        index = position.max(index + offset + 1);
    }
    names
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

/// SHA-256 of the inflated, un-predicted `data`. `None` when it does not
/// decode cleanly within [`MAX_INFLATED_BYTES`]; the caller then hashes the
/// raw bytes — a mismatch at worst, never a collision.
fn inflate_digest(data: &[u8], predictor: Predictor) -> Option<[u8; 32]> {
    let mut hasher = Sha256::new();
    inflate(data, predictor, MAX_INFLATED_BYTES, |bytes| {
        hasher.update(bytes)
    })?;
    Some(hasher.finalize().into())
}

/// Inflate zlib `data` and undo `predictor`, handing decoded bytes to `sink`
/// row by row, so a large image never sits decoded in memory. Stops with
/// `None` once more than `limit` bytes have been inflated — the work is
/// bounded by `limit`, not by how far the data expands — and on bad zlib
/// data, an unknown PNG filter type or a truncated last row (which cannot be
/// un-predicted the way another producer would have).
fn inflate(
    data: &[u8],
    predictor: Predictor,
    limit: usize,
    mut sink: impl FnMut(&[u8]),
) -> Option<()> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let (row_len, prefix) = match predictor {
        Predictor::None => (0, 0),
        Predictor::Png { row, .. } => (row, 1),
        Predictor::Tiff { row, .. } => (row, 0),
    };
    let mut previous = vec![0u8; row_len];
    let mut current = vec![0u8; row_len + prefix];
    let mut filled = 0usize;
    let mut inflated = 0usize;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match decoder.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        inflated += read;
        if inflated > limit {
            return None;
        }
        if predictor == Predictor::None {
            sink(&buffer[..read]);
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
                sink(&current[prefix..]);
                previous.copy_from_slice(&current[prefix..]);
                filled = 0;
            }
        }
    }
    (filled == 0).then_some(())
}

/// Decode a content stream to at most `limit` bytes, or `None` (the caller
/// then hashes it undecoded). Flate inflates incrementally and stops at the
/// limit; the other filters decode in one pass, so each runs only when the
/// worst-case expansion of its input fits the limit. Unknown filters and
/// malformed filter entries also give `None`.
fn decode_content(stream: &Stream, limit: usize) -> Option<Vec<u8>> {
    if !stream.dict.has(b"Filter") {
        return (stream.content.len() <= limit).then(|| stream.content.clone());
    }
    let filters = crate::filter::stream_filters(stream)?;
    let params = stream.dict.get(b"DecodeParms").ok();
    let mut data = Cow::Borrowed(stream.content.as_slice());
    for (index, filter) in filters.iter().enumerate() {
        let filter_params = crate::filter::decode_params_at(params, index);
        // Parameters that are there but not readable here (an indirect
        // reference) would decode as if absent: two streams with equal bytes
        // but different predictors must not hash alike, so leave undecoded.
        if filter_params.is_none() && !decode_params_absent(params, index) {
            return None;
        }
        let decoded = if filter.as_slice() == b"FlateDecode" {
            let predictor = Predictor::from_params(filter_params)?;
            let mut out = Vec::new();
            inflate(&data, predictor, limit, |bytes| {
                out.extend_from_slice(bytes)
            })?;
            out
        } else {
            let expansion = match filter.as_slice() {
                b"ASCIIHexDecode" => 1,
                // `z` stands for four zero bytes.
                b"ASCII85Decode" => 4,
                // A two-byte repeat run writes up to 128 bytes.
                b"RunLengthDecode" => 64,
                // A 9-bit code can stand for a 4096-byte string.
                b"LZWDecode" => 4096,
                _ => return None,
            };
            if data.len().saturating_mul(expansion) > limit {
                return None;
            }
            crate::filter::decode_filter(filter, &data, filter_params).ok()?
        };
        if decoded.len() > limit {
            return None;
        }
        data = Cow::Owned(decoded);
    }
    Some(data.into_owned())
}

/// Whether filter `index` genuinely has no `/DecodeParms`: the entry is
/// missing, `null`, or an array slot holding `null` (or none at all).
fn decode_params_absent(params: Option<&Object>, index: usize) -> bool {
    match params {
        None | Some(Object::Null) => true,
        Some(Object::Array(items)) => matches!(items.get(index), None | Some(Object::Null)),
        Some(_) => false,
    }
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
    use std::io::Write;

    use lopdf::{dictionary, Stream};

    use lopdf::{content::Operation, Object};

    use super::{
        decode_content, inflate, is_pje_document_number, is_pje_generated_by, name_tokens,
        resource_operand, Predictor, ResourceType,
    };

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn operation(operator: &str, operands: Vec<Object>) -> Operation {
        Operation::new(operator, operands)
    }

    fn name(value: &str) -> Object {
        Object::Name(value.as_bytes().to_vec())
    }

    #[test]
    fn resource_operand_picks_the_name_each_operator_takes() {
        let cases = [
            // `/F1 12 Tf`: the name comes first, the size after it.
            (
                operation("Tf", vec![name("F1"), 12.into()]),
                Some((0, ResourceType::Font)),
            ),
            (
                operation("Do", vec![name("Im0")]),
                Some((0, ResourceType::XObject)),
            ),
            (
                operation("gs", vec![name("GS1")]),
                Some((0, ResourceType::ExtGState)),
            ),
            (
                operation("sh", vec![name("Sh0")]),
                Some((0, ResourceType::Shading)),
            ),
            (
                operation("cs", vec![name("CS0")]),
                Some((0, ResourceType::ColorSpace)),
            ),
            (
                operation("CS", vec![name("CS0")]),
                Some((0, ResourceType::ColorSpace)),
            ),
            // Uncoloured tiling pattern: components first, the pattern name last.
            (
                operation("scn", vec![0.2.into(), 0.4.into(), 0.6.into(), name("P0")]),
                Some((3, ResourceType::Pattern)),
            ),
            (
                operation("SCN", vec![name("P0")]),
                Some((0, ResourceType::Pattern)),
            ),
            // Plain colour components name nothing.
            (
                operation("scn", vec![0.2.into(), 0.4.into(), 0.6.into()]),
                None,
            ),
            // `/Tag /Props BDC`: the tag is literal, the properties name second.
            (
                operation("BDC", vec![name("OC"), name("MC0")]),
                Some((1, ResourceType::Properties)),
            ),
            (
                operation("DP", vec![name("Mark"), name("MC0")]),
                Some((1, ResourceType::Properties)),
            ),
            // Inline properties dictionary: nothing to resolve.
            (
                operation(
                    "BDC",
                    vec![name("Span"), Object::Dictionary(Default::default())],
                ),
                None,
            ),
            (operation("BMC", vec![name("Span")]), None),
            (operation("Tj", vec![Object::string_literal("F1")]), None),
        ];
        for (op, expected) in cases {
            assert_eq!(
                resource_operand(&op),
                expected,
                "{} {:?}",
                op.operator,
                op.operands
            );
        }
    }

    #[test]
    fn name_tokens_decode_escapes_and_stop_at_delimiters() {
        let names = name_tokens(b"q /F#31 12 Tf/Im0 Do[/CS0]<</Pat#20A 1>>/ BI /W 2 ID \x00/Z EI");
        for expected in [&b"F1"[..], b"Im0", b"CS0", b"Pat A", b"W", b"Z"] {
            assert!(
                names.contains(expected),
                "{}",
                String::from_utf8_lossy(expected)
            );
        }
        assert!(!names.contains(&b"F#31"[..]));
        assert!(!names.contains(&b""[..]));
    }

    #[test]
    fn matches_pje_download_stamp_in_winansi_and_utf8() {
        assert!(is_pje_generated_by(
            b"Este documento foi gerado pelo usu\xe1rio 569.***.***-04 em 17/08/2026 13:43:50"
        ));
        assert!(is_pje_generated_by(
            "Este documento foi gerado pelo usuário 123.***.***-00 em 01/01/2025 00:00:00"
                .as_bytes()
        ));
        assert!(is_pje_document_number(
            b"N\xfamero do documento: 26071613525300000000039184434"
        ));
        assert!(is_pje_document_number("Número do documento: 1".as_bytes()));
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
            assert!(
                !is_pje_generated_by(text),
                "{}",
                String::from_utf8_lossy(text)
            );
        }
        for text in [
            b"N\xfamero do documento: ".as_slice(),
            b"N\xfamero do documento: 123 ",
            b"N\xfamero do documento: 12a",
        ] {
            assert!(
                !is_pje_document_number(text),
                "{}",
                String::from_utf8_lossy(text)
            );
        }
    }

    #[test]
    fn inflate_stops_past_the_limit() {
        let data = zlib(&vec![0u8; 1024 * 1024]);
        let mut total = 0usize;
        assert!(
            inflate(&data, Predictor::None, 1024 * 1024, |bytes| total +=
                bytes.len())
            .is_some()
        );
        assert_eq!(total, 1024 * 1024);
        assert!(inflate(&data, Predictor::None, 1024, |_| {}).is_none());
    }

    #[test]
    fn content_decoding_is_bounded_for_every_filter() {
        let flate = Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            zlib(&vec![b' '; 100_000]),
        );
        assert_eq!(decode_content(&flate, 100_000).unwrap().len(), 100_000);
        assert!(decode_content(&flate, 99_999).is_none());

        // 2 bytes of RunLength can write 128: refused unless the worst case fits.
        let run_length = Stream::new(
            dictionary! { "Filter" => "RunLengthDecode" },
            vec![129, b' ', 128],
        );
        assert_eq!(
            decode_content(&run_length, 3 * 64).unwrap(),
            vec![b' '; 128]
        );
        assert!(decode_content(&run_length, 3 * 64 - 1).is_none());

        let unfiltered = Stream::new(dictionary! {}, b"q Q".to_vec());
        assert!(decode_content(&unfiltered, 3).is_some());
        assert!(decode_content(&unfiltered, 2).is_none());
    }
}
