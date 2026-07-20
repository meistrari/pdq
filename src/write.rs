use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
    rc::Rc,
};

use lopdf::{dictionary, Dictionary, Object, ObjectId, StringFormat};

use crate::{
    copy::{references_document_structure, CopiedDocumentMetadata, CopyOptions, ObjectSource},
    PdfOpsError, Result,
};

const INHERITABLE_PAGE_ATTRS: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
const MAX_COPY_DEPTH: usize = 256;

pub(crate) struct StreamingPdfWriter {
    output: CountingWriter<BufWriter<File>>,
    offsets: BTreeMap<u32, u32>,
    max_id: u32,
    pages_id: ObjectId,
    catalog_id: ObjectId,
    pages: Vec<ObjectId>,
    document_metadata: CopiedDocumentMetadata,
}

impl StreamingPdfWriter {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut output = CountingWriter::new(BufWriter::new(file));
        output.write_all(b"%PDF-1.7\n%\xC7\xEC\x8F\xA2\n")?;

        let mut writer = Self {
            output,
            offsets: BTreeMap::new(),
            max_id: 0,
            pages_id: (0, 0),
            catalog_id: (0, 0),
            pages: Vec::new(),
            document_metadata: CopiedDocumentMetadata::default(),
        };
        writer.pages_id = writer.new_object_id();
        writer.catalog_id = writer.new_object_id();
        Ok(writer)
    }

    pub(crate) fn new_object_id(&mut self) -> ObjectId {
        self.max_id += 1;
        (self.max_id, 0)
    }

    pub(crate) fn pages_id(&self) -> ObjectId {
        self.pages_id
    }

    pub(crate) fn extend_pages(&mut self, pages: impl IntoIterator<Item = ObjectId>) {
        self.pages.extend(pages);
    }

    /// Record document metadata (already written as objects) to be attached
    /// at `finish`: `/Info` on the trailer, XMP `/Metadata` on the catalog.
    pub(crate) fn set_document_metadata(&mut self, metadata: CopiedDocumentMetadata) {
        self.document_metadata = metadata;
    }

    pub(crate) fn write_object(&mut self, id: ObjectId, object: &Object) -> Result<()> {
        let offset = u32::try_from(self.output.bytes_written()).map_err(|_| {
            PdfOpsError::InvalidStructure("streaming output offset exceeds PDF xref limit".into())
        })?;
        self.offsets.insert(id.0, offset);
        writeln!(self.output, "{} {} obj", id.0, id.1)?;
        write_object(&mut self.output, object)?;
        self.output.write_all(b"\nendobj\n")?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let kids = self
            .pages
            .iter()
            .copied()
            .map(Object::Reference)
            .collect::<Vec<_>>();
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => self.pages.len() as i64,
        };
        self.write_object(self.pages_id, &Object::Dictionary(pages))?;

        let mut catalog = dictionary! {
            "Type" => "Catalog",
            "Pages" => self.pages_id,
        };
        if let Some(xmp) = &self.document_metadata.xmp {
            catalog.set("Metadata", xmp.clone());
        }
        self.write_object(self.catalog_id, &Object::Dictionary(catalog))?;

        let xref_start = self.output.bytes_written();
        self.output.write_all(b"xref\n")?;
        writeln!(self.output, "0 {}", self.max_id + 1)?;
        self.output.write_all(b"0000000000 65535 f \n")?;
        for id in 1..=self.max_id {
            let offset = self.offsets.get(&id).copied().ok_or_else(|| {
                PdfOpsError::InvalidStructure(format!("missing xref offset for object {id}"))
            })?;
            writeln!(self.output, "{offset:010} 00000 n ")?;
        }
        self.output.write_all(b"trailer\n")?;
        let mut trailer = dictionary! {
            "Size" => (self.max_id + 1) as i64,
            "Root" => self.catalog_id,
        };
        if let Some(info) = &self.document_metadata.info {
            trailer.set("Info", info.clone());
        }
        write_dictionary(&mut self.output, &trailer)?;
        write!(self.output, "\nstartxref\n{xref_start}\n%%EOF")?;
        self.output.flush()?;
        Ok(())
    }
}

pub(crate) struct StreamingCopyContext<'a> {
    writer: &'a mut StreamingPdfWriter,
    object_map: BTreeMap<ObjectId, ObjectId>,
    inherited_attrs_cache: BTreeMap<ObjectId, Rc<InheritedPageAttrs>>,
    selected_pages: BTreeSet<ObjectId>,
    options: CopyOptions,
    sanitize_structure_refs: bool,
}

