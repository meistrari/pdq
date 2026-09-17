use std::{
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use flate2::{write::ZlibEncoder, Compression};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use pdq::{
    fingerprint_pages, merge, split, FingerprintOptions, MergeInput, PageRangeGroup, SplitOutput,
};
use predicates::prelude::*;
use tempfile::{tempdir, TempDir};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn pdq() -> Command {
    Command::cargo_bin("pdq").unwrap()
}

/// Builds one page's dictionary entries (everything but /Type and /Parent).
/// Receives the page's index and the ids every page will get, so a page can
/// point at itself or its siblings.
type PageBuilder = Box<dyn Fn(&mut Document, usize, &[ObjectId]) -> Dictionary>;

/// Write a PDF built from `pages`. `padding` unrelated objects come first so
/// the same page gets different object numbers in different documents.
fn write_pdf(path: &Path, padding: usize, tree_attrs: Dictionary, pages: Vec<PageBuilder>) {
    let mut document = Document::with_version("1.7");
    for index in 0..padding {
        document.add_object(dictionary! { "Padding" => index as i64 });
    }
    let pages_id = document.new_object_id();
    let page_ids: Vec<ObjectId> = pages.iter().map(|_| document.new_object_id()).collect();
    for (index, build) in pages.iter().enumerate() {
        let mut page = dictionary! { "Type" => "Page", "Parent" => pages_id };
        page.extend(&build(&mut document, index, &page_ids));
        document.objects.insert(page_ids[index], page.into());
    }
    let mut tree = dictionary! {
        "Type" => "Pages",
        "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
        "Count" => page_ids.len() as i64,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    };
    tree.extend(&tree_attrs);
    document.objects.insert(pages_id, tree.into());
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    document.trailer.set("Root", catalog);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save fingerprint fixture: {err}"));
}

fn build(dir: &TempDir, name: &str, padding: usize, pages: Vec<PageBuilder>) -> PathBuf {
    let path = dir.path().join(name);
    write_pdf(&path, padding, Dictionary::new(), pages);
    path
}

fn fingerprints(path: &Path) -> Vec<[u8; 32]> {
    fingerprint_pages(path, &FingerprintOptions::default())
        .unwrap_or_else(|err| panic!("fingerprint {} failed: {err}", path.display()))
        .into_iter()
        .map(|page| page.digest)
        .collect()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn content_stream(data: &[u8], compress: bool) -> Stream {
    if compress {
        Stream::new(dictionary! { "Filter" => "FlateDecode" }, zlib(data))
    } else {
        Stream::new(Dictionary::new(), data.to_vec())
    }
}

fn standard_font(document: &mut Document, base_font: &str) -> ObjectId {
    document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => base_font,
        "Encoding" => "WinAnsiEncoding",
    })
}

/// A page drawing `content` with one standard font named `font_name`.
fn text_page(
    font_name: &'static str,
    base_font: &'static str,
    content: Vec<u8>,
    compress: bool,
) -> PageBuilder {
    Box::new(move |document, _, _| {
        let font = standard_font(document, base_font);
        let contents = document.add_object(content_stream(&content, compress));
        dictionary! {
            "Resources" => dictionary! { "Font" => dictionary! { font_name => font } },
            "Contents" => contents,
        }
    })
}

fn hello(word: &str) -> Vec<u8> {
    format!("BT /F1 12 Tf 72 700 Td ({word}) Tj ET").into_bytes()
}

