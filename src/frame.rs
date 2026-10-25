//! Shared geometry for rendering, hit testing, and the Wayland input region.
//! Corner boxes inherit their width and height from their adjacent edges.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub part: Part,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Fit two opposing edges while leaving space for two gaps and a center.
fn fit_pair(a: u32, b: u32, available: u32) -> (u32, u32) {
    let a = u64::from(a.max(1));
    let b = u64::from(b.max(1));
    if a + b <= u64::from(available) {
        return (a as u32, b as u32);
    }
    let first = (u64::from(available) * a / (a + b))
        .max(1)
        .min(u64::from(available - 1)) as u32;
    (first, available - first)
}

/// All coordinates are logical pixels. Small outputs shrink the requested
/// dimensions proportionally instead of letting opposing shapes overlap.
pub fn layout(width: u32, height: u32, dimensions: [u32; 4], margin: u32) -> Vec<Rect> {
    if width < 3 || height < 3 {
        return Vec::new();
    }
    let m = margin.min((width.min(height) - 3) / 4);
    let (left, right) = fit_pair(dimensions[0], dimensions[1], width - 4 * m - 1);
    let (top, bottom) = fit_pair(dimensions[2], dimensions[3], height - 4 * m - 1);
    let center_x = 2 * m + left;
    let center_y = 2 * m + top;
    let center_w = width - 4 * m - left - right;
    let center_h = height - 4 * m - top - bottom;
    let right_x = width - m - right;
    let bottom_y = height - m - bottom;
    [
        (Part::TopLeft, m, m, left, top),
        (Part::Top, center_x, m, center_w, top),
        (Part::TopRight, right_x, m, right, top),
        (Part::Left, m, center_y, left, center_h),
        (Part::Right, right_x, center_y, right, center_h),
        (Part::BottomLeft, m, bottom_y, left, bottom),
        (Part::Bottom, center_x, bottom_y, center_w, bottom),
        (Part::BottomRight, right_x, bottom_y, right, bottom),
    ]
    .into_iter()
    .map(|(part, x, y, w, h)| Rect { part, x, y, w, h })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asymmetric_edges_derive_corners_and_share_exact_gaps() {
        let rects = layout(1560, 1000, [64, 80, 48, 56], 12);
        let get = |part| *rects.iter().find(|r| r.part == part).unwrap();
        let tl = get(Part::TopLeft);
        let tr = get(Part::TopRight);
        let bl = get(Part::BottomLeft);
        let br = get(Part::BottomRight);
        assert_eq!((tl.w, tl.h), (64, 48));
        assert_eq!((tr.w, tr.h), (80, 48));
        assert_eq!((bl.w, bl.h), (64, 56));
        assert_eq!((br.w, br.h), (80, 56));
        let top = get(Part::Top);
        let left = get(Part::Left);
        let right = get(Part::Right);
        let bottom = get(Part::Bottom);
        assert_eq!(top.x - (tl.x + tl.w), 12);
        assert_eq!(tr.x - (top.x + top.w), 12);
        assert_eq!(left.y - (tl.y + tl.h), 12);
        assert_eq!(right.y - (tr.y + tr.h), 12);
        assert_eq!(bl.y - (left.y + left.h), 12);
        assert_eq!(br.y - (right.y + right.h), 12);
        assert_eq!(bottom.x - (bl.x + bl.w), 12);
        assert_eq!(br.x - (bottom.x + bottom.w), 12);
    }

    #[test]
    fn shapes_stay_on_screen_and_never_overlap_even_when_cramped() {
        for (w, h) in [(0, 0), (2, 2), (3, 3), (17, 11), (320, 240), (1920, 1080)] {
            for m in [0, 12, 80, u32::MAX] {
                for dimensions in [[200, 200, 56, 56], [800, 48, 400, 40], [u32::MAX; 4]] {
                    let rects = layout(w, h, dimensions, m);
                    assert!(rects.len() <= 8);
                    for (i, a) in rects.iter().enumerate() {
                        assert!(a.w > 0 && a.h > 0);
                        assert!(a.x + a.w <= w && a.y + a.h <= h);
                        for b in &rects[i + 1..] {
                            assert!(
                                a.x + a.w <= b.x
                                    || b.x + b.w <= a.x
                                    || a.y + a.h <= b.y
                                    || b.y + b.h <= a.y
                            );
                        }
                    }
                }
            }
        }
    }
}