impl<'a> StreamingCopyContext<'a> {
    pub(crate) fn new(writer: &'a mut StreamingPdfWriter, options: CopyOptions) -> Self {
        Self {
            writer,
            object_map: BTreeMap::new(),
            inherited_attrs_cache: BTreeMap::new(),
            selected_pages: BTreeSet::new(),
            options,
            sanitize_structure_refs: false,
        }
    }

    /// Copy the source's document-level metadata objects — the trailer `/Info`
    /// dictionary and the catalog's XMP `/Metadata` stream — through the
    /// streaming writer. Mirrors `CopyContext::copy_document_metadata_objects`:
    /// best-effort (metadata must never fail the merge) and sanitized (a
    /// malformed metadata object must not smuggle the source page tree into
    /// the output).
    pub(crate) fn copy_document_metadata_objects(
        &mut self,
        source: &impl ObjectSource,
    ) -> CopiedDocumentMetadata {
        let mut metadata = CopiedDocumentMetadata::default();
        if let Some(info) = source.trailer_value(b"Info") {
            metadata.info = self
                .copy_sanitized_value(source, &info, 0)
                .ok()
                .and_then(|copied| match copied {
                    reference @ Object::Reference(_) => Some(reference),
                    Object::Dictionary(dictionary) => {
                        let id = self.writer.new_object_id();
                        self.writer
                            .write_object(id, &Object::Dictionary(dictionary))
                            .ok()?;
                        Some(Object::Reference(id))
                    }
                    _ => None,
                });
        }
        if let Some(Object::Reference(root_id)) = source.trailer_value(b"Root") {
            let xmp = source
                .get_object_value(root_id)
                .ok()
                .and_then(|root| root.as_dict().ok()?.get(b"Metadata").ok().cloned());
            if let Some(value) = xmp {
                metadata.xmp = self
                    .copy_sanitized_value(source, &value, 0)
                    .ok()
                    .filter(|copied| matches!(copied, Object::Reference(_)));
            }
        }
        metadata
    }

    /// See `CopyContext::copy_sanitized_value`.
    fn copy_sanitized_value(
        &mut self,
        source: &impl ObjectSource,
        value: &Object,
        depth: usize,
    ) -> Result<Object> {
        let previous = self.sanitize_structure_refs;
        self.sanitize_structure_refs = true;
        let result = self.copy_value(source, value, depth);
        self.sanitize_structure_refs = previous;
        result
    }

    /// See [`references_document_structure`].
    fn is_document_structure_ref(&self, source: &impl ObjectSource, id: ObjectId) -> bool {
        references_document_structure(source, &self.object_map, id)
    }

    pub(crate) fn copy_pages(
        &mut self,
        source: &impl ObjectSource,
        page_ids: &[ObjectId],
    ) -> Result<Vec<ObjectId>> {
        let mut copied_pages = Vec::with_capacity(page_ids.len());
        for page_id in page_ids {
            copied_pages.push(self.copy_page(source, *page_id)?);
        }
        Ok(copied_pages)
    }

    fn copy_page(&mut self, source: &impl ObjectSource, page_id: ObjectId) -> Result<ObjectId> {
        let new_id = if self.selected_pages.contains(&page_id) {
            self.copy_page_instance(source, page_id)?
        } else {
            let new_id = self.copy_object_at_depth(source, page_id, 0)?;
            self.selected_pages.insert(page_id);
            new_id
        };
        Ok(new_id)
    }

    fn copy_page_instance(
        &mut self,
        source: &impl ObjectSource,
        old_page_id: ObjectId,
    ) -> Result<ObjectId> {
        let object = source
            .get_object_value(old_page_id)
            .map_err(PdfOpsError::Pdf)?;
        let page = object
            .as_dict()
            .map_err(|_| PdfOpsError::InvalidStructure("page is not a dictionary".into()))?;
        if !page.has_type(b"Page") {
            return Err(PdfOpsError::InvalidStructure(
                "copied page does not have /Type /Page".into(),
            ));
        }

        let new_id = self.writer.new_object_id();
        let previous = self.object_map.insert(old_page_id, new_id);
        let copied = match self.copy_page_dictionary(source, old_page_id, page, 0) {
            Ok(copied) => copied,
            Err(err) => {
                restore_object_mapping(&mut self.object_map, old_page_id, previous);
                return Err(err);
            }
        };
        self.writer
            .write_object(new_id, &Object::Dictionary(copied))?;
        restore_object_mapping(&mut self.object_map, old_page_id, previous);
        Ok(new_id)
    }

