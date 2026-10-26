//! Configurable website sections in all four rectangles.
//! A fixed bottom-left corner box opens appearance and content settings.
//!
//! The interactive layout is computed by [`layout`] and shared by the
//! renderer and the pointer hit-testing, so what you see is exactly what you
//! can click. Keyboard selection (`Ui::selected`) is independent of hover:
//! opening the panel highlights the first site, Alt+J/K move within visible
//! rows, and Enter opens the selected site. Rows render a placeholder globe
//! until the background favicon fetch reports an [`Icon`].

use crate::config::{Config, PanelAnimation, Rectangle, Section, Theme};
use crate::editor::{Editor, Field, PAGE_SIZE};
use crate::favicon::{self, Icon};
use crate::frame::{self, Part};
use crate::text::TextRenderer;

#[derive(Clone, Copy)]
pub struct Palette {
    bg: u32,
    header: u32,
    hover: u32,
    accent: u32,
    text: u32,
    divider: u32,
    icon: u32,
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        if dark {
            Self {
                bg: 0xFF1E1E2E,
                header: 0xFF313244,
                hover: 0xFF45475A,
                accent: 0xFF89B4FA,
                text: 0xFFCDD6F4,
                divider: 0xFF45475A,
                icon: 0xFF9399B2,
            }
        } else {
            Self {
                bg: 0xFFF8FAFC,
                header: 0xFFE2E8F0,
                hover: 0xFFDBEAFE,
                accent: 0xFF1D4ED8,
                text: 0xFF1E293B,
                divider: 0xFFCBD5E1,
                icon: 0xFF64748B,
            }
        }
    }
}

pub const SETTINGS_WIDTH: u32 = 460;
pub const SETTINGS_HEIGHT: u32 = 864;

const MARGIN: u32 = 10;
const PAD: u32 = 8;
const ROW_H: u32 = 30;
const ROW_GAP: u32 = 2;
const FONT_PX: f32 = 13.0;
const GLYPH_PX: f32 = 15.0;
/// Pixel step of the browser width/height steppers.
const BUMP: i32 = 40;

/// Shared appearance controls for the sidebar and settings card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleSetting {
    LeftWidth,
    RightWidth,
    TopHeight,
    BottomHeight,
    Margin,
    Radius,
    Opacity,
    DesktopDim,
}

/// An interactive region of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Expand/collapse the website list.
    ToggleSection(usize),
    ScrollRectangle(Rectangle, i32),
    SettingsContents(bool),
    SelectRectangle(Rectangle),
    SelectSection(usize),
    AddSection,
    RemoveSection,
    SaveSection,
    SectionPage(i32),
    SelectWebsite(usize),
    AddWebsite,
    RemoveWebsite,
    SaveWebsite,
    WebsitePage(i32),
    FocusField(Field),
    /// Open website `n` (index into the configured list).
    Open(usize),
    /// Open/close the separate settings surface.
    ToggleSettings,
    RecordShortcut,
    SetTheme(Theme),
    SetPanelAnimation(PanelAnimation),
    BumpFadeDuration(i32),
    BumpPersistence(i32),
    BumpStyle(StyleSetting, i32),
    /// Step the browser window width by `i32` pixels (±`BUMP`).
    BumpWidth(i32),
    /// Step the browser window height by `i32` pixels (±`BUMP`).
    BumpHeight(i32),
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub action: Action,
}

impl Hit {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && x < f64::from(self.x + self.w)
            && y >= f64::from(self.y)
            && y < f64::from(self.y + self.h)
    }

    /// The full stepper row a ±half belongs to: `(x, w)`.
    fn stepper_span(&self) -> (u32, u32) {
        match self.action {
            Action::BumpWidth(delta)
            | Action::BumpHeight(delta)
            | Action::BumpStyle(_, delta)
            | Action::BumpPersistence(delta)
            | Action::BumpFadeDuration(delta)
                if delta < 0 =>
            {
                (self.x, self.w * 2)
            }
            _ => (self.x.saturating_sub(self.w), self.w * 2),
        }
    }
}

/// Panel interaction state; persists while the daemon runs (across hide/show).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Ui {
    /// Collapsed section indices and per-rectangle overflow position.
    pub collapsed: Vec<usize>,
    pub rectangle_scroll: [usize; 4],
    pub contents_tab: bool,
    pub editor: Editor,
    /// Is the separate settings surface open?
    pub settings_open: bool,
    /// Action under the cursor, if any (drives hover highlighting).
    #[serde(skip)]
    pub hover: Option<Action>,
    /// Keyboard-selected website, independent of pointer hover.
    pub selected: Option<usize>,
    #[serde(skip)]
    pub recording_shortcut: bool,
    #[serde(skip)]
    pub shortcut_pending: bool,
    #[serde(skip)]
    pub shortcut_message: String,
}

impl Default for Ui {
    fn default() -> Self {
        // Websites expanded on first run; settings closed until wanted.
        Self {
            collapsed: Vec::new(),
            rectangle_scroll: [0; 4],
            contents_tab: false,
            editor: Editor::default(),
            settings_open: false,
            hover: None,
            selected: None,
            recording_shortcut: false,
            shortcut_pending: false,
            shortcut_message: String::new(),
        }
    }
}

impl Ui {
    pub fn select_first(&mut self, sites: usize) {
        self.collapsed.clear();
        self.rectangle_scroll = [0; 4];
        self.hover = None;
        self.selected = (sites > 0).then_some(0);
    }

    pub fn selected_action(&self, hits: &[Hit]) -> Option<Action> {
        let action = Action::Open(self.selected?);
        hits.iter()
            .any(|hit| hit.action == action)
            .then_some(action)
    }

