#![cfg(feature = "text")]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use pdq::{extract_text, ExtractTextOptions, PageRangeGroup};
use predicates::prelude::*;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn pdq() -> Command {
    Command::cargo_bin("pdq").unwrap()
}

fn extract_all(name: &str) -> Vec<pdq::PageText> {
    extract_text(&fixture(name), &ExtractTextOptions::default()).unwrap()
}

/// A PDF from the checked-out corpus, or `None` when no corpus is present —
/// the same silent skip `tests/corpus.rs` uses so CI without one is
/// unaffected.
fn corpus_file(relative: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(relative);
    path.exists().then_some(path)
}

/// Whether these tests are running against an unpacked crates.io release
/// rather than a checkout of the repository.
///
/// It matters because cargo rewrites Cargo.toml when packaging and drops the
/// [patch.crates-io] table, so a published crate necessarily builds against
/// an unpatched hayro-interpret. Packaging leaves the untouched manifest
/// beside the rewritten one as Cargo.toml.orig, which a git checkout never
/// has — telling the two apart by the patch table alone is impossible, since
/// a release and a patch someone deleted look identical from inside the
/// build.
fn is_packaged_crate() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml.orig")
        .exists()
}

/// Rasterize a page with hayro exactly like `pdq render` does at 72 dpi and
/// return the ink bounding box in points, top-left origin.
fn ink_bbox(name: &str, page_index: usize) -> (f64, f64, f64, f64) {
    use hayro::{
        hayro_interpret::InterpreterSettings, hayro_syntax::Pdf,
        vello_cpu::color::palette::css::WHITE, RenderCache, RenderSettings,
    };

    let data = std::fs::read(fixture(name)).unwrap();
    let pdf = Pdf::new(data).unwrap();
    let page = &pdf.pages()[page_index];
    let cache = RenderCache::new();
    let settings = RenderSettings {
        x_scale: 1.0,
        y_scale: 1.0,
        bg_color: WHITE,
        ..Default::default()
    };
    let pixmap = hayro::render(page, &cache, &InterpreterSettings::default(), &settings);

    let (w, h) = (pixmap.width() as usize, pixmap.height() as usize);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, 0.0f64, 0.0f64);
    for (i, px) in pixmap.data().iter().enumerate() {
        let dark = (px.r as u32 + px.g as u32 + px.b as u32) < 3 * 200;
        if dark {
            let (x, y) = ((i % w) as f64, (i / w) as f64);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + 1.0);
            max_y = max_y.max(y + 1.0);
        }
    }
    assert!(
        min_x < max_x,
        "no ink found on page {page_index} of {name} ({w}x{h})"
    );
    (min_x, min_y, max_x, max_y)
}

#[test]
fn extracts_positioned_runs_from_simple_page() {
    let pages = extract_all("text-simple.pdf");
    assert_eq!(pages.len(), 1);
    let page = &pages[0];

    assert_eq!(page.page, 1);
    assert_eq!(page.width, 612.0);
    assert_eq!(page.height, 792.0);
    assert!(!page.degraded);

    let texts: Vec<&str> = page.runs.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["Invoice", "Hello", "World"]);

    let invoice = &page.runs[0];
    assert!((invoice.x - 72.0).abs() < 0.1, "invoice.x = {}", invoice.x);
    // Baseline is at 792 - 720 = 72 in top-left coords; y is the glyph top,
    // baseline - 0.8 * font_size.
    assert!((invoice.y - 57.6).abs() < 0.1, "invoice.y = {}", invoice.y);
    assert!((invoice.font_size - 18.0).abs() < 0.1);
    // Sum of Helvetica advances for "Invoice" is 3168/1000 em.
    let expected_width = 3168.0 / 1000.0 * 18.0;
    assert!(
        (invoice.width - expected_width).abs() < 1.0,
        "invoice.width = {}, expected {expected_width}",
        invoice.width
    );
    assert!((invoice.height - 18.0).abs() < 0.1, "{}", invoice.height);

    let hello = &page.runs[1];
    assert!((hello.x - 72.0).abs() < 0.1);
    assert!((hello.y - 82.4).abs() < 0.1, "hello.y = {}", hello.y);
    assert!((hello.font_size - 12.0).abs() < 0.1);

    // "World" starts after the width of "Hello" (2278/1000 em in Helvetica)
    // plus the 24pt TJ gap.
    let world = &page.runs[2];
    let expected_x = 72.0 + 2278.0 / 1000.0 * 12.0 + 24.0;
    assert!(
        (world.x - expected_x).abs() < 1.0,
        "world.x = {}, expected {expected_x}",
        world.x
    );
    assert!((world.y - 82.4).abs() < 0.1);
}

