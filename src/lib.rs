pub mod copy;
pub mod count;
mod filter;
pub mod lazy;
pub mod load;
pub mod merge;
pub mod range;
#[cfg(feature = "render")]
pub mod render;
mod repair;
mod scan;
pub mod split;
mod split_template;
#[cfg(feature = "text")]
pub mod text;
mod write;
mod xrefboot;

pub use copy::{CopyContext, CopyOptions};
pub use count::{
    page_count, page_count_fast, page_count_fast_with_password, page_count_with_password,
};
pub use merge::{merge, merge_with_options, MergeInput, MergeOptions};
pub use range::{PageRangeError, PageRangeGroup};
#[cfg(feature = "render")]
pub use render::{render_pages, RenderOptions};
pub use split::{
    split, split_pages, split_pages_with_options, split_pages_with_password, split_with_password,
    SplitOutput, SplitPagesOptions,
};
#[cfg(feature = "text")]
pub use text::{extract_text, ExtractTextOptions, PageText, TextRun};

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

    /// An encrypted input could not be decrypted: either it requires a
    /// password that was not supplied, or the supplied password is wrong.
    #[error("{0}")]
    Password(String),

    #[error("invalid PDF structure: {0}")]
    InvalidStructure(String),
}