#[test]
fn same_page_matches_across_documents_despite_numbering_names_and_layout() {
    let dir = tempdir().unwrap();
    let original = build(
        &dir,
        "original.pdf",
        0,
        vec![
            text_page("F1", "Helvetica", hello("Hello"), false),
            text_page("F1", "Helvetica", hello("World"), false),
        ],
    );
    // Pages swapped, objects renumbered, the font renamed, an unused resource
    // added, the content compressed, re-spaced, commented and split in two.
    let rewritten = build(
        &dir,
        "rewritten.pdf",
        40,
        vec![
            Box::new(|document, _, _| {
                let font = standard_font(document, "Helvetica");
                let unused = standard_font(document, "Courier");
                let contents = document.add_object(content_stream(
                    b"% producer comment\nBT\r\n/Xi9   12 Tf\n72 700 Td (World)Tj\nET",
                    true,
                ));
                dictionary! {
                    "Resources" => dictionary! {
                        "Font" => dictionary! { "Xi9" => font, "Unused" => unused },
                    },
                    "Contents" => contents,
                }
            }),
            Box::new(|document, _, _| {
                let font = standard_font(document, "Helvetica");
                let first = document.add_object(content_stream(b"BT /R7 12 Tf 72 700", false));
                let second = document.add_object(content_stream(b"Td (Hello) Tj ET", true));
                // An indirect /Contents array, as some producers write it.
                let parts = document.add_object(vec![first.into(), second.into()]);
                dictionary! {
                    "Resources" => dictionary! { "Font" => dictionary! { "R7" => font } },
                    "Contents" => parts,
                }
            }),
        ],
    );

    let original = fingerprints(&original);
    let rewritten = fingerprints(&rewritten);
    assert_ne!(original[0], original[1]);
    assert_eq!(original[0], rewritten[1]);
    assert_eq!(original[1], rewritten[0]);
}

#[test]
fn same_name_resolving_to_a_different_font_differs() {
    let dir = tempdir().unwrap();
    let path = build(
        &dir,
        "fonts.pdf",
        0,
        vec![
            text_page("F1", "Helvetica", hello("Hello"), false),
            text_page("F1", "Courier", hello("Hello"), false),
        ],
    );
    let pages = fingerprints(&path);
    assert_ne!(pages[0], pages[1]);
}

const IMAGE_WIDTH: usize = 4;
const IMAGE_HEIGHT: usize = 5;

fn gray_pixels() -> Vec<u8> {
    (0..IMAGE_WIDTH * IMAGE_HEIGHT)
        .map(|index| (index * 37 % 251) as u8)
        .collect()
}

/// PNG-filter `pixels` row by row, cycling through all five filter types.
fn png_predict(pixels: &[u8], row: usize, bpp: usize) -> Vec<u8> {
    fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
        let p = i16::from(left) + i16::from(up) - i16::from(up_left);
        let (pa, pb, pc) = (
            (p - i16::from(left)).abs(),
            (p - i16::from(up)).abs(),
            (p - i16::from(up_left)).abs(),
        );
        if pa <= pb && pa <= pc {
            left
        } else if pb <= pc {
            up
        } else {
            up_left
        }
    }

    let mut out = Vec::new();
    let mut previous = vec![0u8; row];
    for (index, current) in pixels.chunks(row).enumerate() {
        let filter = (index % 5) as u8;
        out.push(filter);
        for i in 0..row {
            let left = if i >= bpp { current[i - bpp] } else { 0 };
            let up_left = if i >= bpp { previous[i - bpp] } else { 0 };
            let prediction = match filter {
                0 => 0,
                1 => left,
                2 => previous[i],
                3 => ((u16::from(left) + u16::from(previous[i])) / 2) as u8,
                _ => paeth(left, previous[i], up_left),
            };
            out.push(current[i].wrapping_sub(prediction));
        }
        previous.copy_from_slice(current);
    }
    out
}

fn tiff_predict(pixels: &[u8], row: usize, colors: usize) -> Vec<u8> {
    let mut out = pixels.to_vec();
    for current in out.chunks_mut(row) {
        for i in (colors..row).rev() {
            current[i] = current[i].wrapping_sub(current[i - colors]);
        }
    }
    out
}

