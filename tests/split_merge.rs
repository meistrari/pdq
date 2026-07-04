use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use lopdf::{dictionary, Dictionary, Document, Object, Stream};
use pdq::{
    merge, merge_with_options, page_count, page_count_fast, page_count_with_password, split,
    split_pages, split_pages_with_options, split_pages_with_password, split_with_password,
    MergeInput, MergeOptions, PageRangeGroup, PdfOpsError, SplitOutput, SplitPagesOptions,
};
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn assert_written(path: &Path) {
    let metadata = fs::metadata(path)
        .unwrap_or_else(|err| panic!("expected {} to be written: {err}", path.display()));
    assert!(metadata.is_file(), "{} is not a file", path.display());
    assert!(metadata.len() > 0, "{} should not be empty", path.display());
}

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

    fn assert_npages(&self, path: &Path, expected: usize) {
        if !self.available {
            eprintln!(
                "qpdf unavailable; skipping qpdf --show-npages for {}",
                path.display()
            );
            return;
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
            "qpdf --show-npages failed for {}\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let actual = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or_else(|err| {
                panic!(
                    "qpdf --show-npages returned invalid output for {}: {err}",
                    path.display()
                )
            });
        assert_eq!(
            actual,
            expected,
            "unexpected page count for {}",
            path.display()
        );
    }

    fn validate(&self, path: &Path, expected_pages: usize) {
        self.check_pdf(path);
        self.assert_npages(path, expected_pages);
    }

    /// Encrypt `source` into `dest` by shelling out to qpdf. Returns `false`
    /// when qpdf is unavailable so callers can skip the test.
    fn encrypt(
        &self,
        source: &Path,
        dest: &Path,
        user_password: &str,
        owner_password: &str,
        options: &[&str],
    ) -> bool {
        if !self.available {
            eprintln!(
                "qpdf unavailable; skipping encrypted fixture {}",
                dest.display()
            );
            return false;
        }

        let output = Command::new("qpdf")
            .arg("--allow-weak-crypto")
            .arg("--encrypt")
            .arg(user_password)
            .arg(owner_password)
            .args(options)
            .arg("--")
            .arg(source)
            .arg(dest)
            .output()
            .unwrap_or_else(|err| panic!("failed to run qpdf --encrypt: {err}"));
        assert!(
            output.status.success(),
            "qpdf --encrypt failed for {}\nstdout:\n{}\nstderr:\n{}",
            dest.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn assert_not_encrypted(&self, path: &Path) {
        if !self.available {
            eprintln!(
                "qpdf unavailable; skipping qpdf --is-encrypted for {}",
                path.display()
            );
            return;
        }

        let output = Command::new("qpdf")
            .arg("--is-encrypted")
            .arg(path)
            .output()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to run qpdf --is-encrypted {}: {err}",
                    path.display()
                )
            });
        // qpdf --is-encrypted exits 0 for encrypted files and non-zero otherwise.
        assert!(
            !output.status.success(),
            "{} is unexpectedly encrypted",
            path.display()
        );
    }
}

/// Assert with lopdf that `path` is written without encryption, so it also
/// holds when qpdf is unavailable.
fn assert_decrypted(path: &Path) {
    let document = Document::load(path)
        .unwrap_or_else(|err| panic!("failed to load {}: {err}", path.display()));
    assert!(
        !document.is_encrypted() && !document.was_encrypted(),
        "{} should be written unencrypted",
        path.display()
    );
}

#[test]
fn split_writes_outputs_and_qpdf_validates_page_counts() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("pages-1-3.pdf");
    let rest = temp.path().join("pages-4-z.pdf");

    split(
        &fixture("11-pages.pdf"),
        &[
            SplitOutput {
                range: PageRangeGroup::parse("1-3").unwrap(),
                path: first.clone(),
            },
            SplitOutput {
                range: PageRangeGroup::parse("4-z").unwrap(),
                path: rest.clone(),
            },
        ],
    )
    .unwrap();

    assert_written(&first);
    assert_written(&rest);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&first, 3);
    qpdf.validate(&rest, 8);
}

#[test]
fn page_count_cli_reports_total_pages() {
    let output = Command::new(env!("CARGO_BIN_EXE_pdq"))
        .arg("page-count")
        .arg(fixture("11-pages.pdf"))
        .output()
        .unwrap_or_else(|err| panic!("failed to run pdq page-count: {err}"));
    assert!(
        output.status.success(),
        "pdq page-count failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let reported = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .expect("page-count should print an integer");
    assert_eq!(reported, 11);
}

#[test]
fn page_count_library_matches_object_stream_input() {
    // Object-stream (compressed xref) input exercises the lazy reader's
    // compressed-object path, mirroring the split-pages fixtures.
    assert_eq!(page_count(&fixture("11-pages-objstm.pdf")).unwrap(), 11);
}

#[test]
fn operations_handle_filter_abbreviations_in_xref_object_and_content_streams() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("filter-abbreviation.pdf");
    let split_output = temp.path().join("split.pdf");
    let merged = temp.path().join("merged.pdf");

    write_filter_abbreviation_pdf(&input);

    assert_eq!(page_count(&input).unwrap(), 1);
    assert_eq!(page_count_fast(&input).unwrap(), 1);

    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("1").unwrap(),
            path: split_output.clone(),
        }],
    )
    .unwrap();
    assert_written(&split_output);

    merge(
        &[MergeInput {
            path: input,
            ranges: vec![PageRangeGroup::parse("1").unwrap()],
        }],
        &merged,
    )
    .unwrap();
    assert_written(&merged);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&split_output, 1);
    qpdf.validate(&merged, 1);
    assert_page_resources(&split_output, &qpdf, &["X0"], &[]);
}

