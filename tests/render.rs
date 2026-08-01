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
    assert!(
        std::fs::read_dir(temp.path()).unwrap().next().is_none(),
        "no output should be written for encrypted input"
    );
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

/// The companion to `deeply_nested_trailer_extracts_instead_of_aborting` in
/// tests/text.rs: render aborted on the same file, and its rayon workers are
/// the more fragile half (Rust's default 2 MiB stack against the loading
/// thread's 8 MiB), so both paths need the check.
#[test]
fn deeply_nested_trailer_renders_instead_of_aborting() {
    let input = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join("qpdf-qtest/0413-issue-202.pdf");
    if !input.exists() {
        eprintln!("skipping: corpus not present");
        return;
    }

    let temp = tempdir().unwrap();
    let pattern = temp.path().join("nested-%d.png");
    render_pages(
        &input,
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 20.0,
            pages: Some(PageRangeGroup::parse("1").unwrap()),
        },
    )
    .unwrap();

    let (width, height) = png_dimensions(&temp.path().join("nested-01.png"));
    assert!(width > 0 && height > 0);
}

// ---- Tiling-pattern memory-blowup regressions ------------------------------
//
// Real-world signature pages (iText 5.x, Brazilian PJe ecosystem) fill the
// page with a tiling pattern declaring /XStep 99999 /YStep 99999: "paint the
// cell once, never repeat". hayro sizes the tile pixmap from step × scale, so
// before the clamp carried on our hayro fork this saturated the u16 cast at
// 65535×65535 and attempted a ~16 GiB allocation. The fixtures below are
// synthetic replicas of that structure; the tests assert both survival and
// exact cell placement, so they fail if the clamp ever distorts phase or
// spacing.

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

/// A 200x200pt page whose content fills with a tiling pattern: a red 20x20pt
/// cell spaced `step` points apart in both directions.
fn tiling_pattern_pdf(step: &str, page_content: &str) -> Vec<u8> {
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
         /Resources << /Pattern << /P1 4 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        stream_object(
            &format!(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2 \
                 /BBox [0 0 20 20] /XStep {step} /YStep {step} /Resources << >> "
            ),
            "1 0 0 rg 0 0 20 20 re f",
        ),
        stream_object("", page_content),
    ])
}

/// The same pathological pattern, but painted from inside a Form XObject with
/// a scaling and translating /Matrix. The pattern lives in the form's
/// resources, so its cell must anchor to the form's space, not the page's.
fn tiling_pattern_in_form_pdf() -> Vec<u8> {
    build_pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
         /Resources << /XObject << /Xf1 6 0 R >> >> /Contents 5 0 R >>"
            .to_string(),
        stream_object(
            "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2 \
             /BBox [0 0 20 20] /XStep 99999 /YStep 99999 /Resources << >> ",
            "1 0 0 rg 0 0 20 20 re f",
        ),
        stream_object("", "/Xf1 Do"),
        stream_object(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
             /Matrix [2 0 0 2 30 40] /Resources << /Pattern << /P1 4 0 R >> >> ",
            "/Pattern cs /P1 scn 0 0 100 100 re f",
        ),
    ])
}

fn render_first_page(pdf_bytes: Vec<u8>, scale: f32) -> hayro::vello_cpu::Pixmap {
    use hayro::{
        hayro_interpret::InterpreterSettings, hayro_syntax::Pdf, RenderCache, RenderSettings,
    };

    let Ok(pdf) = Pdf::new(pdf_bytes) else {
        panic!("fixture should parse");
    };
    let settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        ..Default::default()
    };
    hayro::render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &settings,
    )
}

fn pixel_at(pixmap: &hayro::vello_cpu::Pixmap, x: u32, y: u32) -> [u8; 4] {
    let index = ((y * u32::from(pixmap.width()) + x) * 4) as usize;
    pixmap.data_as_u8_slice()[index..index + 4]
        .try_into()
        .unwrap()
}

