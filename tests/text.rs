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

fn run_texts(pages: &[pdq::PageText]) -> Vec<&str> {
    pages
        .iter()
        .flat_map(|p| &p.runs)
        .map(|r| r.text.as_str())
        .collect()
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

/// The default must extract appearance-stream text, and it must do so through
/// `ExtractTextOptions::default()` specifically: a derived `Default` would
/// leave the flag `false` and silently reinstate the data loss for every
/// caller, including the CLI.
#[test]
fn annotation_appearance_streams_are_extracted_by_default() {
    let pages = extract_all("text-annotations.pdf");
    let texts = run_texts(&pages);
    assert!(texts.contains(&"Content"), "{texts:?}");
    assert!(texts.contains(&"Filled"), "{texts:?}");

    // The widget's value lands inside the widget's /Rect [200 600 300 620],
    // converted to the top-left origin the JSON uses.
    let filled = pages[0]
        .runs
        .iter()
        .find(|r| r.text == "Filled")
        .expect("filled widget value");
    assert!(
        (200.0..=300.0).contains(&filled.x),
        "filled.x = {}",
        filled.x
    );
    assert!(
        (792.0 - 620.0..=792.0 - 600.0).contains(&filled.y),
        "filled.y = {}",
        filled.y
    );
}

/// Hidden annotations (`/F` bit 2) are invisible on the page, so their text
/// must never reach the output under any option.
#[test]
fn hidden_annotations_are_never_extracted() {
    for annotations in [true, false] {
        let options = ExtractTextOptions {
            annotations,
            ..Default::default()
        };
        let pages = extract_text(&fixture("text-annotations.pdf"), &options).unwrap();
        assert!(
            !run_texts(&pages).contains(&"Hidden"),
            "hidden widget extracted with annotations={annotations}"
        );
    }
}

/// Known limitation, pinned deliberately: NoView (`/F` bit 6) annotations are
/// printed but not displayed, and poppler, mupdf and pdf-oxide all suppress
/// them. hayro's annotation loop only checks the Hidden bit, and pdq cannot
/// filter from the outside — the loop is inside `interpret_page` and every
/// hook it would need is `pub(crate)`. If a hayro upgrade starts honouring
/// the flag this test fails, which is the point: update it and the README
/// limitation together rather than letting the two drift.
#[test]
fn noview_annotations_are_extracted_as_upstream_does() {
    let pages = extract_all("text-annotations.pdf");
    let texts = run_texts(&pages);
    assert!(texts.contains(&"Noview"), "{texts:?}");
}

#[test]
fn annotations_can_be_disabled() {
    let options = ExtractTextOptions {
        annotations: false,
        ..Default::default()
    };
    let pages = extract_text(&fixture("text-annotations.pdf"), &options).unwrap();
    assert_eq!(run_texts(&pages), ["Content"]);
}

/// Appearance streams are drawn through the same device as page content, so a
/// widget whose value also appears in the content stream could in principle be
/// emitted twice. Nothing deduplicates runs, so the absence of overlap is a
/// property worth holding onto.
#[test]
fn annotation_runs_do_not_duplicate_page_content() {
    let pages = extract_all("text-annotations.pdf");
    let runs = &pages[0].runs;
    for (i, a) in runs.iter().enumerate() {
        for b in &runs[i + 1..] {
            let overlaps = a.x < b.x + b.width
                && b.x < a.x + a.width
                && a.y < b.y + b.height
                && b.y < a.y + a.height;
            assert!(
                !(a.text.trim() == b.text.trim() && overlaps),
                "run '{}' emitted twice at overlapping positions",
                a.text
            );
        }
    }
}

#[test]
fn text_cli_includes_annotations_unless_opted_out() {
    let with_annots = pdq()
        .arg("text")
        .arg(fixture("text-annotations.pdf"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&with_annots).unwrap();
    let runs = value[0]["runs"].as_array().unwrap();
    let texts: Vec<&str> = runs.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert!(texts.contains(&"Filled"), "{texts:?}");
    assert!(!texts.contains(&"Hidden"), "{texts:?}");

    let without = pdq()
        .arg("text")
        .arg("--no-annotations")
        .arg(fixture("text-annotations.pdf"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&without).unwrap();
    let runs = value[0]["runs"].as_array().unwrap();
    let texts: Vec<&str> = runs.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["Content"]);
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

/// A one-page PDF carrying `count` widget annotations, all sharing one
/// appearance stream that draws the word "Note", over page content that draws
/// "Body". Built rather than committed because the interesting counts are in
/// the thousands.
fn write_annotation_flood(path: &std::path::Path, count: usize) {
    const FONT: usize = 4;
    const APPEARANCE: usize = 5;
    const CONTENTS: usize = 6;
    const FIRST_ANNOT: usize = 7;

    let appearance = b"BT /F1 12 Tf 2 4 Td (Note) Tj ET";
    let contents = b"BT /F1 12 Tf 20 100 Td (Body) Tj ET";
    let annots = (0..count)
        .map(|i| format!("{} 0 R", FIRST_ANNOT + i))
        .collect::<Vec<_>>()
        .join(" ");

    let mut bodies = vec![
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents {CONTENTS} 0 R\
             /Resources<</Font<</F1 {FONT} 0 R>>>>/Annots[{annots}]>>"
        ),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
        format!(
            "<</Type/XObject/Subtype/Form/BBox[0 0 60 20]\
             /Resources<</Font<</F1 {FONT} 0 R>>>>/Length {}>>\nstream\n{}\nendstream",
            appearance.len(),
            String::from_utf8_lossy(appearance)
        ),
        format!(
            "<</Length {}>>\nstream\n{}\nendstream",
            contents.len(),
            String::from_utf8_lossy(contents)
        ),
    ];
    bodies.extend((0..count).map(|_| {
        format!("<</Type/Annot/Subtype/Widget/Rect[10 10 70 30]/F 4/AP<</N {APPEARANCE} 0 R>>>>")
    }));

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(bodies.len());
    for (index, body) in bodies.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref_at = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
            bodies.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, &pdf).unwrap();
}

/// A page carrying more annotations than any real document skips their
/// appearance streams, because hayro-syntax resolves each one by rebuilding
/// its object stream's whole offset table — 32,768 `/Link` annotations in
/// `corpus/pdfjs/bug1978317.pdf` cost 25 s and yield no text. Page content is
/// untouched and the page is marked degraded so the omission is visible.
#[test]
fn an_annotation_flood_is_skipped_and_marked_degraded() {
    let temp = tempfile::tempdir().unwrap();

    let at_cap = temp.path().join("at-cap.pdf");
    write_annotation_flood(&at_cap, 4_096);
    let pages = extract_text(&at_cap, &ExtractTextOptions::default()).unwrap();
    assert!(
        run_texts(&pages).contains(&"Note"),
        "the cap itself must still be extracted: {:?}",
        run_texts(&pages)
    );
    assert!(!pages[0].degraded);

    let past_cap = temp.path().join("past-cap.pdf");
    write_annotation_flood(&past_cap, 4_097);
    let pages = extract_text(&past_cap, &ExtractTextOptions::default()).unwrap();
    let texts = run_texts(&pages);
    assert!(
        texts.contains(&"Body"),
        "page content survives the skip: {texts:?}"
    );
    assert!(
        !texts.contains(&"Note"),
        "annotations past the cap must be skipped: {texts:?}"
    );
    assert!(
        pages[0].degraded,
        "a skipped annotation layer must be reported, not dropped silently"
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