#[test]
fn page_count_fast_matches_strict_on_fixtures() {
    for name in ["11-pages.pdf", "11-pages-objstm.pdf"] {
        let path = fixture(name);
        let strict = page_count(&path)
            .unwrap_or_else(|err| panic!("strict page_count failed for {name}: {err}"));
        let fast = page_count_fast(&path)
            .unwrap_or_else(|err| panic!("page_count_fast failed for {name}: {err}"));
        assert_eq!(fast, strict, "fast and strict counts diverge for {name}");
        assert_eq!(fast, 11, "unexpected page count for {name}");
    }
}

#[test]
fn page_count_fast_falls_back_when_count_is_missing_or_malformed() {
    let temp = tempdir().unwrap();
    let cases: [(&str, Option<Object>); 4] = [
        ("missing", None),
        ("wrong-type", Some(Object::Name(b"three".to_vec()))),
        ("negative", Some(Object::Integer(-3))),
        // Far larger than the xref size (the fixture has ~6 objects): a page
        // needs at least one object, so this /Count is provably a lie.
        ("implausible", Some(Object::Integer(1_000_000))),
    ];
    for (label, count) in cases {
        let input = temp.path().join(format!("count-{label}.pdf"));
        write_three_page_pdf_with_count(&input, count);
        assert_eq!(
            page_count_fast(&input).unwrap(),
            3,
            "fast path must fall back to the walk for {label} /Count"
        );
        assert_eq!(
            page_count(&input).unwrap(),
            3,
            "strict walk must count real pages for {label} /Count"
        );
    }
}

#[test]
fn page_count_fast_trusts_plausible_count_by_design() {
    // Documented market semantics (same as `qpdf --show-npages`): a lying but
    // plausible /Count is trusted by the fast path and only caught by --strict.
    let temp = tempdir().unwrap();
    let input = temp.path().join("count-lying-plausible.pdf");
    write_three_page_pdf_with_count(&input, Some(Object::Integer(2)));

    assert_eq!(page_count_fast(&input).unwrap(), 2);
    assert_eq!(page_count(&input).unwrap(), 3);
}

#[test]
fn page_count_infers_missing_or_bogus_page_tree_types() {
    let temp = tempdir().unwrap();
    let cases = [
        ("missing-interior", None, Some(b"Page".as_ref())),
        (
            "bogus-interior",
            Some(b"Pagez".as_ref()),
            Some(b"Page".as_ref()),
        ),
        ("missing-leaf", Some(b"Pages".as_ref()), None),
        (
            "bogus-leaf",
            Some(b"Pages".as_ref()),
            Some(b"Pagez".as_ref()),
        ),
        ("missing-both", None, None),
        (
            "bogus-both",
            Some(b"Pagez".as_ref()),
            Some(b"Pagez".as_ref()),
        ),
    ];

    for (label, pages_type, page_type) in cases {
        let input = temp.path().join(format!("{label}.pdf"));
        write_page_tree_type_inference_pdf(&input, pages_type, page_type);
        assert_eq!(
            page_count(&input).unwrap(),
            1,
            "strict walk should infer page-tree node kind for {label}"
        );
    }
}

#[test]
fn split_normalizes_inferred_page_tree_leaf_types() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("missing-leaf-type.pdf");
    let split_output = temp.path().join("split.pdf");
    let split_pages_pattern = temp.path().join("page-%d.pdf");

    write_page_tree_type_inference_pdf(&input, Some(b"Pages"), None);

    split(
        &input,
        &[SplitOutput {
            range: PageRangeGroup::parse("1").unwrap(),
            path: split_output.clone(),
        }],
    )
    .unwrap();
    split_pages(&input, split_pages_pattern.to_str().unwrap()).unwrap();

    let split_page_output = temp.path().join("page-1.pdf");
    assert_written(&split_output);
    assert_written(&split_page_output);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&split_output, 1);
    qpdf.validate(&split_page_output, 1);

    for output in [&split_output, &split_page_output] {
        let document = Document::load(output)
            .unwrap_or_else(|err| panic!("failed to load {}: {err}", output.display()));
        let page_id = document.get_pages()[&1];
        let page = document
            .get_object(page_id)
            .unwrap()
            .as_dict()
            .unwrap_or_else(|_| {
                panic!("page object should be a dictionary in {}", output.display())
            });
        assert!(
            page.has_type(b"Page"),
            "{} should write /Type /Page",
            output.display()
        );
    }
}

#[test]
#[ignore = "external qpdf fixture is not checked into the repository"]
fn page_count_reads_no_pages_types_fixture_when_available() {
    let input = fixture("no-pages-types.pdf");
    if !input.exists() {
        eprintln!("no-pages-types.pdf fixture unavailable; skipping qpdf fixture regression");
        return;
    }

    assert_eq!(page_count(&input).unwrap(), 1);
}

#[test]
fn page_count_reports_clear_error_for_unwalkable_inferred_interior() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("bad-kids-type.pdf");
    write_page_tree_with_bad_kids(&input, Object::Name(b"not-an-array".to_vec()));

    let err = page_count(&input).unwrap_err().to_string();
    assert!(
        err.contains("2 0 R") && err.contains("/Kids") && err.contains("expected array"),
        "expected a contextual /Kids error, got: {err}"
    );
    assert!(
        !err.contains("Linearized"),
        "page tree errors should not leak lopdf's get_type fallback: {err}"
    );
}