    fn copy_object_at_depth(
        &mut self,
        source: &impl ObjectSource,
        old_id: ObjectId,
        depth: usize,
    ) -> Result<ObjectId> {
        check_copy_depth(depth)?;
        if let Some(new_id) = self.object_map.get(&old_id) {
            return Ok(*new_id);
        }

        let new_id = self.writer.new_object_id();
        self.object_map.insert(old_id, new_id);

        let object = match source.get_object_value(old_id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => {
                self.writer.write_object(new_id, &Object::Null)?;
                return Ok(new_id);
            }
            Err(err) => return Err(PdfOpsError::Pdf(err)),
        };

        let copied = match object {
            Cow::Borrowed(object) => match object {
                Object::Dictionary(dict) if dict.has_type(b"Page") => Object::Dictionary(
                    self.copy_page_dictionary(source, old_id, dict, depth + 1)?,
                ),
                Object::Stream(stream) => {
                    Object::Stream(self.copy_stream(source, stream.clone(), depth + 1)?)
                }
                _ => self.copy_value(source, object, depth + 1)?,
            },
            Cow::Owned(object) => match object {
                Object::Dictionary(dict) if dict.has_type(b"Page") => Object::Dictionary(
                    self.copy_page_dictionary(source, old_id, &dict, depth + 1)?,
                ),
                Object::Stream(stream) => {
                    Object::Stream(self.copy_stream(source, stream, depth + 1)?)
                }
                object => self.copy_owned_value(source, object, depth + 1)?,
            },
        };
        self.writer.write_object(new_id, &copied)?;
        Ok(new_id)
    }

    fn copy_page_dictionary(
        &mut self,
        source: &impl ObjectSource,
        old_page_id: ObjectId,
        page: &Dictionary,
        depth: usize,
    ) -> Result<Dictionary> {
        check_copy_depth(depth)?;
        let mut copied = Dictionary::new();
        for (key, value) in page.iter() {
            if key.as_slice() == b"Parent" {
                continue;
            }
            if key.as_slice() == b"Annots" {
                if !self.options.copy_annotations {
                    continue;
                }
                copied.set(
                    key.clone(),
                    self.copy_sanitized_value(source, value, depth + 1)?,
                );
                continue;
            }
            copied.set(key.clone(), self.copy_value(source, value, depth + 1)?);
        }

        for key in INHERITABLE_PAGE_ATTRS {
            if copied.has(key) {
                continue;
            }
            if let Some(value) = self.inherited_attr(source, old_page_id, page, key)? {
                copied.set(key.to_vec(), self.copy_value(source, &value, depth + 1)?);
            }
        }
        copied.set("Parent", self.writer.pages_id());

        Ok(copied)
    }

    fn copy_value(
        &mut self,
        source: &impl ObjectSource,
        value: &Object,
        depth: usize,
    ) -> Result<Object> {
        check_copy_depth(depth)?;
        match value {
            Object::Reference(id) => {
                if self.sanitize_structure_refs && self.is_document_structure_ref(source, *id) {
                    return Ok(Object::Null);
                }
                Ok(Object::Reference(self.copy_object_at_depth(
                    source,
                    *id,
                    depth + 1,
                )?))
            }
            Object::Array(items) => {
                let mut copied = Vec::with_capacity(items.len());
                for item in items {
                    copied.push(self.copy_value(source, item, depth + 1)?);
                }
                Ok(Object::Array(copied))
            }
            Object::Dictionary(dict) => {
                let mut copied = Dictionary::new();
                for (key, value) in dict.iter() {
                    // Same /Kids drop as CopyContext::copy_dictionary (see the
                    // comment there): AcroForm field nodes reached from a
                    // sanitized subtree must not drag sibling widgets from
                    // other pages along.
                    if self.sanitize_structure_refs && key.as_slice() == b"Kids" {
                        continue;
                    }
                    copied.set(key.clone(), self.copy_value(source, value, depth + 1)?);
                }
                Ok(Object::Dictionary(copied))
            }
            Object::Stream(stream) => Ok(Object::Stream(self.copy_stream(
                source,
                stream.clone(),
                depth + 1,
            )?)),
            _ => Ok(value.clone()),
        }
    }

