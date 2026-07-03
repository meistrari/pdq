use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use lopdf::{dictionary, Document, Object};
use rayon::prelude::*;

use crate::{
    copy::{copy_pages, resolve_page_ids, ObjectSource},
    lazy::LazyPdf,
    load::{load_document, map_file, same_file},
    range::{PageRangeError, PageRangeGroup},
    write::{copy_all_pages_streaming, stream_pdf_atomically},
    PdfOpsError, Result,
};

#[derive(Debug, Clone)]
pub struct SplitOutput {
    pub range: PageRangeGroup,
    pub path: PathBuf,
}

pub fn split(input: &Path, outputs: &[SplitOutput]) -> Result<()> {
    let mmap = map_file(input)?;
    let source = LazyPdf::parse(&mmap, input)?;
    let pages = source.page_ids()?;
    if pages.is_empty() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    let resolved_outputs = outputs
        .iter()
        .map(|output| {
            let page_numbers = output.range.resolve(pages.len())?;
            let page_ids = page_numbers
                .iter()
                .map(|page_number| {
                    pages.get(*page_number - 1).copied().ok_or_else(|| {
                        PdfOpsError::InvalidStructure(format!("missing page {page_number}"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ResolvedSplitOutput {
                path: output.path.clone(),
                page_numbers,
                page_ids,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    reject_duplicate_output_paths(&resolved_outputs)?;

    if let [output] = resolved_outputs.as_slice() {
        if is_identity_order(&output.page_numbers, pages.len()) {
            return stream_pdf_atomically(&output.path, |writer| {
                copy_all_pages_streaming(writer, &source, &pages)
            });
        }
    }

    for output in outputs {
        if same_file(input, &output.path)? {
            return split_eager(input, outputs);
        }
    }

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
        .zip(pages)
        .map(|(page_number, page_id)| {
            Ok(ResolvedSplitOutput {
                path: render_output_pattern(output_pattern, page_number, width)?,
                page_numbers: vec![page_number],
                page_ids: vec![page_id],
            })
        })
        .collect::<Result<Vec<_>>>()?;

    run_split_outputs(&source, &resolved_outputs)
}

#[derive(Debug, Clone)]
struct ResolvedSplitOutput {
    path: PathBuf,
    page_numbers: Vec<usize>,
    page_ids: Vec<lopdf::ObjectId>,
}

fn split_eager(input: &Path, outputs: &[SplitOutput]) -> Result<()> {
    let source = load_document(input)?;
    let pages = source.get_pages();
    let resolved_outputs = outputs
        .iter()
        .map(|output| {
            let page_numbers = output.range.resolve(pages.len())?;
            let page_ids = resolve_page_ids(&pages, &page_numbers)?;
            Ok(ResolvedSplitOutput {
                path: output.path.clone(),
                page_numbers,
                page_ids,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    reject_duplicate_output_paths(&resolved_outputs)?;

    run_split_outputs(&source, &resolved_outputs)
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

fn is_identity_order(page_numbers: &[usize], page_count: usize) -> bool {
    page_numbers.len() == page_count
        && page_numbers
            .iter()
            .enumerate()
            .all(|(index, page_number)| *page_number == index + 1)
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

#[cfg(test)]
mod tests {
    use super::is_identity_order;

    #[test]
    fn identity_order_detection_handles_composites_and_rejects_reorders() {
        assert!(is_identity_order(&[1, 2, 3], 3));
        assert!(is_identity_order(&[1, 2, 3, 4], 4));
        assert!(!is_identity_order(&[1, 2], 3));
        assert!(!is_identity_order(&[3, 2, 1], 3));
        assert!(!is_identity_order(&[1, 1, 2, 3], 3));
        assert!(!is_identity_order(&[], 1));
    }
}
