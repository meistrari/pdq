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
            })
        })
        .collect::<Result<Vec<_>>>()?;

    run_split_outputs(&source, &resolved_outputs)
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
