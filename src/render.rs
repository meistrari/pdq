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

pub fn render_pages(input: &Path, output_pattern: &str, options: &RenderOptions) -> Result<()> {
    validate_output_pattern(output_pattern)?;
    if !options.dpi.is_finite() || options.dpi <= 0.0 {
        return Err(PdfOpsError::InvalidStructure(format!(
            "render dpi must be a positive number, got {}",
            options.dpi
        )));
    }
    let scale = options.dpi / POINTS_PER_INCH;

    let data = fs::read(input)?;
    let pdf = Pdf::new(data).map_err(|err| load_error(err, input))?;
    let pages = pdf.pages();

    let selected = match &options.pages {
        Some(range) => dedupe_preserving_order(&range.resolve(pages.len())?),
        None => (1..=pages.len()).collect(),
    };
    let width = pages.len().to_string().len();

    let interpreter_settings = InterpreterSettings::default();
    let render_settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: WHITE,
        ..Default::default()
    };

    selected.par_iter().try_for_each(|&page_number| {
        let page = &pages[page_number - 1];
        let (page_width, page_height) = page.render_dimensions();
        if page_width * scale > MAX_PIXMAP_DIMENSION || page_height * scale > MAX_PIXMAP_DIMENSION {
            return Err(PdfOpsError::Unsupported(format!(
                "page {page_number} is too large to render at {} dpi",
                options.dpi
            )));
        }

        // RenderCache is Rc-based and cannot cross threads; it only shares
        // work across pages, so a per-page cache costs almost nothing.
        let cache = RenderCache::new();
        let pixmap = hayro::render(page, &cache, &interpreter_settings, &render_settings);
        let png = pixmap.into_png().map_err(|err| {
            PdfOpsError::InvalidStructure(format!(
                "failed to encode page {page_number} as PNG: {err}"
            ))
        })?;
        fs::write(
            render_output_pattern(output_pattern, page_number, width)?,
            png,
        )?;
        Ok(())
    })
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
