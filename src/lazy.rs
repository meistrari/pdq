use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use lopdf::{xref::XrefEntry, Document, LoadOptions, Object, ObjectId, ObjectStream, Reader};

use crate::{copy::ObjectSource, PdfOpsError, Result};

const OBJECT_STREAM_CACHE_LIMIT: usize = 512;

pub(crate) struct LazyPdf<'a> {
    reader: Reader<'a>,
    object_streams: Mutex<ObjectStreamCache>,
}

impl<'a> LazyPdf<'a> {
    pub(crate) fn parse(buffer: &'a [u8], path: &Path) -> Result<Self> {
        // Fast path: build only the xref table + trailer straight from the
        // buffer. On any anomaly (including encrypted files) fall back to the
        // full lopdf parse so behavior is identical to the slow path.
        let document = match crate::xrefboot::bootstrap_document(buffer) {
            Some(document) => document,
            None => {
                if std::env::var_os("PDQ_TIMING").is_some() {
                    eprintln!("phase parse: xref bootstrap fell back to full lopdf parse");
                }
                Document::load_mem_with_options(buffer, LoadOptions::with_filter(drop_object))?
            }
        };
        if document.is_encrypted() || document.was_encrypted() {
            return Err(PdfOpsError::Unsupported(format!(
                "encrypted PDFs are not supported: {}",
                path.display()
            )));
        }

        let reader = Reader {
            buffer,
            document,
            encryption_state: None,
            raw_objects: BTreeMap::new(),
            password: None,
            strict: false,
        };
        validate_compressed_containers(&reader.document)?;
        Ok(Self {
            reader,
            object_streams: Mutex::new(ObjectStreamCache::default()),
        })
    }

    pub(crate) fn page_ids(&self) -> Result<Vec<ObjectId>> {
        let mut page_ids = Vec::new();
        self.walk_pages(|id| page_ids.push(id))?;
        Ok(page_ids)
    }

    /// Count leaf `/Page` objects without materializing their ids — an O(1)-memory
    /// walk sharing `walk_pages` with `page_ids`, so a count can never disagree
    /// with the pages `split`/`split-pages` would actually resolve.
    pub(crate) fn count_pages(&self) -> Result<usize> {
        let mut count = 0usize;
        self.walk_pages(|_| count += 1)?;
        Ok(count)
    }

    /// Trusted-count fast path with `qpdf --show-npages` semantics: return the
    /// root `/Pages` node's `/Count` without walking the tree, so a
    /// lying-but-plausible `/Count` IS trusted. Any anomaly — missing `/Count`,
    /// not a direct non-negative integer, or a value larger than the xref size
    /// (every page needs at least one xref entry, so a bigger count is provably
    /// a lie) — falls back to the validated walk (`count_pages`).
    pub(crate) fn count_pages_fast(&self) -> Result<usize> {
        match self.trusted_root_count() {
            Some(count) => Ok(count),
            None => self.count_pages(),
        }
    }

    /// Read the root `/Pages` `/Count` if — and only if — it is plausible.
    fn trusted_root_count(&self) -> Option<usize> {
        let root_id = self
            .reader
            .document
            .trailer
            .get(b"Root")
            .ok()?
            .as_reference()
            .ok()?;
        let root = self.get_object_value(root_id).ok()?;
        let pages_id = root
            .as_dict()
            .ok()?
            .get(b"Pages")
            .ok()?
            .as_reference()
            .ok()?;
        let pages = self.get_object_value(pages_id).ok()?;
        let count = pages.as_dict().ok()?.get(b"Count").ok()?.as_i64().ok()?;
        // `usize::try_from` rejects negative counts.
        let count = usize::try_from(count).ok()?;
        (count <= self.reader.document.reference_table.size as usize).then_some(count)
    }