/// A page drawing one image XObject with the given dictionary and data.
fn image_page(extra: Dictionary, data: Vec<u8>) -> PageBuilder {
    Box::new(move |document, _, _| {
        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => IMAGE_WIDTH as i64,
            "Height" => IMAGE_HEIGHT as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        };
        dict.extend(&extra);
        let image = document.add_object(Stream::new(dict, data.clone()));
        let contents =
            document.add_object(content_stream(b"q 100 0 0 100 0 0 cm /Im0 Do Q", false));
        dictionary! {
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image } },
            "Contents" => contents,
        }
    })
}

#[test]
fn images_match_across_compression_and_predictors_but_not_pixel_changes() {
    let dir = tempdir().unwrap();
    let pixels = gray_pixels();
    let mut changed = pixels.clone();
    changed[7] ^= 1;
    let predictor = |value: i64| {
        dictionary! {
            "Filter" => "FlateDecode",
            "DecodeParms" => dictionary! {
                "Predictor" => value,
                "Columns" => IMAGE_WIDTH as i64,
            },
        }
    };

    let path = build(
        &dir,
        "images.pdf",
        0,
        vec![
            image_page(Dictionary::new(), pixels.clone()),
            image_page(dictionary! { "Filter" => "FlateDecode" }, zlib(&pixels)),
            image_page(predictor(15), zlib(&png_predict(&pixels, IMAGE_WIDTH, 1))),
            image_page(predictor(2), zlib(&tiff_predict(&pixels, IMAGE_WIDTH, 1))),
            image_page(dictionary! { "Filter" => "FlateDecode" }, zlib(&changed)),
            image_page(predictor(15), zlib(&png_predict(&changed, IMAGE_WIDTH, 1))),
        ],
    );

    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1], "Flate vs uncompressed");
    assert_eq!(pages[0], pages[2], "PNG predictor");
    assert_eq!(pages[0], pages[3], "TIFF predictor");
    assert_ne!(pages[0], pages[4], "one bit flipped");
    assert_eq!(pages[4], pages[5], "flipped bit through a predictor");
}

#[test]
fn one_bit_predicted_images_match_their_plain_form() {
    // The PJe signature QR code shape: 1 bit per component under PNG
    // predictor 15, where a row is ceil(columns / 8) bytes, not `columns`.
    let dir = tempdir().unwrap();
    let columns = 12usize;
    let row = columns.div_ceil(8);
    let bits: Vec<u8> = (0..row * IMAGE_HEIGHT)
        .map(|index| (index * 91 % 256) as u8 & if index % 2 == 1 { 0xF0 } else { 0xFF })
        .collect();
    let one_bit = |extra: Dictionary, data: Vec<u8>| -> PageBuilder {
        let mut dict = dictionary! {
            "Width" => columns as i64,
            "BitsPerComponent" => 1,
        };
        dict.extend(&extra);
        image_page(dict, data)
    };

    let path = build(
        &dir,
        "one-bit.pdf",
        0,
        vec![
            one_bit(Dictionary::new(), bits.clone()),
            one_bit(
                dictionary! {
                    "Filter" => "FlateDecode",
                    "DecodeParms" => dictionary! {
                        "Predictor" => 15,
                        "Columns" => columns as i64,
                        "BitsPerComponent" => 1,
                    },
                },
                zlib(&png_predict(&bits, row, 1)),
            ),
        ],
    );
    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1]);
}

/// A page with fixed text plus whatever annotations `annots` builds.
fn annotated_page(annots: fn(&mut Document, usize, &[ObjectId]) -> Vec<Object>) -> PageBuilder {
    Box::new(move |document, index, page_ids| {
        let font = standard_font(document, "Helvetica");
        let contents = document.add_object(content_stream(&hello("Annotated"), false));
        let mut page = dictionary! {
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => contents,
        };
        let annotations = annots(document, index, page_ids);
        if !annotations.is_empty() {
            page.set("Annots", annotations);
        }
        page
    })
}

