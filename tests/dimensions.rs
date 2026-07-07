use std::path::{Path, PathBuf};

use assert_cmd::Command;
use lopdf::{dictionary, Dictionary, Document, Object, Stream};
use pdq::{page_dimensions, page_dimensions_with_password, PageDimensions};
use predicates::prelude::*;
use tempfile::tempdir;

const POINTS_PER_MM: f64 = 1.0 / (10.0 * 2.54) * 72.0;
const A4_WIDTH: f32 = (210.0 * POINTS_PER_MM) as f32;
const A4_HEIGHT: f32 = (297.0 * POINTS_PER_MM) as f32;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn pdq() -> Command {
    Command::cargo_bin("pdq").unwrap()
}

/// One page description: extra entries merged into the page dictionary.
fn write_pdf(path: &Path, pages_attrs: Dictionary, page_dicts: Vec<Dictionary>) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();

    let kids = page_dicts
        .into_iter()
        .map(|extra| {
            let content_id = document.add_object(Object::Stream(Stream::new(
                Dictionary::new(),
                b"q Q".to_vec(),
            )));
            let mut page = dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Resources" => dictionary! {},
                "Contents" => content_id,
            };
            page.extend(&extra);
            Object::Reference(document.add_object(page))
        })
        .collect::<Vec<_>>();

    let mut pages = dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(kids.clone()),
        "Count" => kids.len() as i64,
    };
    pages.extend(&pages_attrs);
    document.objects.insert(pages_id, pages.into());
    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save dimensions fixture: {err}"));
}

/// Write a PDF from raw object bodies (`1 0 obj` onward, `/Root` = object 1)
/// with a correct xref, for values lopdf's writer cannot round-trip.
fn write_raw_pdf(path: &Path, bodies: &[String]) {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in bodies.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            bodies.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, pdf).unwrap();
}

fn media_box(x0: i64, y0: i64, x1: i64, y1: i64) -> Dictionary {
    dictionary! {
        "MediaBox" => Object::Array(vec![x0.into(), y0.into(), x1.into(), y1.into()]),
    }
}

#[test]
fn reports_each_page_size_in_mixed_size_documents() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("mixed.pdf");
    write_pdf(
        &input,
        Dictionary::new(),
        vec![media_box(0, 0, 419, 595), media_box(0, 0, 340, 680)],
    );

    let pages = page_dimensions(&input).unwrap();
    assert_eq!(
        pages,
        vec![
            PageDimensions {
                width: 419.0,
                height: 595.0,
                rotation: 0
            },
            PageDimensions {
                width: 340.0,
                height: 680.0,
                rotation: 0
            },
        ]
    );
}

#[test]
fn inherits_media_box_and_rotate_from_the_pages_node() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("inherited.pdf");
    let mut pages_attrs = media_box(0, 0, 200, 400);
    pages_attrs.set("Rotate", 90);
    write_pdf(
        &input,
        pages_attrs,
        vec![Dictionary::new(), media_box(0, 0, 300, 500)],
    );

    let pages = page_dimensions(&input).unwrap();
    // Page 1 inherits both: 200x400 rotated 90 reports swapped.
    assert_eq!(
        pages[0],
        PageDimensions {
            width: 400.0,
            height: 200.0,
            rotation: 90
        }
    );
    // Page 2 overrides the box but still inherits the rotation.
    assert_eq!(
        pages[1],
        PageDimensions {
            width: 500.0,
            height: 300.0,
            rotation: 90
        }
    );
}

#[test]
fn crop_box_intersected_with_media_box_wins_over_media_box() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("cropped.pdf");
    let mut page = media_box(0, 0, 600, 800);
    // Extends past the media box on the right: the effective width is
    // clipped to the intersection (600 - 100 = 500).
    page.set(
        "CropBox",
        Object::Array(vec![100.into(), 50.into(), 700.into(), 750.into()]),
    );
    write_pdf(&input, Dictionary::new(), vec![page]);

    let pages = page_dimensions(&input).unwrap();
    assert_eq!(
        pages,
        vec![PageDimensions {
            width: 500.0,
            height: 700.0,
            rotation: 0
        }]
    );
}

