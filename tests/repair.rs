//! Best-effort repair of damaged cross-reference data (issue #14).
//!
//! Fixtures are healthy files from `tests/fixtures` damaged in-memory: the
//! mutations model the two real-world classes — destroyed xref/trailer data
//! (load-time reconstruction) and syntactically-fine tables whose offsets lie
//! (fetch-time repair retry).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use lopdf::Document;
use pdq::{merge_with_options, page_count, split_pages, MergeInput, MergeOptions, PageRangeGroup};
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

fn write_damaged(temp: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = temp.path().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// Cut everything from the last occurrence of `pattern` onwards.
fn truncate_at(bytes: &[u8], pattern: &[u8]) -> Vec<u8> {
    let pos = bytes
        .windows(pattern.len())
        .rposition(|window| window == pattern)
        .unwrap_or_else(|| panic!("pattern {:?} not found", String::from_utf8_lossy(pattern)));
    bytes[..pos].to_vec()
}

/// Swap the 10-digit offsets of the first two in-use entries of the last
/// classic xref table, so both entries point at the wrong object while the
/// table stays perfectly parseable — the fetch-time damage class.
fn swap_first_two_xref_offsets(bytes: &[u8]) -> Vec<u8> {
    // `\nxref` cannot match inside `startxref`, unlike a bare `xref`.
    let table = bytes
        .windows(5)
        .rposition(|window| window == b"\nxref")
        .expect("classic xref table not found");
    let mut positions = Vec::new();
    let mut pos = table;
    while positions.len() < 2 && pos + 20 <= bytes.len() {
        // An in-use entry line: "NNNNNNNNNN GGGGG n ".
        if bytes[pos..].starts_with(b"0")
            && bytes[pos..pos + 10].iter().all(u8::is_ascii_digit)
            && &bytes[pos + 10..pos + 11] == b" "
            && &bytes[pos + 16..pos + 18] == b" n"
        {
            positions.push(pos);
            pos += 20;
        } else {
            pos += 1;
        }
    }
    let [first, second] = positions[..] else {
        panic!("fewer than two in-use xref entries found");
    };
    let mut damaged = bytes.to_vec();
    let (a, b) = (
        bytes[first..first + 10].to_vec(),
        bytes[second..second + 10].to_vec(),
    );
    damaged[first..first + 10].copy_from_slice(&b);
    damaged[second..second + 10].copy_from_slice(&a);
    damaged
}

#[test]
fn destroyed_classic_xref_reconstructs_on_load() {
    // Cutting at the `xref` keyword removes the table, trailer, and
    // startxref: only the raw object scan can load this, and the root must
    // come from the /Type /Catalog fallback.
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages.pdf")).unwrap();
    let damaged = write_damaged(&temp, "truncated.pdf", &truncate_at(&bytes, b"xref"));

    assert_eq!(page_count(&damaged).unwrap(), 11);

    let pattern = temp.path().join("page-%d.pdf");
    split_pages(&damaged, pattern.to_str().unwrap()).unwrap();
    for page in 1..=11 {
        assert_page_count(&temp.path().join(format!("page-{page:02}.pdf")), 1);
    }
}

#[test]
fn destroyed_xref_stream_reconstructs_on_load() {
    // The object-stream fixture keeps its /Type /XRef stream object but
    // loses `startxref`: reconstruction must expand the object stream's
    // members and recover the trailer from the xref stream's dictionary.
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages-objstm.pdf")).unwrap();
    let damaged = write_damaged(
        &temp,
        "truncated-objstm.pdf",
        &truncate_at(&bytes, b"startxref"),
    );

    assert_eq!(page_count(&damaged).unwrap(), 11);

    let pattern = temp.path().join("page-%d.pdf");
    split_pages(&damaged, pattern.to_str().unwrap()).unwrap();
    for page in 1..=11 {
        assert_page_count(&temp.path().join(format!("page-{page:02}.pdf")), 1);
    }
}

#[test]
fn lying_xref_offsets_repair_at_fetch_time() {
    // The table parses fine (the bootstrap accepts it) but its first two
    // entries point at each other's objects, so the first fetch raises an
    // object-id mismatch and the operation retries against a reconstructed
    // table.
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages.pdf")).unwrap();
    let damaged = write_damaged(&temp, "lying.pdf", &swap_first_two_xref_offsets(&bytes));

    assert_eq!(page_count(&damaged).unwrap(), 11);

    let pattern = temp.path().join("page-%d.pdf");
    split_pages(&damaged, pattern.to_str().unwrap()).unwrap();
    for page in 1..=11 {
        assert_page_count(&temp.path().join(format!("page-{page:02}.pdf")), 1);
    }
}

#[test]
fn merge_repairs_damaged_inputs_alongside_healthy_ones() {
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages.pdf")).unwrap();
    let truncated = write_damaged(&temp, "truncated.pdf", &truncate_at(&bytes, b"xref"));
    let lying = write_damaged(&temp, "lying.pdf", &swap_first_two_xref_offsets(&bytes));
    let output = temp.path().join("merged.pdf");

    // Whole-file merge takes the streaming path; the lying input aborts the
    // first attempt and the merge restarts with it force-repaired.
    merge_with_options(
        &[
            MergeInput::all(&truncated),
            MergeInput::all(&lying),
            MergeInput::all(fixture("11-pages.pdf")),
        ],
        &output,
        MergeOptions::default(),
    )
    .unwrap();
    assert_page_count(&output, 33);

    // Ranged merge takes the eager per-input copy path and its own restart
    // loop.
    let ranged_output = temp.path().join("merged-ranged.pdf");
    merge_with_options(
        &[
            MergeInput {
                path: lying.clone(),
                ranges: vec![PageRangeGroup::parse("1-3").unwrap()],
            },
            MergeInput::all(fixture("11-pages.pdf")),
        ],
        &ranged_output,
        MergeOptions::default(),
    )
    .unwrap();
    assert_page_count(&ranged_output, 14);
}

#[test]
fn repaired_single_input_merge_is_rewritten_not_byte_copied() {
    // `merge` with one whole input normally byte-copies, but a repaired
    // source must be rewritten so the output carries a valid xref instead of
    // a verbatim copy of the damage.
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages.pdf")).unwrap();
    let damaged_bytes = truncate_at(&bytes, b"xref");
    let damaged = write_damaged(&temp, "truncated.pdf", &damaged_bytes);
    let output = temp.path().join("preserved.pdf");

    merge_with_options(
        &[MergeInput::all(&damaged)],
        &output,
        MergeOptions {
            preserve_whole_single_input: true,
            ..MergeOptions::default()
        },
    )
    .unwrap();

    assert_ne!(
        fs::read(&output).unwrap(),
        damaged_bytes,
        "damaged input must not be byte-copied"
    );
    assert_page_count(&output, 11);
}

#[test]
fn encrypted_damaged_input_is_not_repaired() {
    // Repair cannot decrypt, so a damaged encrypted file must keep a hard
    // error instead of producing garbage output.
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("user-password.pdf")).unwrap();
    let damaged = write_damaged(&temp, "encrypted.pdf", &truncate_at(&bytes, b"startxref"));

    assert!(page_count(&damaged).is_err());
    let pattern = temp.path().join("page-%d.pdf");
    assert!(split_pages(&damaged, pattern.to_str().unwrap()).is_err());
}