fn square(document: &mut Document, modified: &str, name: &str, page: Option<ObjectId>) -> Object {
    let appearance = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 40.into(), 40.into()],
        },
        b"0 0 1 rg 0 0 40 40 re f".to_vec(),
    ));
    let mut annotation = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Square",
        "Rect" => vec![10.into(), 10.into(), 50.into(), 50.into()],
        "M" => Object::string_literal(modified),
        "NM" => Object::string_literal(name),
        "AP" => dictionary! { "N" => appearance },
    };
    if let Some(page) = page {
        annotation.set("P", page);
    }
    document.add_object(annotation).into()
}

fn link(document: &mut Document, target: ObjectId) -> Object {
    document
        .add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 20.into(), 20.into()],
            "Dest" => vec![target.into(), "Fit".into()],
        })
        .into()
}

#[test]
fn annotations_count_but_bookkeeping_and_link_targets_do_not() {
    let dir = tempdir().unwrap();
    let path = build(
        &dir,
        "annotations.pdf",
        0,
        vec![
            annotated_page(|_, _, _| Vec::new()),
            annotated_page(|document, index, ids| {
                vec![square(
                    document,
                    "D:20260817134350",
                    "a-1",
                    Some(ids[index]),
                )]
            }),
            annotated_page(|document, _, _| {
                vec![square(document, "D:20260901080706", "b-2", None)]
            }),
            annotated_page(|document, _, ids| vec![link(document, ids[0])]),
            annotated_page(|document, _, ids| vec![link(document, ids[1])]),
        ],
    );

    let pages = fingerprints(&path);
    assert_ne!(pages[0], pages[1], "an added annotation is drawn");
    assert_eq!(pages[1], pages[2], "/M, /NM and /P are bookkeeping");
    assert_ne!(pages[0], pages[3], "a link annotation still counts");
    assert_eq!(
        pages[3], pages[4],
        "link targets are page numbers, not drawing"
    );
}

fn geometry_page(attrs: Dictionary) -> PageBuilder {
    Box::new(move |document, _, _| {
        let font = standard_font(document, "Helvetica");
        let contents = document.add_object(content_stream(&hello("Geometry"), false));
        let mut page = dictionary! {
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => contents,
        };
        page.extend(&attrs);
        page
    })
}

#[test]
fn geometry_takes_part_with_equivalent_spellings_normalized() {
    let dir = tempdir().unwrap();
    let letter = || vec![0.into(), 0.into(), 612.into(), 792.into()];
    let path = build(
        &dir,
        "geometry.pdf",
        0,
        vec![
            // 0: MediaBox inherited from the page tree.
            geometry_page(Dictionary::new()),
            // 1: the same box spelled on the page, CropBox equal to it, reals.
            geometry_page(dictionary! {
                "MediaBox" => vec![0.into(), Object::Real(0.0), Object::Real(612.0), 792.into()],
                "CropBox" => letter(),
            }),
            // 2: rotated.
            geometry_page(dictionary! { "Rotate" => 90 }),
            // 3: the same rotation spelled differently.
            geometry_page(dictionary! { "Rotate" => 450 }),
            // 4: cropped.
            geometry_page(dictionary! {
                "CropBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            }),
        ],
    );

    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1]);
    assert_ne!(pages[0], pages[2]);
    assert_eq!(pages[2], pages[3]);
    assert_ne!(pages[0], pages[4]);
}

/// The stamp's document-number line; its presence is what makes a stamp line
/// maskable.
const DOCUMENT_NUMBER: &[u8] =
    b"BT /F1 7 Tf 1 0 0 1 70 -28 Tm (N\xfamero do documento: 26071613525300000000039184434) Tj ET\n";

const STAMP_A: &[u8] =
    b"Este documento foi gerado pelo usu\xe1rio 569.***.***-04 em 17/08/2026 13:43:50";
const STAMP_B: &[u8] =
    b"Este documento foi gerado pelo usu\xe1rio 111.***.***-99 em 01/09/2026 08:07:06";