    /// Move within the visible websites, stopping at either end.
    pub fn move_selection(&mut self, hits: &[Hit], down: bool) {
        let rows: Vec<usize> = hits
            .iter()
            .filter_map(|hit| match hit.action {
                Action::Open(index) => Some(index),
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        let position = rows.iter().position(|index| Some(*index) == self.selected);
        let next = match position {
            Some(position) if down => (position + 1).min(rows.len() - 1),
            Some(position) => position.saturating_sub(1),
            None => 0,
        };
        self.selected = Some(rows[next]);
        self.hover = None;
    }
}

/// Compute section headers and website rows in a rectangle. Horizontal rails
/// flow left to right; vertical rails flow down. Paging reaches overflow items.
pub fn layout(
    width: u32,
    height: u32,
    ui: &Ui,
    sections: &[Section],
    rectangle: Rectangle,
) -> Vec<Hit> {
    if width < 40 || height < 40 {
        return Vec::new();
    }
    let horizontal = matches!(rectangle, Rectangle::Top | Rectangle::Bottom);
    let mut items = Vec::new();
    let mut site_index = 0;
    for (index, section) in sections.iter().enumerate() {
        if section.rectangle == rectangle {
            items.push(Action::ToggleSection(index));
            if !ui.collapsed.contains(&index) {
                items.extend((site_index..site_index + section.websites.len()).map(Action::Open));
            }
        }
        site_index += section.websites.len();
    }
    let available = if horizontal { width - 20 } else { height - 20 };
    let nominal_cell = if horizontal { 150 } else { ROW_H + ROW_GAP };
    let initial_capacity = (available / nominal_cell).max(1) as usize;
    let controls = if horizontal { 56 } else { 28 };
    let scrollable = items.len() > initial_capacity && available >= controls + 24;
    let content_space = available - if scrollable { controls } else { 0 };
    let cell = nominal_cell.min(content_space);
    let capacity = (content_space / cell).max(1) as usize;
    let slot = rectangle as usize;
    let offset = ui.rectangle_scroll[slot].min(items.len().saturating_sub(capacity));
    let mut hits = Vec::new();
    for (i, action) in items.iter().skip(offset).take(capacity).enumerate() {
        hits.push(Hit {
            x: if horizontal {
                MARGIN + i as u32 * cell
            } else {
                MARGIN
            },
            y: if horizontal {
                MARGIN
            } else {
                MARGIN + i as u32 * cell
            },
            w: if horizontal {
                cell.saturating_sub(4)
            } else {
                width - MARGIN * 2
            },
            h: if horizontal {
                height - MARGIN * 2
            } else {
                ROW_H.min(cell)
            },
            action: *action,
        });
    }
    if scrollable {
        // Leave room for both controls even on the narrowest rail.
        if horizontal {
            for (i, delta) in [-1, 1].into_iter().enumerate() {
                hits.push(Hit {
                    x: width - 60 + i as u32 * 26,
                    y: MARGIN,
                    w: 24,
                    h: height - 20,
                    action: Action::ScrollRectangle(rectangle, delta),
                });
            }
        } else {
            for (i, delta) in [-1, 1].into_iter().enumerate() {
                hits.push(Hit {
                    x: MARGIN + i as u32 * (width - 20) / 2,
                    y: height - 34,
                    w: (width - 20) / 2,
                    h: 24,
                    action: Action::ScrollRectangle(rectangle, delta),
                });
            }
        }
    }
    hits
}

/// Clamp before stepping, including after sections are collapsed or resized.
pub fn scroll_rectangle(
    width: u32,
    height: u32,
    ui: &mut Ui,
    sections: &[Section],
    rectangle: Rectangle,
    delta: i32,
) {
    let visible = layout(width, height, ui, sections, rectangle)
        .iter()
        .filter(|hit| matches!(hit.action, Action::Open(_) | Action::ToggleSection(_)))
        .count()
        .max(1);
    let total: usize = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.rectangle == rectangle)
        .map(|(i, s)| {
            1 + if ui.collapsed.contains(&i) {
                0
            } else {
                s.websites.len()
            }
        })
        .sum();
    let max = total.saturating_sub(visible);
    let offset = &mut ui.rectangle_scroll[rectangle as usize];
    *offset = (*offset)
        .min(max)
        .saturating_add_signed(delta as isize)
        .min(max);
}

fn rectangle_for(part: Part) -> Option<Rectangle> {
    match part {
        Part::Left => Some(Rectangle::Left),
        Part::Right => Some(Rectangle::Right),
        Part::Top => Some(Rectangle::Top),
        Part::Bottom => Some(Rectangle::Bottom),
        _ => None,
    }
}

/// The only settings launcher occupies the entire bottom-left box.
pub fn frame_layout(
    width: u32,
    height: u32,
    dimensions: [u32; 4],
    margin: u32,
    ui: &Ui,
    sections: &[Section],
) -> Vec<Hit> {
    let mut hits = Vec::new();
    for rect in frame::layout(width, height, dimensions, margin) {
        match rect.part {
            part if rectangle_for(part).is_some() => hits.extend(
                layout(rect.w, rect.h, ui, sections, rectangle_for(part).unwrap())
                    .into_iter()
                    .map(|mut hit| {
                        hit.x += rect.x;
                        hit.y += rect.y;
                        hit
                    }),
            ),
            Part::BottomLeft => hits.push(Hit {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
                action: Action::ToggleSettings,
            }),
            _ => {}
        }
    }
    hits
}

/// Which action, if any, lives at the given surface-local coordinates.
pub fn hit_test(hits: &[Hit], x: f64, y: f64) -> Option<Action> {
    // Topmost first: later regions are drawn on top.
    hits.iter()
        .rev()
        .find(|hit| hit.contains(x, y))
        .map(|hit| hit.action)
}

/// Everything the renderer needs beyond geometry.
pub struct Content<'a> {
    pub config: &'a Config,
    /// Row labels, parallel to the configured websites.
    pub labels: &'a [String],
    /// Fetched favicons, parallel to the configured websites.
    pub icons: &'a [Option<Icon>],
    /// Text renderer; without it the panel draws boxes but no text.
    pub font: Option<&'a TextRenderer>,
    /// Browser window defaults shown in the Settings steppers.
    pub browser_width: u32,
    pub browser_height: u32,
    pub theme: Theme,
    pub shortcut: &'a str,
    pub palette: Palette,
    pub margin: u32,
    pub border_radius: u32,
    pub opacity: u32,
    pub desktop_dim: u32,
    pub panel_animation: PanelAnimation,
    pub fade_duration_ms: u32,
    /// Left/right widths and top/bottom heights, in that order.
    pub dimensions: [u32; 4],
}

/// Render one rectangle using the same layout as pointer and keyboard input.
pub fn draw_rectangle(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    ui: &Ui,
    content: &Content,
    rectangle: Rectangle,
) {
    let stride = width as usize * 4;
    fill(canvas, stride, 0, 0, width, height, content.palette.bg);
    for hit in layout(width, height, ui, &content.config.sections, rectangle) {
        let selected =
            ui.hover == Some(hit.action) || ui.selected.map(Action::Open) == Some(hit.action);
        if selected {
            rounded_fill(
                canvas,
                stride,
                hit.x,
                hit.y,
                hit.w,
                hit.h,
                6,
                content.palette.hover,
            );
        }
        match hit.action {
            Action::ToggleSection(index) => draw_header(
                canvas,
                width,
                stride,
                &hit,
                content,
                &content.config.sections[index].title,
                !ui.collapsed.contains(&index),
            ),
            Action::Open(index) => draw_row(canvas, width, stride, &hit, index, content),
            Action::ScrollRectangle(_, delta) => draw_button(
                canvas,
                width,
                stride,
                &hit,
                content,
                if delta < 0 { "‹" } else { "›" },
                selected,
            ),
            _ => {}
        }
    }
    finish_surface(
        canvas,
        width,
        height,
        content.border_radius,
        content.opacity,
    );
}

/// A separate, non-interactive surface below the browser and the cards.
pub fn draw_backdrop(canvas: &mut [u8], desktop_dim: u32) {
    let alpha = ((desktop_dim.min(100) * 255 + 50) / 100) as u8;
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, alpha]);
    }
}

/// The overlay layer contains only cards. Its empty center must remain fully
/// transparent so it cannot dim a browser stacked above the separate backdrop.
pub fn draw_frame(canvas: &mut [u8], width: u32, height: u32, ui: &Ui, content: &Content) {
    canvas.fill(0);
    for rect in frame::layout(width, height, content.dimensions, content.margin) {
        let mut card = vec![0; (rect.w * rect.h * 4) as usize];
        if let Some(rectangle) = rectangle_for(rect.part) {
            draw_rectangle(&mut card, rect.w, rect.h, ui, content, rectangle);
        } else {
            let settings = rect.part == Part::BottomLeft;
            let selected =
                settings && (ui.settings_open || ui.hover == Some(Action::ToggleSettings));
            fill(
                &mut card,
                rect.w as usize * 4,
                0,
                0,
                rect.w,
                rect.h,
                if selected {
                    content.palette.hover
                } else {
                    content.palette.bg
                },
            );
            if settings {
                let size = 20
                    .min(rect.w.saturating_sub(8))
                    .min(rect.h.saturating_sub(8));
                let cx = rect.w / 2;
                let cy = rect.h / 2;
                // A gear with a circular hole, drawn without relying on font glyphs.
                for y in 0..size {
                    for x in 0..size {
                        let dx = x as f32 + 0.5 - size as f32 / 2.0;
                        let dy = y as f32 + 0.5 - size as f32 / 2.0;
                        let distance = dx.hypot(dy);
                        let tooth = (dy.atan2(dx) * 8.0).cos() > 0.0;
                        let outer = size as f32 * if tooth { 0.49 } else { 0.38 };
                        if distance >= size as f32 * 0.19 && distance < outer {
                            set_pixel(
                                &mut card,
                                rect.w,
                                rect.w as usize * 4,
                                (cx - size / 2 + x) as i32,
                                (cy - size / 2 + y) as i32,
                                content.palette.text,
                            );
                        }
                    }
                }
            }
            finish_surface(
                &mut card,
                rect.w,
                rect.h,
                content.border_radius,
                content.opacity,
            );
        }
        for y in 0..rect.h {
            let source = (y * rect.w * 4) as usize;
            let target = (((rect.y + y) * width + rect.x) * 4) as usize;
            let len = rect.w as usize * 4;
            canvas[target..target + len].copy_from_slice(&card[source..source + len]);
        }
    }
}