#[track_caller]
fn assert_red(pixmap: &hayro::vello_cpu::Pixmap, x: u32, y: u32) {
    let [r, g, b, _] = pixel_at(pixmap, x, y);
    assert!(
        r >= 240 && g <= 15 && b <= 15,
        "expected red at ({x},{y}), got ({r},{g},{b})"
    );
}

#[track_caller]
fn assert_white(pixmap: &hayro::vello_cpu::Pixmap, x: u32, y: u32) {
    let [r, g, b, _] = pixel_at(pixmap, x, y);
    assert!(
        r >= 240 && g >= 240 && b >= 240,
        "expected white at ({x},{y}), got ({r},{g},{b})"
    );
}

/// Before the clamp, this attempted the 16 GiB tile pixmap and aborted the
/// process; it must now complete and place the single cell at the pattern
/// origin. PDF origin is bottom-left, pixmap rows are top-down, so at
/// 144 dpi (scale 2.0) point (x, y) maps to pixel (2x, 400 - 2y).
#[test]
fn huge_step_tiling_pattern_renders_single_cell() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("tiling-huge-step.pdf");
    let bytes = tiling_pattern_pdf("99999", "/Pattern cs /P1 scn 0 0 200 200 re f");
    std::fs::write(&input, &bytes).unwrap();

    let pattern = temp.path().join("tile-%d.png");
    render_pages(
        &input,
        pattern.to_str().unwrap(),
        &RenderOptions {
            dpi: 144.0,
            pages: None,
        },
    )
    .unwrap();
    assert_eq!(png_dimensions(&temp.path().join("tile-1.png")), (400, 400));

    let pixmap = render_first_page(bytes, 2.0);
    assert_red(&pixmap, 20, 380); // (10,10)pt: inside the cell at the origin
    assert_white(&pixmap, 120, 280); // (60,60)pt: red if the period collapsed to the cell
    assert_white(&pixmap, 300, 100); // (150,150)pt: red if the pattern repeated
}

/// The CTM at fill time differs from the pattern's anchor space (the page
/// root), so this fails if the clamp maps the visible window through the
/// path transform instead of the pattern matrix.
#[test]
fn huge_step_tiling_pattern_phase_survives_ctm_translation() {
    let bytes = tiling_pattern_pdf(
        "99999",
        "q 1 0 0 1 40 30 cm /Pattern cs /P1 scn -40 -30 200 200 re f Q",
    );
    let pixmap = render_first_page(bytes, 2.0);
    assert_red(&pixmap, 20, 380); // cell must stay anchored to the page origin
    assert_white(&pixmap, 120, 280);
    assert_white(&pixmap, 300, 100);
}

/// Patterns used inside a form anchor to the form's space: the cell is 20pt
/// scaled by the form's /Matrix [2 0 0 2 30 40], landing on page points
/// (30..70, 40..80).
#[test]
fn huge_step_tiling_pattern_anchors_to_transformed_form() {
    let pixmap = render_first_page(tiling_pattern_in_form_pdf(), 2.0);
    assert_red(&pixmap, 100, 280); // (50,60)pt: inside the transformed cell
    assert_white(&pixmap, 20, 380); // (10,10)pt: red if anchored to the page
    assert_white(&pixmap, 240, 160); // (120,120)pt: inside the form, past the cell
}

/// A tame step must keep tiling exactly as before the clamp: cells every
/// 50pt starting at the origin.
#[test]
fn tiling_pattern_normal_spacing_is_preserved() {
    let bytes = tiling_pattern_pdf("50", "/Pattern cs /P1 scn 0 0 200 200 re f");
    let pixmap = render_first_page(bytes, 2.0);
    assert_red(&pixmap, 20, 380); // (10,10)pt: first cell
    assert_red(&pixmap, 120, 380); // (60,10)pt: second column
    assert_red(&pixmap, 220, 180); // (110,110)pt: third row and column
    assert_white(&pixmap, 70, 380); // (35,10)pt: gap between columns
    assert_white(&pixmap, 20, 330); // (10,35)pt: gap between rows
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
