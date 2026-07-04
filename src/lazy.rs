use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use lopdf::{xref::XrefEntry, Document, Object, ObjectId, ObjectStream, Reader};

use crate::{
    copy::ObjectSource,
    load::{decorate_load_error, ensure_decrypted, load_options},
    PdfOpsError, Result,
};

const OBJECT_STREAM_CACHE_LIMIT: usize = 512;

/// A parsed PDF input: lazily backed by the mmap for plain files, eagerly
/// loaded for encrypted files.
///
/// Unencrypted inputs keep the metadata-only lazy reader — the fast path that
/// parses objects on demand straight from the buffer. Encrypted inputs cannot
/// use it: the lazy reader would hand out still-encrypted bytes. lopdf also
/// ignores the metadata filter for encrypted PDFs and materializes every
/// object while decrypting during the load, so by the time we know the input
/// was encrypted we already hold a fully decrypted document — using it
/// eagerly costs nothing extra.
//
// One PdfSource exists per input file and it is only handled by reference,
// so the enum's size is irrelevant; boxing a variant would just add an
// indirection to every object lookup on the lazy hot path.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PdfSource<'a> {
    Lazy(LazyPdf<'a>),
    Eager(Document),
}

impl<'a> PdfSource<'a> {
    /// Parse `buffer`, transparently decrypting encrypted inputs. The empty
    /// user password is always tried first, then `password` when provided.
    pub(crate) fn open(buffer: &'a [u8], path: &Path, password: Option<&str>) -> Result<Self> {
        let document =
            Document::load_mem_with_options(buffer, load_options(password, Some(drop_object)))
                .map_err(|err| decorate_load_error(err, path))?;
        ensure_decrypted(&document, path)?;
        if document.was_encrypted() {
            return Ok(Self::Eager(document));
        }
        Ok(Self::Lazy(LazyPdf::new(buffer, document)?))
    }

    pub(crate) fn page_ids(&self) -> Result<Vec<ObjectId>> {
        match self {
            Self::Lazy(lazy) => lazy.page_ids(),
            Self::Eager(document) => Ok(document.page_iter().collect()),
        }
    }

    pub(crate) fn count_pages(&self) -> Result<usize> {
        match self {
            Self::Lazy(lazy) => lazy.count_pages(),
            Self::Eager(document) => Ok(document.page_iter().count()),
        }
    }

    /// True when the input was encrypted and decrypted during the load. Such
    /// inputs must be rewritten — never byte-copied — so that outputs are
    /// always unencrypted.
    pub(crate) fn was_encrypted(&self) -> bool {
        match self {
            Self::Lazy(_) => false,
            Self::Eager(document) => document.was_encrypted(),
        }
    }
}

impl ObjectSource for PdfSource<'_> {
    fn get_object_value(&self, id: ObjectId) -> std::result::Result<Cow<'_, Object>, lopdf::Error> {
        match self {
            Self::Lazy(lazy) => lazy.get_object_value(id),
            Self::Eager(document) => document.get_object_value(id),
        }
    }
}

pub(crate) struct LazyPdf<'a> {
    reader: Reader<'a>,
    object_streams: Mutex<ObjectStreamCache>,
}

impl<'a> LazyPdf<'a> {
    /// Build the lazy reader from a metadata-only `Document` (trailer and
    /// xref, no objects) previously loaded from `buffer`.
    fn new(buffer: &'a [u8], document: Document) -> Result<Self> {
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

            let object = self.get_owned(id)?;
            let dict = object.as_dict().map_err(|_| {
                PdfOpsError::InvalidStructure("page tree node is not a dictionary".into())
            })?;
            match dict.get_type().map_err(PdfOpsError::Pdf)? {
                b"Page" => on_page(id),
                b"Pages" => {
                    let kids = dict
                        .get(b"Kids")
                        .and_then(Object::as_array)
                        .map_err(PdfOpsError::Pdf)?;
                    for kid in kids.iter().rev() {
                        if let Ok(kid_id) = kid.as_reference() {
                            stack.push((kid_id, depth + 1));
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn get_owned(&self, id: ObjectId) -> Result<Object> {
        self.get_object_value(id)
            .map(Cow::into_owned)
            .map_err(PdfOpsError::Pdf)
    }

    fn get_compressed_object(
        &self,
        id: ObjectId,
        container: u32,
    ) -> std::result::Result<Object, lopdf::Error> {
        let cached_objects = {
            self.object_streams
                .lock()
                .expect("object stream cache mutex poisoned")
                .get(container)
        };
        let objects = if let Some(objects) = cached_objects {
            objects
        } else {
            let container_id = (container, 0);
            let mut already_seen = HashSet::new();
            let container_object = self.reader.get_object(container_id, &mut already_seen)?;
            let mut container_stream = container_object.as_stream()?.clone();
            let object_stream = Arc::new(ObjectStream::new(&mut container_stream)?.objects);
            self.object_streams
                .lock()
                .expect("object stream cache mutex poisoned")
                .insert(container, Arc::clone(&object_stream));
            object_stream
        };
        objects
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