/// Separate settings surface: close button, appearance choices, and size controls.
pub fn settings_layout(width: u32, ui: &Ui, config: &Config) -> Vec<Hit> {
    let inset = 20;
    let w = width.saturating_sub(inset * 2);
    let mut hits = vec![Hit {
        x: width.saturating_sub(52),
        y: 16,
        w: 32,
        h: 32,
        action: Action::ToggleSettings,
    }];
    hits.push(Hit {
        x: 146,
        y: 16,
        w: 118,
        h: 32,
        action: Action::SettingsContents(false),
    });
    hits.push(Hit {
        x: 270,
        y: 16,
        w: 118,
        h: 32,
        action: Action::SettingsContents(true),
    });
    if ui.contents_tab {
        hits.extend(contents_layout(ui, config));
        return hits;
    }
    for (i, theme) in [Theme::Light, Theme::Dark, Theme::System]
        .into_iter()
        .enumerate()
    {
        let x = inset + w * i as u32 / 3;
        hits.push(Hit {
            x,
            y: 88,
            w: inset + w * (i as u32 + 1) / 3 - x - 4,
            h: 34,
            action: Action::SetTheme(theme),
        });
    }
    // Two columns of independent edge thickness controls.
    let column = (w - 12) / 2;
    for (i, setting) in [
        StyleSetting::LeftWidth,
        StyleSetting::RightWidth,
        StyleSetting::TopHeight,
        StyleSetting::BottomHeight,
    ]
    .into_iter()
    .enumerate()
    {
        let x = inset + (i as u32 % 2) * (column + 12);
        let y = 160 + (i as u32 / 2) * 40;
        hits.push(Hit {
            x,
            y,
            w: column / 2,
            h: 34,
            action: Action::BumpStyle(setting, -4),
        });
        hits.push(Hit {
            x: x + column / 2,
            y,
            w: column / 2,
            h: 34,
            action: Action::BumpStyle(setting, 4),
        });
    }
    let half = w / 2;
    for (y, a, b) in [
        (
            268,
            Action::BumpStyle(StyleSetting::Margin, -2),
            Action::BumpStyle(StyleSetting::Margin, 2),
        ),
        (
            308,
            Action::BumpStyle(StyleSetting::Radius, -2),
            Action::BumpStyle(StyleSetting::Radius, 2),
        ),
        (
            348,
            Action::BumpStyle(StyleSetting::Opacity, -5),
            Action::BumpStyle(StyleSetting::Opacity, 5),
        ),
        (
            388,
            Action::BumpStyle(StyleSetting::DesktopDim, -5),
            Action::BumpStyle(StyleSetting::DesktopDim, 5),
        ),
        (
            506,
            Action::BumpFadeDuration(-25),
            Action::BumpFadeDuration(25),
        ),
        (586, Action::BumpWidth(-BUMP), Action::BumpWidth(BUMP)),
        (626, Action::BumpHeight(-BUMP), Action::BumpHeight(BUMP)),
        (704, Action::BumpPersistence(-1), Action::BumpPersistence(1)),
    ] {
        hits.push(Hit {
            x: inset,
            y,
            w: half,
            h: 34,
            action: a,
        });
        hits.push(Hit {
            x: inset + half,
            y,
            w: half,
            h: 34,
            action: b,
        });
    }
    for (i, mode) in [
        PanelAnimation::DesktopDefault,
        PanelAnimation::FadeIn,
        PanelAnimation::Instant,
    ]
    .into_iter()
    .enumerate()
    {
        let x = inset + w * i as u32 / 3;
        hits.push(Hit {
            x,
            y: 466,
            w: inset + w * (i as u32 + 1) / 3 - x - 4,
            h: 34,
            action: Action::SetPanelAnimation(mode),
        });
    }
    hits.push(Hit {
        x: inset,
        y: 784,
        w,
        h: 34,
        action: Action::RecordShortcut,
    });
    hits
}

/// Only the four rectangles are selectable; corner content has no editor.
fn contents_layout(ui: &Ui, config: &Config) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut add = |x, y, w, action| {
        hits.push(Hit {
            x,
            y,
            w,
            h: 32,
            action,
        })
    };
    for (i, rectangle) in Rectangle::ALL.into_iter().enumerate() {
        add(
            20 + i as u32 * 106,
            92,
            100,
            Action::SelectRectangle(rectangle),
        );
    }
    add(250, 134, 90, Action::AddSection);
    let editor = &ui.editor;
    let sections = editor.sections(config);
    for (row, &index) in sections
        .iter()
        .skip(editor.section_page * PAGE_SIZE)
        .take(PAGE_SIZE)
        .enumerate()
    {
        add(20, 174 + row as u32 * 36, 420, Action::SelectSection(index));
    }
    if editor.section_page > 0 {
        add(20, 282, 100, Action::SectionPage(-1));
    }
    if (editor.section_page + 1) * PAGE_SIZE < sections.len() {
        add(340, 282, 100, Action::SectionPage(1));
    }
    if let Some(section) = editor.section.and_then(|i| config.sections.get(i)) {
        add(346, 134, 94, Action::RemoveSection);
        add(20, 332, 310, Action::FocusField(Field::SectionName));
        add(342, 332, 98, Action::SaveSection);
        add(250, 380, 90, Action::AddWebsite);
        if editor.website.is_some() {
            add(346, 380, 94, Action::RemoveWebsite);
        }
        for (row, index) in (0..section.websites.len())
            .skip(editor.website_page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .enumerate()
        {
            add(20, 420 + row as u32 * 34, 420, Action::SelectWebsite(index));
        }
        if editor.website_page > 0 {
            add(20, 526, 100, Action::WebsitePage(-1));
        }
        if (editor.website_page + 1) * PAGE_SIZE < section.websites.len() {
            add(340, 526, 100, Action::WebsitePage(1));
        }
        if editor.website.is_some() || editor.adding_website {
            add(20, 588, 420, Action::FocusField(Field::WebsiteTitle));
            add(20, 648, 420, Action::FocusField(Field::WebsiteUrl));
            add(280, 698, 160, Action::SaveWebsite);
        }
    }
    hits
}

