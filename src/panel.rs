//! Software rendering for the panel.
//!
//! The MVP draws a placeholder launcher: a background, an accent strip on the
//! screen edge, a header block and a few list rows. Text rendering is a
//! follow-up; the point here is the surface + latency plumbing.

use crate::config::Side;

const BG: u32 = 0xFF1E1E2E;
const HEADER: u32 = 0xFF313244;
const ACCENT: u32 = 0xFF89B4FA;
const ITEM: u32 = 0xFF45475A;
const ITEM_HIGHLIGHT: u32 = 0xFF585B70;

/// Draw the panel into an ARGB8888 canvas.
pub fn draw(canvas: &mut [u8], width: u32, height: u32, side: Side) {
    let stride = width as usize * 4;

    fill(canvas, stride, 0, 0, width, height, BG);

    // Accent strip along the screen edge.
    let strip = 3;
    let strip_x = match side {
        Side::Left => 0,
        Side::Right => width.saturating_sub(strip),
    };
    fill(canvas, stride, strip_x, 0, strip, height, ACCENT);

    // Header block.
    fill(canvas, stride, 12, 16, width.saturating_sub(24), 28, HEADER);

    // Placeholder list rows.
    let mut y = 64;
    for i in 0..12 {
        if y + 36 > height {
            break;
        }
        let color = if i == 0 { ITEM_HIGHLIGHT } else { ITEM };
        fill(canvas, stride, 12, y, width.saturating_sub(24), 36, color);
        y += 48;
    }
}

/// Fill an axis-aligned rectangle, clipped to the canvas.
fn fill(canvas: &mut [u8], stride: usize, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let bytes = color.to_le_bytes();
    for row in y..y.saturating_add(h) {
        let row_start = row as usize * stride;
        for col in x..x.saturating_add(w) {
            let offset = row_start + col as usize * 4;
            if offset + 4 <= canvas.len() {
                canvas[offset..offset + 4].copy_from_slice(&bytes);
            }
        }
    }
}
