#![cfg(feature = "render")]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use pdq::{render_pages, PageRangeGroup, PdfOpsError, RenderOptions};
use predicates::prelude::*;
use tempfile::tempdir;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn png_dimensions(path: &Path) -> (u32, u32) {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    assert_eq!(bytes[..8], PNG_MAGIC, "{} is not a PNG", path.display());
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (width, height)
}

#[test]
fn render_writes_png_for_each_page() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("page-%d.png");

    render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 72.0,
            pages: None,
        },
    )
    .unwrap();

    for page in 1..=11 {
        let path = temp.path().join(format!("page-{page:02}.png"));
        let (width, height) = png_dimensions(&path);
        assert!(
            width > 0 && height > 0,
            "empty render for {}",
            path.display()
        );
    }
}

#[test]
fn render_scales_dimensions_with_dpi() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("dpi-%d.png");

    render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 144.0,
            pages: Some(PageRangeGroup::parse("1").unwrap()),
        },
    )
    .unwrap();

    let (width_144, height_144) = png_dimensions(&temp.path().join("dpi-01.png"));

    let pattern = temp.path().join("base-%d.png");
    render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 72.0,
            pages: Some(PageRangeGroup::parse("1").unwrap()),
        },
    )
    .unwrap();

    let (width_72, height_72) = png_dimensions(&temp.path().join("base-01.png"));
    assert_eq!(width_144, width_72 * 2);
    assert_eq!(height_144, height_72 * 2);
}

#[test]
fn render_selected_pages_keeps_original_numbering() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("sel-%d.png");

    render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 72.0,
            pages: Some(PageRangeGroup::parse("2,11").unwrap()),
        },
    )
    .unwrap();

    assert!(temp.path().join("sel-02.png").exists());
    assert!(temp.path().join("sel-11.png").exists());
    assert!(!temp.path().join("sel-01.png").exists());
    assert!(!temp.path().join("sel-03.png").exists());
}

#[test]
fn render_rejects_out_of_bounds_pages() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("oob-%d.png");

    let error = render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 72.0,
            pages: Some(PageRangeGroup::parse("12").unwrap()),
        },
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::Range(_)));
    assert!(!temp.path().join("oob-12.png").exists());
}

#[test]
fn render_rejects_encrypted_inputs_with_unsupported_error() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("enc-%d.png");

    let error = render_pages(
        &fixture("user-password.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions::default(),
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::Unsupported(_)));
    assert!(!temp.path().join("enc-1.png").exists());
}

#[test]
fn render_rejects_pattern_without_placeholder() {
    let temp = tempdir().unwrap();

    let error = render_pages(
        &fixture("11-pages.pdf"),
        temp.path().join("page.png").to_str().unwrap(),
        &RenderOptions::default(),
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::InvalidStructure(_)));
}

#[test]
fn render_rejects_nonpositive_dpi() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("bad-%d.png");

    let error = render_pages(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 0.0,
            pages: None,
        },
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::InvalidStructure(_)));
}

#[test]
fn render_cli_writes_selected_page() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("cli-%d.png");

    Command::cargo_bin("pdq")
        .unwrap()
        .arg("render")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(&pattern)
        .arg("--dpi")
        .arg("72")
        .arg("--pages")
        .arg("3")
        .assert()
        .success();

    let (width, height) = png_dimensions(&temp.path().join("cli-03.png"));
    assert!(width > 0 && height > 0);
}

#[test]
fn render_cli_rejects_invalid_pages_range() {
    let temp = tempdir().unwrap();

    Command::cargo_bin("pdq")
        .unwrap()
        .arg("render")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(temp.path().join("bad-%d.png"))
        .arg("--pages")
        .arg("abc")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid page number"));
}
