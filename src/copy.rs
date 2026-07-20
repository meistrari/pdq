use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
    rc::Rc,
    sync::Arc,
};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::{filter::decode_stream_content, scan, scan::UsedNames, PdfOpsError, Result};

const INHERITABLE_PAGE_ATTRS: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
const MAX_COPY_DEPTH: usize = 256;
const RESOURCE_PRUNE_MIN_NAMES: usize = 6;

pub(crate) trait ObjectSource {
    fn get_object_value(&self, id: ObjectId) -> std::result::Result<Cow<'_, Object>, lopdf::Error>;

    /// The source trailer's value for `key` (`/Info`, `/Root`), when the
    /// source exposes a trailer. Used to carry document-level metadata into
    /// outputs; sources that cannot provide one (test doubles) keep the
    /// default.
    fn trailer_value(&self, _key: &[u8]) -> Option<Object> {
        None
    }
}

/// Distilled inheritable page attributes per page-tree node. `Arc` (not `Rc`)
/// so a pre-warmed cache can be shared across parallel split workers.
pub(crate) type InheritedAttrsCache = BTreeMap<ObjectId, Arc<InheritedPageAttrs>>;

impl ObjectSource for Document {
    fn get_object_value(&self, id: ObjectId) -> std::result::Result<Cow<'_, Object>, lopdf::Error> {
        Ok(Cow::Borrowed(self.get_object(id)?))
    }

    fn trailer_value(&self, key: &[u8]) -> Option<Object> {
        self.trailer.get(key).ok().cloned()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CopyOptions {
    pub copy_annotations: bool,
    pub prune_resources: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            // Annotations carry user-visible content — link targets, form
            // widgets, and signature appearances (the gov.br stamp on Brazilian
            // court documents) — so dropping them loses real page content.
            // References back into document structure are sanitized during the
            // copy (see `copy_sanitized_value`), which is what made copying
            // them safe to enable.
            copy_annotations: true,
            prune_resources: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct CopyContext {
    object_map: BTreeMap<ObjectId, ObjectId>,
    dictionary_cache: BTreeMap<ObjectId, Rc<Dictionary>>,
    inherited_attrs_cache: InheritedAttrsCache,
    used_names_cache: BTreeMap<ObjectId, Option<UsedNames>>,
    selected_pages: BTreeSet<ObjectId>,
    options: CopyOptions,
    prune_nested_resources: bool,
    sanitize_structure_refs: bool,
}

impl CopyContext {
    pub fn new(options: CopyOptions) -> Self {
        Self::with_object_map(options, BTreeMap::new())
    }

    /// Create a context whose object map is pre-seeded with `old id -> new id`
    /// mappings. Seeded objects are treated as already copied: references to
    /// them are rewritten to the seeded ids and their contents are never
    /// visited. Used by the split-pages template writer, which serializes the
    /// shared object closure once and reuses it across every output.
    pub(crate) fn with_object_map(
        options: CopyOptions,
        object_map: BTreeMap<ObjectId, ObjectId>,
    ) -> Self {
        Self::with_state(options, object_map, BTreeMap::new())
    }

    /// Like [`Self::with_object_map`], additionally pre-warming the
    /// inherited-attributes cache. Resolving inheritable attributes otherwise
    /// re-fetches page-tree ancestors per context — for a flat 12,000-page
    /// tree that means cloning a 12,000-entry `/Kids` array per output.
    pub(crate) fn with_state(
        options: CopyOptions,
        object_map: BTreeMap<ObjectId, ObjectId>,
        inherited_attrs_cache: InheritedAttrsCache,
    ) -> Self {
        Self {
            object_map,
            dictionary_cache: BTreeMap::new(),
            inherited_attrs_cache,
            used_names_cache: BTreeMap::new(),
            selected_pages: BTreeSet::new(),
            options,
            prune_nested_resources: false,
            sanitize_structure_refs: false,
        }
    }

    /// Consume the context, returning the `old id -> new id` map of every
    /// source object visited by the copy (including seeded entries) plus the
    /// inherited-attributes cache accumulated along the way.
    pub(crate) fn into_state(self) -> (BTreeMap<ObjectId, ObjectId>, InheritedAttrsCache) {
        (self.object_map, self.inherited_attrs_cache)
    }

    pub(crate) fn copy_page(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        page_id: ObjectId,
    ) -> Result<ObjectId> {
        let new_id = if self.selected_pages.contains(&page_id) {
            self.copy_page_instance(source, target, page_id)?
        } else {
            let new_id = self.copy_object(source, target, page_id)?;
            self.selected_pages.insert(page_id);
            new_id
        };
        let page = target
            .get_object(new_id)
            .map_err(PdfOpsError::Pdf)?
            .as_dict()
            .map_err(|_| PdfOpsError::InvalidStructure("copied page is not a dictionary".into()))?;
        if !page.has_type(b"Page") {
            return Err(PdfOpsError::InvalidStructure(
                "copied page does not have /Type /Page".into(),
            ));
        }
        Ok(new_id)
    }

    fn copy_page_instance(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
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

        let new_id = target.new_object_id();
        let previous = self.object_map.insert(old_page_id, new_id);
        target.objects.insert(new_id, Object::Null);

        let copied = match self.copy_page_dictionary(source, target, old_page_id, page, 0) {
            Ok(copied) => copied,
            Err(err) => {
                restore_object_mapping(&mut self.object_map, old_page_id, previous);
                return Err(err);
            }
        };
        target.objects.insert(new_id, Object::Dictionary(copied));
        restore_object_mapping(&mut self.object_map, old_page_id, previous);
        Ok(new_id)
    }

    pub(crate) fn copy_object(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        old_id: ObjectId,
    ) -> Result<ObjectId> {
        self.copy_object_at_depth(source, target, old_id, 0)
    }

    fn copy_object_at_depth(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        old_id: ObjectId,
        depth: usize,
    ) -> Result<ObjectId> {
        check_copy_depth(depth)?;
        if let Some(new_id) = self.object_map.get(&old_id) {
            return Ok(*new_id);
        }

        let new_id = target.new_object_id();
        self.object_map.insert(old_id, new_id);
        target.objects.insert(new_id, Object::Null);

        let object = match source.get_object_value(old_id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => {
                target.objects.insert(new_id, Object::Null);
                return Ok(new_id);
            }
            Err(err) => return Err(PdfOpsError::Pdf(err)),
        };
        let copied = match object {
            Cow::Borrowed(object) => match object {
                Object::Dictionary(dict) if dict.has_type(b"Page") => Object::Dictionary(
                    self.copy_page_dictionary(source, target, old_id, dict, depth + 1)?,
                ),
                Object::Stream(stream) => Object::Stream(self.copy_stream(
                    source,
                    target,
                    stream.clone(),
                    Some(old_id),
                    depth + 1,
                )?),
                _ => self.copy_value(source, target, object, depth + 1)?,
            },
            Cow::Owned(object) => match object {
                Object::Dictionary(dict) if dict.has_type(b"Page") => Object::Dictionary(
                    self.copy_page_dictionary(source, target, old_id, &dict, depth + 1)?,
                ),
                Object::Stream(stream) => Object::Stream(self.copy_stream(
                    source,
                    target,
                    stream,
                    Some(old_id),
                    depth + 1,
                )?),
                object => self.copy_owned_value(source, target, object, depth + 1)?,
            },
        };
        target.objects.insert(new_id, copied);
        Ok(new_id)
    }

    fn copy_page_dictionary(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
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
                    self.copy_sanitized_value(source, target, value, depth + 1)?,
                );
                continue;
            }
            if key.as_slice() == b"Resources" {
                copied.set(
                    key.clone(),
                    self.copy_page_resources(source, target, old_page_id, page, value, depth + 1)?,
                );
            } else {
                copied.set(
                    key.clone(),
                    self.copy_value(source, target, value, depth + 1)?,
                );
            }
        }

        for key in INHERITABLE_PAGE_ATTRS {
            if copied.has(key) {
                continue;
            }
            if let Some(value) = self.inherited_attr(source, old_page_id, page, key)? {
                if key == b"Resources" {
                    copied.set(
                        key.to_vec(),
                        self.copy_page_resources(
                            source,
                            target,
                            old_page_id,
                            page,
                            &value,
                            depth + 1,
                        )?,
                    );
                } else {
                    copied.set(
                        key.to_vec(),
                        self.copy_value(source, target, &value, depth + 1)?,
                    );
                }
            }
        }

        Ok(copied)
    }

    fn copy_page_resources(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        _old_page_id: ObjectId,
        page: &Dictionary,
        value: &Object,
        depth: usize,
    ) -> Result<Object> {
        check_copy_depth(depth)?;
        if !self.options.prune_resources {
            return self.copy_value(source, target, value, depth + 1);
        }

        let Some(resources) = self.resolve_dictionary(source, value)? else {
            return self.copy_value(source, target, value, depth + 1);
        };
        if !self.should_prune_resources(source, &resources)? {
            return self.copy_value(source, target, value, depth + 1);
        }
        let Some(used) = scan::collect_used_names(
            source,
            page,
            &resources,
            &mut self.dictionary_cache,
            &mut self.used_names_cache,
        )?
        else {
            return self.copy_value(source, target, value, depth + 1);
        };
        match self.copy_pruned_resources(source, target, &resources, &used, depth + 1)? {
            Some(pruned) => Ok(pruned),
            None => self.copy_value(source, target, value, depth + 1),
        }
    }

    fn copy_pruned_resources(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        resources: &Dictionary,
        used: &UsedNames,
        depth: usize,
    ) -> Result<Option<Object>> {
        check_copy_depth(depth)?;
        let mut copied = Dictionary::new();
        for (key, value) in resources.iter() {
            if key.as_slice() == b"Font" || key.as_slice() == b"XObject" {
                let Some(resource_dict) = self.resolve_dictionary(source, value)? else {
                    return Ok(None);
                };
                let mut pruned = Dictionary::new();
                for (name, resource_value) in resource_dict.iter() {
                    if used.contains(name) {
                        pruned.set(
                            name.clone(),
                            self.copy_resource_value(source, target, resource_value, depth + 1)?,
                        );
                    }
                }
                copied.set(key.clone(), Object::Dictionary(pruned));
            } else {
                copied.set(
                    key.clone(),
                    self.copy_value(source, target, value, depth + 1)?,
                );
            }
        }
        Ok(Some(Object::Dictionary(copied)))
    }

    fn copy_value(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
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
                    target,
                    *id,
                    depth + 1,
                )?))
            }
            Object::Array(items) => {
                let mut copied = Vec::with_capacity(items.len());
                for item in items {
                    copied.push(self.copy_value(source, target, item, depth + 1)?);
                }
                Ok(Object::Array(copied))
            }
            Object::Dictionary(dict) => Ok(Object::Dictionary(
                self.copy_dictionary(source, target, dict, depth)?,
            )),
            Object::Stream(stream) => Ok(Object::Stream(self.copy_stream(
                source,
                target,
                stream.clone(),
                None,
                depth + 1,
            )?)),
            _ => Ok(value.clone()),
        }
    }

    fn copy_owned_value(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
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
                    target,
                    id,
                    depth + 1,
                )?))
            }
            Object::Array(items) => {
                let mut copied = Vec::with_capacity(items.len());
                for item in items {
                    copied.push(self.copy_owned_value(source, target, item, depth + 1)?);
                }
                Ok(Object::Array(copied))
            }
            Object::Dictionary(dict) => {
                let mut copied = lopdf::Dictionary::new();
                for (key, value) in dict {
                    // Same /Kids drop as `copy_dictionary` (see the comment
                    // there): field nodes reached from a sanitized subtree
                    // must not drag sibling widgets from other pages along.
                    if self.sanitize_structure_refs && key.as_slice() == b"Kids" {
                        continue;
                    }
                    copied.set(
                        key,
                        self.copy_owned_value(source, target, value, depth + 1)?,
                    );
                }
                Ok(Object::Dictionary(copied))
            }
            Object::Stream(stream) => Ok(Object::Stream(self.copy_stream(
                source,
                target,
                stream,
                None,
                depth + 1,
            )?)),
            value => Ok(value),
        }
    }

    fn copy_stream(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        mut stream: lopdf::Stream,
        stream_id: Option<ObjectId>,
        depth: usize,
    ) -> Result<lopdf::Stream> {
        stream.dict =
            self.copy_stream_dictionary(source, target, &stream, &stream.dict, stream_id, depth)?;
        Ok(stream)
    }

    fn copy_stream_dictionary(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        stream: &lopdf::Stream,
        dict: &Dictionary,
        stream_id: Option<ObjectId>,
        depth: usize,
    ) -> Result<Dictionary> {
        check_copy_depth(depth)?;
        if !self.options.prune_resources || !self.prune_nested_resources || !is_form_xobject(dict) {
            return self.copy_dictionary(source, target, dict, depth + 1);
        }

        let Ok(resources_value) = dict.get(b"Resources") else {
            return self.copy_dictionary(source, target, dict, depth + 1);
        };
        let Some(resources) = self.resolve_dictionary(source, resources_value)? else {
            return self.copy_dictionary(source, target, dict, depth + 1);
        };
        let Some(used) = scan::collect_used_names_from_stream(
            source,
            stream_id,
            || decode_stream_content(stream).ok(),
            &resources,
            &mut self.dictionary_cache,
            &mut self.used_names_cache,
        )?
        else {
            return self.copy_dictionary(source, target, dict, depth + 1);
        };

        let mut copied = Dictionary::new();
        for (key, value) in dict.iter() {
            if key.as_slice() == b"Resources" {
                match self.copy_pruned_resources(source, target, &resources, &used, depth + 1)? {
                    Some(pruned) => copied.set(key.clone(), pruned),
                    None => {
                        copied.set(
                            key.clone(),
                            self.copy_value(source, target, value, depth + 1)?,
                        );
                    }
                }
            } else {
                copied.set(
                    key.clone(),
                    self.copy_value(source, target, value, depth + 1)?,
                );
            }
        }
        Ok(copied)
    }

    fn copy_dictionary(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        dict: &Dictionary,
        depth: usize,
    ) -> Result<Dictionary> {
        check_copy_depth(depth)?;
        let mut copied = Dictionary::new();
        for (key, value) in dict.iter() {
            if self.sanitize_structure_refs && key.as_slice() == b"Kids" {
                // Inside a sanitized (annotation/metadata) subtree the only
                // dictionaries carrying /Kids are AcroForm field nodes reached
                // through a widget's /Parent — field nodes have no /Type, so
                // the structure guard cannot catch them. Their /Kids lists the
                // field's widgets across the WHOLE document; copying it would
                // drag sibling widgets (and their appearance streams) from
                // other pages into a subset output. Dropping /Kids keeps the
                // field itself — crucially its /V signature value — while the
                // output's own widget is already reachable via the page's
                // /Annots.
                continue;
            }
            copied.set(
                key.clone(),
                self.copy_value(source, target, value, depth + 1)?,
            );
        }
        Ok(copied)
    }

    fn copy_resource_value(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        value: &Object,
        depth: usize,
    ) -> Result<Object> {
        let previous = self.prune_nested_resources;
        self.prune_nested_resources = true;
        let result = self.copy_value(source, target, value, depth);
        self.prune_nested_resources = previous;
        result
    }

    /// Copy `value` with document-structure references sanitized: any
    /// reference that resolves to the catalog, a page-tree node, the structure
    /// tree, or a page that is not part of this copy is replaced with null
    /// instead of being followed. Annotations (and document metadata) may
    /// legally point back into the document — `/Dest` arrays, popup parents,
    /// DocMDP `/Data` — and following such a reference from a page-subset copy
    /// would pull unrelated pages, or the entire source document, into the
    /// output.
    fn copy_sanitized_value(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
        value: &Object,
        depth: usize,
    ) -> Result<Object> {
        let previous = self.sanitize_structure_refs;
        self.sanitize_structure_refs = true;
        let result = self.copy_value(source, target, value, depth);
        self.sanitize_structure_refs = previous;
        result
    }

    /// See [`references_document_structure`].
    fn is_document_structure_ref(&self, source: &impl ObjectSource, id: ObjectId) -> bool {
        references_document_structure(source, &self.object_map, id)
    }

    /// Copy the source's document-level metadata objects — the trailer `/Info`
    /// dictionary and the catalog's XMP `/Metadata` stream — into `target`,
    /// returning reference values for [`attach_document_metadata`] to hang on
    /// the output once `finish_pages` has built its trailer and catalog.
    ///
    /// Metadata is best-effort: a damaged `/Info` or `/Metadata` object must
    /// never fail the page operation itself, so copy errors leave the
    /// corresponding slot empty (any objects copied before the error stay in
    /// `target` unreferenced, which is harmless).
    pub(crate) fn copy_document_metadata_objects(
        &mut self,
        source: &impl ObjectSource,
        target: &mut Document,
    ) -> CopiedDocumentMetadata {
        let mut metadata = CopiedDocumentMetadata::default();
        if let Some(info) = source.trailer_value(b"Info") {
            metadata.info = self
                .copy_sanitized_value(source, target, &info, 0)
                .ok()
                .and_then(|copied| match copied {
                    reference @ Object::Reference(_) => Some(reference),
                    // A direct trailer /Info dictionary is materialized as an
                    // indirect object, which is what the spec requires of the
                    // output trailer.
                    Object::Dictionary(dictionary) => {
                        Some(Object::Reference(target.add_object(dictionary)))
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
                    .copy_sanitized_value(source, target, &value, 0)
                    .ok()
                    .filter(|copied| matches!(copied, Object::Reference(_)));
            }
        }
        metadata
    }

    fn resolve_dictionary<'a>(
        &mut self,
        source: &impl ObjectSource,
        value: &'a Object,
    ) -> Result<Option<ResolvedDictionary<'a>>> {
        match value {
            Object::Dictionary(dict) => Ok(Some(ResolvedDictionary::Borrowed(dict))),
            Object::Reference(id) => {
                if let Some(cached) = self.dictionary_cache.get(id) {
                    return Ok(Some(ResolvedDictionary::Shared(Rc::clone(cached))));
                }
                let object = match source.get_object_value(*id) {
                    Ok(object) => object,
                    Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
                    Err(err) => return Err(PdfOpsError::Pdf(err)),
                };
                let Some(dict) = object.as_dict().ok().cloned() else {
                    return Ok(None);
                };
                let dict = Rc::new(dict);
                self.dictionary_cache.insert(*id, Rc::clone(&dict));
                Ok(Some(ResolvedDictionary::Shared(dict)))
            }
            _ => Ok(None),
        }
    }

    fn should_prune_resources(
        &mut self,
        source: &impl ObjectSource,
        resources: &Dictionary,
    ) -> Result<bool> {
        let font_count = self.resource_name_count(source, resources, b"Font")?;
        let xobject_count = self.resource_name_count(source, resources, b"XObject")?;
        Ok(font_count + xobject_count > RESOURCE_PRUNE_MIN_NAMES)
    }

    fn resource_name_count(
        &mut self,
        source: &impl ObjectSource,
        resources: &Dictionary,
        resource_type: &[u8],
    ) -> Result<usize> {
        let Ok(value) = resources.get(resource_type) else {
            return Ok(0);
        };
        Ok(self
            .resolve_dictionary(source, value)?
            .map_or(0, |dict| dict.len()))
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
    ) -> Result<Option<Arc<InheritedPageAttrs>>> {
        if let Some(attrs) = self.inherited_attrs_cache.get(&id) {
            return Ok(Some(Arc::clone(attrs)));
        }

        let object = match source.get_object_value(id) {
            Ok(object) => object,
            Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
            Err(err) => return Err(PdfOpsError::Pdf(err)),
        };
        let dict = object.as_dict().map_err(|_| {
            PdfOpsError::InvalidStructure("page tree node is not a dictionary".into())
        })?;
        let attrs = Arc::new(InheritedPageAttrs::from_dict(dict));
        self.inherited_attrs_cache.insert(id, Arc::clone(&attrs));
        Ok(Some(attrs))
    }
}

