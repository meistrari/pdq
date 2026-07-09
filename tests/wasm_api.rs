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