/// A PJe-style page: the download stamp strip (page label, `stamp` in its own
/// text object, optionally the document number) plus a body line.
fn pje_page(page_label: &str, stamp: &[u8], document_number: bool) -> PageBuilder {
    let content = [
        b"BT /F1 9 Tf 1 0 0 1 490 -53 Tm (Num. 39526322 - P\xe1g. ".as_slice(),
        page_label.as_bytes(),
        b") Tj ET\n",
        if document_number {
            DOCUMENT_NUMBER
        } else {
            b""
        },
        b"BT /F1 7 Tf 1 0 0 1 70 -18 Tm ",
        stamp,
        b" ET\nBT /F1 12 Tf 72 700 Td (Corpo da peti\xe7\xe3o) Tj ET",
    ]
    .concat();
    text_page("F1", "Helvetica", content, true)
}

fn tj(text: &[u8]) -> Vec<u8> {
    [b"(".as_slice(), text, b") Tj"].concat()
}

#[test]
fn pje_download_stamp_is_masked_but_page_labels_are_not() {
    let dir = tempdir().unwrap();
    let path = build(
        &dir,
        "pje.pdf",
        0,
        vec![
            pje_page("1", &tj(STAMP_A), true),
            pje_page("1", &tj(STAMP_B), true),
            pje_page(
                "1",
                b"[(Este documento foi gerado pelo usu\xe1rio 222.***.***-00 ) -12 (em 02/09/2026 09:00:00)] TJ",
                true,
            ),
            pje_page("2", &tj(STAMP_A), true),
            // Not the stamp shape (no time): an ordinary string, not masked.
            pje_page(
                "1",
                &tj(b"Este documento foi gerado pelo usu\xe1rio 569.***.***-04 em 17/08/2026"),
                true,
            ),
            pje_page(
                "1",
                b"[(Este documento foi gerado pelo usu\xe1rio 333.***.***-11 ) -12 (em 03/09/2026 10:11:12)] TJ",
                true,
            ),
        ],
    );

    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1], "another user and time");
    assert_eq!(pages[2], pages[5], "the stamp shown through TJ");
    assert_ne!(pages[0], pages[3], "another page label");
    assert_ne!(pages[0], pages[4], "an unmasked string");
}

#[test]
fn stamp_sentence_outside_the_stamp_is_not_masked() {
    let dir = tempdir().unwrap();
    // The sentence as body text: same text object as other text, with the
    // document number present.
    let in_paragraph = |stamp: &'static [u8]| -> PageBuilder {
        let content = [
            DOCUMENT_NUMBER,
            b"BT /F1 12 Tf 72 700 Td (Certifico que:) Tj 0 -14 Td (".as_slice(),
            stamp,
            b") Tj ET",
        ]
        .concat();
        text_page("F1", "Helvetica", content, false)
    };
    let path = build(
        &dir,
        "quoted.pdf",
        0,
        vec![
            // No document-number line on the page.
            pje_page("1", &tj(STAMP_A), false),
            pje_page("1", &tj(STAMP_B), false),
            in_paragraph(STAMP_A),
            in_paragraph(STAMP_B),
        ],
    );

    let pages = fingerprints(&path);
    assert_ne!(pages[0], pages[1], "no document number: nothing is masked");
    assert_ne!(pages[2], pages[3], "shown with other text: not masked");
}

fn form_page(
    form_name: &'static str,
    page_resources: fn(&mut Document) -> Dictionary,
    form_resources: Option<fn(&mut Document) -> Dictionary>,
    form_content: &'static [u8],
    compress: bool,
) -> PageBuilder {
    Box::new(move |document, _, _| {
        let mut stream = content_stream(form_content, compress);
        stream.dict.set("Type", "XObject");
        stream.dict.set("Subtype", "Form");
        stream
            .dict
            .set("BBox", vec![0.into(), 0.into(), 100.into(), 100.into()]);
        if let Some(form_resources) = form_resources {
            stream.dict.set("Resources", form_resources(document));
        }
        let form = document.add_object(stream);
        let mut resources = page_resources(document);
        resources.set("XObject", dictionary! { form_name => form });
        let contents =
            document.add_object(content_stream(format!("/{form_name} Do").as_bytes(), false));
        dictionary! { "Resources" => resources, "Contents" => contents }
    })
}