#[test]
fn page_count_rejects_non_reference_page_tree_kids() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("null-page-tree-kid.pdf");
    write_page_tree_with_bad_kids(&input, Object::Array(vec![Object::Null]));

    let err = page_count(&input).unwrap_err().to_string();
    assert!(
        err.contains("2 0 R") && err.contains("/Kids[0]") && err.contains("indirect references"),
        "expected a contextual non-reference kid error, got: {err}"
    );
}

#[test]
fn page_count_encrypted_inputs_behave_the_same_in_both_modes() {
    // A real user password is required: without it both modes fail with the
    // password error, never a wrong count.
    let needs_password = fixture("user-password.pdf");
    assert!(
        matches!(
            page_count(&needs_password).unwrap_err(),
            PdfOpsError::Password(_)
        ),
        "strict page_count must ask for a password"
    );
    assert!(
        matches!(
            page_count_fast(&needs_password).unwrap_err(),
            PdfOpsError::Password(_)
        ),
        "page_count_fast must ask for a password"
    );

    // Owner-password-only files decrypt with the empty user password and must
    // count identically in fast and strict modes.
    let owner_only = fixture("owner-only.pdf");
    assert_eq!(page_count(&owner_only).unwrap(), 11);
    assert_eq!(page_count_fast(&owner_only).unwrap(), 11);
}

#[test]
fn merge_cli_writes_output_and_qpdf_validates_page_count() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("pages-1-3.pdf");
    let rest = temp.path().join("pages-4-z.pdf");
    let merged = temp.path().join("merged.pdf");

    split(
        &fixture("11-pages.pdf"),
        &[
            SplitOutput {
                range: PageRangeGroup::parse("1-3").unwrap(),
                path: first.clone(),
            },
            SplitOutput {
                range: PageRangeGroup::parse("4-z").unwrap(),
                path: rest.clone(),
            },
        ],
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pdq"))
        .arg("merge")
        .arg("--output")
        .arg(&merged)
        .arg(&first)
        .arg(&rest)
        .output()
        .unwrap_or_else(|err| panic!("failed to run pdq merge: {err}"));
    assert!(
        output.status.success(),
        "pdq merge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_written(&merged);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&merged, 11);
}

#[test]
fn merge_library_writes_selected_ranges_and_qpdf_validates_page_count() {
    let temp = tempdir().unwrap();
    let merged = temp.path().join("selected-ranges.pdf");

    merge(
        &[MergeInput {
            path: fixture("11-pages.pdf"),
            ranges: vec![
                PageRangeGroup::parse("1-2").unwrap(),
                PageRangeGroup::parse("10-z").unwrap(),
            ],
        }],
        &merged,
    )
    .unwrap();

    assert_written(&merged);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&merged, 4);
}

#[test]
fn merge_library_rejects_empty_inputs() {
    let temp = tempdir().unwrap();
    let merged = temp.path().join("merged.pdf");

    let error = merge(&[], &merged).unwrap_err();

    assert!(matches!(error, PdfOpsError::Range(_)));
    assert!(!merged.exists());
}

#[test]
fn merge_with_preserve_whole_single_input_copies_bytes() {
    let temp = tempdir().unwrap();
    let input = fixture("11-pages.pdf");
    let merged = temp.path().join("merged.pdf");

    merge_with_options(
        &[MergeInput::all(&input)],
        &merged,
        MergeOptions {
            preserve_whole_single_input: true,
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_eq!(fs::read(&merged).unwrap(), fs::read(&input).unwrap());
}

#[test]
fn merge_with_preserve_whole_single_input_does_not_truncate_same_file() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("in-place.pdf");
    fs::copy(fixture("11-pages.pdf"), &input).unwrap();

    merge_with_options(
        &[MergeInput::all(&input)],
        &input,
        MergeOptions {
            preserve_whole_single_input: true,
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_written(&input);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&input, 11);
}

#[test]
fn split_duplicate_page_range_writes_distinct_valid_pages() {
    let temp = tempdir().unwrap();
    let repeated = temp.path().join("repeated.pdf");

    split(
        &fixture("11-pages.pdf"),
        &[SplitOutput {
            range: PageRangeGroup::parse("1,1,2").unwrap(),
            path: repeated.clone(),
        }],
    )
    .unwrap();

    assert_written(&repeated);

    let qpdf = QpdfValidator::detect();
    qpdf.validate(&repeated, 3);
}

#[test]
fn split_pages_writes_one_output_per_page() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("page-%d.pdf");

    split_pages(&fixture("11-pages.pdf"), pattern.to_str().unwrap()).unwrap();

    let qpdf = QpdfValidator::detect();
    for page in 1..=11 {
        let path = temp.path().join(format!("page-{page:02}.pdf"));
        assert_written(&path);
        qpdf.validate(&path, 1);
    }
}

#[test]
fn split_pages_cli_writes_one_output_per_page() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("cli-page-%d.pdf");

    let output = Command::new(env!("CARGO_BIN_EXE_pdq"))
        .arg("split-pages")
        .arg(fixture("11-pages.pdf"))
        .arg("--output")
        .arg(&pattern)
        .output()
        .unwrap_or_else(|err| panic!("failed to run pdq split-pages: {err}"));
    assert!(
        output.status.success(),
        "pdq split-pages failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let qpdf = QpdfValidator::detect();
    for page in 1..=11 {
        let path = temp.path().join(format!("cli-page-{page:02}.pdf"));
        assert_written(&path);
        qpdf.validate(&path, 1);
    }
}

#[test]
fn split_pages_with_options_chunks_pages_and_qpdf_validates_page_counts() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("five-pages.pdf");
    let pattern = temp.path().join("chunk-%d.pdf");

    write_simple_pdf(&input, 5);
    split_pages_with_options(
        &input,
        pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 2,
            ..SplitPagesOptions::default()
        },
    )
    .unwrap();

    let qpdf = QpdfValidator::detect();
    for (chunk, expected_pages) in [(1, 2), (2, 2), (3, 1)] {
        let path = temp.path().join(format!("chunk-{chunk}.pdf"));
        assert_written(&path);
        qpdf.validate(&path, expected_pages);
    }
    assert!(!temp.path().join("chunk-4.pdf").exists());
}

