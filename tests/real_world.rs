//! Integration suite against replicas of real-world court documents.
//!
//! The fixtures are built as raw bytes (not through lopdf's writer, which
//! would normalize them) and replicate the two document families used by the
//! README benchmark corpus, at reduced scale:
//!
//! - "PJe-like" (OpenPDF/iText producers): balanced 10-ary page tree,
//!   per-page inline resources, one form XObject per page, image filter zoo
//!   (JBIG2 with globals, CCITTFax, JPX, DCT, Flate with SMask), link
//!   annotations with GoTo destinations, outlines, a named-destination tree,
//!   XMP metadata blobs, zero-length streams, and uncompressed forms.
//! - "TRF4-like" (FPDF/FPDI producers): flat page tree (every page a direct
//!   kid of one /Pages node), MediaBox inherited from the root, and a single
//!   shared /Resources dictionary listing every /TPLn form of the whole file
//!   (the shape that makes naive splitters quadratic), plus depth-3
//!   outlines and a non-UTF8 Info string.
//!
//! For running against directories of actual PDFs, see tests/corpus.rs.

use std::{
    collections::BTreeSet, fmt::Write as _, fs, io::Write as _, path::Path, process::Command,
};

use flate2::{write::ZlibEncoder, Compression};
use lopdf::{Dictionary, Document, Object, ObjectId};
use pdq::{merge, page_count, split, split_pages, MergeInput, PageRangeGroup, SplitOutput};
use tempfile::tempdir;

const PJE_PAGES: usize = 137;
const TRF4_PAGES: usize = 61;
const TINY_JPEG: &[u8] = include_bytes!("fixtures/tiny-gray.jpg");
const LOREM: &[u8] = b"EXCELENTISSIMO SENHOR DOUTOR JUIZ DA VARA FICTICIA. PARTE \
AUTORA FICTICIA, pessoa fisica de existencia meramente ilustrativa, vem expor e \
requerer o que segue neste documento sintetico gerado para fins de teste. ";

// ---------------------------------------------------------------------------
// raw PDF builder
// ---------------------------------------------------------------------------

struct RawPdf {
    header: &'static [u8],
    objects: Vec<Option<Vec<u8>>>,
}

impl RawPdf {
    fn new(header: &'static [u8]) -> Self {
        Self {
            header,
            objects: vec![None],
        }
    }

    fn reserve(&mut self) -> usize {
        self.objects.push(None);
        self.objects.len() - 1
    }

    fn put(&mut self, id: usize, body: Vec<u8>) {
        assert!(self.objects[id].is_none(), "object {id} written twice");
        self.objects[id] = Some(body);
    }

    fn add(&mut self, body: Vec<u8>) -> usize {
        let id = self.reserve();
        self.put(id, body);
        id
    }

    fn add_stream(&mut self, dict_inner: &[u8], data: &[u8]) -> usize {
        let mut body = Vec::with_capacity(dict_inner.len() + data.len() + 48);
        body.extend_from_slice(b"<<");
        body.extend_from_slice(dict_inner);
        write!(body, "/Length {}>>stream", data.len()).unwrap();
        body.push(b'\n');
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.add(body)
    }

