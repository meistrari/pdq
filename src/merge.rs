use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process,
};

use crate::{
    copy::{copy_pages_with_options, CopyOptions},
    lazy::LazyPdf,
    load::map_file,
    range::{PageRangeError, PageRangeGroup},
    split::{empty_document, finish_pages},
    write::{StreamingCopyContext, StreamingPdfWriter},
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
    let temp_output = temp_output_path(output)?;
    let result = merge_whole_inputs_streaming_to_path(inputs, &temp_output);
    match result {
        Ok(()) => {
            fs::rename(&temp_output, output)?;
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_output);
            Err(err)
        }
    }
}

fn merge_whole_inputs_streaming_to_path(inputs: &[MergeInput], output: &Path) -> Result<()> {
    let mut writer = StreamingPdfWriter::create(output)?;

    for input in inputs {
        let mmap = map_file(&input.path)?;
        let source = LazyPdf::parse(&mmap, &input.path)?;
        let page_ids = source.page_ids()?;
        if page_ids.is_empty() {
            return Err(PdfOpsError::Range(PageRangeError::NoPages));
        }

        let copied_pages = {
            let mut context = StreamingCopyContext::new(
                &mut writer,
                CopyOptions {
                    prune_resources: false,
                    ..CopyOptions::default()
                },
            );
            context.copy_pages(&source, &page_ids)?
        };
        writer.extend_pages(copied_pages);
    }

    writer.finish()
}

fn temp_output_path(output: &Path) -> Result<PathBuf> {
    let directory = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pdq-output");
    for attempt in 0..1000 {
        let candidate = directory.join(format!(".{file_name}.pdq-{}-{attempt}.tmp", process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {
                fs::remove_file(&candidate)?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(PdfOpsError::InvalidStructure(format!(
        "could not allocate temporary output next to {}",
        output.display()
    )))
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

#[cfg(unix)]
fn same_file(left: &Path, right: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = fs::metadata(left)?;
    let right = match fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

// Windows has no stable dev/ino equivalent. Canonical paths cover self and
// symlink cases; hard links to the same file slip through, where fs::copy
// then fails with a sharing violation instead of corrupting the input.
#[cfg(not(unix))]
fn same_file(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::canonicalize(left)?;
    let right = match fs::canonicalize(right) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(left == right)
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
