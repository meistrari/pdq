use std::{collections::BTreeMap, path::Path};

use lopdf::{Object, ObjectId};

use crate::{
    copy::ObjectSource, lazy::PdfSource, load::map_file, repair::with_repair_retry, Result,
};

/// Longest reference chain followed when dereferencing an attribute value,
/// and deepest `/Parent` chain followed when resolving inherited attributes.
/// Both match the page-tree depth cap in `lazy::walk_pages_until`.
const MAX_CHAIN: usize = 256;

const POINTS_PER_MM: f64 = 1.0 / (10.0 * 2.54) * 72.0;
/// Fallback page size when no `MediaBox` resolves, matching hayro's default
/// so `dimensions` and `render` agree on damaged documents.
const A4: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: 210.0 * POINTS_PER_MM,
    y1: 297.0 * POINTS_PER_MM,
};
/// hayro's `SCALAR_NEARLY_ZERO`: boxes this thin fall back to A4 there, so
/// they must fall back here too.
const NEARLY_ZERO: f32 = 1.0 / (1 << 12) as f32;

/// The rendered geometry of one page: the effective box size in PDF points
/// with `/Rotate` already applied, plus the normalized rotation itself.
///
/// `width`/`height` equal hayro's `Page::render_dimensions`, so a render at
/// `dpi` produces a `floor(width * dpi / 72)` × `floor(height * dpi / 72)`
/// pixel image of the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageDimensions {
    /// Visible width in PDF points (already swapped with `height` when the
    /// page is rotated 90 or 270 degrees).
    pub width: f32,
    /// Visible height in PDF points.
    pub height: f32,
    /// Normalized page rotation: 0, 90, 180, or 270.
    pub rotation: u16,
}

/// Report every page's size (PDF points) and rotation straight from the page
/// tree — no rasterization.
///
/// Uses the same lazy, mmap-backed reader and page-tree walk as `page-count`,
/// so the page list can never disagree with the pages `split`/`render` would
/// resolve, and the whole call stays a metadata walk: content streams are
/// never touched. The effective box is CropBox intersected with MediaBox
/// (CropBox defaulting to MediaBox), `/Rotate` of 90/270 swaps width and
/// height, and both attributes are resolved through page-tree inheritance —
/// exactly the geometry hayro uses for `render`, so `width * dpi / 72`
/// (floored) is the pixel width `render` produces at `dpi`.
///
/// Damage is tolerated per page rather than failing the document: a missing
/// or malformed box falls back to the inherited value, then A4; a missing or
/// malformed `/Rotate` falls back to 0. Encrypted PDFs with an empty user
/// password are decrypted transparently; files that need a real password
/// require [`page_dimensions_with_password`].
pub fn page_dimensions(input: &Path) -> Result<Vec<PageDimensions>> {
    page_dimensions_with_password(input, None)
}

/// Like [`page_dimensions`], additionally decrypting encrypted inputs with
/// `password` when the empty user password does not authenticate.
pub fn page_dimensions_with_password(
    input: &Path,
    password: Option<&str>,
) -> Result<Vec<PageDimensions>> {
    let timing = std::env::var_os("PDQ_TIMING").is_some();
    let start = std::time::Instant::now();
    let mmap = map_file(input)?;
    // Damaged inputs whose xref lies about offsets get one retry against a
    // reconstructed table; the closure re-runs (and re-logs) in that case.
    with_repair_retry(&mmap, input, password, |source| {
        if timing {
            eprintln!("phase parse: {:?}", start.elapsed());
        }
        let walk_start = std::time::Instant::now();
        let dimensions = dimensions_impl(source)?;
        if timing {
            eprintln!("phase walk: {:?}", walk_start.elapsed());
        }
        Ok(dimensions)
    })
}

fn dimensions_impl(source: &PdfSource) -> Result<Vec<PageDimensions>> {
    let page_ids = source.page_ids()?;
    let mut nodes = NodeCache::default();
    Ok(page_ids
        .into_iter()
        .map(|id| resolve_page(source, &mut nodes, id))
        .collect())
}

/// Per-node geometry attributes, cached so shared ancestors are parsed once.
/// `None` values mean the attribute is absent or malformed at this node — in
/// both cases resolution continues up the `/Parent` chain, matching hayro's
/// leniency.
#[derive(Debug, Clone, Copy)]
struct NodeGeometry {
    media_box: Option<Rect>,
    crop_box: Option<Rect>,
    rotate: Option<i64>,
    parent: Option<ObjectId>,
}

#[derive(Default)]
struct NodeCache(BTreeMap<ObjectId, Option<NodeGeometry>>);

impl NodeCache {
    fn get(&mut self, source: &PdfSource, id: ObjectId) -> Option<NodeGeometry> {
        if let Some(cached) = self.0.get(&id) {
            return *cached;
        }
        let geometry = node_geometry(source, id);
        self.0.insert(id, geometry);
        geometry
    }
}

