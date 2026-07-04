use std::path::{Path, PathBuf};

use assert_cmd::Command;
use lopdf::Document;
use predicates::prelude::*;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn pdq() -> Command {
    Command::cargo_bin("pdq").unwrap()
}

fn assert_page_count(path: &Path, expected: usize) {
    let document = Document::load(path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err}", path.display()));
    assert_eq!(
        document.get_pages().len(),
        expected,
        "unexpected page count for {}",
        path.display()
    );
}

fn assert_not_encrypted(path: &Path) {
    let document = Document::load(path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err}", path.display()));
    assert!(
        !document.is_encrypted() && !document.was_encrypted(),
        "{} should be written unencrypted",
        path.display()
    );
}

#[test]
fn split_cli_writes_each_requested_range() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first.pdf");
    let rest = temp.path().join("rest.pdf");

    pdq()
        .arg("split")
        .arg(fixture("11-pages.pdf"))
        .arg("--out")
        .arg("1-3")
        .arg(&first)
        .arg("--out")
        .arg("4-z")
        .arg(&rest)
        .assert()
        .success();

    assert_page_count(&first, 3);
    assert_page_count(&rest, 8);
}

#[test]
fn split_cli_rejects_invalid_range_syntax() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("out.pdf");

    pdq()
        .arg("split")
        .arg(fixture("11-pages.pdf"))
        .arg("--out")
        .arg("abc")
        .arg(&output)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid page number"));

    assert!(!output.exists());
}

#[test]
fn split_cli_rejects_out_of_bounds_range_without_writing() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("out.pdf");

    pdq()
        .arg("split")
        .arg(fixture("11-pages.pdf"))
        .arg("--out")
        .arg("99")
        .arg(&output)
        .assert()
        .failure()
        .stderr(predicate::str::contains("out of bounds"));

    assert!(!output.exists());
}

#[test]
fn split_cli_fails_on_missing_input() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("out.pdf");

    pdq()
        .arg("split")
        .arg(temp.path().join("does-not-exist.pdf"))
        .arg("--out")
        .arg("1")
        .arg(&output)
        .assert()
        .failure();

    assert!(!output.exists());
}

#[test]
fn split_cli_requires_password_for_user_password_input() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("out.pdf");

    pdq()
        .arg("split")
        .arg(fixture("user-password.pdf"))
        .arg("--out")
        .arg("1")
        .arg(&output)
        .assert()
        .failure()
        .stderr(predicate::str::contains("--password"));

    assert!(!output.exists());
}

#[test]
fn split_cli_rejects_wrong_password() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("out.pdf");

    pdq()
        .arg("split")
        .arg(fixture("user-password.pdf"))
        .arg("--password")
        .arg("wrong")
        .arg("--out")
        .arg("1")
        .arg(&output)
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid password"));

    assert!(!output.exists());
}

#[test]
fn split_cli_decrypts_user_password_input_with_password() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("decrypted.pdf");

    pdq()
        .arg("split")
        .arg(fixture("user-password.pdf"))
        .arg("--password")
        .arg("user")
        .arg("--out")
        .arg("1-3")
        .arg(&output)
        .assert()
        .success();

    assert_page_count(&output, 3);
    assert_not_encrypted(&output);
}

#[test]
fn split_pages_cli_decrypts_owner_only_input_without_password() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("owner-only.pdf"))
        .arg("--output")
        .arg(temp.path().join("page-%d.pdf"))
        .assert()
        .success();

    for page in 1..=11 {
        let path = temp.path().join(format!("page-{page:02}.pdf"));
        assert_page_count(&path, 1);
        assert_not_encrypted(&path);
    }
}

#[test]
fn merge_cli_decrypts_single_encrypted_input() {
    let temp = tempdir().unwrap();
    let merged = temp.path().join("merged.pdf");

    pdq()
        .arg("merge")
        .arg("--output")
        .arg(&merged)
        .arg(fixture("owner-only.pdf"))
        .assert()
        .success();

    assert_page_count(&merged, 11);
    assert_not_encrypted(&merged);
}