    fn copy_owned_value(
        &mut self,
        source: &impl ObjectSource,
        value: Object,
        depth: usize,
    ) -> Result<Object> {
        check_copy_depth(depth)?;
        match value {
            Object::Reference(id) => {
                if self.sanitize_structure_refs && self.is_document_structure_ref(source, id) {
                    return Ok(Object::Null);
                }
                Ok(Object::Reference(self.copy_object_at_depth(
                    source,
                    id,
                    depth + 1,
                )?))
            }
            Object::Array(items) => {
                let mut copied = Vec::with_capacity(items.len());
                for item in items {
                    copied.push(self.copy_owned_value(source, item, depth + 1)?);
                }
                Ok(Object::Array(copied))
            }
            Object::Dictionary(dict) => {
                let mut copied = Dictionary::new();
                for (key, value) in dict {
                    if self.sanitize_structure_refs && key.as_slice() == b"Kids" {
                        continue;
                    }
                    copied.set(key, self.copy_owned_value(source, value, depth + 1)?);
                }
                Ok(Object::Dictionary(copied))
            }
            Object::Stream(stream) => Ok(Object::Stream(self.copy_stream(
                source,
                stream,
                depth + 1,
            )?)),
            value => Ok(value),
        }
    }

    fn copy_stream(
        &mut self,
        source: &impl ObjectSource,
        mut stream: lopdf::Stream,
        depth: usize,
    ) -> Result<lopdf::Stream> {
        stream.dict = self.copy_dictionary(source, &stream.dict, depth + 1)?;
        Ok(stream)
    }

    fn copy_dictionary(
        &mut self,
        source: &impl ObjectSource,
        dict: &Dictionary,
        depth: usize,
    ) -> Result<Dictionary> {
        check_copy_depth(depth)?;
        let mut copied = Dictionary::new();
        for (key, value) in dict.iter() {
            copied.set(key.clone(), self.copy_value(source, value, depth + 1)?);
        }
        Ok(copied)
    }

    fn inherited_attr(
        &mut self,
        source: &impl ObjectSource,
        page_id: ObjectId,
        page: &Dictionary,
        key: &[u8],
    ) -> Result<Option<Object>> {
        if let Ok(value) = page.get(key) {
            return Ok(Some(value.clone()));
        }

        let mut current = match page.get(b"Parent") {
            Ok(Object::Reference(parent)) => *parent,
            Ok(_) => {
                return Err(PdfOpsError::InvalidStructure(
                    "page tree parent is not a reference".into(),
                ));
            }
            Err(_) => return Ok(None),
        };
        let mut visited = BTreeSet::from([page_id]);

        loop {
            if !visited.insert(current) {
                return Err(PdfOpsError::InvalidStructure(
                    "cycle detected while resolving inherited page attributes".into(),
                ));
            }

            let Some(attrs) = self.inherited_attrs(source, current)? else {
                return Ok(None);
            };
            if let Some(value) = attrs.get(key) {
                return Ok(Some(value.clone()));
            }
            match attrs.parent {
                ParentRef::Reference(parent) => current = parent,
                ParentRef::Missing => return Ok(None),
                ParentRef::Invalid => {
                    return Err(PdfOpsError::InvalidStructure(
                        "page tree parent is not a reference".into(),
                    ));
                }
            }
        }
    }

    fn inherited_attrs(
        &mut self,
        source: &impl ObjectSource,
        id: ObjectId,
    ) -> Result<Option<Rc<InheritedPageAttrs>>> {
        if let Some(attrs) = self.inherited_attrs_cache.get(&id) {
            return Ok(Some(Rc::clone(attrs)));
        }

        let object = match source.get_object_value(id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
            Err(err) => return Err(PdfOpsError::Pdf(err)),
        };
        let dict = object.as_dict().map_err(|_| {
            PdfOpsError::InvalidStructure("page tree node is not a dictionary".into())
        })?;
        let attrs = Rc::new(InheritedPageAttrs::from_dict(dict));
        self.inherited_attrs_cache.insert(id, Rc::clone(&attrs));
        Ok(Some(attrs))
    }
}

#[derive(Debug, Default)]
struct InheritedPageAttrs {
    resources: Option<Object>,
    media_box: Option<Object>,
    crop_box: Option<Object>,
    rotate: Option<Object>,
    parent: ParentRef,
}

impl InheritedPageAttrs {
    fn from_dict(dict: &Dictionary) -> Self {
        Self {
            resources: dict.get(b"Resources").ok().cloned(),
            media_box: dict.get(b"MediaBox").ok().cloned(),
            crop_box: dict.get(b"CropBox").ok().cloned(),
            rotate: dict.get(b"Rotate").ok().cloned(),
            parent: match dict.get(b"Parent") {
                Ok(Object::Reference(parent)) => ParentRef::Reference(*parent),
                Ok(_) => ParentRef::Invalid,
                Err(_) => ParentRef::Missing,
            },
        }
    }