#[test]
fn extracted_geometry_matches_rendered_ink() {
    let pages = extract_all("text-simple.pdf");
    let runs = &pages[0].runs;
    let (min_x, min_y, max_x, max_y) = ink_bbox("text-simple.pdf", 0);

    for run in runs {
        assert!(
            run.x >= (min_x - 2.0) as f32 && run.x <= (max_x + 2.0) as f32,
            "run '{}' x={} outside ink x range [{min_x}, {max_x}]",
            run.text,
            run.x
        );
        assert!(
            run.y as f64 >= min_y - run.font_size as f64 && run.y as f64 <= max_y + 2.0,
            "run '{}' y={} outside ink y range [{min_y}, {max_y}]",
            run.text,
            run.y
        );
    }

    // The first run starts at the leftmost ink.
    assert!((runs[0].x - min_x as f32).abs() < 2.5);

    // The union of the run boxes covers all rendered ink.
    let union_min_x = runs.iter().map(|r| r.x).fold(f32::MAX, f32::min);
    let union_min_y = runs.iter().map(|r| r.y).fold(f32::MAX, f32::min);
    let union_max_x = runs.iter().map(|r| r.x + r.width).fold(0.0, f32::max);
    let union_max_y = runs.iter().map(|r| r.y + r.height).fold(0.0, f32::max);
    assert!(
        union_min_x as f64 <= min_x + 1.0,
        "{union_min_x} vs {min_x}"
    );
    assert!(
        union_min_y as f64 <= min_y + 1.0,
        "{union_min_y} vs {min_y}"
    );
    assert!(
        union_max_x as f64 >= max_x - 1.0,
        "{union_max_x} vs {max_x}"
    );
    assert!(
        union_max_y as f64 >= max_y - 1.0,
        "{union_max_y} vs {max_y}"
    );
}

#[test]
fn rotated_page_matches_render_geometry() {
    let pages = extract_all("text-rotate90.pdf");
    let page = &pages[0];

    // /Rotate 90 swaps the rendered dimensions, exactly like `pdq render`.
    assert_eq!(page.width, 792.0);
    assert_eq!(page.height, 612.0);
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].text, "Rotated");

    let run = &page.runs[0];
    let (min_x, min_y, max_x, max_y) = ink_bbox("text-rotate90.pdf", 0);
    // Rotated text advances along the page-space y axis, so the ink is
    // taller than it is wide.
    assert!((max_y - min_y) > (max_x - min_x));

    // The box follows the vertical advance: tall and narrow, one em wide,
    // and covering the rendered ink.
    assert!(run.height > run.width, "{}x{}", run.width, run.height);
    assert!((run.width - run.font_size).abs() < 0.1, "{}", run.width);
    assert!(run.x as f64 <= min_x + 1.0, "{} vs {min_x}", run.x);
    assert!(run.y as f64 <= min_y + 1.0, "{} vs {min_y}", run.y);
    assert!(
        (run.x + run.width) as f64 >= max_x - 1.0,
        "{} vs {max_x}",
        run.x + run.width
    );
    assert!(
        (run.y + run.height) as f64 >= max_y - 1.0,
        "{} vs {max_y}",
        run.y + run.height
    );
}

#[test]
fn kerned_word_gaps_synthesize_spaces() {
    let pages = extract_all("text-kerned-spaces.pdf");
    let texts: Vec<&str> = pages[0].runs.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        ["Scaled Dot-Product Attention", "Hello world", "Kern gap"]
    );
}