enum ResolvedDictionary<'a> {
    Borrowed(&'a Dictionary),
    Shared(Rc<Dictionary>),
}

impl Deref for ResolvedDictionary<'_> {
    type Target = Dictionary;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(dict) => dict,
            Self::Shared(dict) => dict,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct InheritedPageAttrs {
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

fn check_copy_depth(depth: usize) -> Result<()> {
    if depth > MAX_COPY_DEPTH {
        return Err(PdfOpsError::InvalidStructure(format!(
            "PDF object nesting exceeds maximum copy depth of {MAX_COPY_DEPTH}"
        )));
    }
    Ok(())
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

fn is_form_xobject(dict: &Dictionary) -> bool {
    dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Form")
}

/// True when `id` resolves to document structure that a sanitized copy must
/// not follow. Pages already visited by the copy (the annotation's own page
/// via `/P`, or a sibling page of the same output) stay referenceable through
/// `object_map`; a page copied later in the same output is conservatively
/// nulled — that only affects intra-output `/Dest` links, never page content.
/// Shared by `CopyContext` and `StreamingCopyContext` so the structure-type
/// list can never drift between the two.
pub(crate) fn references_document_structure(
    source: &impl ObjectSource,
    object_map: &BTreeMap<ObjectId, ObjectId>,
    id: ObjectId,
) -> bool {
    if object_map.contains_key(&id) {
        return false;
    }
    let Ok(object) = source.get_object_value(id) else {
        return false;
    };
    let Ok(dict) = object.as_dict() else {
        return false;
    };
    dict.has_type(b"Page")
        || dict.has_type(b"Pages")
        || dict.has_type(b"Catalog")
        || dict.has_type(b"StructTreeRoot")
}

/// Document-level metadata copied into a target by
/// [`CopyContext::copy_document_metadata_objects`], as reference values ready
/// to be attached to the output trailer and catalog.
#[derive(Debug, Default)]
pub(crate) struct CopiedDocumentMetadata {
    pub(crate) info: Option<Object>,
    pub(crate) xmp: Option<Object>,
}

/// Attach previously copied document metadata to `target`: `/Info` on the
/// trailer and the XMP `/Metadata` stream on the catalog. Must run after
/// `finish_pages` has set the trailer's `/Root`.
pub(crate) fn attach_document_metadata(
    target: &mut Document,
    metadata: &CopiedDocumentMetadata,
) -> Result<()> {
    if let Some(info) = &metadata.info {
        target.trailer.set("Info", info.clone());
    }
    if let Some(xmp) = &metadata.xmp {
        let catalog_id = target
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(PdfOpsError::Pdf)?;
        let catalog = target
            .get_object_mut(catalog_id)?
            .as_dict_mut()
            .map_err(|_| PdfOpsError::InvalidStructure("catalog is not a dictionary".into()))?;
        catalog.set("Metadata", xmp.clone());
    }
    Ok(())
}

pub(crate) fn copy_pages_with_context(
    source: &impl ObjectSource,
    target: &mut Document,
    page_ids: &[ObjectId],
    context: &mut CopyContext,
) -> Result<Vec<ObjectId>> {
    let mut copied_pages = Vec::with_capacity(page_ids.len());
    for page_id in page_ids {
        copied_pages.push(context.copy_page(source, target, *page_id)?);
    }
    Ok(copied_pages)
}

pub(crate) fn resolve_page_ids(
    pages: &BTreeMap<u32, ObjectId>,
    page_numbers: &[usize],
) -> Result<Vec<ObjectId>> {
    let mut page_ids = Vec::with_capacity(page_numbers.len());
    for page_number in page_numbers {
        let page_key = u32::try_from(*page_number).map_err(|_| {
            PdfOpsError::InvalidStructure(format!(
                "page {page_number} cannot be represented by lopdf"
            ))
        })?;
        let page_id = pages
            .get(&page_key)
            .copied()
            .ok_or_else(|| PdfOpsError::InvalidStructure(format!("missing page {page_number}")))?;
        page_ids.push(page_id);
    }
    Ok(page_ids)
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object};

    use super::{CopyContext, CopyOptions, MAX_COPY_DEPTH};
    use crate::PdfOpsError;

    #[test]
    fn rejects_deep_acyclic_reference_chain_before_stack_overflow() {
        let mut source = Document::with_version("1.7");
        for idx in 1..=(MAX_COPY_DEPTH + 4) {
            let id = (idx as u32, 0);
            let next_id = ((idx + 1) as u32, 0);
            source
                .objects
                .insert(id, Object::Array(vec![Object::Reference(next_id)]));
        }
        source
            .objects
            .insert(((MAX_COPY_DEPTH + 5) as u32, 0), Object::Null);

        let mut target = Document::with_version("1.7");
        let mut context = CopyContext::new(CopyOptions::default());
        let error = context
            .copy_object(&source, &mut target, (1, 0))
            .unwrap_err();

        assert!(matches!(error, PdfOpsError::InvalidStructure(_)));
    }
}