    /// Walk the page tree in document order, invoking `on_page` for each leaf
    /// `/Page` object. The single source of truth for "which objects are pages",
    /// shared by `page_ids` (collect) and `count_pages` (count). Guards against
    /// malformed/adversarial trees via cycle detection, a depth cap, and an
    /// object-visit cap.
    fn walk_pages(&self, mut on_page: impl FnMut(ObjectId)) -> Result<()> {
        let root_id = self
            .reader
            .document
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(PdfOpsError::Pdf)?;
        let root = self.get_owned(root_id)?;
        let pages_id = root
            .as_dict()
            .map_err(|_| PdfOpsError::InvalidStructure("catalog is not a dictionary".into()))?
            .get(b"Pages")
            .and_then(Object::as_reference)
            .map_err(PdfOpsError::Pdf)?;

        let mut stack = vec![(pages_id, 0usize)];
        let mut visited = BTreeSet::new();
        let max_iters = self
            .reader
            .document
            .reference_table
            .entries
            .len()
            .saturating_mul(2)
            .max(1);
        let mut iters = 0usize;

        // Borrowing the current object-stream container keeps the hot path
        // clone-free: consecutive page objects usually live in the same
        // container, so we hold its Arc instead of cloning every node dict
        // out of the cache (which dominated large-document walks).
        let mut current_container: Option<(u32, Arc<BTreeMap<ObjectId, Object>>)> = None;

        while let Some((id, depth)) = stack.pop() {
            iters += 1;
            if iters > max_iters {
                return Err(PdfOpsError::InvalidStructure(
                    "page tree traversal exceeded object limit".into(),
                ));
            }
            if depth > 256 {
                return Err(PdfOpsError::InvalidStructure(
                    "page tree exceeds maximum depth".into(),
                ));
            }
            if !visited.insert(id) {
                return Err(PdfOpsError::InvalidStructure(
                    "cycle detected in page tree".into(),
                ));
            }

            match self.classify_page_node(id, &mut current_container)? {
                PageNode::Leaf => on_page(id),
                PageNode::Interior(kids) => {
                    for kid_id in kids.into_iter().rev() {
                        stack.push((kid_id, depth + 1));
                    }
                }
                PageNode::Other => {}
            }
        }

        Ok(())
    }

    /// Classify a page-tree node without cloning it out of the object-stream
    /// cache: compressed nodes are borrowed from their container map, plain
    /// nodes are parsed once from the raw buffer.
    fn classify_page_node(
        &self,
        id: ObjectId,
        current_container: &mut Option<(u32, Arc<BTreeMap<ObjectId, Object>>)>,
    ) -> Result<PageNode> {
        if let Some(XrefEntry::Compressed { container, .. }) =
            self.reader.document.reference_table.get(id.0)
        {
            let container = *container;
            let objects = match current_container {
                Some((cached, objects)) if *cached == container => Arc::clone(objects),
                _ => {
                    let objects = self
                        .container_objects(container)
                        .map_err(|err| PdfOpsError::Pdf(normalize_missing_xref(err, id)))?;
                    *current_container = Some((container, Arc::clone(&objects)));
                    objects
                }
            };
            let object = objects
                .get(&(id.0, 0))
                .ok_or(PdfOpsError::Pdf(lopdf::Error::ObjectNotFound(id)))?;
            return classify_page_dict(object);
        }

        let mut already_seen = HashSet::new();
        let object = self
            .reader
            .get_object(id, &mut already_seen)
            .map_err(|err| PdfOpsError::Pdf(normalize_missing_xref(err, id)))?;
        classify_page_dict(&object)
    }

    fn get_owned(&self, id: ObjectId) -> Result<Object> {
        self.get_object_value(id)
            .map(Cow::into_owned)
            .map_err(PdfOpsError::Pdf)
    }