    fn write_to(&self, path: &Path, trailer_inner: &[u8]) {
        let mut out = Vec::new();
        out.extend_from_slice(self.header);
        let mut offsets = vec![0usize; self.objects.len()];
        for (id, body) in self.objects.iter().enumerate().skip(1) {
            let body = body
                .as_ref()
                .unwrap_or_else(|| panic!("object {id} was reserved but never written"));
            offsets[id] = out.len();
            writeln!(out, "{id} 0 obj").unwrap();
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_at = out.len();
        writeln!(out, "xref\n0 {}", self.objects.len()).unwrap();
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets[1..] {
            writeln!(out, "{offset:010} 00000 n ").unwrap();
        }
        out.extend_from_slice(b"trailer\n<<");
        out.extend_from_slice(trailer_inner);
        writeln!(
            out,
            "/Size {}>>\nstartxref\n{xref_at}\n%%EOF",
            self.objects.len()
        )
        .unwrap();
        fs::write(path, out).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
    }
}

fn junk(seed: &mut u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Remap bytes that would terminate or escape a PDF literal string.
fn string_safe(mut bytes: Vec<u8>) -> Vec<u8> {
    for byte in &mut bytes {
        *byte = match *byte {
            b'\\' => b'|',
            b'(' => b'{',
            b')' => b'}',
            b'\r' => b'~',
            other => other,
        };
    }
    bytes
}

/// Incompressible padding that strict content parsers accept: an off-page
/// text draw. Real scanned filings carry high-entropy payloads per page;
/// this keeps stream sizes realistic without breaking `Content::decode`.
fn padded_ops(base: &[u8], pad: usize, seed: &mut u64) -> Vec<u8> {
    let mut ops = base.to_vec();
    ops.extend_from_slice(b"\nq BT /F1 4 Tf 0 -20000 Td (");
    ops.extend_from_slice(&string_safe(junk(seed, pad)));
    ops.extend_from_slice(b") Tj ET Q");
    ops
}

/// Padding inside a `%` comment. Some producers emit comments in content
/// streams; strict parsers reject them, which forces pdq's split onto its
/// no-prune fallback path.
fn comment_padded_ops(base: &[u8], pad: usize, seed: &mut u64) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut ops = base.to_vec();
    ops.extend_from_slice(b"\n% ");
    ops.extend(junk(seed, pad).iter().map(|b| ALPHABET[(b & 63) as usize]));
    ops
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

// ---------------------------------------------------------------------------
// PJe-like fixture (OpenPDF merge of many scanned filings)
// ---------------------------------------------------------------------------

fn build_pje_like(path: &Path, pages: usize) {
    let seed = &mut 0x5dee_ce66_d001_u64;
    let mut pdf = RawPdf::new(b"%PDF-1.5\n%\xe2\xe3\xcf\xd3\n");

    // shared header seal: RGB image with a gray SMask, drawn on every page
    let img0 = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/ColorSpace/DeviceGray/Width 16/Height 16\
          /BitsPerComponent 8/Filter/FlateDecode",
        &deflate(&junk(seed, 256)),
    );
    let img1_dict = format!(
        "/Type/XObject/Subtype/Image/ColorSpace/DeviceRGB/Width 16/Height 16\
         /BitsPerComponent 8/SMask {img0} 0 R/Filter/FlateDecode"
    );
    let img1 = pdf.add_stream(img1_dict.as_bytes(), &deflate(&junk(seed, 768)));

    // scanned-page image zoo: filters pdq must pass through untouched
    let jbig2_globals = pdf.add_stream(b"", &junk(seed, 64));
    let im_jbig2_dict = format!(
        "/Subtype/Image/Width 64/Height 64/BitsPerComponent 1/ImageMask true\
         /Filter/JBIG2Decode/DecodeParms<</JBIG2Globals {jbig2_globals} 0 R>>"
    );
    // note: no /Type key, like the untyped images PJe emits
    let im_jbig2 = pdf.add_stream(im_jbig2_dict.as_bytes(), &junk(seed, 600));
    let im_ccitt = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/Width 128/Height 128/BitsPerComponent 1\
          /Filter/CCITTFaxDecode/DecodeParms<</K -1/Columns 128/Rows 128>>",
        &junk(seed, 900),
    );
    let im_jpx = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/Width 32/Height 32/ColorSpace/DeviceRGB\
          /BitsPerComponent 8/Filter/JPXDecode",
        &junk(seed, 1200),
    );
    let im_dct = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/Width 8/Height 8/ColorSpace/DeviceGray\
          /BitsPerComponent 8/Filter/DCTDecode",
        TINY_JPEG,
    );

    // fonts: standard-14 page fonts plus a pool with the structures the
    // originals carry (ToUnicode CMaps, descriptors, embedded font programs)
    let std_fonts: Vec<usize> = [
        b"Times-Roman".as_slice(),
        b"Times-Bold",
        b"Helvetica",
        b"Helvetica-Bold",
    ]
    .iter()
    .map(|name| {
        let mut body = b"<</Type/Font/Subtype/Type1/BaseFont/".to_vec();
        body.extend_from_slice(name);
        body.extend_from_slice(b"/Encoding/WinAnsiEncoding>>");
        pdf.add(body)
    })
    .collect();
    let mut pool_fonts = Vec::new();
    for i in 0..6 {
        let tounicode = if i % 3 == 0 {
            let cmap = pdf.add_stream(
                b"/Filter/FlateDecode",
                &deflate(
                    b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
                      1 begincodespacerange <00> <FF> endcodespacerange endcmap end end",
                ),
            );
            format!("/ToUnicode {cmap} 0 R")
        } else {
            String::new()
        };
        let descriptor = if i % 2 == 0 {
            let font_file = pdf.add_stream(
                b"/Subtype/Type1C/Filter/FlateDecode",
                &deflate(&junk(seed, 400)),
            );
            let descriptor = pdf.add(
                format!(
                    "<</Type/FontDescriptor/FontName/FicFont{i}/Flags 32\
                     /FontBBox[-100 -200 1100 900]/ItalicAngle 0/Ascent 720\
                     /Descent -200/CapHeight 660/StemV 80/FontFile3 {font_file} 0 R>>"
                )
                .into_bytes(),
            );
            format!("/FontDescriptor {descriptor} 0 R")
        } else {
            String::new()
        };
        pool_fonts.push(
            pdf.add(
                format!(
                    "<</Type/Font/Subtype/Type1/BaseFont/FicPool{i}\
                 /Encoding/WinAnsiEncoding{tounicode}{descriptor}>>"
                )
                .into_bytes(),
            ),
        );
    }

    // pages, contents, one form per page
    let page_ids: Vec<usize> = (0..pages).map(|_| pdf.reserve()).collect();
    let mut annots = vec![None; pages];
    let mut forms = Vec::with_capacity(pages);
    let mut contents = Vec::with_capacity(pages);
    for (i, &page_id) in page_ids.iter().enumerate() {
        let pool_font = pool_fonts[(i / 10) % pool_fonts.len()];
        let mut form_ops = Vec::new();
        for line in 0..5 {
            let start = (i * 7 + line * 41) % LOREM.len();
            let end = (start + 60).min(LOREM.len());
            write!(form_ops, "BT /F1 10 Tf 46 {} Td (", 780 - line * 16).unwrap();
            form_ops.extend_from_slice(&LOREM[start..end]);
            form_ops.extend_from_slice(b") Tj ET\n");
        }
        let form_dict = format!(
            "/Type/XObject/Subtype/Form/FormType 1/BBox[0 -19.84 595.32 841.92]\
             /Matrix[1 0 0 1 0 19.84]\
             /Resources<</ProcSet[/PDF/Text]/Font<</F1 {} 0 R/F2 {pool_font} 0 R>>>>",
            std_fonts[i % std_fonts.len()],
        );
        let form = if i % 50 == 25 {
            // a few uncompressed forms, as in the originals
            pdf.add_stream(form_dict.as_bytes(), &padded_ops(&form_ops, 200, seed))
        } else {
            let mut dict = form_dict.into_bytes();
            dict.extend_from_slice(b"/Filter/FlateDecode");
            pdf.add_stream(&dict, &deflate(&padded_ops(&form_ops, 300, seed)))
        };
        forms.push(form);

        let mut ops = format!(
            "q 51.6 0 0 51.6 266.7 786 cm /img1 Do Q\n\
             q BT /F1 9 Tf 498 806 Td (Fls.: {}) Tj ET Q\n\
             q 1 0 0 1 0 0 cm /Xf0 Do Q",
            i + 1
        )
        .into_bytes();
        if i % 5 == 0 {
            ops.extend_from_slice(b"\nq 40 0 0 40 40 80 cm /imD Do Q");
        }
        if i % 7 == 0 {
            ops.extend_from_slice(b"\n0 g q 100 0 0 100 60 200 cm /imJ Do Q");
        }
        if i % 11 == 0 {
            ops.extend_from_slice(b"\n0 g q 100 0 0 100 180 200 cm /imC Do Q");
        }
        if i % 13 == 0 {
            ops.extend_from_slice(b"\nq 60 0 0 60 320 200 cm /imX Do Q");
        }
        contents.push(pdf.add_stream(
            b"/Filter/FlateDecode",
            &deflate(&padded_ops(&ops, 400, seed)),
        ));

        if i % 17 == 0 {
            // self-referential GoTo link, like PJe's "Fls." header links
            let dest = pdf.add(format!("[{page_id} 0 R/XYZ null null 0]").into_bytes());
            annots[i] = Some(
                pdf.add(
                    format!(
                        "<</Subtype/Link/A<</S/GoTo/D {dest} 0 R>>/C[0 0 1]\
                     /Border[0 0 0]/Rect[152.02 570.4 442.98 584.4]>>"
                    )
                    .into_bytes(),
                ),
            );
        }
    }

    // balanced page tree, built bottom-up in chunks of 10 (iText style)
    struct TreeNode {
        id: usize,
        count: usize,
        kids: Vec<usize>,
    }
    let mut parent_of = vec![0usize; pdf.objects.len() + pages * 2];
    let mut level: Vec<(usize, usize)> = page_ids.iter().map(|&id| (id, 1)).collect();
    let mut nodes: Vec<TreeNode> = Vec::new();
    while level.len() > 1 {
        let mut next = Vec::new();
        for group in level.chunks(10) {
            let id = pdf.reserve();
            if parent_of.len() <= id {
                parent_of.resize(id + 16, 0);
            }
            for &(child, _) in group {
                parent_of[child] = id;
            }
            let count = group.iter().map(|&(_, count)| count).sum();
            nodes.push(TreeNode {
                id,
                count,
                kids: group.iter().map(|&(child, _)| child).collect(),
            });
            next.push((id, count));
        }
        level = next;
    }
    let root_pages = level[0].0;
    for node in &nodes {
        let mut body = b"<</Type/Pages/Kids[".to_vec();
        for kid in &node.kids {
            write!(body, "{kid} 0 R ").unwrap();
        }
        write!(body, "]/Count {}", node.count).unwrap();
        if node.id == root_pages {
            body.extend_from_slice(b"/ITXT(1.3.26)");
        } else {
            write!(body, "/Parent {} 0 R", parent_of[node.id]).unwrap();
        }
        body.extend_from_slice(b">>");
        pdf.put(node.id, body);
    }
    for (i, &page_id) in page_ids.iter().enumerate() {
        let mut xobjects = format!("/img0 {img0} 0 R/img1 {img1} 0 R/Xf0 {} 0 R", forms[i]);
        if i % 5 == 0 {
            write!(xobjects, "/imD {im_dct} 0 R").unwrap();
        }
        if i % 7 == 0 {
            write!(xobjects, "/imJ {im_jbig2} 0 R").unwrap();
        }
        if i % 11 == 0 {
            write!(xobjects, "/imC {im_ccitt} 0 R").unwrap();
        }
        if i % 13 == 0 {
            write!(xobjects, "/imX {im_jpx} 0 R").unwrap();
        }
        let annot_entry = match annots[i] {
            Some(annot) => format!("/Annots[{annot} 0 R]"),
            None => String::new(),
        };
        pdf.put(
            page_id,
            format!(
                "<</Type/Page/Contents {} 0 R/Resources<</Font<</F1 {} 0 R/F2 {} 0 R\
                 /F3 {} 0 R/F4 {} 0 R>>/XObject<<{xobjects}>>>>{annot_entry}\
                 /Parent {} 0 R/MediaBox[0 0 595 842]>>",
                contents[i],
                std_fonts[0],
                std_fonts[1],
                std_fonts[2],
                std_fonts[3],
                parent_of[page_id],
            )
            .into_bytes(),
        );
    }

    // side objects the originals carry: XMP packets, a /Type/Stream font
    // blob, zero-length streams -- all reachable through a catalog anchor
    let xmp = pdf.add_stream(
        b"/Type/Metadata/Subtype/XML",
        b"<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
          <x:xmpmeta xmlns:x=\"adobe:ns:meta/\"></x:xmpmeta>\
          <?xpacket end=\"w\"?>",
    );
    let type_stream_blob = pdf.add_stream(
        b"/Type/Stream/Length1 1200/Filter/FlateDecode",
        &deflate(&junk(seed, 600)),
    );
    let zero_a = pdf.add_stream(b"", b"");
    let zero_b = pdf.add_stream(b"", b"");
    let anchor = pdf
        .add(format!("[{xmp} 0 R {type_stream_blob} 0 R {zero_a} 0 R {zero_b} 0 R]").into_bytes());

    // flat outlines plus the matching named-destination tree
    let outline_count = pages / 10;
    let outline_root = pdf.reserve();
    let outline_ids: Vec<usize> = (0..outline_count).map(|_| pdf.reserve()).collect();
    let mut dest_names = Vec::new();
    for (j, &outline_id) in outline_ids.iter().enumerate() {
        let target = page_ids[(j * pages) / outline_count];
        let prev = if j > 0 {
            format!("/Prev {} 0 R", outline_ids[j - 1])
        } else {
            String::new()
        };
        let next = if j + 1 < outline_count {
            format!("/Next {} 0 R", outline_ids[j + 1])
        } else {
            String::new()
        };
        pdf.put(
            outline_id,
            format!(
                "<</Title(01/01/2026 - Documento Ficticio {:03})\
                 /A<</S/GoTo/D[{target} 0 R/XYZ null null 0]>>\
                 /Parent {outline_root} 0 R{prev}{next}>>",
                j + 1
            )
            .into_bytes(),
        );
        dest_names.push(format!("(dest{j:03})[{target} 0 R/XYZ null null 0]"));
    }
    pdf.put(
        outline_root,
        format!(
            "<</Type/Outlines/First {} 0 R/Last {} 0 R/Count {outline_count}>>",
            outline_ids[0],
            outline_ids[outline_count - 1],
        )
        .into_bytes(),
    );
    let dests_leaf = pdf.add(format!("<</Names[{}]>>", dest_names.join(" ")).into_bytes());
    let names_root = pdf.add(format!("<</Dests {dests_leaf} 0 R>>").into_bytes());

    let info = pdf.add(
        b"<</Title(PROCESSO: 0000000-00.0000.5.05.0000 - ACAO TRABALHISTA - RITO ORDINARIO)\
          /Subject(RECLAMANTE: PARTE AUTORA FICTICIA ; RECLAMADO: RECLAMADA FICTICIA S.A.)\
          /Author(Processo Judicial Eletronico)/Creator(PJe - 2.19.3)\
          /Producer(OpenPDF 1.3.26)/CreationDate(D:20260101080000-03'00')>>"
            .to_vec(),
    );
    let catalog = pdf.add(
        format!(
            "<</Names {names_root} 0 R/Type/Catalog/Outlines {outline_root} 0 R\
             /Pages {root_pages} 0 R/PieceInfo<</PDQfill<</Private {anchor} 0 R>>>>>>"
        )
        .into_bytes(),
    );

    pdf.write_to(
        path,
        format!(
            "/Info {info} 0 R/ID[<f1283887edb47ad64a6737686df39e30>\
             <f1283887edb47ad64a6737686df39e30>]/Root {catalog} 0 R"
        )
        .as_bytes(),
    );
}