fn draw_contents(canvas: &mut [u8], width: u32, height: u32, ui: &Ui, content: &Content) {
    let stride = width as usize * 4;
    let editor = &ui.editor;
    let section = editor.section.and_then(|i| content.config.sections.get(i));
    let sections = editor.sections(content.config);
    if let Some(font) = content.font {
        let section_page = format!(
            "Page {} of {}",
            editor.section_page + 1,
            sections.len().div_ceil(PAGE_SIZE).max(1)
        );
        let website_page = format!(
            "Page {} of {}",
            editor.website_page + 1,
            section
                .map_or(0, |s| s.websites.len())
                .div_ceil(PAGE_SIZE)
                .max(1)
        );
        let mut labels = vec![
            ("Settings", 24, 38),
            ("Choose a rectangle. Corner boxes stay fixed.", 24, 76),
            ("Sections", 24, 156),
            (section_page.as_str(), 164, 302),
        ];
        if section.is_some() {
            labels.extend([
                ("Section name", 24, 326),
                ("Websites", 24, 402),
                (website_page.as_str(), 164, 546),
            ]);
            if editor.website.is_some() || editor.adding_website {
                labels.extend([("Title (optional)", 24, 580), ("URL", 24, 640)]);
            } else {
                labels.push(("Add a website to this section.", 24, 580));
            }
        } else {
            labels.push(("Add a section to this rectangle.", 24, 342));
        }
        for (label, x, y) in labels {
            font.draw(
                canvas,
                width,
                stride,
                x,
                y,
                if label == "Settings" { 18.0 } else { FONT_PX },
                label,
                content.palette.text,
            );
        }
        let message = font.ellipsize(&editor.message, FONT_PX, 408.0);
        font.draw(
            canvas,
            width,
            stride,
            24,
            756,
            FONT_PX,
            &message,
            content.palette.text,
        );
    }
    for hit in settings_layout(width, ui, content.config) {
        let hover = ui.hover == Some(hit.action);
        let (label, selected) = match hit.action {
            Action::ToggleSettings => ("×".into(), hover),
            Action::SettingsContents(contents) => (
                if contents { "Contents" } else { "Appearance" }.into(),
                contents,
            ),
            Action::SelectRectangle(rectangle) => {
                (rectangle.label().into(), rectangle == editor.rectangle)
            }
            Action::SelectSection(index) => (
                content.config.sections[index].title.clone(),
                editor.section == Some(index),
            ),
            Action::SelectWebsite(index) => (
                section.unwrap().websites[index].label(),
                editor.website == Some(index),
            ),
            Action::AddSection | Action::AddWebsite => ("+ Add".into(), hover),
            Action::RemoveSection | Action::RemoveWebsite => ("Remove".into(), hover),
            Action::SaveSection => ("Rename".into(), hover),
            Action::SaveWebsite => (
                if editor.adding_website {
                    "Add website"
                } else {
                    "Save website"
                }
                .into(),
                hover,
            ),
            Action::SectionPage(delta) | Action::WebsitePage(delta) => (
                if delta < 0 {
                    "‹ Previous"
                } else {
                    "Next ›"
                }
                .into(),
                hover,
            ),
            Action::FocusField(field) => {
                draw_input(canvas, width, stride, &hit, content, ui, field);
                continue;
            }
            _ => continue,
        };
        draw_button(canvas, width, stride, &hit, content, &label, selected);
    }
    finish_surface(
        canvas,
        width,
        height,
        content.border_radius,
        content.opacity,
    );
}

fn draw_input(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    hit: &Hit,
    content: &Content,
    ui: &Ui,
    field: Field,
) {
    let focused = ui.editor.focus == Some(field);
    rounded_fill(
        canvas,
        stride,
        hit.x,
        hit.y,
        hit.w,
        hit.h,
        6,
        if focused {
            content.palette.accent
        } else {
            content.palette.divider
        },
    );
    rounded_fill(
        canvas,
        stride,
        hit.x + 1,
        hit.y + 1,
        hit.w - 2,
        hit.h - 2,
        5,
        content.palette.bg,
    );
    let Some(font) = content.font else {
        return;
    };
    let input = ui.editor.input(field);
    let mut start = 0;
    if focused {
        while font.measure(&input.text[start..input.cursor], FONT_PX)
            > hit.w.saturating_sub(24) as f32
        {
            start += input.text[start..].chars().next().unwrap().len_utf8();
        }
    }
    if focused && input.selected_all {
        fill(
            canvas,
            stride,
            hit.x + 4,
            hit.y + 4,
            hit.w - 8,
            hit.h - 8,
            content.palette.hover,
        );
    }
    let text = font.ellipsize(
        &input.text[start..],
        FONT_PX,
        hit.w.saturating_sub(16) as f32,
    );
    font.draw(
        canvas,
        width,
        stride,
        (hit.x + 8) as i32,
        (hit.y + 21) as i32,
        FONT_PX,
        &text,
        content.palette.text,
    );
    if focused {
        let x = hit.x
            + 8
            + font
                .measure(&input.text[start..input.cursor], FONT_PX)
                .round() as u32;
        fill(
            canvas,
            stride,
            x,
            hit.y + 6,
            1,
            hit.h - 12,
            content.palette.accent,
        );
    }
}

fn draw_button(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    hit: &Hit,
    content: &Content,
    label: &str,
    selected: bool,
) {
    let bg = if selected {
        content.palette.accent
    } else {
        content.palette.header
    };
    rounded_fill(canvas, stride, hit.x, hit.y, hit.w, hit.h, 8, bg);
    if let Some(font) = content.font {
        let label = font.ellipsize(label, FONT_PX, hit.w.saturating_sub(16) as f32);
        font.draw(
            canvas,
            width,
            stride,
            (hit.x + PAD) as i32,
            (hit.y as f32 + (hit.h as f32 + font.ascent(FONT_PX)) / 2.0) as i32,
            FONT_PX,
            &label,
            if selected {
                if content.palette.bg == Palette::new(true).bg {
                    0xFF111827
                } else {
                    0xFFFFFFFF
                }
            } else {
                content.palette.text
            },
        );
    }
}

/// Scale the complete settings card to fit smaller outputs, including its hits.
pub fn settings_hits(width: u32, height: u32, ui: &Ui, config: &Config) -> Vec<Hit> {
    settings_layout(SETTINGS_WIDTH, ui, config)
        .into_iter()
        .map(|hit| {
            let x = (hit.x * width).div_ceil(SETTINGS_WIDTH);
            let y = (hit.y * height).div_ceil(SETTINGS_HEIGHT);
            Hit {
                x,
                y,
                w: ((hit.x + hit.w) * width).div_ceil(SETTINGS_WIDTH) - x,
                h: ((hit.y + hit.h) * height).div_ceil(SETTINGS_HEIGHT) - y,
                action: hit.action,
            }
        })
        .collect()
}

pub fn draw_settings(canvas: &mut [u8], width: u32, height: u32, ui: &Ui, content: &Content) {
    if width == SETTINGS_WIDTH && height == SETTINGS_HEIGHT {
        draw_settings_native(canvas, width, height, ui, content);
        return;
    }
    let mut native = vec![0; (SETTINGS_WIDTH * SETTINGS_HEIGHT * 4) as usize];
    draw_settings_native(&mut native, SETTINGS_WIDTH, SETTINGS_HEIGHT, ui, content);
    for y in 0..height {
        for x in 0..width {
            let src = ((y * SETTINGS_HEIGHT / height * SETTINGS_WIDTH + x * SETTINGS_WIDTH / width)
                * 4) as usize;
            let dst = ((y * width + x) * 4) as usize;
            canvas[dst..dst + 4].copy_from_slice(&native[src..src + 4]);
        }
    }
}