#[test]
fn unrepairable_damage_reports_damaged_xref() {
    // No recoverable objects at all: the error must name the damaged xref
    // (issue #14's acceptance criterion) instead of lopdf's generic message.
    let temp = tempdir().unwrap();
    let damaged = write_damaged(
        &temp,
        "hopeless.pdf",
        b"%PDF-1.4\nthis file has no objects and no xref\n%%EOF\n",
    );

    let err = page_count(&damaged).unwrap_err().to_string();
    assert!(
        err.contains("damaged cross-reference"),
        "error should name the damaged xref, got: {err}"
    );
}

#[test]
fn damaged_input_cli_warns_on_stderr_and_succeeds() {
    let temp = tempdir().unwrap();
    let bytes = fs::read(fixture("11-pages.pdf")).unwrap();
    let damaged = write_damaged(&temp, "truncated.pdf", &truncate_at(&bytes, b"xref"));

    pdq()
        .arg("page-count")
        .arg(&damaged)
        .assert()
        .success()
        .stdout("11\n")
        .stderr(predicate::str::contains("damaged cross-reference data"));
}

#[test]
fn healthy_inputs_never_touch_the_repair_path() {
    // The repair warning doubles as the observable marker that the scanner
    // ran: healthy fixtures must produce silent, warning-free runs.
    for name in ["11-pages.pdf", "11-pages-objstm.pdf", "owner-only.pdf"] {
        pdq()
            .arg("page-count")
            .arg(fixture(name))
            .assert()
            .success()
            .stdout("11\n")
            .stderr("");
    }
}
