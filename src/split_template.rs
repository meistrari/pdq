//! Shared-prefix template writer for `split-pages`.
//!
//! Every single-page output of a split shares the document-level objects
//! (fonts, resource dictionaries, ...) reachable from all pages. Instead of
//! re-copying and re-serializing those objects 12,000 times, this module:
//!
//! 1. probes a few sample pages with the regular copy machinery,
//! 2. takes the intersection of their source-object closures (which is closed
//!    under references, because the copy of a shared object walks the same
//!    edges no matter which page reached it),
//! 3. serializes that shared closure once into a byte prefix with fixed
//!    object ids `1..=k` and precomputed xref entry lines, and
//! 4. per output, copies only the page-specific objects (page dict, content
//!    streams, page-unique resources) with the shared mapping pre-seeded,
//!    then emits prefix + tail + xref + trailer in a single buffer write.
//!
//! Any anomaly during preparation makes `prepare` return `None`, and the
//! caller falls back to the per-output `Document::save` path, so behavior on
//! unusual documents is unchanged.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write as _,
    path::Path,
    sync::{Condvar, Mutex},
};

use lopdf::{dictionary, Object, ObjectId};

use crate::{
    copy::{CopyContext, CopyOptions, InheritedAttrsCache, ObjectSource},
    split::{empty_document, finish_pages},
    write::{write_dictionary, write_object},
    PdfOpsError, Result,
};

const HEADER: &[u8] = b"%PDF-1.7\n%\xC7\xEC\x8F\xA2\n";
const MAX_REMAP_DEPTH: usize = 256;

pub(crate) struct SinglePageTemplate {
    /// Header plus the serialized shared objects (ids `1..=shared_count`).
    prefix: Vec<u8>,
    /// Precomputed 20-byte xref entry lines for objects `1..=shared_count`.
    xref_prefix: Vec<u8>,
    shared_count: u32,
    /// Source id -> fixed template id, seeded into every per-output copy.
    seed_map: BTreeMap<ObjectId, ObjectId>,
    /// Inherited page attributes resolved once during probing, so per-output
    /// copies do not re-fetch (and re-clone) page-tree ancestor nodes.
    inherited_attrs: InheritedAttrsCache,
}

impl SinglePageTemplate {
    /// Build a template for splitting `pages` into single-page outputs.
    /// Returns `None` when the fast path cannot be used safely; the caller
    /// must then fall back to the generic split path.
    pub(crate) fn prepare(
        source: &impl ObjectSource,
        pages: &[ObjectId],
    ) -> Option<SinglePageTemplate> {
        if pages.is_empty() {
            return None;
        }

        let probe_ids = probe_pages(pages);
        let mut probes = Vec::with_capacity(probe_ids.len());
        let mut inherited_attrs = InheritedAttrsCache::new();
        for page_id in probe_ids {
            let mut scratch = empty_document();
            let mut context = CopyContext::new(CopyOptions::default());
            context.copy_page(source, &mut scratch, page_id).ok()?;
            let (object_map, probe_attrs) = context.into_state();
            inherited_attrs.extend(probe_attrs);
            probes.push((scratch, object_map));
        }

        // Intersection of the probes' source closures. Each closure is closed
        // under references, and so is the intersection.
        let (first_doc, first_map) = &probes[0];
        let shared: Vec<ObjectId> = first_map
            .keys()
            .filter(|id| probes[1..].iter().all(|(_, map)| map.contains_key(*id)))
            .copied()
            .collect();

        // A page object inside the shared set would put a /Type /Page object
        // in every output without linking it into the page tree; bail out to
        // the generic path for such (pathological) documents.
        let page_set: HashSet<ObjectId> = pages.iter().copied().collect();
        if shared.iter().any(|id| page_set.contains(id)) {
            return None;
        }

        // Fix template ids in the first probe's allocation (DFS) order.
        let mut ordered: Vec<(ObjectId, ObjectId)> = shared
            .iter()
            .map(|old_id| (first_map[old_id], *old_id))
            .collect();
        ordered.sort_unstable();

        let scratch_to_template: HashMap<u32, u32> = ordered
            .iter()
            .enumerate()
            .map(|(index, (scratch_id, _))| (scratch_id.0, index as u32 + 1))
            .collect();

        let mut prefix = HEADER.to_vec();
        let mut xref_prefix = Vec::with_capacity(ordered.len() * 20);
        let mut seed_map = BTreeMap::new();
        for (index, (scratch_id, old_id)) in ordered.iter().enumerate() {
            let template_id = index as u32 + 1;
            let mut object = first_doc.objects.get(scratch_id)?.clone();
            remap_references(&mut object, &scratch_to_template, 0).ok()?;
            let offset = u32::try_from(prefix.len()).ok()?;
            writeln!(xref_prefix, "{offset:010} 00000 n ").ok()?;
            writeln!(prefix, "{template_id} 0 obj").ok()?;
            write_object(&mut prefix, &object).ok()?;
            prefix.extend_from_slice(b"\nendobj\n");
            seed_map.insert(*old_id, (template_id, 0));
        }

        Some(SinglePageTemplate {
            prefix,
            xref_prefix,
            shared_count: ordered.len() as u32,
            seed_map,
            inherited_attrs,
        })
    }