#[test]
fn split_pages_with_options_larger_than_page_count_writes_single_output() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("five-pages.pdf");
    let pattern = temp.path().join("all-%d.pdf");

    write_simple_pdf(&input, 5);
    split_pages_with_options(
        &input,
        pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 9,
            ..SplitPagesOptions::default()
        },
    )
    .unwrap();

    let output = temp.path().join("all-1.pdf");
    assert_written(&output);
    QpdfValidator::detect().validate(&output, 5);
    assert!(!temp.path().join("all-2.pdf").exists());
}

#[test]
fn split_pages_with_options_rejects_zero_pages_per_file() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("should-not-exist-%d.pdf");

    let error = split_pages_with_options(
        &fixture("11-pages.pdf"),
        pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 0,
            ..SplitPagesOptions::default()
        },
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::InvalidStructure(_)));
    assert!(!temp.path().join("should-not-exist-1.pdf").exists());
}

#[test]
fn split_pages_with_options_pads_numbers_to_chunk_count_width() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("twelve-pages.pdf");

    write_simple_pdf(&input, 12);

    let per_page_pattern = temp.path().join("page-%d.pdf");
    split_pages_with_options(
        &input,
        per_page_pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 1,
            ..SplitPagesOptions::default()
        },
    )
    .unwrap();
    for page in 1..=12 {
        assert_written(&temp.path().join(format!("page-{page:02}.pdf")));
    }

    let chunk_pattern = temp.path().join("chunk-%d.pdf");
    split_pages_with_options(
        &input,
        chunk_pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 5,
            ..SplitPagesOptions::default()
        },
    )
    .unwrap();

    let qpdf = QpdfValidator::detect();
    for (chunk, expected_pages) in [(1, 5), (2, 5), (3, 2)] {
        let path = temp.path().join(format!("chunk-{chunk}.pdf"));
        assert_written(&path);
        qpdf.validate(&path, expected_pages);
    }
    assert!(!temp.path().join("chunk-4.pdf").exists());
}

#[test]
fn split_pages_with_options_chunks_and_decrypts_user_password_input() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("chunk-%d.pdf");

    // pages_per_file and password must compose: chunk while decrypting.
    split_pages_with_options(
        &fixture("user-password.pdf"),
        pattern.to_str().unwrap(),
        &SplitPagesOptions {
            pages_per_file: 3,
            password: Some("user".to_string()),
        },
    )
    .unwrap();

    let qpdf = QpdfValidator::detect();
    for (chunk, expected_pages) in [(1, 3), (2, 3), (3, 3), (4, 2)] {
        let path = temp.path().join(format!("chunk-{chunk}.pdf"));
        assert_written(&path);
        assert_decrypted(&path);
        qpdf.validate(&path, expected_pages);
        qpdf.assert_not_encrypted(&path);
    }
    assert!(!temp.path().join("chunk-5.pdf").exists());
}

#[test]
fn split_pages_writes_object_stream_input() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("obj-page-%d.pdf");
    split_pages(&fixture("11-pages-objstm.pdf"), pattern.to_str().unwrap()).unwrap();

    let qpdf = QpdfValidator::detect();
    for page in 1..=11 {
        let path = temp.path().join(format!("obj-page-{page:02}.pdf"));
        assert_written(&path);
        qpdf.validate(&path, 1);
    }
}

#[test]
fn split_pages_treats_missing_references_as_null() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("dangling-reference.pdf");
    let pattern = temp.path().join("dangling-page-%d.pdf");

    write_one_page_pdf_with_dangling_reference(&input);
    split_pages(&input, pattern.to_str().unwrap()).unwrap();

    let output = temp.path().join("dangling-page-1.pdf");
    assert_written(&output);
    QpdfValidator::detect().validate(&output, 1);
}

#[test]
fn split_pages_prunes_shared_page_resources() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("shared-resources.pdf");
    let pattern = temp.path().join("pruned-page-%d.pdf");

    write_shared_resources_fixture(&input, false);
    split_pages(&input, pattern.to_str().unwrap()).unwrap();

    let qpdf = QpdfValidator::detect();
    assert_page_resources(&temp.path().join("pruned-page-1.pdf"), &qpdf, &["X0"], &[]);
    assert_page_resources(
        &temp.path().join("pruned-page-2.pdf"),
        &qpdf,
        &["X1"],
        &["F1"],
    );
    assert_page_resources(&temp.path().join("pruned-page-3.pdf"), &qpdf, &["X2"], &[]);
}

#[test]
fn merge_whole_document_keeps_shared_page_resources() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("shared-resources.pdf");
    let merged = temp.path().join("merged.pdf");

    write_shared_resources_fixture(&input, false);
    merge(&[MergeInput::all(&input)], &merged).unwrap();

    assert_page_resources_in_document(
        &merged,
        &QpdfValidator::detect(),
        3,
        1,
        &["X0", "X1", "X2", "X3", "X4", "X5", "X6"],
        &["F1"],
    );
}

