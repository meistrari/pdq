pub mod copy;
pub mod count;
pub mod lazy;
pub mod load;
pub mod merge;
pub mod range;
mod scan;
pub mod split;
mod write;
mod xrefboot;

pub use copy::{CopyContext, CopyOptions};
pub use count::{page_count, page_count_fast};
pub use merge::{merge, merge_with_options, MergeInput, MergeOptions};
pub use range::{PageRangeError, PageRangeGroup};
pub use split::{split, split_pages, SplitOutput};

pub type Result<T> = std::result::Result<T, PdfOpsError>;

#[derive(Debug, thiserror::Error)]
pub enum PdfOpsError {
    #[error("{0}")]
    Range(#[from] PageRangeError),

    #[error("pdf error: {0}")]
    Pdf(#[from] lopdf::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported PDF feature: {0}")]
    Unsupported(String),

    #[error("invalid PDF structure: {0}")]
    InvalidStructure(String),
}
