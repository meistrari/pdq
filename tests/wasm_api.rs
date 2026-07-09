use std::fs;
use std::path::{Path, PathBuf};

use pdq::{
    page_count, page_count_fast, page_count_fast_from_bytes,
    page_count_fast_from_bytes_with_password, page_count_fast_with_password, page_count_from_bytes,
    page_count_from_bytes_with_password, page_count_with_password,
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

    let temp = tempfile::tempdir().unwrap();
    let pattern = temp.path().join("page-%d.png");
    render_pages(&path, pattern.to_str().unwrap(), &options).unwrap();

    for page in &from_bytes {
        let written = fs::read(temp.path().join(format!("page-{:02}.png", page.page))).unwrap();
        assert_eq!(&page.png, &written);
        assert!(page.width > 0 && page.height > 0);
    }
}