fn draw_settings_native(canvas: &mut [u8], width: u32, height: u32, ui: &Ui, content: &Content) {
    let stride = width as usize * 4;
    fill(canvas, stride, 0, 0, width, height, content.palette.bg);
    // A subtle frame and generous padding give settings a floating-card feel.
    fill(canvas, stride, 0, 0, width, 1, content.palette.divider);
    fill(
        canvas,
        stride,
        0,
        height.saturating_sub(1),
        width,
        1,
        content.palette.divider,
    );
    if ui.contents_tab {
        draw_contents(canvas, width, height, ui, content);
        return;
    }
    if let Some(font) = content.font {
        for (label, y) in [
            ("Settings", 38),
            ("Appearance", 76),
            ("Edge panel sizes (px)", 146),
            ("Spacing and style", 254),
            ("Panel opening (applies next open)", 450),
            ("Browser window size", 570),
            ("Persist Changes to Panels", 690),
            ("Open panels shortcut", 770),
        ] {
            font.draw(
                canvas,
                width,
                stride,
                24,
                y,
                if label == "Settings" { 18.0 } else { FONT_PX },
                label,
                content.palette.text,
            );
        }
    }
    let hits = settings_layout(width, ui, content.config);
    // Each pair shares one rounded card; paint it before either half's text.
    for pair in hits.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        match left.action {
            Action::BumpWidth(delta)
            | Action::BumpHeight(delta)
            | Action::BumpStyle(_, delta)
            | Action::BumpFadeDuration(delta)
            | Action::BumpPersistence(delta)
                if delta < 0 => {}
            _ => continue,
        }
        let hover = ui.hover == Some(left.action) || ui.hover == Some(right.action);
        rounded_fill(
            canvas,
            stride,
            left.x,
            left.y,
            left.w + right.w,
            left.h,
            8,
            if hover {
                content.palette.hover
            } else {
                content.palette.header
            },
        );
    }
    for hit in hits {
        let hover = ui.hover == Some(hit.action);
        match hit.action {
            Action::ToggleSettings => draw_button(canvas, width, stride, &hit, content, "×", hover),
            Action::SettingsContents(contents) => draw_button(
                canvas,
                width,
                stride,
                &hit,
                content,
                if contents { "Contents" } else { "Appearance" },
                contents == ui.contents_tab,
            ),
            Action::RecordShortcut => {
                let label = if ui.recording_shortcut {
                    "Press keys… Esc cancels".to_string()
                } else if ui.shortcut_pending {
                    "Applying shortcut…".to_string()
                } else {
                    format!("{}   ·   Click to change", content.shortcut)
                };
                draw_button(
                    canvas,
                    width,
                    stride,
                    &hit,
                    content,
                    &label,
                    ui.recording_shortcut || hover,
                );
            }
            Action::SetTheme(theme) => {
                draw_button(
                    canvas,
                    width,
                    stride,
                    &hit,
                    content,
                    match theme {
                        Theme::Light => "Light",
                        Theme::Dark => "Dark",
                        Theme::System => "System",
                    },
                    theme == content.theme,
                );
                if hover {
                    fill(
                        canvas,
                        stride,
                        hit.x + 8,
                        hit.y + hit.h - 3,
                        hit.w.saturating_sub(16),
                        2,
                        content.palette.accent,
                    );
                }
            }
            Action::SetPanelAnimation(mode) => draw_button(
                canvas,
                width,
                stride,
                &hit,
                content,
                match mode {
                    PanelAnimation::DesktopDefault => "Desktop Default",
                    PanelAnimation::FadeIn => "Fade In",
                    PanelAnimation::Instant => "Instant",
                },
                mode == content.panel_animation,
            ),
            Action::BumpWidth(delta)
            | Action::BumpHeight(delta)
            | Action::BumpStyle(_, delta)
            | Action::BumpPersistence(delta)
            | Action::BumpFadeDuration(delta) => {
                let label = match hit.action {
                    Action::BumpPersistence(_) => content.config.persist_changes_to_panels.label(),
                    Action::BumpFadeDuration(_) => {
                        format!("Fade duration {} ms", content.fade_duration_ms)
                    }
                    Action::BumpWidth(_) => format!("Width {}", content.browser_width),
                    Action::BumpStyle(StyleSetting::LeftWidth, _) => {
                        format!("Left width {}", content.dimensions[0])
                    }
                    Action::BumpStyle(StyleSetting::RightWidth, _) => {
                        format!("Right width {}", content.dimensions[1])
                    }
                    Action::BumpStyle(StyleSetting::TopHeight, _) => {
                        format!("Top height {}", content.dimensions[2])
                    }
                    Action::BumpStyle(StyleSetting::BottomHeight, _) => {
                        format!("Bottom height {}", content.dimensions[3])
                    }
                    Action::BumpStyle(StyleSetting::Margin, _) => {
                        format!("Margin {} px", content.margin)
                    }
                    Action::BumpStyle(StyleSetting::Radius, _) => {
                        format!("Corner radius {} px", content.border_radius)
                    }
                    Action::BumpStyle(StyleSetting::Opacity, _) => {
                        format!("Opacity {}%", content.opacity)
                    }
                    Action::BumpStyle(StyleSetting::DesktopDim, _) => {
                        format!("Desktop dimming {}%", content.desktop_dim)
                    }
                    _ => format!("Height {}", content.browser_height),
                };
                draw_stepper(canvas, width, stride, &hit, delta, &label, &label, content);
            }
            _ => {}
        }
    }
    if let Some(font) = content.font {
        font.draw(
            canvas,
            width,
            stride,
            24,
            752,
            12.0,
            if content.config.persist_changes_to_panels == crate::config::Persistence::Login {
                "Screen locking does not reset panels."
            } else {
                "Timed options reset after panels stay hidden."
            },
            content.palette.text,
        );
        let message = font.ellipsize(
            &ui.shortcut_message,
            FONT_PX,
            width.saturating_sub(40) as f32,
        );
        font.draw(
            canvas,
            width,
            stride,
            24,
            844,
            FONT_PX,
            &message,
            content.palette.text,
        );
    }
    finish_surface(
        canvas,
        width,
        height,
        content.border_radius,
        content.opacity,
    );
}

fn rounded_fill(
    canvas: &mut [u8],
    stride: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: u32,
) {
    let r = radius.min(w / 2).min(h / 2) as f32;
    for row in 0..h {
        let dy = (r - (row as f32 + 0.5))
            .max(row as f32 + 0.5 - (h as f32 - r))
            .max(0.0);
        let inset = if dy > 0.0 {
            (r - (r * r - dy * dy).max(0.0).sqrt()).round() as u32
        } else {
            0
        };
        fill(
            canvas,
            stride,
            x + inset,
            y + row,
            w.saturating_sub(inset * 2),
            1,
            color,
        );
    }
}