fn node_geometry(source: &PdfSource, id: ObjectId) -> Option<NodeGeometry> {
    let object = source.get_object_value(id).ok()?;
    let dict = object.as_dict().ok()?;
    Some(NodeGeometry {
        media_box: dict.get(b"MediaBox").ok().and_then(|v| rect(source, v)),
        crop_box: dict.get(b"CropBox").ok().and_then(|v| rect(source, v)),
        rotate: dict.get(b"Rotate").ok().and_then(|v| integer(source, v)),
        parent: match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => Some(*parent),
            _ => None,
        },
    })
}

/// Resolve one page's geometry, walking up the `/Parent` chain for inherited
/// attributes. Any damage along the way (missing node, cycle, broken parent
/// link) just stops the chain and the defaults apply — a single bad page must
/// not fail the document.
fn resolve_page(source: &PdfSource, nodes: &mut NodeCache, page_id: ObjectId) -> PageDimensions {
    let mut media_box = None;
    let mut crop_box = None;
    let mut rotate = None;

    let mut current = Some(page_id);
    let mut hops = 0usize;
    while let Some(id) = current {
        hops += 1;
        if hops > MAX_CHAIN {
            break;
        }
        let Some(node) = nodes.get(source, id) else {
            break;
        };
        media_box = media_box.or(node.media_box);
        crop_box = crop_box.or(node.crop_box);
        rotate = rotate.or(node.rotate);
        if media_box.is_some() && crop_box.is_some() && rotate.is_some() {
            break;
        }
        // A parent pointing back down the chain would loop; the hop cap
        // bounds it, and revisiting cached nodes costs nothing.
        current = node.parent;
    }

    // From here on this mirrors hayro's `Page::base_dimensions` /
    // `render_dimensions` exactly — including the f64→f32 casts — so the
    // reported size floors to the same pixel size `render` produces.
    let media_box = media_box.unwrap_or(A4);
    let effective = crop_box.unwrap_or(media_box).intersect(media_box);
    let (mut width, mut height) = if (effective.width() as f32).abs() <= NEARLY_ZERO
        || (effective.height() as f32).abs() <= NEARLY_ZERO
    {
        (A4.width() as f32, A4.height() as f32)
    } else {
        (
            effective.width().max(1.0) as f32,
            effective.height().max(1.0) as f32,
        )
    };

    // Coordinates big enough to overflow f32 parse as infinity; `render`
    // refuses such pages ("too large to render"), so there is no pixel size
    // to agree with — fall back to A4 rather than emit non-JSON `inf`.
    if !width.is_finite() || !height.is_finite() {
        (width, height) = (A4.width() as f32, A4.height() as f32);
    }

    let rotation = match rotate.unwrap_or(0).rem_euclid(360) {
        90 => 90,
        180 => 180,
        270 => 270,
        // Anything that is not a right angle renders unrotated.
        _ => 0,
    };
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut width, &mut height);
    }

    PageDimensions {
        width,
        height,
        rotation,
    }
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn intersect(&self, other: Rect) -> Rect {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        Rect {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }

    fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    fn height(&self) -> f64 {
        self.y1 - self.y0
    }
}

/// Parse a PDF rectangle, tolerating indirect references at both the array
/// and the coordinate level. Coordinates go through f32 like hayro's reader,
/// keeping the arithmetic bit-identical with `render`. The first four
/// elements must all be numeric — a malformed element rejects the whole
/// rectangle (hayro stops at it too) instead of shifting later coordinates
/// into its slot.
fn rect(source: &PdfSource, object: &Object) -> Option<Rect> {
    let object = dereference(source, object)?;
    let array = object.as_array().ok()?;
    let mut coords = array.iter().map(|value| number(source, value));
    let x0 = coords.next()?? as f64;
    let y0 = coords.next()?? as f64;
    let x1 = coords.next()?? as f64;
    let y1 = coords.next()?? as f64;
    Some(Rect {
        x0: x0.min(x1),
        y0: y0.min(y1),
        x1: x1.max(x0),
        y1: y1.max(y0),
    })
}

fn number(source: &PdfSource, object: &Object) -> Option<f32> {
    match dereference(source, object)? {
        Object::Integer(value) => Some(value as f32),
        Object::Real(value) => Some(value),
        _ => None,
    }
}

fn integer(source: &PdfSource, object: &Object) -> Option<i64> {
    match dereference(source, object)? {
        Object::Integer(value) => Some(value),
        // hayro truncates a real /Rotate; keep parity.
        Object::Real(value) => Some(value as i64),
        _ => None,
    }
}

/// Follow reference chains to a direct object, cloning only when the value
/// is indirect. `None` on dangling references or reference cycles.
fn dereference(source: &PdfSource, object: &Object) -> Option<Object> {
    let mut object = object.clone();
    let mut hops = 0usize;
    while let Object::Reference(id) = object {
        hops += 1;
        if hops > MAX_CHAIN {
            return None;
        }
        object = source.get_object_value(id).ok()?.into_owned();
    }
    Some(object)
}
