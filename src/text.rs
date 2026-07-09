use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use hayro::hayro_interpret::{
    font::Glyph,
    hayro_cmap::BfString,
    hayro_syntax::{page::Page, LoadPdfError, Pdf},
    interpret_page, BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, InterpreterWarning, Paint, PathDrawMode, SoftMask, TransformExt,
};
use kurbo::{Affine, BezPath, Point, Rect, Vec2};

use crate::{
    range::{dedupe_preserving_order, PageRangeGroup},
    PdfOpsError, Result,
};

/// Emitted `y` is the approximate glyph top: baseline minus this fraction of
/// the on-page em size (pdf.js's text-layer fallback ascent).
const ASCENT_FACTOR: f64 = 0.8;
/// hayro glyph transforms take font units (thousandths of an em) as input.
const FONT_UNITS_PER_EM: f64 = 1000.0;
/// Gap past a glyph's advance (in em) that reads as a word space rather than
/// kerning or tracking, synthesized as ' ' for PDFs that encode word gaps as
/// TJ offsets instead of space glyphs (LaTeX). Poppler's `minWordBreakSpace`
/// and pdf.js's `SPACE_IN_FLOW_MIN_FACTOR` both sit near 0.1 em; kerning
/// pairs stay well below it, word spaces (~0.2-0.5 em) well above.
const WORD_GAP_MIN: f64 = 0.1;
/// Widest gap still treated as an in-flow word space (pdf.js's
/// `SPACE_IN_FLOW_MAX_FACTOR`); anything wider starts a new run.
const WORD_GAP_MAX: f64 = 0.6;

#[derive(Debug, Clone, Default)]
pub struct ExtractTextOptions {
    pub pages: Option<PageRangeGroup>,
    pub password: Option<String>,
}

/// A horizontal (or uniformly-oriented) sequence of glyphs positioned on the
/// page. Coordinates are PDF points at 72 dpi with a top-left origin, after
/// the same rotation and cropbox handling `render` applies. `x`/`y` is the
/// top-left of the run's axis-aligned bounding box; for horizontal text
/// `width` is the sum of glyph advances and `height` equals `font_size`,
/// while rotated/vertical text yields a narrow, tall box.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageText {
    pub page: usize,
    pub width: f32,
    pub height: f32,
    /// True when at least one glyph on the page could not be mapped to
    /// Unicode (emitted as U+FFFD) or the interpreter reported a font it
    /// could not decode. Distinguishes "extraction failed" from "no text".
    pub degraded: bool,
    pub runs: Vec<TextRun>,
}

pub fn extract_text(input: &Path, options: &ExtractTextOptions) -> Result<Vec<PageText>> {
    let data = fs::read(input)?;
    let pdf = load_pdf(data, options.password.as_deref(), input)?;
    let pages = pdf.pages();

    let selected = match &options.pages {
        Some(range) => dedupe_preserving_order(&range.resolve(pages.len())?),
        None => (1..=pages.len()).collect(),
    };

    let cache = InterpreterCache::new();
    Ok(selected
        .iter()
        .map(|&page_number| extract_page(&pages[page_number - 1], page_number, &cache))
        .collect())
}

fn load_pdf(data: Vec<u8>, password: Option<&str>, input: &Path) -> Result<Pdf> {
    let result = match password {
        Some(password) => Pdf::new_with_password(data, password),
        None => Pdf::new(data),
    };
    result.map_err(|err| match err {
        LoadPdfError::Decryption(_) => PdfOpsError::Password(match password {
            Some(_) => format!("invalid password for {}", input.display()),
            None => format!(
                "{} is encrypted; supply --password to decrypt it",
                input.display()
            ),
        }),
        LoadPdfError::Invalid => PdfOpsError::InvalidStructure(format!(
            "failed to parse PDF for text extraction: {}",
            input.display()
        )),
    })
}