fn helvetica_f1(document: &mut Document) -> Dictionary {
    let font = standard_font(document, "Helvetica");
    dictionary! { "Font" => dictionary! { "F1" => font } }
}

#[test]
fn form_xobjects_resolve_their_own_names_and_inherit_the_callers() {
    let dir = tempdir().unwrap();
    let no_resources = |_: &mut Document| Dictionary::new();
    let path = build(
        &dir,
        "forms.pdf",
        0,
        vec![
            // 0: a form with its own font /F1.
            form_page(
                "Fm0",
                no_resources,
                Some(helvetica_f1),
                b"BT /F1 12 Tf (Inside) Tj ET",
                false,
            ),
            // 1: the same drawing with the form and its font renamed.
            form_page(
                "X3",
                no_resources,
                Some(|document| {
                    let font = standard_font(document, "Helvetica");
                    dictionary! { "Font" => dictionary! { "R7" => font } }
                }),
                b"BT /R7 12 Tf (Inside) Tj ET",
                true,
            ),
            // 2: different form content.
            form_page(
                "Fm0",
                no_resources,
                Some(helvetica_f1),
                b"BT /F1 12 Tf (Outside) Tj ET",
                false,
            ),
            // 3 and 4: forms without /Resources draw with the page's font.
            form_page(
                "Fm0",
                helvetica_f1,
                None,
                b"BT /F1 12 Tf (Inside) Tj ET",
                false,
            ),
            form_page(
                "Fm0",
                |document| {
                    let font = standard_font(document, "Courier");
                    dictionary! { "Font" => dictionary! { "F1" => font } }
                },
                None,
                b"BT /F1 12 Tf (Inside) Tj ET",
                false,
            ),
        ],
    );

    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1]);
    assert_ne!(pages[0], pages[2]);
    assert_ne!(
        pages[3], pages[4],
        "inherited names resolve to different fonts"
    );
}

/// A page with one widget annotation that has no appearance stream, whose
/// parent field carries `value`.
fn widget_page(value: &'static str) -> PageBuilder {
    Box::new(move |document, _, _| {
        let font = standard_font(document, "Helvetica");
        let contents = document.add_object(content_stream(&hello("Form"), false));
        let field_id = document.new_object_id();
        let widget = document.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => vec![10.into(), 10.into(), 200.into(), 30.into()],
            "Parent" => field_id,
        });
        document.objects.insert(
            field_id,
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal(format!("name-{value}")),
                "V" => Object::string_literal(value),
                "DA" => Object::string_literal("/Helv 10 Tf 0 g"),
                "Kids" => vec![widget.into()],
            }
            .into(),
        );
        dictionary! {
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => contents,
            "Annots" => vec![widget.into()],
        }
    })
}

#[test]
fn widgets_hash_the_value_they_inherit_from_their_field() {
    let dir = tempdir().unwrap();
    let path = build(
        &dir,
        "widgets.pdf",
        0,
        vec![
            widget_page("Alice"),
            widget_page("Bob"),
            widget_page("Alice"),
        ],
    );
    let pages = fingerprints(&path);
    assert_ne!(pages[0], pages[1], "the parent field's /V is drawn");
    assert_eq!(pages[0], pages[2], "the field name /T is not");
}

/// A page whose content uses one resource of `category` under `name`.
fn resource_page(
    category: &'static str,
    name: &'static str,
    resource: fn() -> Object,
    content: &'static str,
) -> PageBuilder {
    Box::new(move |document, _, _| {
        let resource = document.add_object(resource());
        let contents =
            document.add_object(content_stream(content.replace("$", name).as_bytes(), true));
        dictionary! {
            "Resources" => dictionary! { category => dictionary! { name => resource } },
            "Contents" => contents,
        }
    })
}

