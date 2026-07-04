use std::{fs, io::Write, path::Path};

use flate2::{write::ZlibEncoder, Compression};
use pdq::{merge, page_count, split, MergeInput, PageRangeGroup, SplitOutput};
use tempfile::tempdir;

fn push_object(pdf: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
    let offset = pdf.len();
    writeln!(pdf, "{id} 0 obj").unwrap();
    pdf.extend_from_slice(body);
    pdf.extend_from_slice(b"\nendobj\n");
    offset
}

fn push_xref_row(rows: &mut Vec<u8>, entry_type: u8, second: usize, third: u8) {
    rows.push(entry_type);
    rows.extend_from_slice(&u16::try_from(second).unwrap().to_be_bytes());
    rows.push(third);
}

fn encode_tiff_predictor_2(mut data: Vec<u8>, colors: usize, columns: usize) -> Vec<u8> {
    for row in data.chunks_exact_mut(columns) {
        for index in (colors..columns).rev() {
            row[index] = row[index].wrapping_sub(row[index - colors]);
        }
    }
    data
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn one_page_tiff_predictor_xref_pdf() -> Vec<u8> {
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.5\n");

    let object_1 = push_object(&mut pdf, 1, b"<< /Type /Catalog /Pages 2 0 R >>");
    let object_2 = push_object(&mut pdf, 2, b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>");
    let object_3 = push_object(
        &mut pdf,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents 4 0 R >>",
    );
    let object_4 = push_object(&mut pdf, 4, b"<< /Length 0 >>\nstream\n\nendstream");

    let xref_offset = pdf.len();
    let mut rows = Vec::new();
    push_xref_row(&mut rows, 0, 0, 0);
    push_xref_row(&mut rows, 1, object_1, 0);
    push_xref_row(&mut rows, 1, object_2, 0);
    push_xref_row(&mut rows, 1, object_3, 0);
    push_xref_row(&mut rows, 1, object_4, 0);
    push_xref_row(&mut rows, 1, xref_offset, 0);

    let columns = 4;
    let encoded = encode_tiff_predictor_2(rows, 1, columns);
    let compressed = zlib(&encoded);

    pdf.extend_from_slice(b"5 0 obj\n");
    write!(
        pdf,
        "<< /Type /XRef /Size 6 /W [1 2 1] /Index [0 6] /Root 1 0 R /Filter /FlateDecode /DecodeParms << /Predictor 2 /Colors 1 /BitsPerComponent 8 /Columns {columns} >> /Length {} >>\nstream\n",
        compressed.len()
    )
    .unwrap();
    pdf.extend_from_slice(&compressed);
    pdf.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
    write!(pdf, "{xref_offset}\n%%EOF\n").unwrap();
    pdf
}

fn write_fixture(path: &Path) {
    fs::write(path, one_page_tiff_predictor_xref_pdf()).unwrap();
}

#[test]
fn predictor_2_xref_stream_page_count_split_and_merge() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("predictor-2-xref.pdf");
    write_fixture(&input);

    assert_eq!(page_count(&input).unwrap(), 1);

    let split_output = temp.path().join("split.pdf");
    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("1").unwrap(),
            path: split_output.clone(),
        }],
    )
    .unwrap();
    assert_eq!(page_count(&split_output).unwrap(), 1);

    let merged = temp.path().join("merged.pdf");
    merge(
        &[MergeInput::all(&input), MergeInput::all(&split_output)],
        &merged,
    )
    .unwrap();
    assert_eq!(page_count(&merged).unwrap(), 2);
}
