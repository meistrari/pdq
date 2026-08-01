//! Allocation-bound regression for the pathological tiling-pattern render.
//!
//! Lives in its own integration-test binary because it installs a tracking
//! global allocator: keeping it isolated means the counters only see this
//! test and the allocator cannot slow down or distort the rest of the suite.
//!
//! Before the hayro tiling clamp, the fixture below made `Pixmap::new`
//! request a single 65535x65535 RGBA buffer (~16 GiB), so the largest-single-
//! allocation bound is the invariant that actually failed in production.

#![cfg(feature = "render")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use pdq::{render_pages, RenderOptions};
use tempfile::tempdir;

const MIB: usize = 1024 * 1024;

/// Requests above this are denied before ever reaching the system allocator,
/// so a regression fails fast with an allocation error ("memory allocation of
/// N bytes failed") instead of reserving and touching 16 GiB until the host's
/// OOM killer takes the test runner down. The denied size still lands in
/// `LARGEST` for the failure message.
const HARD_CEILING: usize = 512 * MIB;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

struct TrackingAllocator;

fn deny(size: usize) -> bool {
    if size > HARD_CEILING {
        LARGEST.fetch_max(size, Ordering::Relaxed);
        return true;
    }
    false
}

fn record(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(current, Ordering::Relaxed);
    LARGEST.fetch_max(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if deny(layout.size()) {
            return std::ptr::null_mut();
        }
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if deny(layout.size()) {
            return std::ptr::null_mut();
        }
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if deny(new_size) {
            // The original block stays valid when realloc fails.
            return std::ptr::null_mut();
        }
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            record(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

// Minimal copy of the programmatic generator in tests/render.rs (integration
// test binaries cannot share code, and the repo's convention is to keep each
// test file standalone).
fn build_pdf(bodies: &[String]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let start_xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start_xref}\n%%EOF\n",
            bodies.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn stream_object(dict_entries: &str, content: &str) -> String {
    format!(
        "<< {dict_entries}/Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

/// The incident shape: an A4 page fully painted by a tiling pattern whose
/// cell is the whole page and whose steps mean "never repeat".
fn incident_shaped_pdf() -> Vec<u8> {
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.276 841.89] \
         /Resources << /Pattern << /P1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        stream_object(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2 \
             /BBox [0 0 595.276 841.89] /XStep 99999 /YStep 99999 \
             /Resources << >> ",
            "0.9 0.9 0.9 rg 0 0 595.276 841.89 re f",
        ),
        stream_object("", "/Pattern cs /P1 scn 0 0 595.276 841.89 re f"),
    ])
}

#[test]
fn pathological_tiling_render_stays_within_allocation_bounds() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("incident-shaped.pdf");
    std::fs::write(&input, incident_shaped_pdf()).unwrap();
    let output = temp.path().join("page-%d.png");

    // Reset the window right before the render so setup noise is excluded.
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
    LARGEST.store(0, Ordering::Relaxed);

    // 300 dpi doubles the incident's default so the bound has margin to spare
    // yet still sits far below the failure mode (a 16 GiB single request).
    render_pages(
        &input,
        output.to_str().unwrap(),
        &RenderOptions {
            dpi: 300.0,
            pages: None,
        },
    )
    .unwrap();

    let peak = PEAK.load(Ordering::Relaxed);
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        largest < 100 * MIB,
        "largest single allocation was {} MiB, expected < 100 MiB",
        largest / MIB
    );
    assert!(
        peak < 512 * MIB,
        "peak heap usage was {} MiB, expected < 512 MiB",
        peak / MIB
    );
}