// ---------------------------------------------------------------------------
// TRF4-like fixture (FPDF/FPDI "Documento Unificado")
// ---------------------------------------------------------------------------

fn build_trf4_like(path: &Path, pages: usize, comment_padding: bool) {
    let seed = &mut 0x0bad_cafe_f00d_u64;
    let mut pdf = RawPdf::new(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n");

    let pages_obj = pdf.reserve();
    let shared_resources = pdf.reserve();
    assert_eq!(pages_obj, 1, "FPDF layout puts the page tree at object 1");
    assert_eq!(
        shared_resources, 2,
        "FPDF layout puts resources at object 2"
    );

    // pages and their tiny contents; MediaBox comes only from the root node
    let mut page_ids = Vec::with_capacity(pages);
    for i in 0..pages {
        let page_id = pdf.reserve();
        let base = format!(
            "2 J\n0.57 w\nq 0 J 1 w 0 j 0 G 0 g \
             1.0000 0 0 1.0000 0.0000 0.0000 cm /TPL{i} Do Q\n"
        );
        let ops = if comment_padding {
            comment_padded_ops(base.as_bytes(), 220, seed)
        } else {
            padded_ops(base.as_bytes(), 220, seed)
        };
        let content = pdf.add_stream(b"/Filter/FlateDecode", &deflate(&ops));
        pdf.put(
            page_id,
            format!(
                "<</Type/Page/Parent {pages_obj} 0 R\
                 /Resources {shared_resources} 0 R/Contents {content} 0 R>>"
            )
            .into_bytes(),
        );
        page_ids.push(page_id);
    }

    // per-source-document font clusters, as FPDI copies them
    let cluster_count = pages.div_ceil(5);
    let mut cluster_fonts = Vec::with_capacity(cluster_count);
    for c in 0..cluster_count {
        let descriptor = if c % 3 == 0 {
            let font_file = pdf.add_stream(
                b"/Subtype/Type1C/Filter/FlateDecode",
                &deflate(&junk(seed, 350)),
            );
            let widths = pdf.add(b"[500 500 500 500 500 500 500 500]".to_vec());
            let descriptor = pdf.add(
                format!(
                    "<</Type/FontDescriptor/FontName/DocFont{c}/Flags 32\
                     /FontBBox[-100 -200 1100 900]/ItalicAngle 0/Ascent 700\
                     /Descent -210/CapHeight 650/StemV 80/FontFile3 {font_file} 0 R>>"
                )
                .into_bytes(),
            );
            format!(
                "/FirstChar 32/LastChar 39/Widths {widths} 0 R\
                 /FontDescriptor {descriptor} 0 R"
            )
        } else {
            String::new()
        };
        cluster_fonts.push(
            pdf.add(
                format!(
                    "<</Type/Font/Subtype/Type1/BaseFont/Helvetica\
                 /Encoding/WinAnsiEncoding{descriptor}>>"
                )
                .into_bytes(),
            ),
        );
    }
    let f1 = cluster_fonts[0];

    // one TPL form per page, each with an indirect nested resources dict
    let mut tpls = Vec::with_capacity(pages);
    for i in 0..pages {
        let cluster_font = cluster_fonts[(i / 5) % cluster_fonts.len()];
        let mut xobject_entry = String::new();
        if i % 12 == 0 {
            let image = pdf.add_stream(
                b"/Type/XObject/Subtype/Image/ColorSpace/DeviceRGB/Width 16\
                  /Height 16/BitsPerComponent 8/Filter/FlateDecode",
                &deflate(&junk(seed, 768)),
            );
            xobject_entry = format!("/Im1 {image} 0 R");
        } else if i % 20 == 7 {
            let image = pdf.add_stream(
                b"/Type/XObject/Subtype/Image/ColorSpace/DeviceGray/Width 8\
                  /Height 8/BitsPerComponent 8/Filter/DCTDecode",
                TINY_JPEG,
            );
            xobject_entry = format!("/Im1 {image} 0 R");
        }
        let nested = pdf.add(
            format!(
                "<</ProcSet[/PDF/Text/ImageB/ImageC/ImageI]\
                 /Font<</F1 {cluster_font} 0 R>>/XObject<<{xobject_entry}>>>>"
            )
            .into_bytes(),
        );
        let mut ops = Vec::new();
        for line in 0..4 {
            let start = (i * 11 + line * 47) % LOREM.len();
            let end = (start + 55).min(LOREM.len());
            write!(
                ops,
                "BT /F1 10 Tf 52 {} Td (Fl. {}: ",
                760 - line * 18,
                i + 1
            )
            .unwrap();
            ops.extend_from_slice(&LOREM[start..end]);
            ops.extend_from_slice(b") Tj ET\n");
        }
        if !xobject_entry.is_empty() {
            ops.extend_from_slice(b"q 50 0 0 50 300 300 cm /Im1 Do Q\n");
        }
        let dict = format!(
            "/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.28 841.89]\
             /Resources {nested} 0 R/Filter/FlateDecode"
        );
        tpls.push(pdf.add_stream(dict.as_bytes(), &deflate(&padded_ops(&ops, 250, seed))));
    }

    // object 2: the shared resources dictionary listing every TPL in the file
    let mut resources =
        format!("<</ProcSet[/PDF/Text/ImageB/ImageC/ImageI]/Font<</F1 {f1} 0 R>>/XObject<<")
            .into_bytes();
    for (i, tpl) in tpls.iter().enumerate() {
        write!(resources, "/TPL{i} {tpl} 0 R").unwrap();
    }
    resources.extend_from_slice(b">>>>");
    pdf.put(shared_resources, resources);

    pdf.put(pages_obj, {
        let mut body = b"<</Type/Pages/Kids[".to_vec();
        for page_id in &page_ids {
            write!(body, "{page_id} 0 R ").unwrap();
        }
        write!(body, "]/Count {pages}/MediaBox[0 0 595.28 841.89]>>").unwrap();
        body
    });

    // depth-3 outlines: processo -> evento -> anexo
    let outline_root = pdf.reserve();
    let top = pdf.reserve();
    let l2_count = 6;
    let l3_per_l2 = pages / 8;
    let l2_ids: Vec<usize> = (0..l2_count).map(|_| pdf.reserve()).collect();
    let l3_ids: Vec<Vec<usize>> = (0..l2_count)
        .map(|_| (0..l3_per_l2).map(|_| pdf.reserve()).collect())
        .collect();
    pdf.put(
        outline_root,
        format!("<</Type/Outlines/First {top} 0 R/Last {top} 0 R>>").into_bytes(),
    );
    pdf.put(
        top,
        format!(
            "<</Title(PROCESSO 0000000-00.0000.4.00.0000/XX - Parte 1)\
             /Parent {outline_root} 0 R/First {} 0 R/Last {} 0 R\
             /Dest[{} 0 R/XYZ 0 841.89 null]/Count 0>>",
            l2_ids[0],
            l2_ids[l2_count - 1],
            page_ids[0],
        )
        .into_bytes(),
    );
    for (j, &l2_id) in l2_ids.iter().enumerate() {
        let target = page_ids[(j * pages) / l2_count];
        let prev = if j > 0 {
            format!("/Prev {} 0 R", l2_ids[j - 1])
        } else {
            String::new()
        };
        let next = if j + 1 < l2_count {
            format!("/Next {} 0 R", l2_ids[j + 1])
        } else {
            String::new()
        };
        pdf.put(
            l2_id,
            format!(
                "<</Title(Evento {} - Documento sintetico)/Parent {top} 0 R\
                 /First {} 0 R/Last {} 0 R/Count 0\
                 /Dest[{target} 0 R/XYZ 0 841.89 null]{prev}{next}>>",
                j + 1,
                l3_ids[j][0],
                l3_ids[j][l3_per_l2 - 1],
            )
            .into_bytes(),
        );
        for (k, &l3_id) in l3_ids[j].iter().enumerate() {
            let target = page_ids[((j * pages) / l2_count + k) % pages];
            let prev = if k > 0 {
                format!("/Prev {} 0 R", l3_ids[j][k - 1])
            } else {
                String::new()
            };
            let next = if k + 1 < l3_per_l2 {
                format!("/Next {} 0 R", l3_ids[j][k + 1])
            } else {
                String::new()
            };
            pdf.put(
                l3_id,
                format!(
                    "<</Title(ANEXO{} - PECA{})/Parent {l2_id} 0 R\
                     /Dest[{target} 0 R/XYZ 0 841.89 null]{prev}{next}>>",
                    k + 1,
                    j + 1,
                )
                .into_bytes(),
            );
        }
    }

    // FPDF-style Info; /Author is UTF-8 bytes in a plain hex string (renders
    // as mojibake in viewers, exactly like the source files)
    let info = pdf.add(
        b"<</Producer(FPDF 1.86)/Author<54524634202D20496E666F726DC3A174696361>\
          /Subject(Documento Unificado)/Title(Documento Unificado)\
          /CreationDate(D:20260101120000-03'00')>>"
            .to_vec(),
    );
    let catalog = pdf.add(
        format!(
            "<</Type/Catalog/Pages {pages_obj} 0 R/Outlines {outline_root} 0 R\
             /PageMode/UseOutlines>>"
        )
        .into_bytes(),
    );

    // no /ID in the trailer, matching FPDF output
    pdf.write_to(
        path,
        format!("/Root {catalog} 0 R/Info {info} 0 R").as_bytes(),
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct QpdfValidator {
    available: bool,
}

impl QpdfValidator {
    fn detect() -> Self {
        let available = matches!(
            Command::new("qpdf").arg("--version").output(),
            Ok(output) if output.status.success()
        );
        Self { available }
    }

    fn check_pdf(&self, path: &Path) {
        if !self.available {
            eprintln!(
                "qpdf unavailable; skipping qpdf --check for {}",
                path.display()
            );
            return;
        }
        let output = Command::new("qpdf")
            .arg("--check")
            .arg(path)
            .output()
            .unwrap_or_else(|err| panic!("failed to run qpdf --check {}: {err}", path.display()));
        assert!(
            output.status.success(),
            "qpdf --check failed for {}\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn npages(&self, path: &Path) -> Option<usize> {
        if !self.available {
            return None;
        }
        let output = Command::new("qpdf")
            .arg("--show-npages")
            .arg(path)
            .output()
            .unwrap_or_else(|err| {
                panic!("failed to run qpdf --show-npages {}: {err}", path.display())
            });
        assert!(
            output.status.success(),
            "qpdf --show-npages failed for {}",
            path.display()
        );
        Some(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .expect("qpdf --show-npages should print an integer"),
        )
    }

    fn validate(&self, path: &Path, expected_pages: usize) {
        self.check_pdf(path);
        if let Some(actual) = self.npages(path) {
            assert_eq!(actual, expected_pages, "page count for {}", path.display());
        }
    }
}

fn split_page_name(page: usize, total: usize) -> String {
    let width = total.to_string().len();
    format!("page-{page:0width$}.pdf")
}

fn resolve_dict<'a>(document: &'a Document, object: &'a Object) -> &'a Dictionary {
    match object {
        Object::Reference(id) => document
            .get_object(*id)
            .expect("dangling reference")
            .as_dict()
            .expect("referenced object is not a dictionary"),
        Object::Dictionary(dict) => dict,
        other => panic!("expected dictionary, found {other:?}"),
    }
}

fn single_page_id(document: &Document) -> ObjectId {
    let pages = document.get_pages();
    assert_eq!(pages.len(), 1, "expected exactly one page");
    *pages.values().next().unwrap()
}

fn page_xobject_names(document: &Document, page_id: ObjectId) -> Vec<String> {
    let page = document.get_object(page_id).unwrap().as_dict().unwrap();
    let resources = resolve_dict(document, page.get(b"Resources").unwrap());
    let xobjects = resolve_dict(document, resources.get(b"XObject").unwrap());
    xobjects
        .iter()
        .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
        .collect()
}

fn effective_media_box(document: &Document, page_id: ObjectId) -> Vec<Object> {
    let mut current = page_id;
    for _ in 0..16 {
        let dict = document.get_object(current).unwrap().as_dict().unwrap();
        if let Ok(media_box) = dict.get(b"MediaBox") {
            return media_box
                .as_array()
                .expect("MediaBox is not an array")
                .clone();
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => current = *parent,
            _ => break,
        }
    }
    panic!("no MediaBox found for page {page_id:?} or its ancestors");
}

fn stream_filters(document: &Document) -> BTreeSet<Vec<u8>> {
    let mut filters = BTreeSet::new();
    for object in document.objects.values() {
        if let Object::Stream(stream) = object {
            match stream.dict.get(b"Filter") {
                Ok(Object::Name(name)) => {
                    filters.insert(name.clone());
                }
                Ok(Object::Array(items)) => {
                    for item in items {
                        if let Object::Name(name) = item {
                            filters.insert(name.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    filters
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn pje_like_fixture_is_valid_and_counts_pages_across_deep_tree() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("pje-like.pdf");
    build_pje_like(&input, PJE_PAGES);

    assert_eq!(page_count(&input).unwrap(), PJE_PAGES);
    assert_eq!(Document::load(&input).unwrap().get_pages().len(), PJE_PAGES);
    QpdfValidator::detect().validate(&input, PJE_PAGES);
}

#[test]
fn trf4_like_fixture_is_valid_and_counts_pages_in_flat_tree() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like.pdf");
    build_trf4_like(&input, TRF4_PAGES, false);

    assert_eq!(page_count(&input).unwrap(), TRF4_PAGES);
    assert_eq!(
        Document::load(&input).unwrap().get_pages().len(),
        TRF4_PAGES
    );
    QpdfValidator::detect().validate(&input, TRF4_PAGES);
}

#[test]
fn pje_like_split_pages_keeps_drawn_filter_zoo_and_drops_annots() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("pje-like.pdf");
    build_pje_like(&input, PJE_PAGES);
    let out_dir = temp.path().join("out");
    fs::create_dir(&out_dir).unwrap();

    split_pages(&input, out_dir.join("page-%d.pdf").to_str().unwrap()).unwrap();

    let outputs: Vec<_> = fs::read_dir(&out_dir).unwrap().collect();
    assert_eq!(outputs.len(), PJE_PAGES);

    // page 36 (index 35) draws the JBIG2 scan: the stream and its globals
    // must survive the split untouched
    let jbig2_page = out_dir.join(split_page_name(36, PJE_PAGES));
    let document = Document::load(&jbig2_page).unwrap();
    let mut found_jbig2 = false;
    for object in document.objects.values() {
        if let Object::Stream(stream) = object {
            if stream.dict.get(b"Filter").and_then(Object::as_name).ok() != Some(b"JBIG2Decode") {
                continue;
            }
            found_jbig2 = true;
            let parms = resolve_dict(&document, stream.dict.get(b"DecodeParms").unwrap());
            let globals = parms.get(b"JBIG2Globals").unwrap();
            let Object::Reference(globals_id) = globals else {
                panic!("JBIG2Globals should stay an indirect reference");
            };
            assert!(
                matches!(document.get_object(*globals_id), Ok(Object::Stream(_))),
                "JBIG2Globals must resolve to a stream in the split output"
            );
        }
    }
    assert!(
        found_jbig2,
        "JBIG2 image should survive in {}",
        jbig2_page.display()
    );

    // page 18 (index 17) had a link annotation; pdq drops annotations on
    // split by default (CopyOptions::copy_annotations = false)
    let annotated_page = out_dir.join(split_page_name(18, PJE_PAGES));
    let document = Document::load(&annotated_page).unwrap();
    let page = document
        .get_object(single_page_id(&document))
        .unwrap()
        .as_dict()
        .unwrap();
    assert!(
        page.get(b"Annots").is_err(),
        "split outputs should not carry annotations by default"
    );

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&jbig2_page, 1);
    qpdf.validate(&annotated_page, 1);
    qpdf.validate(&out_dir.join(split_page_name(PJE_PAGES, PJE_PAGES)), 1);
}

#[test]
fn trf4_like_split_pages_prunes_the_shared_resource_dictionary() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like.pdf");
    build_trf4_like(&input, TRF4_PAGES, false);
    let out_dir = temp.path().join("out");
    fs::create_dir(&out_dir).unwrap();

    split_pages(&input, out_dir.join("page-%d.pdf").to_str().unwrap()).unwrap();

    let input_len = fs::metadata(&input).unwrap().len();
    let qpdf = QpdfValidator::detect();
    for page in [1, 30, TRF4_PAGES] {
        let path = out_dir.join(split_page_name(page, TRF4_PAGES));
        let document = Document::load(&path).unwrap();
        let names = page_xobject_names(&document, single_page_id(&document));
        assert_eq!(
            names,
            vec![format!("TPL{}", page - 1)],
            "page {page} must keep only its own template after pruning"
        );
        qpdf.validate(&path, 1);
    }

    // regression guard for the quadratic failure mode: without pruning each
    // output embeds the whole shared dictionary and its closure
    let mut largest = 0;
    for entry in fs::read_dir(&out_dir).unwrap() {
        largest = largest.max(fs::metadata(entry.unwrap().path()).unwrap().len());
    }
    assert!(
        largest < input_len / 3,
        "single-page output of {largest} bytes suggests the shared resources \
         dictionary was copied unpruned (input is {input_len} bytes)"
    );
}

#[test]
fn trf4_like_split_output_resolves_inherited_media_box() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like.pdf");
    build_trf4_like(&input, TRF4_PAGES, false);
    let output = temp.path().join("page-5.pdf");

    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("5").unwrap(),
            path: output.clone(),
        }],
    )
    .unwrap();

    let document = Document::load(&output).unwrap();
    let media_box = effective_media_box(&document, single_page_id(&document));
    assert_eq!(
        media_box.len(),
        4,
        "split page must still resolve the MediaBox its source inherited"
    );
}

#[test]
fn pje_like_range_split_partitions_the_document() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("pje-like.pdf");
    build_pje_like(&input, PJE_PAGES);
    let head = temp.path().join("head.pdf");
    let tail = temp.path().join("tail.pdf");

    split(
        &input,
        &[
            SplitOutput {
                range: PageRangeGroup::parse("1-3").unwrap(),
                path: head.clone(),
            },
            SplitOutput {
                range: PageRangeGroup::parse("4-z").unwrap(),
                path: tail.clone(),
            },
        ],
    )
    .unwrap();

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&head, 3);
    qpdf.validate(&tail, PJE_PAGES - 3);
    assert_eq!(Document::load(&head).unwrap().get_pages().len(), 3);
    assert_eq!(
        Document::load(&tail).unwrap().get_pages().len(),
        PJE_PAGES - 3
    );
}

#[test]
fn pje_like_full_rewrite_preserves_the_filter_zoo() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("pje-like.pdf");
    build_pje_like(&input, PJE_PAGES);
    let output = temp.path().join("rewritten.pdf");

    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("1-z").unwrap(),
            path: output.clone(),
        }],
    )
    .unwrap();

    let document = Document::load(&output).unwrap();
    assert_eq!(document.get_pages().len(), PJE_PAGES);
    let filters = stream_filters(&document);
    for filter in [
        b"JBIG2Decode".as_slice(),
        b"CCITTFaxDecode",
        b"JPXDecode",
        b"DCTDecode",
        b"FlateDecode",
    ] {
        assert!(
            filters.contains(filter),
            "full rewrite lost streams with filter {}",
            String::from_utf8_lossy(filter)
        );
    }
    QpdfValidator::detect().validate(&output, PJE_PAGES);
}