#[test]
fn merge_selected_ranges_prunes_shared_page_resources() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("shared-resources.pdf");
    let merged = temp.path().join("merged-range.pdf");

    write_shared_resources_fixture(&input, false);
    merge(
        &[MergeInput {
            path: input,
            ranges: vec![PageRangeGroup::parse("1").unwrap()],
        }],
        &merged,
    )
    .unwrap();

    assert_page_resources(&merged, &QpdfValidator::detect(), &["X0"], &[]);
}

#[test]
fn split_pages_falls_back_when_content_stream_is_malformed() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("malformed-content.pdf");
    let pattern = temp.path().join("fallback-page-%d.pdf");

    write_shared_resources_fixture(&input, true);
    split_pages(&input, pattern.to_str().unwrap()).unwrap();

    let qpdf = QpdfValidator::detect();
    assert_page_resources(
        &temp.path().join("fallback-page-1.pdf"),
        &qpdf,
        &["X0", "X1", "X2", "X3", "X4", "X5", "X6"],
        &["F1"],
    );
}

#[test]
fn split_pages_falls_back_when_form_with_resources_uses_page_font() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("form-missing-font.pdf");
    let pattern = temp.path().join("form-fallback-%d.pdf");

    write_form_font_fixture(&input, true);
    split_pages(&input, pattern.to_str().unwrap()).unwrap();

    assert_page_resources(
        &temp.path().join("form-fallback-1.pdf"),
        &QpdfValidator::detect(),
        &["X0", "X1", "X2", "X3", "X4", "X5", "X6"],
        &["F1"],
    );
}

#[test]
fn split_pages_keeps_page_font_used_by_form_without_resources() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("form-inherits-font.pdf");
    let pattern = temp.path().join("form-inherits-%d.pdf");

    write_form_font_fixture(&input, false);
    split_pages(&input, pattern.to_str().unwrap()).unwrap();

    assert_page_resources(
        &temp.path().join("form-inherits-1.pdf"),
        &QpdfValidator::detect(),
        &["X0"],
        &["F1"],
    );
}

#[test]
fn split_requires_password_for_user_password_inputs() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("should-not-exist.pdf");

    let error = split(
        &fixture("user-password.pdf"),
        &[SplitOutput {
            range: PageRangeGroup::parse("1").unwrap(),
            path: output.clone(),
        }],
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::Password(_)));
    assert!(
        error.to_string().contains("--password"),
        "error should point at --password: {error}"
    );
    assert!(!output.exists());
}

#[test]
fn split_pages_requires_password_for_user_password_inputs() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("should-not-exist-%d.pdf");

    let error = split_pages(&fixture("user-password.pdf"), pattern.to_str().unwrap()).unwrap_err();

    assert!(matches!(error, PdfOpsError::Password(_)));
    assert!(
        error.to_string().contains("--password"),
        "error should point at --password: {error}"
    );
    assert!(!temp.path().join("should-not-exist-1.pdf").exists());
}

#[test]
fn split_pages_rejects_wrong_password_with_invalid_password_error() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("should-not-exist-%d.pdf");

    let error = split_pages_with_password(
        &fixture("user-password.pdf"),
        pattern.to_str().unwrap(),
        Some("wrong"),
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::Password(_)));
    assert!(
        error.to_string().contains("invalid password"),
        "error should report an invalid password: {error}"
    );
    assert!(!temp.path().join("should-not-exist-1.pdf").exists());
}

#[test]
fn page_count_reads_owner_only_encrypted_input_without_password() {
    assert_eq!(page_count(&fixture("owner-only.pdf")).unwrap(), 11);
}

#[test]
fn page_count_with_password_reads_user_password_input() {
    assert_eq!(
        page_count_with_password(&fixture("user-password.pdf"), Some("user")).unwrap(),
        11
    );
}

