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
        let document =
            Document::load_mem_with_options(buffer, LoadOptions::with_filter(drop_object))?;
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

        let mut page_ids = Vec::new();
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
                b"Page" => page_ids.push(id),
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

        Ok(page_ids)
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
