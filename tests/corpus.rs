//! Corpus harness: runs pdq against directories full of real PDFs.
//!
//! Point PDQ_CORPUS_DIR at a directory of PDFs (searched recursively;
//! defaults to `corpus/` in the repo root, which `scripts/fetch_corpus.sh`
//! populates). The test skips silently when no corpus is present, so CI
//! without a corpus is unaffected. Run with:
//!
//! ```sh
//! cargo test --release --test corpus -- --ignored --nocapture
//! ```
//!
//! The test is `#[ignore]`d so plain `cargo test --all-targets` stays fast
//! even when a corpus is present (a debug-mode corpus run also times out on
//! very large documents).
//!
//! For every PDF the harness establishes ground truth with qpdf, then runs
//! pdq's page-count, first/last-page split, whole-document rewrite,
//! split-pages, and self-merge, classifying each file:
//!
//! - PASS: everything agreed
//! - SKIP: input rejected by both qpdf and pdq (encrypted, corrupt)
//! - NOTE: pdq accepted a file qpdf rejects (extra leniency, not validated)
//! - WARN: pdq refused input that qpdf handles (MVP scope gaps)
//! - FAIL: panic, timeout, page-count mismatch, or invalid output produced
//!   from a qpdf-clean input -- always bugs
//!
//! WARNs become failures with PDQ_CORPUS_STRICT=1. PDQ_CORPUS_MAX_FILES=N
//! caps the run for quick iterations.

use std::{
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use lopdf::Document;
use pdq::{merge, page_count, split, split_pages, MergeInput, PageRangeGroup, SplitOutput};
use tempfile::tempdir;

const FILE_TIMEOUT: Duration = Duration::from_secs(180);
const SPLIT_PAGES_MAX: usize = 3_000;
const SELF_MERGE_MAX: usize = 1_000;

// ---------------------------------------------------------------------------
// qpdf ground truth
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Qpdf {
    available: bool,
}

impl Qpdf {
    fn detect() -> Self {
        let available = matches!(
            Command::new("qpdf").arg("--version").output(),
            Ok(output) if output.status.success()
        );
        Self { available }
    }

    fn npages(&self, path: &Path) -> Option<Result<usize, String>> {
        if !self.available {
            return None;
        }
        let output = Command::new("qpdf")
            .args(["--show-npages", "--"])
            .arg(path)
            .output()
            .expect("failed to spawn qpdf");
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Some(
                stdout
                    .trim()
                    .parse()
                    .map_err(|err| format!("unparsable qpdf npages output: {err}")),
            )
        } else {
            Some(Err(String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("qpdf failed")
                .to_string()))
        }
    }

    /// `--show-npages` trusts the declared /Count, which fuzzed files abuse
    /// (e.g. a tree with /Count 9999999999 but one real page). This walks
    /// the actual page tree via the JSON dump instead.
    fn npages_via_walk(&self, path: &Path) -> Option<usize> {
        if !self.available {
            return None;
        }
        let output = Command::new("qpdf")
            .args(["--warning-exit-0", "--json", "--json-key=pages", "--"])
            .arg(path)
            .output()
            .expect("failed to spawn qpdf");
        if !output.status.success() {
            return None;
        }
        // each entry in the pages array carries exactly one "object" key
        let text = String::from_utf8_lossy(&output.stdout);
        Some(text.matches("\"object\"").count())
    }

    fn is_encrypted(&self, path: &Path) -> bool {
        if !self.available {
            return false;
        }
        matches!(
            Command::new("qpdf").args(["--is-encrypted", "--"]).arg(path).output(),
            Ok(output) if output.status.code() == Some(0)
        )
    }

    fn check_ok(&self, path: &Path) -> bool {
        if !self.available {
            return false;
        }
        matches!(
            Command::new("qpdf")
                .args(["--warning-exit-0", "--check", "--"])
                .arg(path)
                .output(),
            Ok(output) if output.status.success()
        )
    }
}

// ---------------------------------------------------------------------------
// per-file examination
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Findings {
    pages: Option<usize>,
    skip: Option<String>,
    note: Option<String>,
    warns: Vec<String>,
    fails: Vec<String>,
}