fn shading(end_color: f32) -> Object {
    dictionary! {
        "ShadingType" => 2,
        "ColorSpace" => "DeviceRGB",
        "Coords" => vec![0.into(), 0.into(), 100.into(), 0.into()],
        "Function" => dictionary! {
            "FunctionType" => 2,
            "Domain" => vec![0.into(), 1.into()],
            "C0" => vec![1.into(), 0.into(), 0.into()],
            "C1" => vec![0.into(), 0.into(), Object::Real(end_color)],
            "N" => 1,
        },
    }
    .into()
}

#[test]
fn every_resource_operator_resolves_names_to_resources() {
    struct Case {
        category: &'static str,
        content: &'static str,
        resource: fn() -> Object,
        different: fn() -> Object,
    }
    let cases = [
        Case {
            category: "ColorSpace",
            content: "/$ cs /$ CS 0.5 sc 0.5 SC 0 0 10 10 re B",
            resource: || {
                vec!["CalGray".into(), dictionary! { "WhitePoint" => vec![Object::Real(0.9505), 1.into(), Object::Real(1.089)], "Gamma" => 1 }.into()].into()
            },
            different: || {
                vec!["CalGray".into(), dictionary! { "WhitePoint" => vec![Object::Real(0.9505), 1.into(), Object::Real(1.089)], "Gamma" => Object::Real(2.2) }.into()].into()
            },
        },
        Case {
            category: "ExtGState",
            content: "/$ gs 0 0 10 10 re f",
            resource: || dictionary! { "Type" => "ExtGState", "ca" => Object::Real(0.5) }.into(),
            different: || dictionary! { "Type" => "ExtGState", "ca" => Object::Real(0.7) }.into(),
        },
        Case {
            category: "Pattern",
            content: "/Pattern cs /$ scn /Pattern CS 0.2 0.4 0.6 /$ SCN 0 0 10 10 re B",
            resource: || dictionary! { "PatternType" => 2, "Shading" => shading(1.0) }.into(),
            different: || dictionary! { "PatternType" => 2, "Shading" => shading(0.5) }.into(),
        },
        Case {
            category: "Shading",
            content: "/$ sh",
            resource: || shading(1.0),
            different: || shading(0.5),
        },
        Case {
            category: "Properties",
            content: "/OC /$ BDC /Span <</ActualText (x)>> BDC 0 0 10 10 re f EMC EMC /Mark /$ DP",
            resource: || {
                dictionary! { "Type" => "OCG", "Name" => Object::string_literal("Layer A") }.into()
            },
            different: || {
                dictionary! { "Type" => "OCG", "Name" => Object::string_literal("Layer B") }.into()
            },
        },
    ];

    let dir = tempdir().unwrap();
    for case in cases {
        let path = build(
            &dir,
            &format!("{}.pdf", case.category),
            0,
            vec![
                resource_page(case.category, "R0", case.resource, case.content),
                resource_page(case.category, "Xi42", case.resource, case.content),
                resource_page(case.category, "R0", case.different, case.content),
            ],
        );
        let pages = fingerprints(&path);
        assert_eq!(pages[0], pages[1], "{}: renamed resource", case.category);
        assert_ne!(pages[0], pages[2], "{}: different resource", case.category);
    }
}

#[test]
fn inline_image_the_parser_skips_still_distinguishes_pages() {
    // lopdf skips an inline image whose colour space it cannot size and keeps
    // an empty `BI`; hashing that would make these two pages collide.
    let dir = tempdir().unwrap();
    let inline = |fill: u8| -> PageBuilder {
        let content = [
            b"q BI /W 2 /H 2 /CS /ICCBased /BPC 8 ID\n".as_slice(),
            &[fill; 12],
            b"\nEI Q",
        ]
        .concat();
        text_page("F1", "Helvetica", content, false)
    };
    let path = build(&dir, "inline.pdf", 0, vec![inline(0x00), inline(0xFF)]);
    let pages = fingerprints(&path);
    assert_ne!(pages[0], pages[1]);
}

