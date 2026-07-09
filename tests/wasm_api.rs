use std::fs;
use std::path::{Path, PathBuf};

use pdq::{
    merge_from_bytes, merge_from_bytes_with_options, page_count, page_count_fast,
    page_count_fast_from_bytes, page_count_fast_from_bytes_with_password,
    page_count_fast_with_password, page_count_from_bytes, page_count_from_bytes_with_password,
    page_count_with_password, split_from_bytes, split_pages_from_bytes, MergeBytesInput,
    PageRangeError, PageRangeGroup, PdfOpsError, SplitBytesOutput, SplitPagesOptions,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn page_count_from_bytes_matches_path_api() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    assert_eq!(
        page_count_from_bytes(&bytes).unwrap(),
        page_count(&path).unwrap()
    );
}

#[test]
fn page_count_fast_from_bytes_matches_path_api() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    assert_eq!(
        page_count_fast_from_bytes(&bytes).unwrap(),
        page_count_fast(&path).unwrap()
    );
}

#[test]
fn page_count_from_bytes_with_password_matches_path_api() {
    let path = fixture("user-password.pdf");
    let bytes = fs::read(&path).unwrap();

    assert_eq!(
        page_count_from_bytes_with_password(&bytes, Some("user")).unwrap(),
        page_count_with_password(&path, Some("user")).unwrap()
    );
}

#[test]
fn page_count_fast_from_bytes_with_password_matches_path_api() {
    let path = fixture("user-password.pdf");
    let bytes = fs::read(&path).unwrap();

    assert_eq!(
        page_count_fast_from_bytes_with_password(&bytes, Some("user")).unwrap(),
        page_count_fast_with_password(&path, Some("user")).unwrap()
    );
}

#[test]
fn split_from_bytes_returns_requested_range() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let outputs = split_from_bytes(
        &bytes,
        &[SplitBytesOutput {
            range: PageRangeGroup::parse("1-3".to_string()).unwrap(),
        }],
    )
    .unwrap();

    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].index, 0);
    assert_eq!(page_count_from_bytes(&outputs[0].pdf).unwrap(), 3);
}

#[test]
fn split_pages_from_bytes_one_per_page() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let outputs = split_pages_from_bytes(
        &bytes,
        &SplitPagesOptions {
            pages_per_file: 1,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(outputs.len(), 11);
    for (index, output) in outputs.iter().enumerate() {
        assert_eq!(output.index, index);
        assert_eq!(page_count_from_bytes(&output.pdf).unwrap(), 1);
    }
}

#[test]
fn split_pages_from_bytes_chunks_of_five() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let outputs = split_pages_from_bytes(
        &bytes,
        &SplitPagesOptions {
            pages_per_file: 5,
            ..Default::default()
        },
    )
    .unwrap();

    let page_counts: Vec<usize> = outputs
        .iter()
        .map(|output| page_count_from_bytes(&output.pdf).unwrap())
        .collect();
    assert_eq!(page_counts, vec![5, 5, 1]);
}

#[cfg(feature = "text")]
#[test]
fn extract_text_from_bytes_matches_path_api() {
    use pdq::{extract_text, extract_text_from_bytes, ExtractTextOptions};

    let path = fixture("text-simple.pdf");
    let bytes = fs::read(&path).unwrap();

    let options = ExtractTextOptions::default();
    assert_eq!(
        extract_text_from_bytes(&bytes, &options).unwrap(),
        extract_text(&path, &options).unwrap()
    );
}

#[cfg(feature = "text")]
#[test]
fn extract_text_from_bytes_matches_path_api_with_page_selection() {
    use pdq::{extract_text, extract_text_from_bytes, ExtractTextOptions, PageRangeGroup};

    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let options = ExtractTextOptions {
        pages: Some(PageRangeGroup::parse("3,5-7".to_string()).unwrap()),
        ..Default::default()
    };
    assert_eq!(
        extract_text_from_bytes(&bytes, &options).unwrap(),
        extract_text(&path, &options).unwrap()
    );
}

#[cfg(feature = "render")]
#[test]
fn render_pages_from_bytes_matches_path_api() {
    use pdq::{render_pages, render_pages_from_bytes, PageRangeGroup, RenderOptions};

    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let options = RenderOptions {
        dpi: 72.0,
        pages: Some(PageRangeGroup::parse("2,4").unwrap()),
    };

    let from_bytes = render_pages_from_bytes(&bytes, &options).unwrap();
    assert_eq!(from_bytes.len(), 2);
    assert_eq!(
        from_bytes.iter().map(|p| p.page).collect::<Vec<_>>(),
        [2, 4]
    );
    for page in &from_bytes {
        assert!(page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    let temp = tempfile::tempdir().unwrap();
    let pattern = temp.path().join("page-%d.png");
    render_pages(&path, pattern.to_str().unwrap(), &options).unwrap();

    for page in &from_bytes {
        let written = fs::read(temp.path().join(format!("page-{:02}.png", page.page))).unwrap();
        assert_eq!(&page.png, &written);
        assert!(page.width > 0 && page.height > 0);
    }
}

#[test]
fn merge_from_bytes_combines_whole_inputs() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let inputs = [
        MergeBytesInput {
            bytes: bytes.clone(),
            ranges: Vec::new(),
        },
        MergeBytesInput {
            bytes: bytes.clone(),
            ranges: Vec::new(),
        },
    ];

    let merged = merge_from_bytes(&inputs).unwrap();
    assert_eq!(page_count_from_bytes(&merged).unwrap(), 22);
}

#[test]
fn merge_from_bytes_applies_page_ranges() {
    let path = fixture("11-pages.pdf");
    let bytes = fs::read(&path).unwrap();

    let inputs = [
        MergeBytesInput {
            bytes: bytes.clone(),
            ranges: vec![PageRangeGroup::parse("1-2".to_string()).unwrap()],
        },
        MergeBytesInput {
            bytes: bytes.clone(),
            ranges: vec![PageRangeGroup::parse("3".to_string()).unwrap()],
        },
    ];

    let merged = merge_from_bytes(&inputs).unwrap();
    assert_eq!(page_count_from_bytes(&merged).unwrap(), 3);
}

#[test]
fn merge_from_bytes_rejects_empty_inputs() {
    let err = merge_from_bytes(&[]).unwrap_err();
    assert!(matches!(err, PdfOpsError::Range(PageRangeError::NoPages)));
}

#[test]
fn merge_from_bytes_with_options_accepts_password() {
    let path = fixture("user-password.pdf");
    let bytes = fs::read(&path).unwrap();

    let inputs = [MergeBytesInput {
        bytes,
        ranges: Vec::new(),
    }];

    let merged = merge_from_bytes_with_options(
        &inputs,
        pdq::MergeBytesOptions {
            password: Some("user".to_string()),
        },
    )
    .unwrap();
    assert_eq!(
        page_count_from_bytes(&merged).unwrap(),
        page_count_with_password(&path, Some("user")).unwrap()
    );
}