struct Report {
    path: PathBuf,
    bytes: u64,
    elapsed: Duration,
    findings: Findings,
}

fn describe_panic(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "opaque panic payload".to_string())
}

/// Runs a pdq/lopdf operation, translating panics into findings.
fn guarded<T>(fails: &mut Vec<String>, label: &str, op: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(op)) {
        Ok(value) => Some(value),
        Err(payload) => {
            fails.push(format!("{label} panicked: {}", describe_panic(payload)));
            None
        }
    }
}

fn looks_unsupported(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("unsupported") || lower.contains("encrypt") || lower.contains("password")
}

fn examine(path: &Path, qpdf: Qpdf) -> Findings {
    let mut f = Findings::default();

    let expected = qpdf.npages(path);
    let counted = guarded(&mut f.fails, "page-count", || page_count(path));
    let pages = match (expected, counted) {
        (_, None) => return f, // page-count panicked; nothing else is safe
        (Some(Ok(expected)), Some(Ok(counted))) => {
            if expected != counted {
                // Arbitrate with qpdf's real page-tree walk: --show-npages
                // trusts the declared /Count, which fuzzed files abuse.
                if qpdf.npages_via_walk(path) == Some(counted) {
                    f.pages = Some(counted);
                    f.note = Some(format!(
                        "pdq={counted} matches qpdf's page walk; --show-npages says \
                         {expected} (declared /Count)"
                    ));
                    return f;
                }
                // qpdf's walk also disagrees; only a bug on clean inputs,
                // since qpdf reconstructs damaged files heuristically.
                let message = format!("page-count mismatch: pdq={counted} qpdf={expected}");
                if qpdf.check_ok(path) {
                    f.fails.push(message);
                } else {
                    f.warns
                        .push(format!("{message} (input fails qpdf --check)"));
                }
                return f;
            }
            counted
        }
        (Some(Ok(expected)), Some(Err(err))) => {
            if qpdf.is_encrypted(path) || looks_unsupported(&err.to_string()) {
                f.skip = Some(format!("out of MVP scope ({err})"));
            } else {
                f.warns.push(format!(
                    "qpdf counts {expected} pages but pdq errored: {err}"
                ));
            }
            return f;
        }
        (Some(Err(qpdf_err)), Some(Ok(counted))) => {
            f.pages = Some(counted);
            f.note = Some(format!(
                "pdq counts {counted} pages where qpdf fails ({qpdf_err}); leniency not validated"
            ));
            return f;
        }
        (Some(Err(qpdf_err)), Some(Err(pdq_err))) => {
            f.skip = Some(format!("rejected by qpdf ({qpdf_err}) and pdq ({pdq_err})"));
            return f;
        }
        (None, Some(Ok(counted))) => counted, // no qpdf: proceed self-consistently
        (None, Some(Err(err))) => {
            f.skip = Some(format!("pdq errored and no qpdf for ground truth: {err}"));
            return f;
        }
    };
    f.pages = Some(pages);
    if pages == 0 {
        // qpdf agrees (or is absent): a legitimately page-less document;
        // nothing to split or merge
        f.note = Some("zero-page document".to_string());
        return f;
    }

    let temp = tempdir().expect("tempdir");
    let input_clean = qpdf.check_ok(path);

    // extract first and last page
    let first = temp.path().join("first.pdf");
    let last = temp.path().join("last.pdf");
    let split_result = guarded(&mut f.fails, "split first/last", || {
        split(
            path,
            &[
                SplitOutput {
                    range: PageRangeGroup::parse("1").unwrap(),
                    path: first.clone(),
                },
                SplitOutput {
                    range: PageRangeGroup::parse("z").unwrap(),
                    path: last.clone(),
                },
            ],
        )
    });
    match split_result {
        Some(Ok(())) => {
            for output in [&first, &last] {
                match guarded(&mut f.fails, "load split output", || Document::load(output)) {
                    Some(Ok(document)) => {
                        let pages_in_output = document.get_pages().len();
                        if pages_in_output != 1 {
                            f.fails.push(format!(
                                "{} has {pages_in_output} pages, expected 1",
                                output.file_name().unwrap().to_string_lossy()
                            ));
                        }
                    }
                    Some(Err(err)) => f.fails.push(format!(
                        "split output {} unreadable by lopdf: {err}",
                        output.file_name().unwrap().to_string_lossy()
                    )),
                    None => {}
                }
                if input_clean && !qpdf.check_ok(output) {
                    f.fails.push(format!(
                        "split output {} fails qpdf --check although input was clean",
                        output.file_name().unwrap().to_string_lossy()
                    ));
                }
            }
        }
        Some(Err(err)) => f.warns.push(format!("split first/last errored: {err}")),
        None => {}
    }

    // whole-document rewrite
    let rewritten = temp.path().join("rewritten.pdf");
    let rewrite_result = guarded(&mut f.fails, "rewrite 1-z", || {
        split(
            path,
            &[SplitOutput {
                range: PageRangeGroup::parse("1-z").unwrap(),
                path: rewritten.clone(),
            }],
        )
    });
    match rewrite_result {
        Some(Ok(())) => {
            if let Some(Ok(actual)) = qpdf.npages(&rewritten) {
                if actual != pages {
                    f.fails
                        .push(format!("rewrite has {actual} pages, expected {pages}"));
                }
            }
            if input_clean && !qpdf.check_ok(&rewritten) {
                f.fails
                    .push("rewrite fails qpdf --check although input was clean".to_string());
            }
        }
        Some(Err(err)) => f.warns.push(format!("rewrite 1-z errored: {err}")),
        None => {}
    }

    // single-page explosion
    if pages <= SPLIT_PAGES_MAX {
        let pages_dir = temp.path().join("pages");
        fs::create_dir(&pages_dir).unwrap();
        let pattern = pages_dir.join("page-%d.pdf");
        let split_pages_result = guarded(&mut f.fails, "split-pages", || {
            split_pages(path, pattern.to_str().unwrap())
        });
        match split_pages_result {
            Some(Ok(())) => {
                let produced = fs::read_dir(&pages_dir).unwrap().count();
                if produced != pages {
                    f.fails.push(format!(
                        "split-pages produced {produced} outputs for {pages} pages"
                    ));
                }
            }
            Some(Err(err)) => f.warns.push(format!("split-pages errored: {err}")),
            None => {}
        }
    }

    // self-merge doubles the page count
    if pages <= SELF_MERGE_MAX {
        let merged = temp.path().join("merged.pdf");
        let merge_result = guarded(&mut f.fails, "self-merge", || {
            merge(&[MergeInput::all(path), MergeInput::all(path)], &merged)
        });
        match merge_result {
            Some(Ok(())) => {
                let merged_pages =
                    guarded(&mut f.fails, "count self-merge", || page_count(&merged));
                if let Some(Ok(actual)) = merged_pages {
                    if actual != pages * 2 {
                        f.fails.push(format!(
                            "self-merge has {actual} pages, expected {}",
                            pages * 2
                        ));
                    }
                }
                if input_clean && !qpdf.check_ok(&merged) {
                    f.fails
                        .push("self-merge fails qpdf --check although input was clean".to_string());
                }
            }
            Some(Err(err)) => f.warns.push(format!("self-merge errored: {err}")),
            None => {}
        }
    }

    f
}