/// Clip corners with subpixel coverage and keep ARGB channels premultiplied.
/// Alpha zero exposes the desktop; 100% opacity leaves the interior opaque.
fn finish_surface(canvas: &mut [u8], width: u32, height: u32, radius: u32, opacity: u32) {
    let r = radius.min(width / 2).min(height / 2) as f32;
    let opacity = opacity.min(100) as f32 / 100.0;
    for y in 0..height {
        for x in 0..width {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let dx = (r - px).max(px - (width as f32 - r)).max(0.0);
            let dy = (r - py).max(py - (height as f32 - r)).max(0.0);
            let coverage = if dx > 0.0 && dy > 0.0 {
                (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let scale = coverage * opacity;
            let i = ((y * width + x) * 4) as usize;
            for channel in &mut canvas[i..i + 4] {
                *channel = (*channel as f32 * scale).round() as u8;
            }
        }
    }
}

/// Section header: title, drop-down caret, and a divider while expanded.
fn draw_header(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    hit: &Hit,
    content: &Content,
    title: &str,
    expanded: bool,
) {
    let Some(font) = content.font else {
        return;
    };
    let baseline = hit.y as f32 + (hit.h as f32 + font.ascent(FONT_PX)) / 2.0;
    if hit.w < 100 {
        draw_globe(
            canvas,
            width,
            stride,
            hit.x + 4,
            hit.y + hit.h.saturating_sub(favicon::ICON) / 2,
            content.palette.icon,
        );
    } else {
        font.draw(
            canvas,
            width,
            stride,
            (hit.x + PAD) as i32,
            baseline.round() as i32,
            FONT_PX,
            &font.ellipsize(title, FONT_PX, hit.w.saturating_sub(32) as f32),
            content.palette.text,
        );
    }
    // Drop-down caret on the right: ▲ expanded, ▼ collapsed.
    let cx = (hit.x + hit.w - PAD - 4) as i32;
    let cy = hit.y as i32 + (hit.h / 2) as i32;
    draw_caret(
        canvas,
        width,
        stride,
        cx,
        cy,
        expanded,
        content.palette.text,
    );
    // Divider between the header and its content.
    if expanded {
        let y = hit.y + hit.h - 1;
        fill(
            canvas,
            stride,
            hit.x + PAD,
            y,
            hit.w.saturating_sub(PAD * 2),
            1,
            content.palette.divider,
        );
    }
}

fn draw_row(canvas: &mut [u8], width: u32, stride: usize, hit: &Hit, n: usize, content: &Content) {
    // Favicon (or a placeholder globe) on the left.
    let compact = hit.w < 100;
    let box_x = if compact {
        hit.x + (hit.w - favicon::ICON) / 2
    } else {
        hit.x + PAD
    };
    let box_y = hit.y + (hit.h.saturating_sub(favicon::ICON)) / 2;
    match content.icons.get(n).and_then(|icon| icon.as_ref()) {
        Some(icon) => draw_icon(canvas, width, stride, box_x, box_y, icon),
        None => draw_globe(canvas, width, stride, box_x, box_y, content.palette.icon),
    }

    // Slim rails show centered favicons; wider panels also show site labels.
    if compact {
        return;
    }
    let Some(font) = content.font else {
        return;
    };
    let Some(label) = content.labels.get(n) else {
        return;
    };
    let text_x = box_x + favicon::ICON + 10;
    let text_max = (hit.x + hit.w).saturating_sub(text_x + 6) as f32;
    let text = font.ellipsize(label, FONT_PX, text_max);
    let top = hit.y as f32 + (hit.h as f32 - font.line_height(FONT_PX)) / 2.0;
    let baseline = top + font.ascent(FONT_PX);
    font.draw(
        canvas,
        width,
        stride,
        text_x as i32,
        baseline.round() as i32,
        FONT_PX,
        &text,
        content.palette.text,
    );
}

/// One half of a stepper row: the ± glyph sits at the outer edge; the
/// left half additionally draws the row's value label (centred on the full
/// row so it is rendered exactly once).
fn draw_stepper(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    hit: &Hit,
    delta: i32,
    short: &str,
    long: &str,
    content: &Content,
) {
    let Some(font) = content.font else {
        return;
    };
    let glyph = if delta < 0 { "\u{2212}" } else { "+" };
    let glyph_w = font.measure(glyph, GLYPH_PX);
    let cx = if delta < 0 {
        hit.x as f32 + 16.0
    } else {
        (hit.x + hit.w) as f32 - 16.0
    };
    let baseline = hit.y as f32 + (ROW_H as f32 + font.ascent(GLYPH_PX)) / 2.0;
    font.draw(
        canvas,
        width,
        stride,
        (cx - glyph_w / 2.0).round() as i32,
        baseline.round() as i32,
        GLYPH_PX,
        glyph,
        content.palette.text,
    );

    if delta >= 0 {
        return; // value label is drawn by the left half only
    }
    let (full_x, full_w) = hit.stepper_span();
    let label = if full_w >= 220 { long } else { short };
    let max_w = full_w.saturating_sub(76) as f32;
    let label = font.ellipsize(label, FONT_PX, max_w);
    let label_w = font.measure(&label, FONT_PX);
    let x = full_x as f32 + (full_w as f32 - label_w) / 2.0;
    let baseline = hit.y as f32 + (ROW_H as f32 + font.ascent(FONT_PX)) / 2.0;
    font.draw(
        canvas,
        width,
        stride,
        x.round() as i32,
        baseline.round() as i32,
        FONT_PX,
        &label,
        content.palette.text,
    );
}

/// Blit a premultiplied RGBA icon, centred in its box.
fn draw_icon(canvas: &mut [u8], width: u32, stride: usize, box_x: u32, box_y: u32, icon: &Icon) {
    let x = box_x + favicon::ICON.saturating_sub(icon.w) / 2;
    let y = box_y + favicon::ICON.saturating_sub(icon.h) / 2;
    blit_premultiplied(
        canvas, width, stride, x as i32, y as i32, icon.w, icon.h, &icon.rgba,
    );
}

/// Placeholder globe for sites whose favicon hasn't arrived (or failed).
fn draw_globe(canvas: &mut [u8], width: u32, stride: usize, box_x: u32, box_y: u32, color: u32) {
    let cx = box_x as i32 + (favicon::ICON / 2) as i32;
    let cy = box_y as i32 + (favicon::ICON / 2) as i32;
    let radius = favicon::ICON as f32 / 2.0 - 0.5;
    // Outline.
    for dy in -(favicon::ICON as i32 / 2)..=favicon::ICON as i32 / 2 {
        for dx in -(favicon::ICON as i32 / 2)..=favicon::ICON as i32 / 2 {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if (dist - radius).abs() < 0.6 {
                set_pixel(canvas, width, stride, cx + dx, cy + dy, color);
            }
        }
    }
    // Equator and meridian.
    for dx in -(radius as i32)..=radius as i32 {
        set_pixel(canvas, width, stride, cx + dx, cy, color);
    }
    for dy in -(radius as i32)..=radius as i32 {
        set_pixel(canvas, width, stride, cx, cy + dy, color);
    }
}

/// Small filled caret, `up` for ▲ and `down` for ▼.
fn draw_caret(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    cx: i32,
    cy: i32,
    up: bool,
    color: u32,
) {
    const HEIGHT: i32 = 5;
    for row in 0..HEIGHT {
        let half = if up { row } else { HEIGHT - 1 - row };
        let y = cy - HEIGHT / 2 + row;
        fill_clipped(canvas, width, stride, cx - half, y, half * 2 + 1, 1, color);
    }
}

/// Blend an RGBA (premultiplied) source rectangle over the canvas.
fn blit_premultiplied(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    src: &[u8],
) {
    for row in 0..h {
        let dy = y + row as i32;
        if dy < 0 || dy as u32 >= canvas.len() as u32 / stride as u32 {
            continue;
        }
        for col in 0..w {
            let dx = x + col as i32;
            if dx < 0 || dx as u32 >= width {
                continue;
            }
            let si = (row * w + col) as usize * 4;
            if si + 4 > src.len() {
                continue;
            }
            let offset = dy as usize * stride + dx as usize * 4;
            if offset + 4 > canvas.len() {
                continue;
            }
            let alpha = u32::from(src[si + 3]);
            let inv = 255 - alpha;
            let dst = &canvas[offset..offset + 4];
            // Source is RGBA; the canvas stores ARGB as little-endian bytes
            // [B, G, R, A] — swap R/B while merging.
            let out_b = u32::from(src[si + 2]) + u32::from(dst[0]) * inv / 255;
            let out_g = u32::from(src[si + 1]) + u32::from(dst[1]) * inv / 255;
            let out_r = u32::from(src[si]) + u32::from(dst[2]) * inv / 255;
            let out_a = alpha + u32::from(dst[3]) * inv / 255;
            canvas[offset] = out_b.min(255) as u8;
            canvas[offset + 1] = out_g.min(255) as u8;
            canvas[offset + 2] = out_r.min(255) as u8;
            canvas[offset + 3] = out_a.min(255) as u8;
        }
    }
}

fn set_pixel(canvas: &mut [u8], width: u32, stride: usize, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x as u32 >= width {
        return;
    }
    let offset = y as usize * stride + x as usize * 4;
    if offset + 4 > canvas.len() {
        return;
    }
    canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
}

/// Fill an axis-aligned rectangle with an opaque colour, clipped to the canvas.
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

/// Like [`fill`], but with signed coordinates (used by the caret).
fn fill_clipped(
    canvas: &mut [u8],
    width: u32,
    stride: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) {
    let bytes = color.to_le_bytes();
    for row in y..y + h {
        if row < 0 {
            continue;
        }
        for col in x..x + w {
            if col < 0 || col as u32 >= width {
                continue;
            }
            let offset = row as usize * stride + col as usize * 4;
            if offset + 4 <= canvas.len() {
                canvas[offset..offset + 4].copy_from_slice(&bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::favicon::Icon;
    use crate::text::TextRenderer;

    fn test_sections(sites: usize) -> Vec<Section> {
        vec![Section {
            rectangle: Rectangle::Left,
            title: "Websites".into(),
            websites: (0..sites)
                .map(|_| crate::config::Website::new("https://example.com/"))
                .collect(),
        }]
    }

    #[test]
    fn all_rectangles_reach_overflow_content_without_customizing_corners() {
        let mut config = Config {
            sections: vec![],
            ..Config::default()
        };
        for rectangle in Rectangle::ALL {
            let mut section = test_sections(12).remove(0);
            section.rectangle = rectangle;
            config.sections.push(section);
        }
        let mut ui = Ui::default();
        let rects = frame::layout(1200, 800, [64; 4], 12);
        for rect in &rects {
            let Some(rectangle) = rectangle_for(rect.part) else {
                continue;
            };
            let mut seen = std::collections::HashSet::new();
            for _ in 0..30 {
                let hits = layout(rect.w, rect.h, &ui, &config.sections, rectangle);
                for hit in &hits {
                    assert!(hit.x + hit.w <= rect.w && hit.y + hit.h <= rect.h);
                    assert_eq!(
                        hit_test(
                            &hits,
                            (hit.x + hit.w / 2) as f64,
                            (hit.y + hit.h / 2) as f64
                        ),
                        Some(hit.action)
                    );
                    if let Action::Open(index) = hit.action {
                        seen.insert(index);
                    }
                }
                scroll_rectangle(rect.w, rect.h, &mut ui, &config.sections, rectangle, 1);
            }
            assert_eq!(seen.len(), 12, "unreachable sites in {rectangle:?}");
            let last = ui.rectangle_scroll[rectangle as usize];
            scroll_rectangle(rect.w, rect.h, &mut ui, &config.sections, rectangle, -1);
            assert_eq!(
                ui.rectangle_scroll[rectangle as usize],
                last.saturating_sub(1)
            );
        }
        let hits = frame_layout(1200, 800, [64; 4], 12, &ui, &config.sections);
        for rect in rects.iter().filter(|r| rectangle_for(r.part).is_none()) {
            let action = hit_test(
                &hits,
                (rect.x + rect.w / 2) as f64,
                (rect.y + rect.h / 2) as f64,
            );
            assert_eq!(
                action,
                (rect.part == Part::BottomLeft).then_some(Action::ToggleSettings)
            );
        }
    }

    #[test]
    fn contents_controls_are_reachable_on_scaled_surfaces_and_later_pages() {
        let mut config = Config::default();
        let mut ui = Ui {
            contents_tab: true,
            ..Ui::default()
        };
        ui.editor.select_rectangle(Rectangle::Left, &config);
        for _ in 0..7 {
            ui.editor.apply(Action::AddSection, &mut config);
        }
        for _ in 0..8 {
            ui.editor.apply(Action::AddWebsite, &mut config);
            ui.editor.url.text = "https://example.org".into();
            ui.editor.apply(Action::SaveWebsite, &mut config);
        }
        for (width, height) in [(SETTINGS_WIDTH, SETTINGS_HEIGHT), (320, 400)] {
            let hits = settings_hits(width, height, &ui, &config);
            assert!(hits.iter().any(|h| h.action == Action::SectionPage(-1)));
            assert!(hits.iter().any(|h| h.action == Action::WebsitePage(-1)));
            assert!(hits.iter().any(|h| h.action == Action::SaveWebsite));
            for hit in &hits {
                assert!(hit.x + hit.w <= width && hit.y + hit.h <= height);
                assert_eq!(
                    hit_test(
                        &hits,
                        (hit.x + hit.w / 2) as f64,
                        (hit.y + hit.h / 2) as f64
                    ),
                    Some(hit.action)
                );
            }
        }
    }

    #[test]
    fn keyboard_selection_starts_at_first_and_stops_at_ends() {
        let mut ui = Ui::default();
        ui.hover = Some(Action::ToggleSection(0));
        ui.collapsed = vec![0];
        ui.select_first(3);
        assert!(ui.collapsed.is_empty());
        assert_eq!(ui.hover, None);
        assert_eq!(ui.selected, Some(0));

        let hits = layout(260, 800, &ui, &test_sections(3), Rectangle::Left);
        assert_eq!(ui.selected_action(&hits), Some(Action::Open(0)));

        ui.move_selection(&hits, false);
        assert_eq!(ui.selected, Some(0));
        ui.move_selection(&hits, true);
        assert_eq!(ui.selected, Some(1));
        ui.move_selection(&hits, true);
        assert_eq!(ui.selected, Some(2));
        ui.move_selection(&hits, true);
        assert_eq!(ui.selected, Some(2));
        assert_eq!(ui.selected_action(&hits), Some(Action::Open(2)));

        ui.collapsed = vec![0];
        let collapsed = layout(260, 800, &ui, &test_sections(3), Rectangle::Left);
        ui.move_selection(&collapsed, true);
        assert_eq!(ui.selected, Some(2));
        assert_eq!(ui.selected_action(&collapsed), None);

        ui.select_first(0);
        assert_eq!(ui.selected, None);
        assert_eq!(ui.selected_action(&hits), None);
    }

    #[test]
    fn settings_launch_only_from_bottom_left_and_all_controls_are_reachable() {
        let ui = Ui::default();
        for (width, height) in [(1560, 1000), (800, 600)] {
            let hits = frame_layout(
                width,
                height,
                [64, 80, 48, 56],
                12,
                &ui,
                &test_sections(100),
            );
            let launcher = hits
                .iter()
                .find(|hit| hit.action == Action::ToggleSettings)
                .unwrap();
            assert_eq!((launcher.x, launcher.y), (12, height - 12 - 56));
            assert_eq!((launcher.w, launcher.h), (64, 56));
            assert_eq!(
                hits.iter()
                    .filter(|h| h.action == Action::ToggleSettings)
                    .count(),
                1
            );
            assert!(layout(200, 600, &ui, &test_sections(100), Rectangle::Left)
                .iter()
                .all(|h| h.action != Action::ToggleSettings));
            assert_eq!(
                hit_test(&hits, f64::from(launcher.x + 4), f64::from(launcher.y + 4)),
                Some(Action::ToggleSettings)
            );
            assert_eq!(
                hit_test(&hits, f64::from(width / 2), f64::from(height / 2)),
                None
            );
        }
        for (width, height) in [(SETTINGS_WIDTH, SETTINGS_HEIGHT), (320, 400)] {
            let hits = settings_hits(width, height, &ui, &Config::default());
            assert_eq!(hits.len(), 34);
            for delta in [-1, 1] {
                assert!(hits
                    .iter()
                    .any(|hit| hit.action == Action::BumpPersistence(delta)));
            }
            for mode in [
                PanelAnimation::DesktopDefault,
                PanelAnimation::FadeIn,
                PanelAnimation::Instant,
            ] {
                assert!(hits
                    .iter()
                    .any(|h| h.action == Action::SetPanelAnimation(mode)));
            }
            for delta in [-25, 25] {
                assert!(hits
                    .iter()
                    .any(|h| h.action == Action::BumpFadeDuration(delta)));
            }
            for delta in [-5, 5] {
                assert!(hits
                    .iter()
                    .any(|h| h.action == Action::BumpStyle(StyleSetting::DesktopDim, delta)));
            }
            for hit in &hits {
                assert_eq!(
                    hit_test(
                        &hits,
                        f64::from(hit.x + hit.w / 2),
                        f64::from(hit.y + hit.h / 2)
                    ),
                    Some(hit.action)
                );
                assert!(hit.x + hit.w <= width);
                assert!(hit.y + hit.h <= height);
            }
            for setting in [
                StyleSetting::LeftWidth,
                StyleSetting::RightWidth,
                StyleSetting::TopHeight,
                StyleSetting::BottomHeight,
            ] {
                assert!(hits
                    .iter()
                    .any(|h| h.action == Action::BumpStyle(setting, 4)));
            }
            for theme in [Theme::Light, Theme::Dark, Theme::System] {
                assert!(hits.iter().any(|h| h.action == Action::SetTheme(theme)));
            }
        }
        assert!(layout(4, 4, &ui, &test_sections(100), Rectangle::Left).is_empty());
    }

    #[test]
    fn rounded_opacity_mask_preserves_premultiplied_alpha() {
        let (w, h) = (80, 60);
        for (radius, opacity) in [(0, 100), (16, 100), (16, 50), (48, 20)] {
            let mut canvas = vec![0u8; (w * h * 4) as usize];
            fill(&mut canvas, w as usize * 4, 0, 0, w, h, 0xFF80A0C0);
            finish_surface(&mut canvas, w, h, radius, opacity);
            let center = ((h / 2 * w + w / 2) * 4) as usize;
            assert_eq!(
                canvas[center + 3],
                (255.0 * opacity as f32 / 100.0).round() as u8
            );
            if radius > 0 {
                assert_eq!(&canvas[..4], &[0, 0, 0, 0]);
            } else {
                assert_eq!(canvas[3], 255);
            }
            assert!(canvas
                .chunks_exact(4)
                .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
            for y in 0..h {
                let left = ((y * w) * 4) as usize;
                let right = ((y * w + w - 1) * 4) as usize;
                assert_eq!(canvas[left + 3], canvas[right + 3]);
            }
        }
    }

    #[test]
    fn dimming_is_isolated_from_the_transparent_browser_overlay() {
        let ui = Ui::default();
        let mut content = Content {
            config: &Config {
                sections: vec![],
                ..Config::default()
            },
            labels: &[],
            icons: &[],
            font: None,
            browser_width: 1280,
            browser_height: 800,
            theme: Theme::Light,
            palette: Palette::new(false),
            shortcut: "Meta+F",
            margin: 12,
            border_radius: 16,
            opacity: 100,
            desktop_dim: 50,
            panel_animation: PanelAnimation::DesktopDefault,
            fade_duration_ms: 150,
            dimensions: [48, 48, 40, 40],
        };
        let (w, h) = (320, 240);
        let mut canvas = vec![0; (w * h * 4) as usize];
        let pixel = |canvas: &[u8], x: u32, y: u32| -> [u8; 4] {
            let i = ((y * w + x) * 4) as usize;
            canvas[i..i + 4].try_into().unwrap()
        };
        for (dim, alpha) in [(0, 0), (20, 51), (50, 128), (100, 255), (u32::MAX, 255)] {
            content.desktop_dim = dim;
            draw_backdrop(&mut canvas, dim);
            assert!(canvas.chunks_exact(4).all(|p| p == [0, 0, 0, alpha]));
            draw_frame(&mut canvas, w, h, &ui, &content);
            // Everything above a browser except the cards must stay transparent,
            // even with 100% dimming. This is the stacking regression.
            for (x, y) in [(160, 120), (0, 0), (66, 32), (12, 12)] {
                assert_eq!(pixel(&canvas, x, y), [0; 4]);
            }
            assert_eq!(pixel(&canvas, 160, 32), content.palette.bg.to_le_bytes());
            let hits = frame_layout(w, h, content.dimensions, content.margin, &ui, &[]);
            assert_eq!(hit_test(&hits, 160.0, 120.0), None);
        }
        content.opacity = 50;
        draw_frame(&mut canvas, w, h, &ui, &content);
        assert_eq!(pixel(&canvas, 160, 32)[3], 128);
    }

    /// Renders a realistic panel (websites expanded, fetched icon +
    /// placeholder globe, hover on the first row) and writes it to
    /// `/tmp/panel_render.png` for visual inspection.
    #[test]
    fn render_sample_panel_png() {
        let font = TextRenderer::load().expect("a system font");
        let labels = vec![
            "localhost:6005".to_string(),
            "Very Long Label That Should Ellipsize".to_string(),
        ];

        // A red icon with transparent corners exercises alpha blending.
        let mut red = vec![0u8; 16 * 16 * 4];
        for (i, px) in red.chunks_exact_mut(4).enumerate() {
            let corner = i % 16 < 2 && i / 16 < 2;
            px[..3].copy_from_slice(&[230, 70, 70]);
            px[3] = if corner { 0 } else { 255 };
        }
        let icons: Vec<Option<Icon>> = vec![
            Some(Icon {
                w: 16,
                h: 16,
                rgba: red,
            }),
            None,
        ];

        let ui = Ui {
            collapsed: Vec::new(),
            rectangle_scroll: [0; 4],
            contents_tab: false,
            editor: Editor::default(),
            settings_open: true,
            hover: Some(Action::Open(0)),
            ..Ui::default()
        };
        let content = Content {
            config: &Config::default(),
            labels: &labels,
            icons: &icons,
            font: Some(&font),
            browser_width: 1280,
            browser_height: 800,
            theme: Theme::System,
            palette: Palette::new(true),
            shortcut: "Meta+F",
            margin: 12,
            border_radius: 16,
            opacity: 100,
            desktop_dim: 20,
            panel_animation: PanelAnimation::DesktopDefault,
            fade_duration_ms: 150,
            dimensions: [64; 4],
        };

        let (width, height) = (260, 240);
        let mut canvas = vec![0u8; (width * height * 4) as usize];
        draw_rectangle(&mut canvas, width, height, &ui, &content, Rectangle::Left);

        // The canvas is premultiplied BGRA; PNG wants straight RGBA.
        let mut rgba = Vec::with_capacity(canvas.len());
        for px in canvas.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        image::save_buffer(
            "/tmp/panel_render.png",
            &rgba,
            width,
            height,
            image::ColorType::Rgba8,
        )
        .expect("failed to write /tmp/panel_render.png");

        // Check the full frame, including pixels the input region leaves to
        // the desktop, rather than only the legacy website-list renderer.
        let (w, h) = (1200, 800);
        let mut frame = vec![0; (w * h * 4) as usize];
        draw_frame(
            &mut frame,
            w,
            h,
            &ui,
            &Content {
                desktop_dim: 0,
                ..content
            },
        );
        let rects = frame::layout(w, h, content.dimensions, content.margin);
        for rect in &rects {
            let center = (((rect.y + rect.h / 2) * w + rect.x + rect.w / 2) * 4) as usize;
            assert_eq!(
                frame[center + 3],
                255,
                "missing frame piece {:?}",
                rect.part
            );
        }
        for y in 0..h {
            for x in 0..w {
                if !rects
                    .iter()
                    .any(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
                {
                    assert_eq!(frame[((y * w + x) * 4 + 3) as usize], 0);
                }
            }
        }
        let rgba: Vec<u8> = frame
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        image::save_buffer(
            "/tmp/frame_render.png",
            &rgba,
            w,
            h,
            image::ColorType::Rgba8,
        )
        .unwrap();

        let mut editor_ui = ui.clone();
        editor_ui.contents_tab = true;
        editor_ui
            .editor
            .select_rectangle(Rectangle::Left, content.config);
        editor_ui.editor.focus = Some(Field::WebsiteUrl);
        let mut editor_canvas = vec![0; (SETTINGS_WIDTH * SETTINGS_HEIGHT * 4) as usize];
        draw_settings(
            &mut editor_canvas,
            SETTINGS_WIDTH,
            SETTINGS_HEIGHT,
            &editor_ui,
            &content,
        );
        let rgba: Vec<u8> = editor_canvas
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        image::save_buffer(
            "/tmp/settings_contents.png",
            &rgba,
            SETTINGS_WIDTH,
            SETTINGS_HEIGHT,
            image::ColorType::Rgba8,
        )
        .unwrap();

        for dark in [false, true] {
            let content = Content {
                palette: Palette::new(dark),
                ..content
            };
            let (w, h) = (SETTINGS_WIDTH, SETTINGS_HEIGHT);
            let mut settings = vec![0u8; (w * h * 4) as usize];
            draw_settings(&mut settings, w, h, &ui, &content);
            let center = ((h / 2 * w + w / 2) * 4) as usize;
            assert_eq!(settings[center + 3], 255);
            assert_eq!(settings[3], 0);
            let rgba: Vec<u8> = settings
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                .collect();
            image::save_buffer(
                format!("/tmp/settings_{}.png", if dark { "dark" } else { "light" }),
                &rgba,
                w,
                h,
                image::ColorType::Rgba8,
            )
            .unwrap();
        }

        // Sanity: content beyond the flat background exists.
        let bg = content.palette.bg.to_le_bytes();
        let painted = canvas
            .chunks_exact(4)
            .filter(|px| px[..3] != bg[..3])
            .count();
        assert!(
            painted > 500,
            "expected text/icon/hover pixels, got {painted}"
        );
    }
}