    fn container_objects(
        &self,
        container: u32,
    ) -> std::result::Result<Arc<BTreeMap<ObjectId, Object>>, lopdf::Error> {
        let cached_objects = {
            self.object_streams
                .lock()
                .expect("object stream cache mutex poisoned")
                .get(container)
        };
        if let Some(objects) = cached_objects {
            return Ok(objects);
        }
        let container_id = (container, 0);
        let mut already_seen = HashSet::new();
        let container_object = self.reader.get_object(container_id, &mut already_seen)?;
        let mut container_stream = container_object.as_stream()?.clone();
        let object_stream = Arc::new(ObjectStream::new(&mut container_stream)?.objects);
        self.object_streams
            .lock()
            .expect("object stream cache mutex poisoned")
            .insert(container, Arc::clone(&object_stream));
        Ok(object_stream)
    }

    fn get_compressed_object(
        &self,
        id: ObjectId,
        container: u32,
    ) -> std::result::Result<Object, lopdf::Error> {
        self.container_objects(container)?
            .get(&(id.0, 0))
            .cloned()
            .ok_or(lopdf::Error::MissingXrefEntry)
    }
}

impl ObjectSource for LazyPdf<'_> {
    fn get_object_value(&self, id: ObjectId) -> std::result::Result<Cow<'_, Object>, lopdf::Error> {
        if let Some(XrefEntry::Compressed { container, .. }) =
            self.reader.document.reference_table.get(id.0)
        {
            return self
                .get_compressed_object(id, *container)
                .map(Cow::Owned)
                .map_err(|err| normalize_missing_xref(err, id));
        }

        let mut already_seen = HashSet::new();
        self.reader
            .get_object(id, &mut already_seen)
            .map(Cow::Owned)
            .map_err(|err| normalize_missing_xref(err, id))
    }
}

enum PageNode {
    Leaf,
    Interior(Vec<ObjectId>),
    Other,
}

fn classify_page_dict(object: &Object) -> Result<PageNode> {
    let dict = object
        .as_dict()
        .map_err(|_| PdfOpsError::InvalidStructure("page tree node is not a dictionary".into()))?;
    match dict.get_type().map_err(PdfOpsError::Pdf)? {
        b"Page" => Ok(PageNode::Leaf),
        b"Pages" => {
            let kids = dict
                .get(b"Kids")
                .and_then(Object::as_array)
                .map_err(PdfOpsError::Pdf)?;
            Ok(PageNode::Interior(
                kids.iter()
                    .filter_map(|kid| kid.as_reference().ok())
                    .collect(),
            ))
        }
        _ => Ok(PageNode::Other),
    }
}

fn drop_object(_: ObjectId, _: &mut Object) -> Option<(ObjectId, Object)> {
    None
}

fn normalize_missing_xref(err: lopdf::Error, id: ObjectId) -> lopdf::Error {
    match err {
        lopdf::Error::MissingXrefEntry => lopdf::Error::ObjectNotFound(id),
        other => other,
    }
}

fn validate_compressed_containers(document: &Document) -> Result<()> {
    for (object_number, entry) in &document.reference_table.entries {
        let XrefEntry::Compressed { container, .. } = entry else {
            continue;
        };
        if !matches!(
            document.reference_table.get(*container),
            Some(XrefEntry::Normal { .. })
        ) {
            return Err(PdfOpsError::InvalidStructure(format!(
                "compressed object {object_number} references a non-normal object stream container {container}"
            )));
        }
    }
    Ok(())
}

#[derive(Default)]
struct ObjectStreamCache {
    entries: VecDeque<(u32, Arc<BTreeMap<ObjectId, Object>>)>,
}

impl ObjectStreamCache {
    fn get(&mut self, container: u32) -> Option<Arc<BTreeMap<ObjectId, Object>>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_container, _)| *cached_container == container)?;
        let (cached_container, objects) = self.entries.remove(index)?;
        self.entries
            .push_back((cached_container, Arc::clone(&objects)));
        Some(objects)
    }

    fn insert(&mut self, container: u32, objects: Arc<BTreeMap<ObjectId, Object>>) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_container, _)| *cached_container == container)
        {
            self.entries.remove(index);
        }
        self.entries.push_back((container, objects));
        while self.entries.len() > OBJECT_STREAM_CACHE_LIMIT {
            self.entries.pop_front();
        }
    }
}