    /// Copy `page_id` onto the template and write a complete single-page PDF
    /// to `path`. `buffer` is caller-provided scratch space so parallel
    /// workers can reuse allocations across outputs.
    pub(crate) fn write_page(
        &self,
        source: &impl ObjectSource,
        page_id: ObjectId,
        path: &Path,
        buffer: &mut Vec<u8>,
        gate: &WriteGate,
    ) -> Result<()> {
        self.page_bytes(source, page_id, buffer)?;
        let _permit = gate.acquire();
        std::fs::write(path, &buffer)?;
        Ok(())
    }

    /// Copy `page_id` onto the template and build a complete single-page PDF
    /// into `buffer`, returning the finished bytes. `buffer` is caller-provided
    /// scratch space so parallel workers can reuse allocations across outputs.
    pub(crate) fn page_bytes<'b>(
        &self,
        source: &impl ObjectSource,
        page_id: ObjectId,
        buffer: &'b mut Vec<u8>,
    ) -> Result<&'b [u8]> {
        let mut scratch = empty_document();
        scratch.max_id = self.shared_count;
        let mut context = CopyContext::with_state(
            CopyOptions::default(),
            self.seed_map.clone(),
            self.inherited_attrs.clone(),
        );
        let new_page_id = context.copy_page(source, &mut scratch, page_id)?;
        finish_pages(&mut scratch, &[new_page_id])?;
        let root = scratch
            .trailer
            .get(b"Root")
            .map_err(PdfOpsError::Pdf)?
            .clone();

        buffer.clear();
        buffer.extend_from_slice(&self.prefix);
        let mut xref_tail = Vec::with_capacity(scratch.objects.len() * 20);
        let mut expected_id = self.shared_count;
        for ((id, generation), object) in &scratch.objects {
            expected_id += 1;
            if *id != expected_id || *generation != 0 {
                return Err(PdfOpsError::InvalidStructure(
                    "split template produced non-contiguous object ids".into(),
                ));
            }
            let offset = u32::try_from(buffer.len()).map_err(|_| {
                PdfOpsError::InvalidStructure("split output offset exceeds PDF xref limit".into())
            })?;
            writeln!(xref_tail, "{offset:010} 00000 n ")?;
            writeln!(buffer, "{id} 0 obj")?;
            write_object(buffer, object)?;
            buffer.extend_from_slice(b"\nendobj\n");
        }

        let max_id = expected_id;
        let xref_start = buffer.len();
        writeln!(buffer, "xref\n0 {}", max_id + 1)?;
        buffer.extend_from_slice(b"0000000000 65535 f \n");
        buffer.extend_from_slice(&self.xref_prefix);
        buffer.extend_from_slice(&xref_tail);
        buffer.extend_from_slice(b"trailer\n");
        let trailer = dictionary! {
            "Size" => (max_id + 1) as i64,
            "Root" => root,
        };
        write_dictionary(buffer, &trailer)?;
        write!(buffer, "\nstartxref\n{xref_start}\n%%EOF")?;

        Ok(buffer.as_slice())
    }
}

/// Bounds concurrent output-file writes. Split outputs are small files landing
/// in one directory; the kernel serializes directory updates, so many parallel
/// writers only pile up system time (measured on this corpus: 12.2 s of sys
/// at 16 writers vs 0.9 s at 4 for the same 12,000 files, with 2× the wall
/// clock). Page building stays on the full rayon pool — only the final
/// create/write/close is gated.
pub(crate) struct WriteGate {
    permits: Mutex<usize>,
    available: Condvar,
}

impl WriteGate {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits.max(1)),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) -> WriteGatePermit<'_> {
        let mut permits = self.permits.lock().expect("write gate mutex poisoned");
        while *permits == 0 {
            permits = self
                .available
                .wait(permits)
                .expect("write gate mutex poisoned");
        }
        *permits -= 1;
        WriteGatePermit(self)
    }
}

struct WriteGatePermit<'a>(&'a WriteGate);

impl Drop for WriteGatePermit<'_> {
    fn drop(&mut self) {
        let mut permits = self.0.permits.lock().expect("write gate mutex poisoned");
        *permits += 1;
        self.0.available.notify_one();
    }
}

/// Sample pages used to estimate the shared object closure: first, middle,
/// and last page (deduplicated). Objects shared by these three are almost
/// always shared by every page; a wrong guess only costs output bytes
/// (unreferenced objects) or speed (shared objects copied per output), never
/// correctness.
fn probe_pages(pages: &[ObjectId]) -> Vec<ObjectId> {
    let mut probe_ids = vec![pages[0], pages[pages.len() / 2], pages[pages.len() - 1]];
    probe_ids.dedup();
    probe_ids
}