/// Real-world check against a LaTeX PDF whose word gaps are all TJ offsets.
/// Skips silently unless PDQ_ATTENTION_PDF points at arXiv 1706.03762.
#[test]
fn attention_pdf_multiword_search_finds_phrase() {
    let Some(path) = std::env::var_os("PDQ_ATTENTION_PDF") else {
        eprintln!("skipping: PDQ_ATTENTION_PDF not set");
        return;
    };
    let options = ExtractTextOptions {
        pages: Some(PageRangeGroup::parse("3-4".to_string()).unwrap()),
        ..Default::default()
    };
    let pages = extract_text(Path::new(&path), &options).unwrap();
    let hit = pages.iter().flat_map(|p| &p.runs).any(|r| {
        r.text
            .to_lowercase()
            .contains("scaled dot-product attention")
    });
    assert!(hit, "phrase not found in any run on pages 3-4");
}

#[test]
fn image_only_page_yields_empty_runs_without_degraded() {
    let pages = extract_all("text-image-only.pdf");
    assert_eq!(pages.len(), 1);
    assert!(pages[0].runs.is_empty());
    assert!(!pages[0].degraded);
}

#[test]
fn unmappable_glyphs_set_degraded_flag() {
    let pages = extract_all("text-degraded.pdf");
    assert_eq!(pages.len(), 1);
    let page = &pages[0];
    assert!(page.degraded, "page with no ToUnicode must be degraded");
    // The glyphs are still emitted, as replacement characters.
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].text, "\u{FFFD}\u{FFFD}\u{FFFD}");
}

/// Guards the AGL glyph-name mapping this repo carries as a hayro-interpret
/// patch (LaurenzV/hayro#1277). The fixture has no ToUnicode, so the only
/// route to Unicode is the glyph name, and none of its three names appear in
/// the AGL verbatim — the specification derives them by stripping the variant
/// suffix and splitting ligature components. Without the patch every glyph
/// comes back as U+FFFD and the page is flagged degraded, so this test fails
/// loudly if the patch is ever dropped from Cargo.toml.
///
/// Skipped when testing an unpacked crates.io release, which by design has no
/// patch to drop; see [`is_packaged_crate`] and
/// .github/publish-patch-allowlist.txt.
#[test]
fn algorithmic_glyph_names_resolve_without_tounicode() {
    if is_packaged_crate() {
        eprintln!("skipping: the published crate builds without the hayro patch");
        return;
    }

    let pages = extract_all("text-glyph-names.pdf");
    assert_eq!(pages.len(), 1);
    let page = &pages[0];

    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].text, "fi-ffl");
    assert!(
        !page.degraded,
        "every glyph name is recoverable, so the page must not be degraded"
    );
}

/// hayro-syntax's object lexer has no recursion cap, so deeply nested
/// dictionaries used to overflow the stack and abort the process (exit 134,
/// no unwinding). `corpus/qpdf-qtest/0413-issue-202.pdf` nests its *trailer*
/// 68,467 levels deep, which killed `Pdf::new` itself — yet the file is
/// perfectly readable given enough stack, so the fix has to extract it, not
/// reject it.
#[test]
fn deeply_nested_trailer_extracts_instead_of_aborting() {
    let Some(path) = corpus_file("qpdf-qtest/0413-issue-202.pdf") else {
        eprintln!("skipping: corpus not present");
        return;
    };

    let output = pdq()
        .arg("text")
        .arg(&path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let pages = value.as_array().unwrap();
    assert_eq!(pages.len(), 10);
    let texts: Vec<&str> = pages[0]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap())
        .collect();
    assert!(
        texts.contains(&"Sample PDF Document"),
        "the file extracts real text once the stack is big enough: {texts:?}"
    );
}