#[test]
fn rotation_is_normalized_to_the_visible_orientation() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("rotated.pdf");
    let rotated = |rotate: i64| {
        let mut page = media_box(0, 0, 300, 500);
        page.set("Rotate", rotate);
        page
    };
    write_pdf(
        &input,
        Dictionary::new(),
        vec![
            rotated(90),
            rotated(180),
            rotated(270),
            rotated(-90),
            rotated(450),
            // Not a right angle: renders unrotated.
            rotated(45),
        ],
    );

    let pages = page_dimensions(&input).unwrap();
    let expect = |width: f32, height: f32, rotation: u16| PageDimensions {
        width,
        height,
        rotation,
    };
    assert_eq!(
        pages,
        vec![
            expect(500.0, 300.0, 90),
            expect(300.0, 500.0, 180),
            expect(500.0, 300.0, 270),
            expect(500.0, 300.0, 270),
            expect(500.0, 300.0, 90),
            expect(300.0, 500.0, 0),
        ]
    );
}

#[test]
fn damaged_geometry_falls_back_instead_of_failing() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("damaged.pdf");
    let mut malformed_box = Dictionary::new();
    malformed_box.set("MediaBox", Object::Array(vec![0.into(), 0.into()]));
    let mut malformed_rotate = media_box(0, 0, 300, 500);
    malformed_rotate.set("Rotate", Object::Name(b"sideways".to_vec()));
    let mut zero_area = Dictionary::new();
    zero_area.set(
        "MediaBox",
        Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
    );
    let mut junk_element = Dictionary::new();
    // A non-numeric element must reject the whole box, not shift the later
    // coordinates into its slot (which would fabricate a 0x800 box here).
    junk_element.set(
        "MediaBox",
        Object::Array(vec![
            0.into(),
            Object::Name(b"junk".to_vec()),
            600.into(),
            800.into(),
        ]),
    );
    write_pdf(
        &input,
        Dictionary::new(),
        // No MediaBox anywhere, a truncated MediaBox, a malformed /Rotate,
        // a zero-area box, and a box with a junk element: every page falls
        // back instead of erroring.
        vec![
            Dictionary::new(),
            malformed_box,
            malformed_rotate,
            zero_area,
            junk_element,
        ],
    );

    let pages = page_dimensions(&input).unwrap();
    let a4 = PageDimensions {
        width: A4_WIDTH,
        height: A4_HEIGHT,
        rotation: 0,
    };
    assert_eq!(
        pages,
        vec![
            a4,
            a4,
            PageDimensions {
                width: 300.0,
                height: 500.0,
                rotation: 0
            },
            a4,
            a4,
        ]
    );
}

/// A coordinate too large for f32 parses as infinity, which `render` refuses
/// ("too large to render") and JSON cannot represent — the page falls back
/// to A4. lopdf cannot round-trip such values, so the file is written raw.
#[test]
fn f32_overflowing_box_falls_back_to_a4() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("overflow.pdf");
    let huge = "9".repeat(40);
    write_raw_pdf(
        &input,
        &[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {huge}.0 100] >>"),
        ],
    );

    let pages = page_dimensions(&input).unwrap();
    assert_eq!(
        pages,
        vec![PageDimensions {
            width: A4_WIDTH,
            height: A4_HEIGHT,
            rotation: 0
        }]
    );
}

#[test]
fn resolves_indirect_media_box_references() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("indirect.pdf");
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let box_id = document.add_object(Object::Array(vec![
        0.into(),
        0.into(),
        Object::Real(419.5),
        595.into(),
    ]));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => Object::Reference(box_id),
        "Resources" => dictionary! {},
    });
    document.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(vec![Object::Reference(page_id)]),
            "Count" => 1,
        }
        .into(),
    );
    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
    document.trailer.set("Root", catalog_id);
    document.save(&input).unwrap();

    let pages = page_dimensions(&input).unwrap();
    assert_eq!(
        pages,
        vec![PageDimensions {
            width: 419.5,
            height: 595.0,
            rotation: 0
        }]
    );
}

#[test]
fn reads_encrypted_inputs_like_page_count() {
    let pages = page_dimensions_with_password(&fixture("user-password.pdf"), Some("user")).unwrap();
    assert_eq!(pages.len(), 11);
    assert_eq!(pages[0].width, 612.0);
    assert_eq!(pages[0].height, 792.0);

    // Owner-only encryption opens with the empty user password.
    let pages = page_dimensions(&fixture("owner-only.pdf")).unwrap();
    assert_eq!(pages.len(), 11);

    let error = page_dimensions(&fixture("user-password.pdf")).unwrap_err();
    assert!(matches!(error, pdq::PdfOpsError::Password(_)));
}

