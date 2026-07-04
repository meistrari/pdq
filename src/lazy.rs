use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use lopdf::{xref::XrefEntry, Dictionary, Document, Object, ObjectId, Reader, Stream};

use crate::{
    copy::ObjectSource,
    filter::decode_stream_in_place,
    load::{
        decorate_load_error, finalize_decrypted_document, load_options, upgrade_damaged_xref_error,
    },
    xrefboot::Lexer,
    PdfOpsError, Result,
};

const OBJECT_STREAM_CACHE_LIMIT: usize = 512;
/// Shard count for the parsed-object cache. Splitting runs under rayon, so a
/// single mutex would serialize every lookup; sharding by object number keeps
/// contention negligible.
const NORMAL_CACHE_SHARDS: usize = 16;
/// Per-shard entry cap (FIFO eviction). Shared objects (fonts, resources) are
/// hit once per output and stay resident through churn; page-unique objects
/// flow through. 16 shards x 512 entries bounds the cache at 8192 objects —
/// far below the ~200k objects of a 100k-page document, and plenty for the
/// handful of hot shared objects that dominate re-parse cost.
const NORMAL_CACHE_SHARD_LIMIT: usize = 512;
/// Streams larger than this are never cached: a cache hit must clone the
/// object anyway, and for a large stream that clone is the same memcpy that
/// dominates a fresh parse — so caching gains nothing while big page-unique
/// content streams would evict the small shared dictionaries the cache is for.
const NORMAL_CACHE_MAX_STREAM_BYTES: usize = 8 * 1024;

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
        // Fast path: build only the xref table + trailer straight from the
        // buffer, skipping lopdf's parse of every object. Encrypted inputs
        // cannot take it (the lazy reader would hand out still-encrypted
        // bytes), and any bootstrap anomaly falls back to the full lopdf
        // parse below so behavior is identical to the slow path.
        if let Some(document) = crate::xrefboot::bootstrap_document(buffer) {
            if !document.is_encrypted() && !document.was_encrypted() {
                // A bootstrap that parses but fails validation (e.g. a
                // compressed entry naming a non-normal container) is damage
                // the full parse or the repair scan may untangle, so only a
                // success returns early.
                if let Ok(lazy) = LazyPdf::new(buffer, document, false) {
                    return Ok(Self::Lazy(lazy));
                }
            }
        } else if std::env::var_os("PDQ_TIMING").is_some() {
            eprintln!("phase parse: xref bootstrap fell back to full lopdf parse");
        }

        let mut document = match Document::load_mem_with_options(
            buffer,
            load_options(password, Some(drop_object)),
        ) {
            Ok(document) => document,
            // A wrong password keeps its dedicated error: the file is not
            // damaged and reconstruction could not decrypt it anyway.
            Err(err @ lopdf::Error::InvalidPassword) => return Err(decorate_load_error(err, path)),
            // Last chance (issue #14): the xref/trailer data is unusable, so
            // rebuild it from a raw object scan. Only already-failing files
            // reach this, so well-formed inputs never pay for it.
            Err(err) => {
                return match Self::open_repaired(buffer, path) {
                    Some(source) => Ok(source),
                    None => Err(upgrade_damaged_xref_error(err, path)),
                }
            }
        };
        finalize_decrypted_document(&mut document, path, Some(buffer))?;
        if document.was_encrypted() {
            return Ok(Self::Eager(document));
        }
        Ok(Self::Lazy(LazyPdf::new(buffer, document, false)?))
    }

    /// Open by reconstructing the xref from a raw object scan, bypassing the
    /// file's own (damaged) cross-reference data entirely. `None` when the
    /// buffer cannot be repaired — encrypted, or no recoverable catalog —
    /// in which case callers surface their original error.
    pub(crate) fn open_repaired(buffer: &'a [u8], path: &Path) -> Option<Self> {
        let document = crate::repair::reconstruct_document(buffer)?;
        let lazy = LazyPdf::new(buffer, document, true).ok()?;
        eprintln!(
            "pdq: warning: {}: damaged cross-reference data; reconstructed by \
             scanning the file (best effort)",
            path.display()
        );
        Some(Self::Lazy(lazy))
    }

    /// True when the source was built from a reconstructed xref. Repaired
    /// sources must be rewritten — never byte-copied — so outputs get a
    /// valid cross-reference table instead of a copy of the damage.
    pub(crate) fn repaired(&self) -> bool {
        match self {
            Self::Lazy(lazy) => lazy.repaired,
            Self::Eager(_) => false,
        }
    }

    pub(crate) fn page_ids(&self) -> Result<Vec<ObjectId>> {
        match self {
            Self::Lazy(lazy) => lazy.page_ids(),
            Self::Eager(document) => Ok(document.page_iter().collect()),
        }
    }

    /// Ids of the first `limit` pages in document order; fewer when the
    /// document has fewer pages. See [`LazyPdf::page_ids_up_to`].
    pub(crate) fn page_ids_up_to(&self, limit: usize) -> Result<Vec<ObjectId>> {
        match self {
            Self::Lazy(lazy) => lazy.page_ids_up_to(limit),
            Self::Eager(document) => Ok(document.page_iter().take(limit).collect()),
        }
    }

    pub(crate) fn count_pages(&self) -> Result<usize> {
        match self {
            Self::Lazy(lazy) => lazy.count_pages(),
            Self::Eager(document) => Ok(document.page_iter().count()),
        }
    }

    /// Trusted-count fast path (`qpdf --show-npages` semantics); see
    /// [`LazyPdf::count_pages_fast`]. Eager (decrypted) documents are already
    /// fully parsed, so counting their page iterator costs nothing extra.
    pub(crate) fn count_pages_fast(&self) -> Result<usize> {
        match self {
            Self::Lazy(lazy) => lazy.count_pages_fast(),
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
    normal_objects: NormalObjectCache,
    /// True when the reference table was reconstructed by `crate::repair`.
    repaired: bool,
}

impl<'a> LazyPdf<'a> {
    /// Build the lazy reader from a metadata-only `Document` (trailer and
    /// xref, no objects) previously loaded from `buffer`.
    fn new(buffer: &'a [u8], document: Document, repaired: bool) -> Result<Self> {
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
            normal_objects: NormalObjectCache::default(),
            repaired,
        })
    }

    pub(crate) fn page_ids(&self) -> Result<Vec<ObjectId>> {
        let mut page_ids = Vec::new();
        self.walk_pages(|id| page_ids.push(id))?;
        Ok(page_ids)
    }

    /// Collect the ids of the first `limit` pages in document order, stopping
    /// the walk as soon as that many leaves were seen. Returns fewer ids when
    /// the document has fewer pages — the caller learns the true count from
    /// the result length. Shares `walk_pages_until` with the full walk, so a
    /// prefix can never disagree with what `page_ids` would return.
    pub(crate) fn page_ids_up_to(&self, limit: usize) -> Result<Vec<ObjectId>> {
        let mut page_ids = Vec::new();
        self.walk_pages_until(|id| {
            page_ids.push(id);
            page_ids.len() < limit
        })?;
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
    /// malformed/adversarial trees via ancestor-cycle detection and a depth
    /// cap. Duplicate/shared kids are valid PDF shapes and count as distinct
    /// occurrences; only a node repeated in its current ancestor path is a
    /// hard page-tree cycle.
    fn walk_pages(&self, mut on_page: impl FnMut(ObjectId)) -> Result<()> {
        self.walk_pages_until(|id| {
            on_page(id);
            true
        })
    }

    /// Like [`walk_pages`], but `on_page` returns whether to keep walking —
    /// `false` stops the traversal early (successfully).
    fn walk_pages_until(&self, mut on_page: impl FnMut(ObjectId) -> bool) -> Result<()> {
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

        enum WalkFrame {
            Enter(ObjectId, usize),
            Exit(ObjectId),
        }

        let mut stack = vec![WalkFrame::Enter(pages_id, 0usize)];
        let mut ancestors = BTreeSet::new();

        // Borrowing the current object-stream container keeps the hot path
        // clone-free: consecutive page objects usually live in the same
        // container, so we hold its Arc instead of cloning every node dict
        // out of the cache (which dominated large-document walks).
        let mut current_container: Option<(u32, Arc<BTreeMap<ObjectId, Object>>)> = None;

        while let Some(frame) = stack.pop() {
            match frame {
                WalkFrame::Exit(id) => {
                    ancestors.remove(&id);
                }
                WalkFrame::Enter(id, depth) => {
                    if depth > 256 {
                        return Err(PdfOpsError::InvalidStructure(
                            "page tree exceeds maximum depth".into(),
                        ));
                    }
                    if !ancestors.insert(id) {
                        return Err(PdfOpsError::InvalidStructure(
                            "cycle detected in page tree".into(),
                        ));
                    }

                    match self.classify_page_node(id, &mut current_container)? {
                        PageNode::Leaf => {
                            ancestors.remove(&id);
                            if !on_page(id) {
                                return Ok(());
                            }
                        }
                        PageNode::Interior(kids) => {
                            stack.push(WalkFrame::Exit(id));
                            for kid_id in kids.into_iter().rev() {
                                stack.push(WalkFrame::Enter(kid_id, depth + 1));
                            }
                        }
                    }
                }
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
            return classify_page_dict(id, object);
        }

        let mut already_seen = HashSet::new();
        let object = self
            .reader
            .get_object(id, &mut already_seen)
            .map_err(|err| PdfOpsError::Pdf(normalize_missing_xref(err, id)))?;
        classify_page_dict(id, &object)
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
        decode_stream_in_place(&mut container_stream)?;
        let object_stream = Arc::new(parse_object_stream(&container_stream)?);
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

        // Normal xref entry: shared objects (font dicts, resources) are
        // requested once per split output, so parse them once and serve
        // clones of the cached Arc afterwards instead of re-running nom
        // over the raw buffer every time.
        if let Some(object) = self.normal_objects.get(id) {
            return Ok(Cow::Owned(Object::clone(&object)));
        }

        let mut already_seen = HashSet::new();
        let object = self
            .reader
            .get_object(id, &mut already_seen)
            .map_err(|err| normalize_missing_xref(err, id))?;
        if cacheable_normal_object(&object) {
            self.normal_objects.insert(id, Arc::new(object.clone()));
        }
        Ok(Cow::Owned(object))
    }
}

enum PageNode {
    Leaf,
    Interior(Vec<ObjectId>),
}

enum PageNodeKind {
    Leaf,
    Interior,
}

fn classify_page_dict(id: ObjectId, object: &Object) -> Result<PageNode> {
    let dict = object.as_dict().map_err(|_| {
        PdfOpsError::InvalidStructure(format!(
            "page tree node {} {} R is not a dictionary",
            id.0, id.1
        ))
    })?;

    match page_node_kind(dict) {
        PageNodeKind::Leaf => Ok(PageNode::Leaf),
        PageNodeKind::Interior => classify_page_tree_interior(id, dict),
    }
}

fn page_node_kind(dict: &Dictionary) -> PageNodeKind {
    match dict.get(b"Type") {
        Ok(Object::Name(name)) if name == b"Page" => PageNodeKind::Leaf,
        Ok(Object::Name(name)) if name == b"Pages" => PageNodeKind::Interior,
        _ if dict.has(b"Kids") => PageNodeKind::Interior,
        _ => PageNodeKind::Leaf,
    }
}

pub(crate) fn inferred_page_leaf(dict: &Dictionary) -> bool {
    matches!(page_node_kind(dict), PageNodeKind::Leaf)
}

fn classify_page_tree_interior(id: ObjectId, dict: &Dictionary) -> Result<PageNode> {
    let kids = match dict.get(b"Kids") {
        Ok(Object::Array(kids)) => kids,
        Ok(other) => {
            return Err(PdfOpsError::InvalidStructure(format!(
                "page tree node {} {} R has /Kids as {}, expected array",
                id.0,
                id.1,
                other.enum_variant()
            )));
        }
        Err(_) => {
            return Err(PdfOpsError::InvalidStructure(format!(
                "page tree node {} {} R is an interior page-tree node but is missing /Kids",
                id.0, id.1
            )));
        }
    };

    let mut kid_ids = Vec::with_capacity(kids.len());
    for (index, kid) in kids.iter().enumerate() {
        match kid {
            Object::Reference(kid_id) => kid_ids.push(*kid_id),
            other => {
                return Err(PdfOpsError::InvalidStructure(format!(
                    "page tree node {} {} R has direct/non-reference /Kids[{index}] as {}; \
                     page tree kids must be indirect references",
                    id.0,
                    id.1,
                    other.enum_variant()
                )));
            }
        }
    }
    Ok(PageNode::Interior(kid_ids))
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

fn parse_object_stream(
    stream: &Stream,
) -> std::result::Result<BTreeMap<ObjectId, Object>, lopdf::Error> {
    if stream.content.is_empty() {
        return Ok(BTreeMap::new());
    }

    let first = object_stream_usize(&stream.dict, b"First")?;
    if first > stream.content.len() {
        return Err(lopdf::Error::InvalidOffset(first));
    }

    let count = object_stream_usize(&stream.dict, b"N")?;
    let entries = parse_object_stream_entries(&stream.content[..first], count);
    let sorted_offsets = sorted_object_stream_offsets(&entries);
    let mut objects = BTreeMap::new();

    for (id, offset) in entries {
        if id == 0 {
            continue;
        }
        let Some(start) = first
            .checked_add(offset)
            .filter(|start| *start < stream.content.len())
        else {
            continue;
        };
        let next_idx = sorted_offsets.partition_point(|next_offset| *next_offset <= offset);
        let end = sorted_offsets
            .get(next_idx)
            .copied()
            .and_then(|next_offset| first.checked_add(next_offset))
            .filter(|end| *end <= stream.content.len())
            .unwrap_or(stream.content.len());
        if start >= end {
            continue;
        }

        let mut lexer = Lexer {
            buffer: &stream.content[start..end],
            pos: 0,
        };
        let Some(object) = lexer.parse_object(0) else {
            return Err(lopdf::Error::InvalidObjectStream(format!(
                "failed to parse object {id} in object stream"
            )));
        };
        objects.insert((id, 0), object);
    }

    Ok(objects)
}

fn object_stream_usize(
    dict: &Dictionary,
    key: &'static [u8],
) -> std::result::Result<usize, lopdf::Error> {
    let value = dict.get(key).and_then(Object::as_i64)?;
    usize::try_from(value).map_err(|err| lopdf::Error::NumericCast(err.to_string()))
}

fn parse_object_stream_entries(index_block: &[u8], count: usize) -> Vec<(u32, usize)> {
    let mut lexer = Lexer {
        buffer: index_block,
        pos: 0,
    };
    let mut entries = Vec::with_capacity(count);

    while entries.len() < count {
        lexer.skip_whitespace();
        let Some(id) = lexer.parse_unsigned::<u32>() else {
            break;
        };
        lexer.skip_whitespace();
        let Some(offset) = lexer.parse_unsigned::<usize>() else {
            break;
        };
        entries.push((id, offset));
    }

    entries
}

fn sorted_object_stream_offsets(entries: &[(u32, usize)]) -> Vec<usize> {
    let mut offsets = entries
        .iter()
        .map(|(_, offset)| *offset)
        .collect::<Vec<_>>();
    offsets.sort_unstable();
    offsets.dedup();
    offsets
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

/// Cache eligibility for objects parsed from `Normal` xref entries: everything
/// except large stream payloads (see `NORMAL_CACHE_MAX_STREAM_BYTES`).
fn cacheable_normal_object(object: &Object) -> bool {
    match object {
        Object::Stream(stream) => stream.content.len() <= NORMAL_CACHE_MAX_STREAM_BYTES,
        _ => true,
    }
}

/// Sharded, bounded FIFO cache of parsed non-object-stream objects.
/// Thread-safe by sharding on the object number so parallel split outputs
/// rarely contend on the same mutex.
struct NormalObjectCache {
    shards: [Mutex<NormalObjectShard>; NORMAL_CACHE_SHARDS],
}

impl Default for NormalObjectCache {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(NormalObjectShard::default())),
        }
    }
}