/// A page whose content lopdf cannot parse cleanly (an inline image with a
/// colour space it cannot size) drawing text with `base_font`, optionally
/// carrying a resource the content never names.
fn unparseable_page(base_font: &'static str, unused_resource: bool) -> PageBuilder {
    Box::new(move |document, _, _| {
        let font = standard_font(document, base_font);
        let content = [
            b"q BI /W 2 /H 2 /CS /ICCBased /BPC 8 ID\n".as_slice(),
            &[0x7Fu8; 12],
            b"\nEI Q BT /F1 12 Tf 72 700 Td (Scanned) Tj ET",
        ]
        .concat();
        let contents = document.add_object(content_stream(&content, false));
        let mut resources = dictionary! { "Font" => dictionary! { "F1" => font } };
        if unused_resource {
            let template = document.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
                },
                b"0 0 10 10 re f".to_vec(),
            ));
            resources.set("XObject", dictionary! { "TPL7" => template });
        }
        dictionary! { "Resources" => resources, "Contents" => contents }
    })
}

#[test]
fn unparseable_content_ignores_resources_it_never_names() {
    let dir = tempdir().unwrap();
    let path = build(
        &dir,
        "unparseable.pdf",
        0,
        vec![
            unparseable_page("Helvetica", true),
            // What `split` leaves after pruning the unused template.
            unparseable_page("Helvetica", false),
            unparseable_page("Courier", false),
        ],
    );
    let pages = fingerprints(&path);
    assert_eq!(pages[0], pages[1], "an unnamed resource is not drawn");
    assert_ne!(pages[1], pages[2], "a named resource still is");
}

#[test]
fn split_and_merge_outputs_keep_page_fingerprints() {
    let dir = tempdir().unwrap();
    let original = fingerprints(&fixture("11-pages.pdf"));
    assert_eq!(original.len(), 11);
    assert_eq!(
        original
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        11
    );
    assert_eq!(fingerprints(&fixture("11-pages-objstm.pdf")), original);

    let part = dir.path().join("part.pdf");
    split(
        &fixture("11-pages.pdf"),
        &[SplitOutput {
            range: PageRangeGroup::parse("3-5").unwrap(),
            path: part.clone(),
        }],
    )
    .unwrap();
    assert_eq!(fingerprints(&part), original[2..5]);

    let merged = dir.path().join("merged.pdf");
    merge(
        &[
            MergeInput::all(fixture("11-pages-objstm.pdf")),
            MergeInput::all(&part),
        ],
        &merged,
    )
    .unwrap();
    let merged = fingerprints(&merged);
    assert_eq!(merged[..11], original[..]);
    assert_eq!(merged[11..], original[2..5]);
}

#[test]
fn fingerprint_cli_prints_versioned_json_for_selected_pages() {
    let output = pdq()
        .arg("fingerprint")
        .arg(fixture("11-pages.pdf"))
        .arg("--pages")
        .arg("2-3,2")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["version"], "pfp1");
    let pages = json["pages"].as_array().unwrap();
    assert_eq!(
        pages
            .iter()
            .map(|p| p["page"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let original =
        fingerprint_pages(&fixture("11-pages.pdf"), &FingerprintOptions::default()).unwrap();
    for page in pages {
        let hex = page["fingerprint"].as_str().unwrap();
        assert_eq!(hex.len(), 64);
        let number = page["page"].as_u64().unwrap() as usize;
        assert_eq!(hex, original[number - 1].hex());
    }
}

#[test]
fn fingerprint_cli_handles_encrypted_inputs() {
    pdq()
        .arg("fingerprint")
        .arg(fixture("owner-only.pdf"))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"page\":11"));

    pdq()
        .arg("fingerprint")
        .arg(fixture("user-password.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--password"));

    pdq()
        .arg("fingerprint")
        .arg(fixture("user-password.pdf"))
        .arg("--password")
        .arg("user")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"page\":11"));
}