    fn get(&self, key: &[u8]) -> Option<&Object> {
        match key {
            b"Resources" => self.resources.as_ref(),
            b"MediaBox" => self.media_box.as_ref(),
            b"CropBox" => self.crop_box.as_ref(),
            b"Rotate" => self.rotate.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum ParentRef {
    Reference(ObjectId),
    #[default]
    Missing,
    Invalid,
}

fn restore_object_mapping(
    object_map: &mut BTreeMap<ObjectId, ObjectId>,
    old_id: ObjectId,
    previous: Option<ObjectId>,
) {
    if let Some(previous) = previous {
        object_map.insert(old_id, previous);
    } else {
        object_map.remove(&old_id);
    }
}

fn check_copy_depth(depth: usize) -> Result<()> {
    if depth > MAX_COPY_DEPTH {
        return Err(PdfOpsError::InvalidStructure(format!(
            "PDF object nesting exceeds maximum copy depth of {MAX_COPY_DEPTH}"
        )));
    }
    Ok(())
}

pub(crate) fn write_object(output: &mut dyn Write, object: &Object) -> std::io::Result<()> {
    match object {
        Object::Null => output.write_all(b"null"),
        Object::Boolean(value) => {
            if *value {
                output.write_all(b"true")
            } else {
                output.write_all(b"false")
            }
        }
        Object::Integer(value) => write!(output, "{value}"),
        Object::Real(value) => write!(output, "{value}"),
        Object::Name(name) => write_name(output, name),
        Object::String(text, format) => write_string(output, text, *format),
        Object::Array(items) => write_array(output, items),
        Object::Dictionary(dict) => write_dictionary(output, dict),
        Object::Stream(stream) => write_stream(output, stream),
        Object::Reference(id) => write!(output, "{} {} R", id.0, id.1),
    }
}

fn write_name(output: &mut dyn Write, name: &[u8]) -> std::io::Result<()> {
    output.write_all(b"/")?;
    for &byte in name {
        if b" \t\n\r\x0C()<>[]{}/%#".contains(&byte) || !(33..=126).contains(&byte) {
            write!(output, "#{byte:02X}")?;
        } else {
            output.write_all(&[byte])?;
        }
    }
    Ok(())
}

fn write_string(output: &mut dyn Write, text: &[u8], format: StringFormat) -> std::io::Result<()> {
    match format {
        StringFormat::Literal => {
            output.write_all(b"(")?;
            for &byte in text {
                match byte {
                    b'(' => output.write_all(b"\\(")?,
                    b')' => output.write_all(b"\\)")?,
                    b'\\' => output.write_all(b"\\\\")?,
                    b'\r' => output.write_all(b"\\r")?,
                    byte => output.write_all(&[byte])?,
                }
            }
            output.write_all(b")")
        }
        StringFormat::Hexadecimal => {
            output.write_all(b"<")?;
            for &byte in text {
                write!(output, "{byte:02X}")?;
            }
            output.write_all(b">")
        }
    }
}

fn write_array(output: &mut dyn Write, items: &[Object]) -> std::io::Result<()> {
    output.write_all(b"[")?;
    let mut first = true;
    for item in items {
        if first {
            first = false;
        } else if needs_separator(item) {
            output.write_all(b" ")?;
        }
        write_object(output, item)?;
    }
    output.write_all(b"]")
}

pub(crate) fn write_dictionary(
    output: &mut dyn Write,
    dictionary: &Dictionary,
) -> std::io::Result<()> {
    output.write_all(b"<<")?;
    for (key, value) in dictionary {
        write_name(output, key)?;
        if needs_separator(value) {
            output.write_all(b" ")?;
        }
        write_object(output, value)?;
    }
    output.write_all(b">>")
}

fn write_stream(output: &mut dyn Write, stream: &lopdf::Stream) -> std::io::Result<()> {
    write_dictionary(output, &stream.dict)?;
    output.write_all(b"stream\n")?;
    output.write_all(&stream.content)?;
    output.write_all(b"\nendstream")
}

fn needs_separator(object: &Object) -> bool {
    matches!(
        object,
        Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Reference(_)
    )
}

struct CountingWriter<W: Write> {
    inner: W,
    bytes_written: usize,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> usize {
        self.bytes_written
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let bytes = self.inner.write(buffer)?;
        self.bytes_written += bytes;
        Ok(bytes)
    }

    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.bytes_written += buffer.len();
        self.inner.write_all(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
