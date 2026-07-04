use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};
use pdq::{
    merge_with_options, page_count, page_count_fast, split, split_pages_with_options, MergeInput,
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
    #[cfg(feature = "render")]
    Render(RenderArgs),
}

#[derive(Debug, Args)]
struct SplitArgs {
    input: PathBuf,

    #[arg(long = "out", required = true, value_names = ["RANGE", "PATH"], num_args = 2)]
    outputs: Vec<String>,
}

#[derive(Debug, Args)]
struct MergeArgs {
    #[arg(short, long)]
    output: PathBuf,

    #[arg(required = true)]
    inputs: Vec<PathBuf>,
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
}

#[derive(Debug, Args)]
struct PageCountArgs {
    input: PathBuf,

    /// Validate the count by walking every page-tree node instead of trusting
    /// the root /Count (slower, but immune to lying metadata; a missing or
    /// implausible /Count already falls back to this walk automatically)
    #[arg(long)]
    strict: bool,
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

fn main() -> ExitCode {
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
            split(&args.input, &outputs)?;
        }
        Command::SplitPages(args) => {
            split_pages_with_options(
                &args.input,
                &args.output,
                &SplitPagesOptions {
                    pages_per_file: args.pages_per_file as usize,
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
                },
            )?;
        }
        Command::PageCount(args) => {
            let count = if args.strict {
                page_count(&args.input)?
            } else {
                page_count_fast(&args.input)?
            };
            println!("{count}");
        }
        #[cfg(feature = "render")]
        Command::Render(args) => {
            let options = pdq::RenderOptions {
                dpi: args.dpi,
                pages: args.pages.map(PageRangeGroup::parse).transpose()?,
            };
            pdq::render_pages(&args.input, &args.output, &options)?;
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
