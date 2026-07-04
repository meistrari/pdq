use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use lopdf::{dictionary, Document, Object};
use rayon::prelude::*;

use crate::{
    copy::{copy_pages, resolve_page_ids, ObjectSource},
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
    let source = load_document(input)?;
    let pages = source.get_pages();
    let resolved_outputs = outputs
        .iter()
        .map(|output| {
            let page_numbers = output.range.resolve(pages.len())?;
            let page_ids = resolve_page_ids(&pages, &page_numbers)?;
            Ok(ResolvedSplitOutput {
                path: output.path.clone(),
                page_ids,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    reject_duplicate_output_paths(&resolved_outputs)?;

    run_split_outputs(&source, &resolved_outputs)
}

pub fn split_pages(input: &Path, output_pattern: &str) -> Result<()> {
    validate_output_pattern(output_pattern)?;

    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    let pages = source.page_ids()?;
    let page_count = pages.len();
    if page_count == 0 {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    let width = page_count.to_string().len();
    let resolved_outputs = (1..=page_count)
        .zip(&pages)
        .map(|(page_number, page_id)| {
            Ok(ResolvedSplitOutput {
                path: render_output_pattern(output_pattern, page_number, width)?,
                page_ids: vec![*page_id],
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Fast path: serialize the objects shared by every page once, then emit
    // only page-specific objects per output. Falls back to the generic
    // per-output Document path whenever the template cannot be prepared.
    if let Some(template) = SinglePageTemplate::prepare(&source, &pages) {
        return run_template_outputs(&source, &template, &resolved_outputs);
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
}

fn run_split_outputs(
    source: &(impl ObjectSource + Sync),
    outputs: &[ResolvedSplitOutput],
) -> Result<()> {
    let run_one = |output: &ResolvedSplitOutput| -> Result<()> {
        let mut target = empty_document();
        let copied_pages = copy_pages(source, &mut target, &output.page_ids)?;
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

fn render_output_pattern(pattern: &str, page_number: usize, width: usize) -> Result<PathBuf> {
    let page = format!("{page_number:0width$}");
    Ok(PathBuf::from(pattern.replacen("%d", &page, 1)))
}

fn validate_output_pattern(pattern: &str) -> Result<()> {
    let occurrences = pattern.match_indices("%d").count();
    if occurrences != 1 {
        return Err(PdfOpsError::InvalidStructure(
            "split-pages output pattern must contain exactly one %d".into(),
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