/// A one-page PDF whose trailer dictionary nests `depth` levels deep, written
/// to `path`. Built rather than committed because the interesting depths run
/// to megabytes of angle brackets.
fn write_trailer_nesting_bomb(path: &std::path::Path, depth: usize) {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (n, body) in [
        &b"<</Type/Catalog/Pages 2 0 R>>"[..],
        &b"<</Type/Pages/Kids[3 0 R]/Count 1>>"[..],
        &b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>"[..],
    ]
    .iter()
    .enumerate()
    {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", n + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = pdf.len();
    pdf.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(b"trailer\n");
    pdf.extend(std::iter::repeat_n(b'<', 2 * depth));
    pdf.extend_from_slice(b"/Size 4/Root 1 0 R");
    pdf.extend(std::iter::repeat_n(b'>', 2 * depth));
    pdf.extend_from_slice(format!("\nstartxref\n{xref_at}\n%%EOF\n").as_bytes());
    std::fs::write(path, &pdf).unwrap();
}

/// Nesting past what the hayro thread's stack can survive must be a clean
/// error, not a signal.
#[test]
fn absurd_uncompressed_nesting_is_a_clean_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested.pdf");
    write_trailer_nesting_bomb(&path, 300_000);

    // `.code(1)` rather than `.failure()`: a process killed by SIGABRT has no
    // exit code at all, and that is exactly the outcome being ruled out.
    pdq()
        .arg("text")
        .arg(&path)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nesting depth"));
}

/// The other side of the cap: nesting *below* it must actually survive, in
/// whatever profile the suite is running under.
///
/// This is the regression test for a cap sized against optimized frames only.
/// An unoptimized hayro spends ~5x the stack per level, so a cap derived from
/// the release figure sat above what a debug build could survive and this file
/// — accepted by the guard — aborted the process with SIGABRT. 130,000 is just
/// past the ~110,000 where a debug build used to die; it costs the hayro
/// thread ~330 MiB of committed stack under `cargo test` and ~64 MiB under
/// `cargo test --release`.
#[test]
fn nesting_below_the_cap_survives_in_every_profile() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested.pdf");
    write_trailer_nesting_bomb(&path, 130_000);

    let output = pdq()
        .arg("text")
        .arg(&path)
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 1);
}

#[test]
fn page_selection_and_out_of_range_errors() {
    let options = ExtractTextOptions {
        pages: Some(PageRangeGroup::parse("2".to_string()).unwrap()),
        ..Default::default()
    };
    let err = extract_text(&fixture("text-simple.pdf"), &options).unwrap_err();
    assert!(err.to_string().contains("2"), "unexpected error: {err}");

    let options = ExtractTextOptions {
        pages: Some(PageRangeGroup::parse("3,5-7".to_string()).unwrap()),
        ..Default::default()
    };
    let pages = extract_text(&fixture("11-pages.pdf"), &options).unwrap();
    let numbers: Vec<usize> = pages.iter().map(|p| p.page).collect();
    assert_eq!(numbers, [3, 5, 6, 7]);
}

#[test]
fn text_cli_outputs_parseable_json_array() {
    let output = pdq()
        .arg("text")
        .arg(fixture("text-simple.pdf"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let pages = value.as_array().unwrap();
    assert_eq!(pages.len(), 1);

    let page = &pages[0];
    assert_eq!(page["page"], 1);
    assert_eq!(page["page_width"], 612.0);
    assert_eq!(page["page_height"], 792.0);
    assert_eq!(page["degraded"], false);

    let runs = page["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["text"], "Invoice");
    assert!(runs[0]["x"].is_number());
    assert!(runs[0]["y"].is_number());
    assert!(runs[0]["width"].is_number());
    assert!(runs[0]["height"].is_number());
    assert!(runs[0]["font_size"].is_number());
}

#[test]
fn text_cli_selects_pages() {
    let output = pdq()
        .arg("text")
        .arg("--pages")
        .arg("2,4")
        .arg(fixture("11-pages.pdf"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let pages = value.as_array().unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0]["page"], 2);
    assert_eq!(pages[1]["page"], 4);
}

#[test]
fn text_cli_rejects_out_of_range_page() {
    pdq()
        .arg("text")
        .arg("--pages")
        .arg("99")
        .arg(fixture("text-simple.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("99"));
}

#[test]
fn text_cli_requires_password_for_encrypted_input() {
    pdq()
        .arg("text")
        .arg(fixture("user-password.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--password"));
}

#[test]
fn text_cli_honors_password() {
    let output = pdq()
        .arg("text")
        .arg("--password")
        .arg("user")
        .arg("--pages")
        .arg("1")
        .arg(fixture("user-password.pdf"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 1);
}

#[test]
fn text_cli_rejects_wrong_password() {
    pdq()
        .arg("text")
        .arg("--password")
        .arg("wrong")
        .arg(fixture("user-password.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("password"));
}
