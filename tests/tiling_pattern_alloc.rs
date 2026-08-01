//! FDN-992 allocation guard, in its own test binary because it installs a
//! counting global allocator: rendering the huge-XStep/YStep fixture must
//! never come close to the 65535×65535×4 ≈ 16 GiB tile pixmap the saturated
//! step sizing used to allocate. This is the "no giant allocation" regression
//! measurement; the pixel-level semantics live in tests/tiling_pattern.rs.

#![cfg(feature = "render")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use pdq::{render_pages, RenderOptions};
use tempfile::tempdir;

struct CountingAlloc;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn record(size: usize) {
    LARGEST.fetch_max(size, Ordering::Relaxed);
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(current, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Pixmap::new goes through vec![0; n] → alloc_zeroed; the 16 GiB
        // attempt lands here, not in alloc().
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Account as free(old) + alloc(new); LARGEST sees the full new size.
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const MIB: usize = 1024 * 1024;

/// Whether this test runs against an unpacked crates.io release rather than
/// a checkout (same convention as tests/text.rs). The published crate loses
/// both `[patch.crates-io]` and `vendor/`, so it builds against the
/// unpatched hayro — where this test would not fail an assertion but
/// re-trigger the FDN-992 ~16 GiB allocation itself. `hayro` is
/// deliberately NOT in .github/publish-patch-allowlist.txt: the release
/// gate blocks publishing until the fix ships upstream.
fn is_packaged_crate() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml.orig")
        .exists()
}

#[test]
fn huge_step_pattern_render_stays_within_memory_budget() {
    if is_packaged_crate() {
        eprintln!("skipping: the published crate builds without the vendored hayro fix");
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tiling-step-overflow.pdf");
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("page-%d.png");

    LARGEST.store(0, Ordering::Relaxed);
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);

    render_pages(
        &fixture,
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 144.0,
            pages: None,
        },
    )
    .unwrap();

    let largest = LARGEST.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    // Before the fix the tile pixmap alone was a single ~16 GiB allocation
    // (65535 × 65535 × 4). After it, the biggest buffer is the page raster
    // (1190 × 1684 × 4 ≈ 8 MiB); the bounds leave generous headroom.
    assert!(
        largest < 100 * MIB,
        "largest single allocation was {largest} bytes"
    );
    assert!(
        peak < 512 * MIB,
        "peak resident allocation was {peak} bytes"
    );
}
