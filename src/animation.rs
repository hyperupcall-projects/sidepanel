//! Client-side opacity animation, paced by each surface's Wayland frame callbacks.

use std::time::{Duration, Instant};

use crate::config::{PanelAnimation, MAX_FADE_DURATION_MS};

#[derive(Clone)]
pub struct Opening {
    pub mode: PanelAnimation,
    duration: Duration,
    started: Option<Instant>,
}

impl Opening {
    pub fn new(mode: PanelAnimation, duration_ms: u32) -> Self {
        Self {
            mode,
            duration: Duration::from_millis(u64::from(duration_ms.min(MAX_FADE_DURATION_MS))),
            started: None,
        }
    }

    /// Start at the first buffer, not while waiting for the initial configure.
    /// Returns opacity and whether another animation frame is needed.
    pub fn sample(&mut self, now: Instant) -> (u8, bool) {
        if self.mode != PanelAnimation::FadeIn || self.duration.is_zero() {
            return (255, false);
        }
        let started = *self.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= self.duration {
            return (255, false);
        }
        let alpha = (elapsed.as_secs_f64() / self.duration.as_secs_f64() * 255.0).round() as u8;
        (alpha, true)
    }
}

/// ARGB8888 buffers have premultiplied color, so scale every channel together.
pub fn apply_opacity(canvas: &mut [u8], opacity: u8) {
    if opacity == 255 {
        return;
    }
    if opacity == 0 {
        canvas.fill(0);
        return;
    }
    for channel in canvas {
        *channel = ((u16::from(*channel) * u16::from(opacity) + 127) / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_starts_on_first_buffer_and_finishes_at_configured_duration() {
        let now = Instant::now();
        let mut opening = Opening::new(PanelAnimation::FadeIn, 400);
        assert_eq!(opening.sample(now), (0, true));
        assert_eq!(
            opening.sample(now + Duration::from_millis(200)),
            (128, true)
        );
        assert!(opening.sample(now + Duration::from_millis(399)).1);
        assert_eq!(
            opening.sample(now + Duration::from_millis(400)),
            (255, false)
        );
        assert_eq!(opening.sample(now + Duration::from_secs(1)), (255, false));
        // A new surface starts fresh; a backdrop cloned from the frame stays in sync.
        let mut fresh = Opening::new(PanelAnimation::FadeIn, 400);
        assert_eq!(fresh.sample(now + Duration::from_secs(1)), (0, true));
        let mut backdrop = fresh.clone();
        assert_eq!(
            backdrop.sample(now + Duration::from_millis(1200)),
            (128, true)
        );
    }

    #[test]
    fn desktop_default_instant_and_zero_duration_need_no_callbacks() {
        for (mode, duration) in [
            (PanelAnimation::DesktopDefault, 150),
            (PanelAnimation::Instant, 150),
            (PanelAnimation::FadeIn, 0),
        ] {
            assert_eq!(
                Opening::new(mode, duration).sample(Instant::now()),
                (255, false)
            );
        }
    }

    #[test]
    fn fading_keeps_premultiplied_colors_and_transparent_gaps() {
        let original = [60, 90, 120, 128, 0, 0, 0, 0];
        let mut canvas = original;
        apply_opacity(&mut canvas, 128);
        assert_eq!(canvas, [30, 45, 60, 64, 0, 0, 0, 0]);
        apply_opacity(&mut canvas, 0);
        assert_eq!(canvas, [0; 8]);
        let mut canvas = original;
        apply_opacity(&mut canvas, 255);
        assert_eq!(canvas, original);
    }
}
