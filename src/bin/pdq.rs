use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use pdq::{
    merge_with_options, page_count_fast_with_password, page_count_with_password,
    page_dimensions_with_password, split_pages_with_options, split_with_password, MergeInput,
    MergeOptions, PageRangeGroup, SplitOutput, SplitPagesOptions,
};

#[derive(Debug, Parser)]
#[command(name = "pdq")]
#[command(about = "Rust-native PDF split and merge MVP")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Split(SplitArgs),
    SplitPages(SplitPagesArgs),
    Merge(MergeArgs),
    /// Print the number of pages (trusts the root /Count like qpdf; --strict walks the page tree)
    PageCount(PageCountArgs),
    /// Print each page's size in PDF points and rotation as JSON, without rendering
    Dimensions(DimensionsArgs),
    #[cfg(feature = "render")]
    Render(RenderArgs),
    /// Extract positioned text runs as JSON (points at 72 dpi, top-left origin)
    #[cfg(feature = "text")]
    Text(TextArgs),
}

#[derive(Debug, Args)]
struct SplitArgs {
    input: PathBuf,

    #[arg(long = "out", required = true, value_names = ["RANGE", "PATH"], num_args = 2)]
    outputs: Vec<String>,

    /// Password for encrypted inputs; outputs are always written decrypted
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[derive(Debug, Args)]
struct MergeArgs {
    #[arg(short, long)]
    output: PathBuf,

    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Password for encrypted inputs; outputs are always written decrypted
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[derive(Debug, Args)]
struct SplitPagesArgs {
    input: PathBuf,

    #[arg(short, long, value_name = "PATTERN")]
    output: String,

    /// Maximum number of pages per output file (%d becomes the chunk index)
    #[arg(
        long,
        value_name = "N",
        default_value_t = 1,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pages_per_file: u64,

    /// Password for encrypted inputs; outputs are always written decrypted
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[derive(Debug, Args)]
struct PageCountArgs {
    input: PathBuf,

    /// Validate the count by walking every page-tree node instead of trusting
    /// the root /Count (slower, but immune to lying metadata; a missing or
    /// implausible /Count already falls back to this walk automatically)
    #[arg(long)]
    strict: bool,

    /// Password for encrypted inputs
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[derive(Debug, Args)]
struct DimensionsArgs {
    input: PathBuf,

    /// Password for encrypted inputs
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[cfg(feature = "render")]
#[derive(Debug, Args)]
struct RenderArgs {
    input: PathBuf,

    #[arg(short, long, value_name = "PATTERN")]
    output: String,

    #[arg(long, default_value_t = 150.0)]
    dpi: f32,

    #[arg(long, value_name = "RANGES")]
    pages: Option<String>,
}

#[cfg(feature = "text")]
#[derive(Debug, Args)]
struct TextArgs {
    input: PathBuf,

    /// Page ranges to extract (same syntax as render); all pages when omitted
    #[arg(long, value_name = "RANGES")]
    pages: Option<String>,

    /// Password for encrypted inputs
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,
}

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> ExitCode {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    if let Err(err) = run() {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Split(args) => {
            let outputs = parse_split_outputs(args.outputs)?;
            split_with_password(&args.input, &outputs, args.password.as_deref())?;
        }
        Command::SplitPages(args) => {
            split_pages_with_options(
                &args.input,
                &args.output,
                &SplitPagesOptions {
                    pages_per_file: args.pages_per_file as usize,
                    password: args.password,
                },
            )?;
        }
        Command::Merge(args) => {
            let inputs = parse_merge_inputs(args.inputs);
            merge_with_options(
                &inputs,
                &args.output,
                MergeOptions {
                    preserve_whole_single_input: true,
                    password: args.password,
                },
            )?;
        }
        Command::PageCount(args) => {
            let count = if args.strict {
                page_count_with_password(&args.input, args.password.as_deref())?
            } else {
                page_count_fast_with_password(&args.input, args.password.as_deref())?
            };
            println!("{count}");
        }
        Command::Dimensions(args) => {
            let pages = page_dimensions_with_password(&args.input, args.password.as_deref())?;
            println!("{}", dimensions_json(&pages));
        }
        #[cfg(feature = "render")]
        Command::Render(args) => {
            let options = pdq::RenderOptions {
                dpi: args.dpi,
                pages: args.pages.map(PageRangeGroup::parse).transpose()?,
            };
            pdq::render_pages(&args.input, &args.output, &options)?;
        }
        #[cfg(feature = "text")]
        Command::Text(args) => {
            let options = pdq::ExtractTextOptions {
                pages: args.pages.map(PageRangeGroup::parse).transpose()?,
                password: args.password,
            };
            let pages = pdq::extract_text(&args.input, &options)?;
            println!("{}", pdq::text::pages_to_json(&pages));
        }
    }
    Ok(())
}

fn parse_split_outputs(
    values: Vec<String>,
) -> Result<Vec<SplitOutput>, Box<dyn std::error::Error>> {
    let mut outputs = Vec::new();
    for pair in values.chunks_exact(2) {
        outputs.push(SplitOutput {
            range: PageRangeGroup::parse(pair[0].clone())?,
            path: PathBuf::from(&pair[1]),
        });
    }
    Ok(outputs)
}

fn parse_merge_inputs(paths: Vec<PathBuf>) -> Vec<MergeInput> {
    paths.into_iter().map(MergeInput::all).collect()
}

fn dimensions_json(pages: &[pdq::PageDimensions]) -> String {
    use std::fmt::Write;

    let mut json = format!("{{\"pages\":{},\"page_sizes\":[", pages.len());
    for (index, page) in pages.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        write!(
            json,
            "{{\"width\":{},\"height\":{},\"rotation\":{}}}",
            page.width, page.height, page.rotation
        )
        .expect("writing to a String cannot fail");
    }
    json.push_str("]}");
    json
}