#[test]
fn page_count_cli_reads_owner_only_encrypted_input_without_password() {
    pdq()
        .arg("page-count")
        .arg(fixture("owner-only.pdf"))
        .assert()
        .success()
        .stdout(predicate::str::diff("11\n"));
}

#[test]
fn page_count_cli_reads_user_password_input_with_password() {
    pdq()
        .arg("page-count")
        .arg(fixture("user-password.pdf"))
        .arg("--password")
        .arg("user")
        .assert()
        .success()
        .stdout(predicate::str::diff("11\n"));
}

#[test]
fn split_cli_requires_range_and_path_pair_for_out() {
    pdq()
        .arg("split")
        .arg(fixture("11-pages.pdf"))
        .arg("--out")
        .arg("1-3")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--out"));
}

#[test]
fn merge_cli_concatenates_inputs_in_order() {
    let temp = tempdir().unwrap();
    let merged = temp.path().join("merged.pdf");

    pdq()
        .arg("merge")
        .arg("--output")
        .arg(&merged)
        .arg(fixture("11-pages.pdf"))
        .arg(fixture("11-pages-objstm.pdf"))
        .assert()
        .success();

    assert_page_count(&merged, 22);
}

#[test]
fn merge_cli_fails_on_missing_input_without_writing() {
    let temp = tempdir().unwrap();
    let merged = temp.path().join("merged.pdf");

    pdq()
        .arg("merge")
        .arg("--output")
        .arg(&merged)
        .arg(fixture("11-pages.pdf"))
        .arg(temp.path().join("does-not-exist.pdf"))
        .assert()
        .failure();

    assert!(!merged.exists());
}

#[test]
fn merge_cli_requires_at_least_one_input() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("merge")
        .arg("--output")
        .arg(temp.path().join("merged.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn split_pages_cli_rejects_pattern_without_placeholder() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(temp.path().join("page.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("%d"));
}

#[test]
fn split_pages_cli_rejects_pattern_with_multiple_placeholders() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(temp.path().join("page-%d-%d.pdf"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("%d"));
}

#[test]
fn split_pages_cli_chunks_pages_per_file() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(temp.path().join("chunk-%d.pdf"))
        .arg("--pages-per-file")
        .arg("4")
        .assert()
        .success();

    assert_page_count(&temp.path().join("chunk-1.pdf"), 4);
    assert_page_count(&temp.path().join("chunk-2.pdf"), 4);
    assert_page_count(&temp.path().join("chunk-3.pdf"), 3);
    assert!(!temp.path().join("chunk-4.pdf").exists());
}

#[test]
fn split_pages_cli_chunks_and_decrypts_with_password() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("user-password.pdf"))
        .arg("--output")
        .arg(temp.path().join("chunk-%d.pdf"))
        .arg("--pages-per-file")
        .arg("3")
        .arg("--password")
        .arg("user")
        .assert()
        .success();

    for (chunk, expected_pages) in [(1, 3), (2, 3), (3, 3), (4, 2)] {
        let path = temp.path().join(format!("chunk-{chunk}.pdf"));
        assert_page_count(&path, expected_pages);
        assert_not_encrypted(&path);
    }
    assert!(!temp.path().join("chunk-5.pdf").exists());
}

#[test]
fn split_pages_cli_rejects_zero_pages_per_file() {
    let temp = tempdir().unwrap();

    pdq()
        .arg("split-pages")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(temp.path().join("chunk-%d.pdf"))
        .arg("--pages-per-file")
        .arg("0")
        .assert()
        .failure()
        .stderr(predicate::str::contains("pages-per-file"));

    assert!(!temp.path().join("chunk-1.pdf").exists());
}

#[test]
fn cli_without_subcommand_prints_usage() {
    pdq()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn cli_help_lists_subcommands() {
    pdq().arg("--help").assert().success().stdout(
        predicate::str::contains("split")
            .and(predicate::str::contains("split-pages"))
            .and(predicate::str::contains("merge")),
    );
}
