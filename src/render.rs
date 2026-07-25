use std::{fs, path::Path};

use hayro::{
    hayro_interpret::InterpreterSettings,
    hayro_syntax::{LoadPdfError, Pdf},
    vello_cpu::color::palette::css::WHITE,
    RenderCache, RenderSettings,
};
use rayon::prelude::*;

use crate::{
    range::{dedupe_preserving_order, PageRangeGroup},
    split::{render_output_pattern, validate_output_pattern},
    PdfOpsError, Result,
};

const POINTS_PER_INCH: f32 = 72.0;
// hayro pixmaps address pixels with u16 coordinates.
const MAX_PIXMAP_DIMENSION: f32 = u16::MAX as f32;
const MEMORY_INPUT_LABEL: &str = "<memory>";

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub dpi: f32,
    pub pages: Option<PageRangeGroup>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            dpi: 150.0,
            pages: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedPage {
    pub page: usize,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

pub fn render_pages(input: &Path, output_pattern: &str, options: &RenderOptions) -> Result<()> {
    validate_output_pattern(output_pattern)?;
    let data = fs::read(input)?;
    let plan = RenderPlan::new(data, options, input)?;

    let render_one = |&page_number: &usize| -> Result<()> {
        let rendered = plan.render_page(page_number)?;
        fs::write(
            render_output_pattern(output_pattern, rendered.page, plan.pad_width)?,
            rendered.png,
        )?;
        Ok(())
    };

    match rayon::ThreadPoolBuilder::new().build() {
        Ok(pool) => pool.install(|| plan.selected.par_iter().try_for_each(render_one)),
        Err(_) => plan.selected.iter().try_for_each(render_one),
    }
}

/// Like [`render_pages`], but takes an in-memory PDF and returns the
/// rendered pages' PNG bytes instead of writing them to disk.
pub fn render_pages_from_bytes(input: &[u8], options: &RenderOptions) -> Result<Vec<RenderedPage>> {
    let plan = RenderPlan::new(input.to_vec(), options, Path::new(MEMORY_INPUT_LABEL))?;

    match rayon::ThreadPoolBuilder::new().build() {
        Ok(pool) => pool.install(|| {
            plan.selected
                .par_iter()
                .map(|&page_number| plan.render_page(page_number))
                .collect()
        }),
        Err(_) => plan
            .selected
            .iter()
            .map(|&page_number| plan.render_page(page_number))
            .collect(),
    }
}

struct RenderPlan {
    pdf: Pdf,
    selected: Vec<usize>,
    pad_width: usize,
    scale: f32,
    dpi: f32,
    interpreter_settings: InterpreterSettings,
    render_settings: RenderSettings,
}

impl RenderPlan {
    fn new(input: Vec<u8>, options: &RenderOptions, label: &Path) -> Result<Self> {
        if !options.dpi.is_finite() || options.dpi <= 0.0 {
            return Err(PdfOpsError::InvalidStructure(format!(
                "render dpi must be a positive number, got {}",
                options.dpi
            )));
        }
        let scale = options.dpi / POINTS_PER_INCH;

        let pdf = Pdf::new(input).map_err(|err| load_error(err, label))?;
        let page_count = pdf.pages().len();

        let selected = match &options.pages {
            Some(range) => dedupe_preserving_order(&range.resolve(page_count)?),
            None => (1..=page_count).collect(),
        };
        let pad_width = page_count.to_string().len();

        let interpreter_settings = InterpreterSettings::default();
        let render_settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
            ..Default::default()
        };

        Ok(Self {
            pdf,
            selected,
            pad_width,
            scale,
            dpi: options.dpi,
            interpreter_settings,
            render_settings,
        })
    }

    fn render_page(&self, page_number: usize) -> Result<RenderedPage> {
        let pages = self.pdf.pages();
        let page = &pages[page_number - 1];
        let (page_width, page_height) = page.render_dimensions();
        if page_width * self.scale > MAX_PIXMAP_DIMENSION
            || page_height * self.scale > MAX_PIXMAP_DIMENSION
        {
            return Err(PdfOpsError::Unsupported(format!(
                "page {page_number} is too large to render at {} dpi",
                self.dpi
            )));
        }

        // RenderCache is Rc-based and cannot cross threads; it only shares
        // work across pages, so a per-page cache costs almost nothing.
        let cache = RenderCache::new();
        let pixmap = hayro::render(
            page,
            &cache,
            &self.interpreter_settings,
            &self.render_settings,
        );
        let width = pixmap.width() as u32;
        let height = pixmap.height() as u32;
        let png = pixmap.into_png().map_err(|err| {
            PdfOpsError::InvalidStructure(format!(
                "failed to encode page {page_number} as PNG: {err}"
            ))
        })?;
        Ok(RenderedPage {
            page: page_number,
            width,
            height,
            png,
        })
    }
}

fn load_error(err: LoadPdfError, input: &Path) -> PdfOpsError {
    match err {
        LoadPdfError::Decryption(_) => PdfOpsError::Unsupported(format!(
            "encrypted PDFs are not supported: {}",
            input.display()
        )),
        LoadPdfError::Invalid => PdfOpsError::InvalidStructure(format!(
            "failed to parse PDF for rendering: {}",
            input.display()
        )),
    }
}
