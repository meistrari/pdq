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
    assert!(
        run.x >= (min_x - 2.0) as f32 && run.x <= (max_x + 2.0) as f32,
        "x={} outside rotated ink x range [{min_x}, {max_x}]",
        run.x
    );
    assert!(
        run.y as f64 >= min_y - run.font_size as f64 && run.y as f64 <= max_y + 2.0,
        "y={} outside rotated ink y range [{min_y}, {max_y}]",
        run.y
    );
    // Rotated text advances along the page-space y axis, so the run's origin
    // must sit at the ink's vertical extremity rather than the horizontal one.
    assert!((max_y - min_y) > (max_x - min_x));
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