/// Rewrite every reference in `object` through `map`. Fails if a reference
/// target is missing from the map — that would mean the shared closure is not
/// actually closed, in which case the caller abandons the template.
fn remap_references(
    object: &mut Object,
    map: &HashMap<u32, u32>,
    depth: usize,
) -> std::result::Result<(), ()> {
    if depth > MAX_REMAP_DEPTH {
        return Err(());
    }
    match object {
        Object::Reference(id) => {
            id.0 = *map.get(&id.0).ok_or(())?;
        }
        Object::Array(items) => {
            for item in items {
                remap_references(item, map, depth + 1)?;
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter_mut() {
                remap_references(value, map, depth + 1)?;
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter_mut() {
                remap_references(value, map, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use lopdf::{dictionary, Document, Object, ObjectId, Stream};

    use super::{probe_pages, remap_references, SinglePageTemplate, WriteGate};

    fn three_page_source() -> (Document, Vec<ObjectId>) {
        let mut doc = Document::with_version("1.7");
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();
        for page in 1..=3 {
            let content_id = doc.add_object(Object::Stream(Stream::new(
                dictionary! {},
                format!("BT /F1 12 Tf 72 720 Td (Page {page}) Tj ET").into_bytes(),
            )));
            page_ids.push(doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => font_id },
                },
            }));
        }
        let kids: Vec<Object> = page_ids.iter().copied().map(Object::Reference).collect();
        doc.objects.insert(
            pages_id,
            dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 3,
                // Only on the tree node: outputs must materialize it into the
                // page via the pre-warmed inherited-attributes cache.
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }
            .into(),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        (doc, page_ids)
    }

    #[test]
    fn template_split_writes_valid_single_page_files() {
        let (doc, page_ids) = three_page_source();
        let template =
            SinglePageTemplate::prepare(&doc, &page_ids).expect("template should be prepared");
        // The shared font must have landed in the template prefix.
        assert!(template.shared_count >= 1);
        assert!(!template.prefix.is_empty());
        assert_eq!(
            template.xref_prefix.len(),
            template.shared_count as usize * 20
        );

        let temp = tempfile::tempdir().unwrap();
        let gate = WriteGate::new(2);
        let mut buffer = Vec::new();
        for (index, page_id) in page_ids.iter().enumerate() {
            let path = temp.path().join(format!("page-{}.pdf", index + 1));
            template
                .write_page(&doc, *page_id, &path, &mut buffer, &gate)
                .unwrap();

            let reloaded = Document::load(&path).unwrap();
            let pages = reloaded.get_pages();
            assert_eq!(pages.len(), 1, "output must contain exactly one page");
            let page = reloaded.get_dictionary(pages[&1]).unwrap();
            assert!(
                page.has(b"MediaBox"),
                "inherited MediaBox must be materialized on the page"
            );
            let content = reloaded.get_page_content(pages[&1]).unwrap();
            assert_eq!(
                String::from_utf8_lossy(&content).trim_end(),
                format!("BT /F1 12 Tf 72 720 Td (Page {}) Tj ET", index + 1),
                "output {} must carry its own page content",
                index + 1
            );
        }
    }

    #[test]
    fn page_bytes_matches_bytes_written_to_disk() {
        let (doc, page_ids) = three_page_source();
        let template =
            SinglePageTemplate::prepare(&doc, &page_ids).expect("template should be prepared");

        let temp = tempfile::tempdir().unwrap();
        let gate = WriteGate::new(2);
        let mut file_buffer = Vec::new();
        let mut memory_buffer = Vec::new();
        for (index, page_id) in page_ids.iter().enumerate() {
            let path = temp.path().join(format!("page-{}.pdf", index + 1));
            template
                .write_page(&doc, *page_id, &path, &mut file_buffer, &gate)
                .unwrap();
            let on_disk = std::fs::read(&path).unwrap();

            let bytes = template
                .page_bytes(&doc, *page_id, &mut memory_buffer)
                .unwrap();
            assert_eq!(
                bytes, on_disk,
                "memory helper must return the same bytes written to disk"
            );
        }
    }

    #[test]
    fn prepare_declines_single_page_documents() {
        // With one page the "shared" closure would include the page itself;
        // prepare must decline and let the generic path handle it.
        let (doc, page_ids) = three_page_source();
        assert!(SinglePageTemplate::prepare(&doc, &page_ids[..1]).is_none());
    }

    #[test]
    fn probe_pages_dedupes_and_covers_ends() {
        let pages: Vec<ObjectId> = (1..=9).map(|n| (n, 0)).collect();
        assert_eq!(probe_pages(&pages), vec![(1, 0), (5, 0), (9, 0)]);
        assert_eq!(probe_pages(&[(7, 0)]), vec![(7, 0)]);
    }

    #[test]
    fn remap_rewrites_nested_references_and_rejects_unknown() {
        let map: HashMap<u32, u32> = [(10, 1), (11, 2)].into_iter().collect();
        let mut object = Object::Array(vec![
            Object::Reference((10, 0)),
            Object::Dictionary(lopdf::dictionary! { "X" => Object::Reference((11, 0)) }),
        ]);
        remap_references(&mut object, &map, 0).unwrap();
        let Object::Array(items) = &object else {
            panic!("not an array")
        };
        assert_eq!(items[0], Object::Reference((1, 0)));

        let mut unknown = Object::Reference((99, 0));
        assert!(remap_references(&mut unknown, &map, 0).is_err());
    }
}