#[test]
fn split_with_password_decrypts_user_password_input() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("decrypted-1-3.pdf");

    split_with_password(
        &fixture("user-password.pdf"),
        &[SplitOutput {
            range: PageRangeGroup::parse("1-3").unwrap(),
            path: output.clone(),
        }],
        Some("user"),
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    let qpdf = QpdfValidator::detect();
    qpdf.validate(&output, 3);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn split_pages_decrypts_owner_only_encrypted_input_without_password() {
    let temp = tempdir().unwrap();
    let pattern = temp.path().join("owner-only-%d.pdf");

    split_pages(&fixture("owner-only.pdf"), pattern.to_str().unwrap()).unwrap();

    let qpdf = QpdfValidator::detect();
    for page in 1..=11 {
        let path = temp.path().join(format!("owner-only-{page:02}.pdf"));
        assert_written(&path);
        assert_decrypted(&path);
        qpdf.validate(&path, 1);
        qpdf.assert_not_encrypted(&path);
    }
}

#[test]
fn merge_decrypts_owner_only_encrypted_input_without_password() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("merged.pdf");

    merge(
        &[
            MergeInput::all(fixture("owner-only.pdf")),
            MergeInput::all(fixture("11-pages.pdf")),
        ],
        &output,
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    let qpdf = QpdfValidator::detect();
    qpdf.validate(&output, 22);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn merge_preserve_whole_single_encrypted_input_rewrites_decrypted() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("merged.pdf");

    // The byte-copy fast path must not apply: it would keep the output
    // encrypted, while outputs are always written decrypted.
    merge_with_options(
        &[MergeInput::all(fixture("owner-only.pdf"))],
        &output,
        MergeOptions {
            preserve_whole_single_input: true,
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    let qpdf = QpdfValidator::detect();
    qpdf.validate(&output, 11);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn merge_with_password_merges_ranges_from_user_password_input() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("merged-ranges.pdf");

    merge_with_options(
        &[MergeInput {
            path: fixture("user-password.pdf"),
            ranges: vec![PageRangeGroup::parse("1-2").unwrap()],
        }],
        &output,
        MergeOptions {
            password: Some("user".to_string()),
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    let qpdf = QpdfValidator::detect();
    qpdf.validate(&output, 2);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn split_pages_decrypts_aes128_encrypted_input() {
    let qpdf = QpdfValidator::detect();
    let temp = tempdir().unwrap();
    let encrypted = temp.path().join("aes128.pdf");
    if !qpdf.encrypt(
        &fixture("11-pages.pdf"),
        &encrypted,
        "",
        "ownerpw",
        &["128", "--use-aes=y"],
    ) {
        return;
    }

    let pattern = temp.path().join("aes128-%d.pdf");
    split_pages(&encrypted, pattern.to_str().unwrap()).unwrap();

    for page in 1..=11 {
        let path = temp.path().join(format!("aes128-{page:02}.pdf"));
        assert_written(&path);
        assert_decrypted(&path);
        qpdf.validate(&path, 1);
        qpdf.assert_not_encrypted(&path);
    }
}

#[test]
fn split_decrypts_rc4_encrypted_input() {
    let qpdf = QpdfValidator::detect();
    let temp = tempdir().unwrap();
    let encrypted = temp.path().join("rc4.pdf");
    if !qpdf.encrypt(
        &fixture("11-pages.pdf"),
        &encrypted,
        "",
        "ownerpw",
        &["128", "--use-aes=n"],
    ) {
        return;
    }

    let output = temp.path().join("rc4-1-3.pdf");
    split(
        &encrypted,
        &[SplitOutput {
            range: PageRangeGroup::parse("1-3").unwrap(),
            path: output.clone(),
        }],
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    qpdf.validate(&output, 3);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn merge_with_password_decrypts_aes256_user_password_input() {
    let qpdf = QpdfValidator::detect();
    let temp = tempdir().unwrap();
    let encrypted = temp.path().join("aes256-user.pdf");
    if !qpdf.encrypt(
        &fixture("11-pages.pdf"),
        &encrypted,
        "userpw",
        "ownerpw",
        &["256"],
    ) {
        return;
    }

    let output = temp.path().join("merged.pdf");
    merge_with_options(
        &[MergeInput::all(&encrypted)],
        &output,
        MergeOptions {
            password: Some("userpw".to_string()),
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_written(&output);
    assert_decrypted(&output);
    qpdf.validate(&output, 11);
    qpdf.assert_not_encrypted(&output);
}

#[test]
fn split_resolves_all_ranges_before_writing_any_output() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first.pdf");
    let bad = temp.path().join("bad.pdf");

    let error = split(
        &fixture("11-pages.pdf"),
        &[
            SplitOutput {
                range: PageRangeGroup::parse("1-3").unwrap(),
                path: first.clone(),
            },
            SplitOutput {
                range: PageRangeGroup::parse("99").unwrap(),
                path: bad.clone(),
            },
        ],
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::Range(_)));
    assert!(!first.exists());
    assert!(!bad.exists());
}

#[test]
fn split_rejects_duplicate_output_paths_before_writing() {
    let temp = tempdir().unwrap();
    let duplicate = temp.path().join("duplicate.pdf");

    let error = split(
        &fixture("11-pages.pdf"),
        &[
            SplitOutput {
                range: PageRangeGroup::parse("1").unwrap(),
                path: duplicate.clone(),
            },
            SplitOutput {
                range: PageRangeGroup::parse("2").unwrap(),
                path: duplicate.clone(),
            },
        ],
    )
    .unwrap_err();

    assert!(matches!(error, PdfOpsError::InvalidStructure(_)));
    assert!(!duplicate.exists());
}

/// Three real leaf pages; the root /Pages carries `count` verbatim as /Count
/// (or omits the key entirely) so tests can probe the trusted-count fast path.
fn write_filter_abbreviation_pdf(path: &Path) {
    let mut pdf = Vec::new();
    let mut offsets = [0usize; 11];

    pdf.extend_from_slice(b"%PDF-1.5\n%\xff\xff\xff\xff\n");

    let (object_stream, first) = object_stream_content(&[
        (2, b"<< /Type /Catalog /Pages 3 0 R >>".as_slice()),
        (3, b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".as_slice()),
        (
            4,
            b"<< /Type /Page /Parent 3 0 R /MediaBox [0 0 100 100] \
              /Resources 5 0 R /Contents 7 0 R >>"
                .as_slice(),
        ),
        (
            5,
            b"<< /XObject << /X0 8 0 R /X1 9 0 R /X2 9 0 R /X3 9 0 R \
              /X4 9 0 R /X5 9 0 R /X6 9 0 R >> >>"
                .as_slice(),
        ),
    ]);
    let compressed_object_stream = zlib_compress(&object_stream);
    offsets[1] = push_stream_object(
        &mut pdf,
        1,
        &format!("/Type /ObjStm /Filter /Fl /N 4 /First {first}"),
        &compressed_object_stream,
    );

    let compressed_content = zlib_compress(b"q /X0 Do Q");
    offsets[7] = push_stream_object(&mut pdf, 7, "/Filter /Fl", &compressed_content);
    offsets[8] = push_stream_object(
        &mut pdf,
        8,
        "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>",
        b"q Q",
    );
    offsets[9] = push_stream_object(
        &mut pdf,
        9,
        "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << >>",
        b"q Q",
    );

    offsets[10] = pdf.len();
    let mut xref_rows = Vec::new();
    push_xref_row(&mut xref_rows, 0, 0, u16::MAX);
    push_xref_row(&mut xref_rows, 1, offsets[1] as u32, 0);
    for (index, object_number) in (2u16..=5).enumerate() {
        assert_eq!(object_number as usize, index + 2);
        push_xref_row(&mut xref_rows, 2, 1, index as u16);
    }
    push_xref_row(&mut xref_rows, 0, 0, u16::MAX);
    push_xref_row(&mut xref_rows, 1, offsets[7] as u32, 0);
    push_xref_row(&mut xref_rows, 1, offsets[8] as u32, 0);
    push_xref_row(&mut xref_rows, 1, offsets[9] as u32, 0);
    push_xref_row(&mut xref_rows, 1, offsets[10] as u32, 0);

    let compressed_xref = zlib_compress(&xref_rows);
    push_stream_object(
        &mut pdf,
        10,
        "/Type /XRef /Filter /Fl /Size 11 /Index [0 11] /W [1 4 2] /Root 2 0 R",
        &compressed_xref,
    );
    write!(pdf, "startxref\n{}\n%%EOF\n", offsets[10]).unwrap();

    fs::write(path, pdf).unwrap_or_else(|err| {
        panic!(
            "failed to write filter abbreviation fixture {}: {err}",
            path.display()
        )
    });
}

fn object_stream_content(objects: &[(u32, &[u8])]) -> (Vec<u8>, usize) {
    let mut header = Vec::new();
    let mut body = Vec::new();
    for (object_number, object) in objects {
        write!(header, "{object_number} {} ", body.len()).unwrap();
        body.extend_from_slice(object);
        body.push(b'\n');
    }
    let first = header.len();
    header.extend_from_slice(&body);
    (header, first)
}

fn push_stream_object(pdf: &mut Vec<u8>, id: u32, dict_entries: &str, content: &[u8]) -> usize {
    let offset = pdf.len();
    write!(
        pdf,
        "{id} 0 obj\n<< {dict_entries} /Length {} >>\nstream\n",
        content.len()
    )
    .unwrap();
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    offset
}

fn push_xref_row(rows: &mut Vec<u8>, entry_type: u8, second: u32, third: u16) {
    rows.push(entry_type);
    rows.extend_from_slice(&second.to_be_bytes());
    rows.extend_from_slice(&third.to_be_bytes());
}

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn write_three_page_pdf_with_count(path: &Path, count: Option<Object>) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let page_ids: Vec<_> = (0..3).map(|_| document.new_object_id()).collect();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
    let mut pages_dict = dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(page_ids.iter().copied().map(Object::Reference).collect()),
    };
    if let Some(count) = count {
        pages_dict.set("Count", count);
    }
    document.objects.insert(pages_id, pages_dict.into());
    for page_id in page_ids {
        document.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
                "Resources" => dictionary! {},
            }
            .into(),
        );
    }
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save page-count fixture: {err}"));
}

fn write_page_tree_type_inference_pdf(
    path: &Path,
    pages_type: Option<&[u8]>,
    page_type: Option<&[u8]>,
) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let page_id = document.new_object_id();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );

    let mut pages = Dictionary::new();
    set_optional_type(&mut pages, pages_type);
    pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages.set("Count", 1);
    document.objects.insert(pages_id, Object::Dictionary(pages));

    let mut page = Dictionary::new();
    set_optional_type(&mut page, page_type);
    page.set("Parent", pages_id);
    page.set(
        "MediaBox",
        Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
    );
    page.set("Resources", Dictionary::new());
    document.objects.insert(page_id, Object::Dictionary(page));

    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save page-tree type fixture: {err}"));
}

fn set_optional_type(dict: &mut Dictionary, type_name: Option<&[u8]>) {
    if let Some(type_name) = type_name {
        dict.set("Type", Object::Name(type_name.to_vec()));
    }
}

fn write_page_tree_with_bad_kids(path: &Path, kids: Object) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );

    let mut pages = Dictionary::new();
    pages.set("Kids", kids);
    pages.set("Count", 1);
    document.objects.insert(pages_id, Object::Dictionary(pages));

    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save bad page-tree fixture: {err}"));
}

fn write_simple_pdf(path: &Path, page_count: usize) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();

    let kids = (0..page_count)
        .map(|_| {
            let content_id = document.add_object(Object::Stream(Stream::new(
                Dictionary::new(),
                b"q Q".to_vec(),
            )));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
                "Resources" => dictionary! {},
                "Contents" => content_id,
            });
            Object::Reference(page_id)
        })
        .collect::<Vec<_>>();

    document.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => page_count as i64,
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
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save simple fixture: {err}"));
}