#[test]
fn reads_object_stream_page_trees() {
    let pages = page_dimensions(&fixture("11-pages-objstm.pdf")).unwrap();
    assert_eq!(pages.len(), 11);
    assert!(pages
        .iter()
        .all(|page| page.width == 612.0 && page.height == 792.0 && page.rotation == 0));
}

#[test]
fn dimensions_cli_prints_json() {
    pdq()
        .arg("dimensions")
        .arg(fixture("11-pages.pdf"))
        .assert()
        .success()
        .stdout(predicate::str::contains(
            r#"{"pages":11,"page_sizes":[{"width":612,"height":792,"rotation":0},"#,
        ));
}

#[test]
fn dimensions_cli_requires_password_for_user_password_input() {
    pdq()
        .arg("dimensions")
        .arg(fixture("user-password.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--password"));

    pdq()
        .arg("dimensions")
        .arg(fixture("user-password.pdf"))
        .arg("--password")
        .arg("user")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""pages":11"#));
}

/// The acceptance criterion from issue #33: for every page,
/// `floor(width × (dpi/72))` in f32 — hayro's own arithmetic — must equal
/// the pixel size `pdq render` produces at that dpi. The DPI set includes
/// 150, 200, and 300, where f32 and f64 disagree on a 612×792 Letter page.
#[cfg(feature = "render")]
#[test]
fn dimensions_match_render_pixel_sizes() {
    use pdq::{render_pages, PageRangeGroup, RenderOptions};

    let png_dimensions = |path: &Path| {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        (width, height)
    };

    let temp = tempdir().unwrap();
    let mixed = temp.path().join("mixed.pdf");
    let mut rotated = media_box(0, 0, 340, 680);
    rotated.set("Rotate", 270);
    let mut cropped = media_box(0, 0, 600, 800);
    cropped.set(
        "CropBox",
        Object::Array(vec![
            Object::Real(10.25),
            0.into(),
            Object::Real(419.75),
            595.into(),
        ]),
    );
    write_pdf(
        &mixed,
        Dictionary::new(),
        vec![
            media_box(0, 0, 419, 595),
            rotated,
            cropped,
            // No box at all: A4 fallback must match render's fallback.
            Dictionary::new(),
        ],
    );

    // The Letter fixture renders only page 1 to keep high-dpi renders cheap.
    let letter = fixture("11-pages.pdf");
    for (input, range) in [(&mixed, None), (&letter, Some("1"))] {
        let pages = page_dimensions(input).unwrap();
        let selected: Vec<usize> = match range {
            Some(_) => vec![1],
            None => (1..=pages.len()).collect(),
        };
        for dpi in [72.0f32, 96.0, 144.0, 150.0, 200.0, 300.0] {
            let out = tempdir().unwrap();
            let pattern = out.path().join("page-%d.png");
            render_pages(
                input,
                pattern.to_str().unwrap(),
                &RenderOptions {
                    dpi,
                    pages: range.map(|range| PageRangeGroup::parse(range).unwrap()),
                },
            )
            .unwrap();

            let width = pages.len().to_string().len();
            for &page_number in &selected {
                let page = &pages[page_number - 1];
                let path = out.path().join(format!("page-{page_number:0width$}.png"));
                let (pixel_width, pixel_height) = png_dimensions(&path);
                // hayro multiplies f32 width by f32 dpi/72 and floors the
                // (exactly widened) product.
                let scale = dpi / 72.0;
                assert_eq!(
                    (pixel_width, pixel_height),
                    (
                        ((page.width * scale) as f64).floor() as u32,
                        ((page.height * scale) as f64).floor() as u32,
                    ),
                    "page {page_number} of {} at {dpi} dpi",
                    input.display(),
                );
                if input == &letter && dpi == 150.0 {
                    // f64 arithmetic would predict 1275×1650 here.
                    assert_eq!((pixel_width, pixel_height), (1275, 1649));
                }
            }
        }
    }
}
