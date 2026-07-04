use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process,
};

use crate::{
    copy::{copy_pages_with_options, CopyOptions},
    lazy::PdfSource,
    load::map_file,
    range::{PageRangeError, PageRangeGroup},
    repair::{is_offset_damage, with_repair_retry},
    split::{empty_document, finish_pages},
    write::{StreamingCopyContext, StreamingPdfWriter},
    PdfOpsError, Result,
};

#[derive(Debug, Clone)]
pub struct MergeInput {
    pub path: PathBuf,
    pub ranges: Vec<PageRangeGroup>,
}

#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    pub preserve_whole_single_input: bool,
    /// Password used to decrypt encrypted inputs. The empty user password is
    /// always tried first, so owner-password-only files merge without this.
    /// Outputs are always written unencrypted.
    pub password: Option<String>,
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
    let password = options.password.as_deref();

    if options.preserve_whole_single_input {
        if let [input] = inputs {
            if input.ranges.is_empty() && !same_file(&input.path, output)? {
                return copy_whole_input(input, output, password);
            }
        }
    }

    if inputs.iter().all(|input| input.ranges.is_empty()) {
        return merge_whole_inputs_streaming(inputs, output, password);
    }

    // A fetch error that proves an input's xref lies restarts the merge with
    // that input force-repaired (the in-progress target document is tainted
    // by the failed copy). Each restart adds one input to the repair set, so
    // the loop is bounded by the input count.
    let mut force_repair: BTreeSet<usize> = BTreeSet::new();
    'attempt: loop {
        let mut target = empty_document();
        let mut merged_pages = Vec::new();

        for (index, input) in inputs.iter().enumerate() {
            let mmap = map_file(&input.path)?;
            let source =
                open_merge_source(&mmap, &input.path, password, force_repair.contains(&index))?;
            let copied = (|| {
                let pages = source.page_ids()?;
                let page_ids = resolve_merge_page_ids(&pages, input)?;
                copy_pages_with_options(
                    &source,
                    &mut target,
                    &page_ids,
                    CopyOptions {
                        prune_resources: !input.ranges.is_empty(),
                        ..CopyOptions::default()
                    },
                )
            })();
            match copied {
                Ok(pages) => merged_pages.extend(pages),
                Err(err) if is_offset_damage(&err) && !source.repaired() => {
                    force_repair.insert(index);
                    continue 'attempt;
                }
                Err(err) => return Err(err),
            }
        }

        finish_pages(&mut target, &merged_pages)?;
        target.save(output)?;
        return Ok(());
    }
}

/// Open one merge input, forcing xref reconstruction for inputs a previous
/// attempt proved damaged. A forced reconstruction that fails is a hard
/// error: the damage is already established, there is nothing to fall back
/// to.
fn open_merge_source<'a>(
    buffer: &'a [u8],
    path: &Path,
    password: Option<&str>,
    force_repair: bool,
) -> Result<PdfSource<'a>> {
    if force_repair {
        PdfSource::open_repaired(buffer, path).ok_or_else(|| {
            PdfOpsError::InvalidStructure(format!(
                "{}: damaged cross-reference data; automatic repair failed",
                path.display()
            ))
        })
    } else {
        PdfSource::open(buffer, path, password)
    }
}

pub(crate) fn merge_whole_inputs_streaming(
    inputs: &[MergeInput],
    output: &Path,
    password: Option<&str>,
) -> Result<()> {
    // Same restart discipline as the ranged merge above: a lying xref aborts
    // the streaming write (its temp file is discarded) and the whole merge
    // re-runs with the failing input force-repaired.
    let mut force_repair: BTreeSet<usize> = BTreeSet::new();
    loop {
        let mut failed_index = None;
        let result = write_streaming_output(output, |writer| {
            for (index, input) in inputs.iter().enumerate() {
                let mmap = map_file(&input.path)?;
                let source =
                    open_merge_source(&mmap, &input.path, password, force_repair.contains(&index))?;
                let appended = (|| {
                    let page_ids = source.page_ids()?;
                    if page_ids.is_empty() {
                        return Err(PdfOpsError::Range(PageRangeError::NoPages));
                    }
                    append_whole_source(writer, &source, &page_ids)
                })();
                if let Err(err) = appended {
                    if !source.repaired() {
                        failed_index = Some(index);
                    }
                    return Err(err);
                }
            }
            Ok(())
        });
        match result {
            Err(err) if is_offset_damage(&err) => match failed_index {
                Some(index) if force_repair.insert(index) => continue,
                _ => return Err(err),
            },
            result => return result,
        }
    }
}

/// Write a streaming merge output atomically: build it in a temporary file
/// next to `output` and rename it into place on success.
fn write_streaming_output(
    output: &Path,
    fill: impl FnOnce(&mut StreamingPdfWriter) -> Result<()>,
) -> Result<()> {
    let temp_output = temp_output_path(output)?;
    let result = (|| {
        let mut writer = StreamingPdfWriter::create(&temp_output)?;
        fill(&mut writer)?;
        writer.finish()
    })();
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

fn append_whole_source(
    writer: &mut StreamingPdfWriter,
    source: &PdfSource<'_>,
    page_ids: &[lopdf::ObjectId],
) -> Result<()> {
    let copied_pages = {
        let mut context = StreamingCopyContext::new(
            writer,
            CopyOptions {
                prune_resources: false,
                ..CopyOptions::default()
            },
        );
        context.copy_pages(source, page_ids)?
    };
    writer.extend_pages(copied_pages);
    Ok(())
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

/// Fast path for a single whole input: byte-copy it to `output`. Encrypted
/// inputs cannot be byte-copied because outputs must always be unencrypted,
/// and repaired inputs cannot either — a byte copy would faithfully preserve
/// the damaged xref the repair just worked around — so both are rewritten
/// through the streaming writer instead.
fn copy_whole_input(input: &MergeInput, output: &Path, password: Option<&str>) -> Result<()> {
    let mmap = map_file(&input.path)?;
    with_repair_retry(&mmap, &input.path, password, |source| {
        let page_ids = source.page_ids()?;
        if page_ids.is_empty() {
            return Err(PdfOpsError::Range(PageRangeError::NoPages));
        }
        if source.was_encrypted() || source.repaired() {
            return write_streaming_output(output, |writer| {
                append_whole_source(writer, source, &page_ids)
            });
        }
        fs::copy(&input.path, output)?;
        Ok(())
    })
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