fn write_one_page_pdf_with_dangling_reference(path: &Path) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let page_id = document.new_object_id();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
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
        page_id,
        dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
            "Resources" => dictionary! {},
            "Foo" => Object::Reference((99, 0)),
        }
        .into(),
    );
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save dangling-reference fixture: {err}"));
}

fn write_shared_resources_fixture(path: &Path, malformed_first_page: bool) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let resources_id = document.new_object_id();
    let font_id = document.new_object_id();
    let x0_id = document.new_object_id();
    let x1_id = document.new_object_id();
    let x2_id = document.new_object_id();
    let page1_id = document.new_object_id();
    let page2_id = document.new_object_id();
    let page3_id = document.new_object_id();
    let content1_id = document.new_object_id();
    let content2_id = document.new_object_id();
    let content3_id = document.new_object_id();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
    document.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(vec![
                Object::Reference(page1_id),
                Object::Reference(page2_id),
                Object::Reference(page3_id),
            ]),
            "Count" => 3,
        }
        .into(),
    );
    document.objects.insert(
        resources_id,
        dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
            "XObject" => dictionary! {
                "X0" => x0_id,
                "X1" => x1_id,
                "X2" => x2_id,
                "X3" => x0_id,
                "X4" => x1_id,
                "X5" => x2_id,
                "X6" => x0_id,
            },
            "ProcSet" => Object::Array(vec![Object::Name(b"PDF".to_vec()), Object::Name(b"Text".to_vec())]),
        }
        .into(),
    );
    document.objects.insert(
        font_id,
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        }
        .into(),
    );

    for (id, label) in [
        (x0_id, b"0".as_slice()),
        (x1_id, b"1".as_slice()),
        (x2_id, b"2".as_slice()),
    ] {
        document.objects.insert(
            id,
            Object::Stream(Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "BBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
                    "Resources" => dictionary! {},
                },
                [b"q ".as_slice(), label, b" 0 0 rg Q".as_slice()].concat(),
            )),
        );
    }

    let page1_content = if malformed_first_page {
        b"q /X0 Do @@@ Q".as_slice()
    } else {
        b"q /X0 Do Q".as_slice()
    };
    document.objects.insert(
        content1_id,
        Object::Stream(Stream::new(Dictionary::new(), page1_content.to_vec())),
    );
    document.objects.insert(
        content2_id,
        Object::Stream(Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf ET q /X1 Do Q".to_vec(),
        )),
    );
    document.objects.insert(
        content3_id,
        Object::Stream(Stream::new(Dictionary::new(), b"q /X2 Do Q".to_vec())),
    );

    for (page_id, content_id) in [
        (page1_id, content1_id),
        (page2_id, content2_id),
        (page3_id, content3_id),
    ] {
        document.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
                "Resources" => resources_id,
                "Contents" => content_id,
            }
            .into(),
        );
    }
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save shared-resources fixture: {err}"));
}

