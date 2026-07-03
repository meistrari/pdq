use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    copy::{copy_pages_with_options, CopyOptions},
    lazy::LazyPdf,
    load::{map_file, same_file},
    range::{PageRangeError, PageRangeGroup},
    split::{empty_document, finish_pages},
    write::{copy_all_pages_streaming, stream_pdf_atomically},
    PdfOpsError, Result,
};

#[derive(Debug, Clone)]
pub struct MergeInput {
    pub path: PathBuf,
    pub ranges: Vec<PageRangeGroup>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeOptions {
    pub preserve_whole_single_input: bool,
}

impl MergeInput {
    pub fn all(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ranges: Vec::new(),
        }
    }
}

pub fn merge(inputs: &[MergeInput], output: &Path) -> Result<()> {
    merge_with_options(inputs, output, MergeOptions::default())
}

pub fn merge_with_options(
    inputs: &[MergeInput],
    output: &Path,
    options: MergeOptions,
) -> Result<()> {
    if inputs.is_empty() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }

    if options.preserve_whole_single_input {
        if let [input] = inputs {
            if input.ranges.is_empty() && !same_file(&input.path, output)? {
                return copy_whole_input(input, output);
            }
        }
    }

    if inputs.iter().all(|input| input.ranges.is_empty()) {
        return merge_whole_inputs_streaming(inputs, output);
    }

    let mut target = empty_document();
    let mut merged_pages = Vec::new();

    for input in inputs {
        let mmap = map_file(&input.path)?;
        let source = LazyPdf::parse(&mmap, &input.path)?;
        let pages = source.page_ids()?;
        let page_ids = resolve_merge_page_ids(&pages, input)?;
        merged_pages.extend(copy_pages_with_options(
            &source,
            &mut target,
            &page_ids,
            CopyOptions {
                prune_resources: !input.ranges.is_empty(),
                ..CopyOptions::default()
            },
        )?);
    }

    finish_pages(&mut target, &merged_pages)?;
    target.save(output)?;
    Ok(())
}

fn merge_whole_inputs_streaming(inputs: &[MergeInput], output: &Path) -> Result<()> {
    stream_pdf_atomically(output, |writer| {
        for input in inputs {
            let mmap = map_file(&input.path)?;
            let source = LazyPdf::parse(&mmap, &input.path)?;
            let page_ids = source.page_ids()?;
            if page_ids.is_empty() {
                return Err(PdfOpsError::Range(PageRangeError::NoPages));
            }
            copy_all_pages_streaming(writer, &source, &page_ids)?;
        }
        Ok(())
    })
}

fn copy_whole_input(input: &MergeInput, output: &Path) -> Result<()> {
    let mmap = map_file(&input.path)?;
    let source = LazyPdf::parse(&mmap, &input.path)?;
    if source.page_ids()?.is_empty() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    fs::copy(&input.path, output)?;
    Ok(())
}

fn resolve_merge_page_ids(
    page_ids: &[lopdf::ObjectId],
    input: &MergeInput,
) -> Result<Vec<lopdf::ObjectId>> {
    if page_ids.is_empty() {
        return Err(PdfOpsError::Range(PageRangeError::NoPages));
    }
    if input.ranges.is_empty() {
        return Ok(page_ids.to_vec());
    }

    let mut resolved = Vec::new();
    for range in &input.ranges {
        for page_number in range.resolve(page_ids.len())? {
            let page_id = page_ids.get(page_number - 1).copied().ok_or_else(|| {
                PdfOpsError::InvalidStructure(format!("missing page {page_number}"))
            })?;
            resolved.push(page_id);
        }
    }
    Ok(resolved)
}
