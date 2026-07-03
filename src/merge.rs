use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::{
    copy::copy_pages,
    lazy::LazyPdf,
    load::map_file,
    range::{PageRangeError, PageRangeGroup},
    split::{empty_document, finish_pages},
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

    let mut target = empty_document();
    let mut merged_pages = Vec::new();

    for input in inputs {
        let mmap = map_file(&input.path)?;
        let source = LazyPdf::parse(&mmap, &input.path)?;
        let pages = source.page_ids()?;
        let page_ids = resolve_merge_page_ids(&pages, input)?;
        merged_pages.extend(copy_pages(&source, &mut target, &page_ids)?);
    }

    finish_pages(&mut target, &merged_pages)?;
    target.save(output)?;
    Ok(())
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

fn same_file(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::metadata(left)?;
    let right = match fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
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