#[test]
fn full_rewrite_replaces_an_existing_output_file() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like.pdf");
    build_trf4_like(&input, TRF4_PAGES, false);
    let output = temp.path().join("rewritten.pdf");
    std::fs::write(&output, b"stale bytes from a previous run").unwrap();

    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("1-z").unwrap(),
            path: output.clone(),
        }],
    )
    .unwrap();

    assert_eq!(
        Document::load(&output).unwrap().get_pages().len(),
        TRF4_PAGES
    );
    QpdfValidator::detect().validate(&output, TRF4_PAGES);
}

#[test]
fn merge_concatenates_both_document_families() {
    let temp = tempdir().unwrap();
    let pje = temp.path().join("pje-like.pdf");
    let trf4 = temp.path().join("trf4-like.pdf");
    build_pje_like(&pje, PJE_PAGES);
    build_trf4_like(&trf4, TRF4_PAGES, false);
    let merged = temp.path().join("merged.pdf");

    merge(&[MergeInput::all(&pje), MergeInput::all(&trf4)], &merged).unwrap();

    assert_eq!(
        Document::load(&merged).unwrap().get_pages().len(),
        PJE_PAGES + TRF4_PAGES
    );
    QpdfValidator::detect().validate(&merged, PJE_PAGES + TRF4_PAGES);
}

