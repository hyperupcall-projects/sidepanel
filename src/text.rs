//! Software text rendering for the panel.
//!
//! One system font is loaded with `fontdue` and rasterised per glyph directly
//! into the ARGB canvas. No font is bundled: common paths are tried, then
//! `fc-match` as a fallback.

use fontdue::{Font, FontSettings, LineMetrics};

/// Common sans-serif fonts, most preferred first.
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
    "/usr/share/fonts/truetype/lato/Lato-Regular.ttf",
    "/usr/share/fonts/fonts-go/Go-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
];

pub struct TextRenderer {
    font: Font,
}

impl TextRenderer {
    /// Load the first usable font; `None` renders the panel without text.
    pub fn load() -> Option<Self> {
        for path in CANDIDATES {
            if let Ok(data) = std::fs::read(path) {
                match Font::from_bytes(data, FontSettings::default()) {
                    Ok(font) => {
                        log::info!("panel font: {path}");
                        return Some(Self { font });
                    }
                    Err(err) => log::debug!("font {path} unusable: {err}"),
                }
            }
        }
        // Last resort: let fontconfig pick something.
        let output = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", "sans"])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Ok(data) = std::fs::read(&path) {
                    if let Ok(font) = Font::from_bytes(data, FontSettings::default()) {
                        log::info!("panel font: {path} (fc-match)");
                        return Some(Self { font });
                    }
                }
            }
        }
        log::warn!("no usable font found; the panel will render without text");
        None
    }

    fn line_metrics(&self, px: f32) -> Option<LineMetrics> {
        self.font.horizontal_line_metrics(px)
    }

    /// Distance from the top of a line to its baseline.
    pub fn ascent(&self, px: f32) -> f32 {
        self.line_metrics(px).map_or(px * 0.8, |m| m.ascent)
    }

    /// Total line height at `px`.
    pub fn line_height(&self, px: f32) -> f32 {
        self.line_metrics(px).map_or(px * 1.2, |m| m.new_line_size)
    }

    /// Width of `text` when rendered at `px`.
    pub fn measure(&self, text: &str, px: f32) -> f32 {
        text.chars()
            .map(|ch| self.font.metrics(ch, px).advance_width)
            .sum()
    }

    /// Truncate `text` with an ellipsis so it fits within `max_w`.
    pub fn ellipsize(&self, text: &str, px: f32, max_w: f32) -> String {
        if max_w <= 0.0 {
            return String::new();
        }
        if self.measure(text, px) <= max_w {
            return text.to_string();
        }
        let ellipsis = self.measure("\u{2026}", px);
        let mut out = String::new();
        let mut width = 0.0;
        for ch in text.chars() {
            let advance = self.font.metrics(ch, px).advance_width;
            if width + advance + ellipsis > max_w {
                break;
            }
            out.push(ch);
            width += advance;
        }
        out.push('\u{2026}');
        out
    }

    /// Draw `text` starting at `x`, with the baseline at `baseline`, clipped
    /// to the canvas. `color` is opaque ARGB.
    pub fn draw(
        &self,
        canvas: &mut [u8],
        width: u32,
        stride: usize,
        x: i32,
        baseline: i32,
        px: f32,
        text: &str,
        color: u32,
    ) {
        let r = ((color >> 16) & 0xff) as u8;
        let g = ((color >> 8) & 0xff) as u8;
        let b = (color & 0xff) as u8;
        let mut pen = x as f32;
        for ch in text.chars() {
            let (m, coverage) = self.font.rasterize(ch, px);
            // `m.ymin` is the bitmap's bottom edge relative to the baseline
            // (positive above), so the top is baseline - ymin - height.
            let gx = pen.round() as i32 + m.xmin;
            let gy = baseline - m.ymin - m.height as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = coverage[row * m.width + col];
                    if cov == 0 {
                        continue;
                    }
                    blend(
                        canvas,
                        width,
                        stride,
                        gx + col as i32,
                        gy + row as i32,
                        r,
                        g,
                        b,
                        cov,
                    );
                }
            }
            pen += m.advance_width;
        }
    }
}

/// Blend one straight-alpha pixel over the premultiplied ARGB canvas.
fn blend(canvas: &mut [u8], width: u32, stride: usize, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
    if x < 0 || y < 0 || x as u32 >= width {
        return;
    }
    let offset = y as usize * stride + x as usize * 4;
    if offset + 4 > canvas.len() {
        return;
    }
    let a = u32::from(a);
    let inv = 255 - a;
    let dst = &canvas[offset..offset + 4];
    let out_b = u32::from(b) * a / 255 + u32::from(dst[0]) * inv / 255;
    let out_g = u32::from(g) * a / 255 + u32::from(dst[1]) * inv / 255;
    let out_r = u32::from(r) * a / 255 + u32::from(dst[2]) * inv / 255;
    let out_a = a + u32::from(dst[3]) * inv / 255;
    canvas[offset] = out_b as u8;
    canvas[offset + 1] = out_g as u8;
    canvas[offset + 2] = out_r as u8;
    canvas[offset + 3] = out_a as u8;
}