// ---------------------------------------------------------------------------
// corpus walk and orchestration
// ---------------------------------------------------------------------------

fn collect_pdfs(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_pdfs(&path, into);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
            && path.is_file()
        {
            into.push(path);
        }
    }
}

fn examine_with_timeout(path: PathBuf, qpdf: Qpdf) -> Report {
    let started = Instant::now();
    let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (sender, receiver) = mpsc::channel();
    let worker_path = path.clone();
    thread::spawn(move || {
        let findings = examine(&worker_path, qpdf);
        let _ = sender.send(findings);
    });
    let findings = receiver.recv_timeout(FILE_TIMEOUT).unwrap_or_else(|_| {
        let mut f = Findings::default();
        f.fails
            .push(format!("timed out after {}s", FILE_TIMEOUT.as_secs()));
        f
    });
    Report {
        path,
        bytes,
        elapsed: started.elapsed(),
        findings,
    }
}

#[test]
#[ignore = "corpus run is heavy; invoke with: cargo test --release --test corpus -- --ignored --nocapture"]
fn corpus_documents_survive_pdq_operations() {
    let corpus_dir = std::env::var("PDQ_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus"));
    if !corpus_dir.is_dir() {
        eprintln!(
            "no corpus at {} (set PDQ_CORPUS_DIR or run scripts/fetch_corpus.sh); skipping",
            corpus_dir.display()
        );
        return;
    }

    let mut files = Vec::new();
    collect_pdfs(&corpus_dir, &mut files);
    files.sort();
    if let Ok(max) = std::env::var("PDQ_CORPUS_MAX_FILES") {
        let max: usize = max.parse().expect("PDQ_CORPUS_MAX_FILES must be a number");
        files.truncate(max);
    }
    assert!(
        !files.is_empty(),
        "corpus {} has no PDFs",
        corpus_dir.display()
    );

    let qpdf = Qpdf::detect();
    if !qpdf.available {
        eprintln!("warning: qpdf not on PATH; running without ground truth");
    }
    eprintln!(
        "examining {} PDFs from {}",
        files.len(),
        corpus_dir.display()
    );

    let next = AtomicUsize::new(0);
    let reports = Mutex::new(Vec::with_capacity(files.len()));
    let workers = thread::available_parallelism().map_or(4, |n| n.get().min(8));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(index) else {
                    break;
                };
                let report = examine_with_timeout(path.clone(), qpdf);
                let line = summarize(&report);
                eprintln!("{line}");
                reports.lock().unwrap().push(report);
            });
        }
    });

    let mut reports = reports.into_inner().unwrap();
    reports.sort_by(|a, b| a.path.cmp(&b.path));

    let strict = std::env::var("PDQ_CORPUS_STRICT").is_ok_and(|v| v == "1");
    let mut pass = 0usize;
    let mut skip = 0usize;
    let mut note = 0usize;
    let mut warn_files = Vec::new();
    let mut fail_files = Vec::new();
    for report in &reports {
        let f = &report.findings;
        if !f.fails.is_empty() {
            fail_files.push(report);
        } else if !f.warns.is_empty() {
            warn_files.push(report);
        } else if f.skip.is_some() {
            skip += 1;
        } else if f.note.is_some() {
            note += 1;
        } else {
            pass += 1;
        }
    }

    eprintln!(
        "\n== corpus summary: {} files | pass {pass} | note {note} | skip {skip} | warn {} | fail {} ==",
        reports.len(),
        warn_files.len(),
        fail_files.len(),
    );
    for report in &warn_files {
        for warning in &report.findings.warns {
            eprintln!("WARN {}: {warning}", report.path.display());
        }
    }
    for report in &fail_files {
        for failure in &report.findings.fails {
            eprintln!("FAIL {}: {failure}", report.path.display());
        }
    }

    assert!(
        fail_files.is_empty(),
        "{} corpus files exposed bugs; see FAIL lines above",
        fail_files.len()
    );
    if strict {
        assert!(
            warn_files.is_empty(),
            "{} corpus files warned and PDQ_CORPUS_STRICT=1; see WARN lines above",
            warn_files.len()
        );
    }
}

fn summarize(report: &Report) -> String {
    let f = &report.findings;
    let status = if !f.fails.is_empty() {
        "FAIL"
    } else if !f.warns.is_empty() {
        "WARN"
    } else if f.skip.is_some() {
        "skip"
    } else if f.note.is_some() {
        "note"
    } else {
        "ok  "
    };
    let pages = f
        .pages
        .map(|p| format!("{p} pages"))
        .unwrap_or_else(|| "? pages".to_string());
    format!(
        "{status} {} ({pages}, {} KB, {:.1}s)",
        report
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        report.bytes / 1024,
        report.elapsed.as_secs_f32(),
    )
}