#[test]
fn trf4_like_split_then_merge_roundtrip_preserves_page_count() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like.pdf");
    build_trf4_like(&input, TRF4_PAGES, false);
    let out_dir = temp.path().join("out");
    fs::create_dir(&out_dir).unwrap();

    split_pages(&input, out_dir.join("page-%d.pdf").to_str().unwrap()).unwrap();

    let inputs: Vec<MergeInput> = (1..=TRF4_PAGES)
        .map(|page| MergeInput::all(out_dir.join(split_page_name(page, TRF4_PAGES))))
        .collect();
    let merged = temp.path().join("roundtrip.pdf");
    merge(&inputs, &merged).unwrap();

    assert_eq!(
        Document::load(&merged).unwrap().get_pages().len(),
        TRF4_PAGES
    );
    QpdfValidator::detect().validate(&merged, TRF4_PAGES);
}

#[test]
fn page_count_rejects_direct_page_tree_kids() {
    // qpdf-qtest's 0213-direct-pages.pdf shape: /Kids holding inline page
    // dictionaries instead of references. pdq used to silently report 0
    // pages for these; it must refuse loudly instead.
    let temp = tempdir().unwrap();
    let input = temp.path().join("direct-pages.pdf");
    let mut pdf = RawPdf::new(b"%PDF-1.3\n%\xe2\xe3\xcf\xd3\n");
    let content = pdf.add_stream(b"/Filter/FlateDecode", &deflate(b"BT ET"));
    let pages = pdf.reserve();
    let kid =
        format!("<</Type/Page/Parent {pages} 0 R/MediaBox[0 0 612 792]/Contents {content} 0 R>>");
    pdf.put(
        pages,
        format!("<</Type/Pages/Count 2/Kids[{kid}{kid}]>>").into_bytes(),
    );
    let catalog = pdf.add(format!("<</Type/Catalog/Pages {pages} 0 R>>").into_bytes());
    pdf.write_to(&input, format!("/Root {catalog} 0 R").as_bytes());

    let err = page_count(&input).unwrap_err();
    assert!(
        err.to_string().contains("direct"),
        "expected a direct-kid structure error, got: {err}"
    );
}

#[test]
fn trf4_like_with_content_comments_still_splits_into_valid_pages() {
    // % comments in content streams defeat strict content parsing, which
    // disables resource pruning; outputs get bigger but must stay correct
    let temp = tempdir().unwrap();
    let input = temp.path().join("trf4-like-comments.pdf");
    build_trf4_like(&input, TRF4_PAGES, true);
    let out_dir = temp.path().join("out");
    fs::create_dir(&out_dir).unwrap();

    split_pages(&input, out_dir.join("page-%d.pdf").to_str().unwrap()).unwrap();

    let outputs: Vec<_> = fs::read_dir(&out_dir).unwrap().collect();
    assert_eq!(outputs.len(), TRF4_PAGES);

    let qpdf = QpdfValidator::detect();
    for page in [1, TRF4_PAGES] {
        let path = out_dir.join(split_page_name(page, TRF4_PAGES));
        assert_eq!(Document::load(&path).unwrap().get_pages().len(), 1);
        qpdf.validate(&path, 1);
    }
}