fn write_form_font_fixture(path: &Path, form_has_own_resources: bool) {
    let mut document = Document::with_version("1.7");
    let catalog_id = document.new_object_id();
    let pages_id = document.new_object_id();
    let resources_id = document.new_object_id();
    let font_id = document.new_object_id();
    let form_id = document.new_object_id();
    let page_id = document.new_object_id();
    let content_id = document.new_object_id();

    document.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
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
        resources_id,
        dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
            "XObject" => dictionary! {
                "X0" => form_id,
                "X1" => form_id,
                "X2" => form_id,
                "X3" => form_id,
                "X4" => form_id,
                "X5" => form_id,
                "X6" => form_id,
            },
        }
        .into(),
    );
    document.objects.insert(
        font_id,
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        }
        .into(),
    );
    let mut form_dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
    };
    if form_has_own_resources {
        form_dict.set("Resources", dictionary! {});
    }
    document.objects.insert(
        form_id,
        Object::Stream(Stream::new(form_dict, b"BT /F1 12 Tf ET".to_vec())),
    );
    document.objects.insert(
        content_id,
        Object::Stream(Stream::new(Dictionary::new(), b"q /X0 Do Q".to_vec())),
    );
    document.objects.insert(
        page_id,
        dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => Object::Array(vec![0.into(), 0.into(), 100.into(), 100.into()]),
            "Resources" => resources_id,
            "Contents" => content_id,
        }
        .into(),
    );
    document.trailer.set("Root", catalog_id);
    document
        .save(path)
        .unwrap_or_else(|err| panic!("failed to save form-font fixture: {err}"));
}

fn assert_page_resources(path: &Path, qpdf: &QpdfValidator, xobjects: &[&str], fonts: &[&str]) {
    assert_page_resources_in_document(path, qpdf, 1, 1, xobjects, fonts);
}

fn assert_page_resources_in_document(
    path: &Path,
    qpdf: &QpdfValidator,
    expected_pages: usize,
    page_number: u32,
    xobjects: &[&str],
    fonts: &[&str],
) {
    assert_written(path);
    qpdf.validate(path, expected_pages);

    let document = Document::load(path)
        .unwrap_or_else(|err| panic!("failed to load split output {}: {err}", path.display()));
    let pages = document.get_pages();
    let page_id = pages
        .get(&page_number)
        .copied()
        .unwrap_or_else(|| panic!("missing page {page_number} in {}", path.display()));
    let page = document
        .get_object(page_id)
        .unwrap_or_else(|err| panic!("failed to read page object {}: {err}", path.display()))
        .as_dict()
        .unwrap_or_else(|_| panic!("page object is not a dictionary in {}", path.display()));
    let resources_obj = page
        .get(b"Resources")
        .unwrap_or_else(|_| panic!("page resources missing in {}", path.display()));
    let resources_owner;
    let resources = match resources_obj {
        Object::Dictionary(dict) => dict,
        Object::Reference(id) => {
            resources_owner = document
                .get_object(*id)
                .unwrap_or_else(|err| panic!("failed to dereference resources: {err}"));
            resources_owner
                .as_dict()
                .unwrap_or_else(|_| panic!("page resources reference is not a dictionary"))
        }
        _ => panic!("page resources are not a dictionary in {}", path.display()),
    };
    assert_resource_keys(resources, b"XObject", xobjects, path);
    assert_resource_keys(resources, b"Font", fonts, path);
}

fn assert_resource_keys(resources: &lopdf::Dictionary, key: &[u8], expected: &[&str], path: &Path) {
    let names = resources
        .get(key)
        .and_then(Object::as_dict)
        .map(|dict| {
            dict.iter()
                .map(|(name, _)| String::from_utf8_lossy(name).to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut names = names;
    names.sort();
    let mut expected = expected
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        names,
        expected,
        "unexpected {} keys in {}",
        String::from_utf8_lossy(key),
        path.display()
    );
}