fn extract_page<'a>(page: &Page<'a>, page_number: usize, cache: &InterpreterCache<'a>) -> PageText {
    let (width, height) = page.render_dimensions();
    let initial_transform = page.initial_transform(true).to_kurbo();

    let font_warning = Arc::new(AtomicBool::new(false));
    let sink_flag = font_warning.clone();
    let settings = InterpreterSettings {
        warning_sink: Arc::new(move |warning| {
            if matches!(warning, InterpreterWarning::UnsupportedFont) {
                sink_flag.store(true, Ordering::Relaxed);
            }
        }),
        // The text layer covers page content only; annotation appearance
        // streams get their own layer in viewers (pdf.js does the same).
        render_annotations: false,
        ..Default::default()
    };

    let mut context = Context::new(
        initial_transform,
        Rect::new(0.0, 0.0, width as f64, height as f64),
        cache,
        page.xref(),
        settings,
    );
    let mut device = TextDevice::default();
    interpret_page(page, &mut context, &mut device);

    let (runs, missing_unicode) = device.finish();
    PageText {
        page: page_number,
        width,
        height,
        degraded: missing_unicode || font_warning.load(Ordering::Relaxed),
        runs,
    }
}

struct RunState {
    text: String,
    /// Baseline origin of the first glyph.
    start: Point,
    /// Baseline point past the last glyph's advance.
    end: Point,
    font_size: f64,
    /// Unit vector pointing from the baseline toward the glyph top.
    up: Vec2,
    /// Unit vector along the advance direction.
    dir: Vec2,
    /// Where the next glyph's origin should land if the text continues
    /// uninterrupted; `None` when the glyph's advance width is unknown.
    expected: Option<Point>,
    last_origin: Point,
}

#[derive(Default)]
struct TextDevice {
    runs: Vec<TextRun>,
    current: Option<RunState>,
    missing_unicode: bool,
    last_fill_origin: Option<Point>,
}

impl TextDevice {
    fn finish(&mut self) -> (Vec<TextRun>, bool) {
        self.flush();
        (std::mem::take(&mut self.runs), self.missing_unicode)
    }

    fn flush(&mut self) {
        if let Some(run) = self.current.take() {
            if !run.text.trim().is_empty() {
                let ascent = run.up * (ASCENT_FACTOR * run.font_size);
                let descent = run.up * ((ASCENT_FACTOR - 1.0) * run.font_size);
                let corners = [
                    run.start + ascent,
                    run.start + descent,
                    run.end + ascent,
                    run.end + descent,
                ];
                let (mut min, mut max) = (corners[0], corners[0]);
                for corner in &corners[1..] {
                    min.x = min.x.min(corner.x);
                    min.y = min.y.min(corner.y);
                    max.x = max.x.max(corner.x);
                    max.y = max.y.max(corner.y);
                }
                self.runs.push(TextRun {
                    text: run.text,
                    x: min.x as f32,
                    y: min.y as f32,
                    width: (max.x - min.x) as f32,
                    height: (max.y - min.y) as f32,
                    font_size: run.font_size as f32,
                });
            }
        }
    }

    /// Whether the glyph at `origin` continues the current run; `Some(true)`
    /// means it continues after a word-sized gap (synthesize a space).
    fn continues_current(
        &self,
        origin: Point,
        font_size: f64,
        up: Vec2,
        dir: Vec2,
    ) -> Option<bool> {
        let current = self.current.as_ref()?;
        if (font_size - current.font_size).abs() > 0.02 * current.font_size
            || current.dir.dot(dir) < 0.99
            || current.up.dot(up) < 0.99
        {
            return None;
        }
        let (reference, gap_range, has_advance) = match current.expected {
            Some(expected) => (expected, (-0.25, WORD_GAP_MAX), true),
            // Without advance metrics (Type3 fonts), measure from the
            // previous origin and accept anything up to a wide glyph plus
            // a word gap; the glyph width is indistinguishable from the
            // gap, so no space can be synthesized.
            None => (current.last_origin, (-0.05, 1.6), false),
        };
        let delta = origin - reference;
        let along = delta.dot(current.dir) / current.font_size;
        let perpendicular = delta.dot(current.up) / current.font_size;
        if perpendicular.abs() > 0.15 || along < gap_range.0 || along > gap_range.1 {
            return None;
        }
        Some(has_advance && along >= WORD_GAP_MIN)
    }
}