impl NormalObjectCache {
    fn shard(&self, id: ObjectId) -> &Mutex<NormalObjectShard> {
        &self.shards[id.0 as usize % NORMAL_CACHE_SHARDS]
    }

    fn get(&self, id: ObjectId) -> Option<Arc<Object>> {
        self.shard(id)
            .lock()
            .expect("normal object cache mutex poisoned")
            .objects
            .get(&id)
            .map(Arc::clone)
    }

    fn insert(&self, id: ObjectId, object: Arc<Object>) {
        let mut shard = self
            .shard(id)
            .lock()
            .expect("normal object cache mutex poisoned");
        if shard.objects.insert(id, object).is_some() {
            // Concurrent miss on the same id: the id is already queued for
            // eviction, so replacing the value must not enqueue it twice.
            return;
        }
        shard.order.push_back(id);
        while shard.order.len() > NORMAL_CACHE_SHARD_LIMIT {
            if let Some(evicted) = shard.order.pop_front() {
                shard.objects.remove(&evicted);
            }
        }
    }
}

#[derive(Default)]
struct NormalObjectShard {
    objects: HashMap<ObjectId, Arc<Object>>,
    order: VecDeque<ObjectId>,
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

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Object, Stream};

    use super::{
        cacheable_normal_object, parse_object_stream, Arc, NormalObjectCache,
        NORMAL_CACHE_MAX_STREAM_BYTES, NORMAL_CACHE_SHARDS, NORMAL_CACHE_SHARD_LIMIT,
    };

    #[test]
    fn normal_cache_hits_and_evicts_fifo_per_shard() {
        let cache = NormalObjectCache::default();
        // Object numbers in the same shard: 1, 1 + SHARDS, 1 + 2*SHARDS, ...
        let in_shard = |slot: usize| (1 + (slot * NORMAL_CACHE_SHARDS) as u32, 0u16);

        cache.insert(in_shard(0), Arc::new(Object::Integer(0)));
        assert_eq!(*cache.get(in_shard(0)).unwrap(), Object::Integer(0));
        assert!(cache.get(in_shard(1)).is_none());

        // Re-inserting the same id must not double-enqueue it for eviction.
        cache.insert(in_shard(0), Arc::new(Object::Integer(42)));
        assert_eq!(*cache.get(in_shard(0)).unwrap(), Object::Integer(42));

        // Filling the shard past its cap evicts the oldest entry only.
        for slot in 1..=NORMAL_CACHE_SHARD_LIMIT {
            cache.insert(in_shard(slot), Arc::new(Object::Integer(slot as i64)));
        }
        assert!(cache.get(in_shard(0)).is_none(), "oldest entry evicted");
        assert!(cache.get(in_shard(1)).is_some(), "newer entries retained");
        assert!(cache.get(in_shard(NORMAL_CACHE_SHARD_LIMIT)).is_some());

        // A different shard is unaffected by the churn above.
        cache.insert((2, 0), Arc::new(Object::Null));
        assert!(cache.get((2, 0)).is_some());
    }

    #[test]
    fn large_streams_are_not_cacheable() {
        let small = Stream::new(
            lopdf::Dictionary::new(),
            vec![0u8; NORMAL_CACHE_MAX_STREAM_BYTES],
        );
        let large = Stream::new(
            lopdf::Dictionary::new(),
            vec![0u8; NORMAL_CACHE_MAX_STREAM_BYTES + 1],
        );
        assert!(cacheable_normal_object(&Object::Stream(small)));
        assert!(!cacheable_normal_object(&Object::Stream(large)));
        assert!(cacheable_normal_object(&Object::Integer(7)));
    }

    #[test]
    fn object_stream_parser_skips_qdf_comments_before_member_objects() {
        let stream = object_stream_from_members(&[
            (
                2,
                b"%% Object stream: object 2, index 0\n<< /Type /Catalog /Pages 3 0 R >>\n",
            ),
            (
                3,
                b"%% Object stream: object 3, index 1\n<< /Type /Pages /Count 1 /Kids [4 0 R] >>\n",
            ),
            (
                4,
                b"%% Object stream: object 4, index 2\n%% Page 1\n<< /Type /Page /Parent 3 0 R >>\n",
            ),
        ]);

        let objects = parse_object_stream(&stream).unwrap();
        assert!(objects
            .get(&(4, 0))
            .unwrap()
            .as_dict()
            .unwrap()
            .has_type(b"Page"));
    }

    #[test]
    fn object_stream_parser_uses_offsets_for_adjacent_numeric_objects() {
        let index = b"7 0\n8 4\n";
        let mut content = index.to_vec();
        content.extend_from_slice(b"12345678");
        let stream = Stream::new(
            dictionary! {
                "Type" => "ObjStm",
                "N" => 2,
                "First" => index.len() as i64,
            },
            content,
        );

        let objects = parse_object_stream(&stream).unwrap();
        assert_eq!(objects.get(&(7, 0)), Some(&Object::Integer(1234)));
        assert_eq!(objects.get(&(8, 0)), Some(&Object::Integer(5678)));
    }

    #[test]
    fn object_stream_parser_reports_member_parse_failure() {
        let stream = object_stream_from_members(&[(9, b"%% only a comment, no object\n")]);

        let err = parse_object_stream(&stream).unwrap_err().to_string();
        assert!(
            err.contains("failed to parse object 9"),
            "unexpected object-stream error: {err}"
        );
    }

    fn object_stream_from_members(members: &[(u32, &[u8])]) -> Stream {
        let mut bodies = Vec::new();
        let mut entries = Vec::new();
        for (id, body) in members {
            entries.push((*id, bodies.len()));
            bodies.extend_from_slice(body);
        }

        let mut content = Vec::new();
        for (id, offset) in &entries {
            content.extend_from_slice(format!("{id} {offset}\n").as_bytes());
        }
        let first = content.len();
        content.extend_from_slice(&bodies);

        Stream::new(
            dictionary! {
                "Type" => "ObjStm",
                "N" => members.len() as i64,
                "First" => first as i64,
            },
            content,
        )
    }
}
