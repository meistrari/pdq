use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use lopdf::{dictionary, Document, Object};
use rayon::prelude::*;

use crate::{
    copy::{copy_pages_with_options, resolve_page_ids, CopyOptions, ObjectSource},
    lazy::LazyPdf,
    load::{load_document, map_file},
    range::{PageRangeError, PageRangeGroup},
    split_template::{SinglePageTemplate, WriteGate},
    PdfOpsError, Result,
};

#[derive(Debug, Clone)]
pub struct SplitOutput {
    pub range: PageRangeGroup,
    pub path: PathBuf,
}

pub fn split(input: &Path, outputs: &[SplitOutput]) -> Result<()> {
    // When every requested range is bounded (no `z`/`rN` endpoint), the page
    // walk can stop at the highest page any output needs — extracting a small
    // range from a huge document must not pay for parsing or enumerating the
    // rest. Unbounded ranges enumerate every page anyway and usually copy
    // most of the document, where the eager parse-once source beats
    // per-object lazy fetches.
    let walk_limit = outputs
        .iter()
        .map(|output| output.range.bounded_max_page())
        .collect::<Option<Vec<_>>>()
        .and_then(|maxes| maxes.into_iter().max());

    if let Some(limit) = walk_limit {
        // Lazy, mmap-backed source (same as `split-pages`): xref-only
        // bootstrap plus a walk that stops at `limit` pages. A shorter result
        // means the document really has fewer pages, so the bounds checks in
        // `resolve` still see the true count. Every bounded output is treated
        // as a subset (a prefix walk cannot prove full coverage), so pruning
        // stays on.
        let mmap = map_file(input)?;
        let source = LazyPdf::parse(&mmap, input)?;
        let ordered_pages = source.page_ids_up_to(limit)?;
        if ordered_pages.is_empty() {
            return Err(PdfOpsError::Range(PageRangeError::NoPages));
        }
        let pages: BTreeMap<u32, lopdf::ObjectId> = (1u32..).zip(ordered_pages).collect();
        let resolved_outputs = resolve_split_outputs(outputs, &pages, true)?;
        reject_duplicate_output_paths(&resolved_outputs)?;
        return run_split_outputs(&source, &resolved_outputs);
    }

    let source = load_document(input)?;
    let pages = source.get_pages();
    let resolved_outputs = resolve_split_outputs(outputs, &pages, false)?;
    reject_duplicate_output_paths(&resolved_outputs)?;
    run_split_outputs(&source, &resolved_outputs)
}