impl<'a> Device<'a> for TextDevice {
    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        _paint: &Paint<'a>,
        draw_mode: &GlyphDrawMode,
    ) {
        let to_device = transform * glyph_transform;
        let origin = to_device * Point::ZERO;
        let em_x = to_device * Point::new(FONT_UNITS_PER_EM, 0.0) - origin;
        let em_y = to_device * Point::new(0.0, FONT_UNITS_PER_EM) - origin;
        let font_size = em_y.hypot();
        if !origin.x.is_finite()
            || !origin.y.is_finite()
            || !font_size.is_finite()
            || font_size <= 0.0
            || em_x.hypot() <= 0.0
        {
            return;
        }

        // FillStroke rendering modes draw the same glyph twice; keep only
        // the fill pass. Stroke-only text still comes through because no
        // fill preceded it at the same origin.
        if matches!(draw_mode, GlyphDrawMode::Stroke(_)) {
            if self.last_fill_origin == Some(origin) {
                return;
            }
        } else {
            self.last_fill_origin = Some(origin);
        }

        let text = match glyph.as_unicode() {
            Some(BfString::Char(c)) => c.to_string(),
            Some(BfString::String(s)) => s,
            None => {
                self.missing_unicode = true;
                '\u{FFFD}'.to_string()
            }
        };

        let up = em_y / font_size;
        let dir = em_x.normalize();
        let advance = match glyph {
            Glyph::Outline(outline) => outline
                .advance_width()
                .map(|w| to_device * Point::new(w as f64, 0.0) - origin),
            Glyph::Type3(_) => None,
        };
        let expected = advance.map(|a| origin + a);
        // Without an advance, estimate the glyph's extent as half an em so
        // the box still covers most of the final glyph.
        let end = expected.unwrap_or(origin + dir * (0.5 * font_size));

        if let Some(word_gap) = self.continues_current(origin, font_size, up, dir) {
            let current = self.current.as_mut().unwrap();
            if word_gap
                && !current.text.ends_with(char::is_whitespace)
                && !text.starts_with(char::is_whitespace)
            {
                current.text.push(' ');
            }
            current.text.push_str(&text);
            current.last_origin = origin;
            current.expected = expected;
            current.end = end;
        } else {
            self.flush();
            self.current = Some(RunState {
                text,
                start: origin,
                end,
                font_size,
                up,
                dir,
                expected,
                last_origin: origin,
            });
        }
    }

    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Serialize pages as the `pdq text` stdout JSON (an array of page objects).
pub fn pages_to_json(pages: &[PageText]) -> String {
    let mut out = String::from("[");
    for (i, page) in pages.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"page\":{},\"page_width\":{},\"page_height\":{},\"degraded\":{},\"runs\":[",
            page.page,
            format_number(page.width),
            format_number(page.height),
            page.degraded
        ));
        for (j, run) in page.runs.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"text\":\"{}\",\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"font_size\":{}}}",
                escape_json(&run.text),
                format_number(run.x),
                format_number(run.y),
                format_number(run.width),
                format_number(run.height),
                format_number(run.font_size)
            ));
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// Numbers rounded to 1/1000 pt; f64 Display keeps them free of float noise.
fn format_number(value: f32) -> String {
    let rounded = (value as f64 * 1000.0).round() / 1000.0;
    if rounded.is_finite() {
        format!("{rounded}")
    } else {
        "0".to_string()
    }
}

fn escape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_control_and_quote_characters() {
        assert_eq!(escape_json("a\"b\\c\nd\u{1}e"), "a\\\"b\\\\c\\nd\\u0001e");
    }

    #[test]
    fn numbers_render_without_float_noise() {
        assert_eq!(format_number(72.0), "72");
        assert_eq!(format_number(57.599_996), "57.6");
        assert_eq!(format_number(f32::NAN), "0");
    }

    #[test]
    fn json_shape_matches_schema() {
        let pages = vec![PageText {
            page: 1,
            width: 612.0,
            height: 792.0,
            degraded: false,
            runs: vec![TextRun {
                text: "Invoice".to_string(),
                x: 72.0,
                y: 70.5,
                width: 57.0,
                height: 18.0,
                font_size: 18.0,
            }],
        }];
        assert_eq!(
            pages_to_json(&pages),
            "[{\"page\":1,\"page_width\":612,\"page_height\":792,\"degraded\":false,\
             \"runs\":[{\"text\":\"Invoice\",\"x\":72,\"y\":70.5,\
             \"width\":57,\"height\":18,\"font_size\":18}]}]"
        );
    }
}