fn resolve_split_outputs(
    outputs: &[SplitOutput],
    pages: &BTreeMap<u32, lopdf::ObjectId>,
    subsets_only: bool,
) -> Result<Vec<ResolvedSplitOutput>> {
    outputs
        .iter()
        .map(|output| {
            let page_numbers = output.range.resolve(pages.len())?;
            let page_ids = resolve_page_ids(pages, &page_numbers)?;
            // Pruning unreferenced resources only pays when the output keeps a
            // subset of the pages: a full-document copy retains every resource
            // anyway, so scanning each page's content (and nested form
            // XObjects) to prove usage is pure overhead. Matches qpdf's
            // `--remove-unreferenced-resources=auto`, which skips pruning for
            // whole-file copies.
            let prune_resources = subsets_only
                || page_ids.iter().collect::<BTreeSet<_>>().len() < pages.len();
            Ok(ResolvedSplitOutput {
                path: output.path.clone(),
                page_ids,
                prune_resources,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct SplitPagesOptions {
    /// Maximum number of consecutive pages written to each output file.
    pub pages_per_file: usize,
}

impl Default for SplitPagesOptions {
    fn default() -> Self {
        Self { pages_per_file: 1 }
    }
}

pub fn split_pages(input: &Path, output_pattern: &str) -> Result<()> {
    split_pages_with_options(input, output_pattern, &SplitPagesOptions::default())
}

pub fn split_pages_with_options(
    input: &Path,
    output_pattern: &str,
    options: &SplitPagesOptions,
) -> Result<()> {
    validate_output_pattern(output_pattern)?;
    if options.pages_per_file == 0 {
        return Err(PdfOpsError::InvalidStructure(
            "pages-per-file must be at least 1".into(),
        ));
    }

    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    let pages = source.page_ids()?;
    let page_count = pages.len();
    if page_count == 0 {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    let chunk_count = page_count.div_ceil(options.pages_per_file);
    let width = chunk_count.to_string().len();
    let resolved_outputs = pages
        .chunks(options.pages_per_file)
        .enumerate()
        .map(|(chunk_index, chunk)| {
            Ok(ResolvedSplitOutput {
                path: render_output_pattern(output_pattern, chunk_index + 1, width)?,
                page_ids: chunk.to_vec(),
                // split-pages always prunes: emitting page subsets is its
                // whole purpose, and pruning single-page outputs (including
                // the one-page-document edge) is long-standing behavior.
                prune_resources: true,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Fast path (single-page outputs only — the template writer emits exactly
    // one page per file): serialize the objects shared by every page once,
    // then emit only page-specific objects per output. Falls back to the
    // generic per-output Document path whenever the template cannot be
    // prepared.
    if options.pages_per_file == 1 {
        if let Some(template) = SinglePageTemplate::prepare(&source, &pages) {
            return run_template_outputs(&source, &template, &resolved_outputs);
        }
    }

    run_split_outputs(&source, &resolved_outputs)
}

/// Concurrent output-file writes allowed during a template split; see
/// [`WriteGate`] for why this is small.
const MAX_CONCURRENT_SPLIT_WRITES: usize = 4;

fn run_template_outputs(
    source: &(impl ObjectSource + Sync),
    template: &SinglePageTemplate,
    outputs: &[ResolvedSplitOutput],
) -> Result<()> {
    let gate = WriteGate::new(
        std::env::var("PDQ_SPLIT_WRITERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(MAX_CONCURRENT_SPLIT_WRITES),
    );
    let run_one = |buffer: &mut Vec<u8>, output: &ResolvedSplitOutput| -> Result<()> {
        template.write_page(source, output.page_ids[0], &output.path, buffer, &gate)
    };

    let pool = rayon::ThreadPoolBuilder::new().build();
    match pool {
        Ok(pool) => pool.install(|| {
            outputs
                .par_iter()
                .try_for_each_init(Vec::new, |buffer, output| run_one(buffer, output))
        })?,
        Err(_) => {
            let mut buffer = Vec::new();
            outputs
                .iter()
                .try_for_each(|output| run_one(&mut buffer, output))?
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ResolvedSplitOutput {
    path: PathBuf,
    page_ids: Vec<lopdf::ObjectId>,
    prune_resources: bool,
}

fn run_split_outputs(
    source: &(impl ObjectSource + Sync),
    outputs: &[ResolvedSplitOutput],
) -> Result<()> {
    let run_one = |output: &ResolvedSplitOutput| -> Result<()> {
        let mut target = empty_document();
        let options = CopyOptions {
            prune_resources: output.prune_resources,
            ..CopyOptions::default()
        };
        let copied_pages = copy_pages_with_options(source, &mut target, &output.page_ids, options)?;
        finish_pages(&mut target, &copied_pages)?;
        target.save(&output.path)?;
        Ok(())
    };

    let pool = rayon::ThreadPoolBuilder::new().build();
    match pool {
        Ok(pool) => pool.install(|| outputs.par_iter().try_for_each(run_one))?,
        Err(_) => outputs.iter().try_for_each(run_one)?,
    }
    Ok(())
}

fn reject_duplicate_output_paths(outputs: &[ResolvedSplitOutput]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for output in outputs {
        if !seen.insert(&output.path) {
            return Err(PdfOpsError::InvalidStructure(format!(
                "duplicate split output path: {}",
                output.path.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn render_output_pattern(
    pattern: &str,
    page_number: usize,
    width: usize,
) -> Result<PathBuf> {
    let page = format!("{page_number:0width$}");
    Ok(PathBuf::from(pattern.replacen("%d", &page, 1)))
}

pub(crate) fn validate_output_pattern(pattern: &str) -> Result<()> {
    let occurrences = pattern.match_indices("%d").count();
    if occurrences != 1 {
        return Err(PdfOpsError::InvalidStructure(
            "output pattern must contain exactly one %d".into(),
        ));
    }
    Ok(())
}

pub(crate) fn empty_document() -> Document {
    Document::with_version("1.7")
}

pub(crate) fn finish_pages(target: &mut Document, pages: &[lopdf::ObjectId]) -> Result<()> {
    let pages_id = target.new_object_id();
    let catalog_id = target.new_object_id();
    let kids: Vec<Object> = pages.iter().copied().map(Object::Reference).collect();
    target.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => pages.len() as i64,
        }
        .into(),
    );
    for page_id in pages {
        let page = target
            .get_object_mut(*page_id)?
            .as_dict_mut()
            .map_err(|_| PdfOpsError::InvalidStructure("page is not a dictionary".into()))?;
        if !page.has_type(b"Page") {
            return Err(PdfOpsError::InvalidStructure(
                "pages tree kid does not have /Type /Page".into(),
            ));
        }
        page.set("Parent", pages_id);
    }
    target.objects.insert(
        catalog_id,
        dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }
        .into(),
    );
    target.trailer.set("Root", catalog_id);
    Ok(())
}
