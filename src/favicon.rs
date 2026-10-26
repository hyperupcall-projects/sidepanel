//! Favicon fetching for the configured websites.
//!
//! Background threads walk new URLs at startup and after content edits:
//!
//!   1. GET the page (capped) and look for `<link rel="icon" href="...">` —
//!      dev servers and SPAs usually declare their icon there;
//!   2. fall back to `<origin>/favicon.ico`.
//!
//! The icon is decoded (PNG/ICO/JPEG/GIF/WebP via `image`, SVG via `resvg`),
//! scaled down to [`ICON`] and reported back through a calloop channel so the
//! panel can repaint as icons arrive. Icons are stored **premultiplied**,
//! matching Wayland's ARGB buffer format.

use std::sync::Arc;
use std::time::Duration;

use image::imageops::FilterType;
use url::Url;

/// Favicon box size in pixels (the panel draws it inside a square of this size).
pub const ICON: u32 = 16;

/// A decoded icon: RGBA8 pixels, **premultiplied** alpha, row-major.
#[derive(Clone)]
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Result reported by the fetch thread.
pub enum Event {
    Ready { url: String, icon: Option<Icon> },
}

const PAGE_LIMIT: u64 = 512 * 1024;
const ICON_LIMIT: u64 = 2 * 1024 * 1024;

/// Fetch favicons for `sites` (parallel to the configured list) in the
/// background, sending one [`Event::Ready`] per site through `tx`.
pub fn spawn(sites: Vec<String>, tx: calloop::channel::Sender<Event>) {
    let result = std::thread::Builder::new()
        .name("favicons".into())
        .spawn(move || {
            let agent = agent();
            for url in &sites {
                let icon = fetch(&agent, url);
                if tx
                    .send(Event::Ready {
                        url: url.clone(),
                        icon,
                    })
                    .is_err()
                {
                    return; // panel is gone
                }
            }
        });
    if let Err(err) = result {
        log::warn!("favicon thread not started: {err}");
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into()
}

fn get_bytes(agent: &ureq::Agent, url: &Url, limit: u64) -> Option<Vec<u8>> {
    let mut response = agent.get(url.as_str()).call().ok()?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .ok()
}

/// Find the favicon for a site URL.
fn fetch(agent: &ureq::Agent, page: &str) -> Option<Icon> {
    let base = Url::parse(page).ok()?;
    let mut candidates: Vec<Url> = Vec::new();

    if let Some(bytes) = get_bytes(agent, &base, PAGE_LIMIT) {
        let html = String::from_utf8_lossy(&bytes);
        if let Some(href) = find_icon_href(&html) {
            match base.join(&href) {
                Ok(url) => candidates.push(url),
                Err(err) => log::debug!("bad icon href {href:?} for {base}: {err}"),
            }
        }
    }
    if let Ok(url) = base.join("/favicon.ico") {
        candidates.push(url);
    }

    for url in candidates {
        if let Some(bytes) = get_bytes(agent, &url, ICON_LIMIT) {
            if let Some(icon) = decode(&bytes) {
                log::debug!("favicon for {base} from {url}");
                return Some(icon);
            }
        }
    }
    log::debug!("no favicon found for {base}");
    None
}

/// Scan HTML for `<link ... rel="...icon..." href="...">` (case-insensitive).
/// Returns the `href` with its original casing intact.
fn find_icon_href(html: &str) -> Option<String> {
    // `to_ascii_lowercase` preserves byte positions, so slices of `lower`
    // can be re-cut from the original `html`.
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("<link") {
        let start = from + found;
        let end = lower[start..]
            .find('>')
            .map_or(lower.len(), |e| start + e + 1);
        let tag_l = &lower[start..end];
        let tag = &html[start..end];
        if attr(tag_l, "rel").is_some_and(|rel| rel.contains("icon")) {
            if let Some(href) = attr(tag, "href") {
                return Some(href.to_string());
            }
        }
        from = end.max(start + 5);
    }
    None
}

/// Value of `name="..."` (or `name='...'` / unquoted) inside `tag`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let at = tag.find(name)?;
    // Attribute names start at the tag start or after whitespace.
    if at > 0 && !tag.as_bytes()[at - 1].is_ascii_whitespace() {
        return None;
    }
    let rest = tag[at + name.len()..].strip_prefix('=')?.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        rest.find('"').map(|end| &rest[..end])
    } else if let Some(rest) = rest.strip_prefix('\'') {
        rest.find('\'').map(|end| &rest[..end])
    } else {
        rest.find(|c: char| c.is_whitespace() || c == '>')
            .map_or(Some(rest), |end| Some(&rest[..end]))
    }
}

fn decode(bytes: &[u8]) -> Option<Icon> {
    if looks_like_svg(bytes) {
        if let Some(icon) = decode_svg(bytes) {
            return Some(icon);
        }
    }
    decode_raster(bytes)
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let Some(start) = bytes.iter().position(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    let head = &bytes[start..];
    head.starts_with(b"<svg")
        || head.starts_with(b"<?xml")
        || head.starts_with(b"<!--")
        || head.starts_with(b"<!DOCTYPE svg")
}

fn decode_raster(bytes: &[u8]) -> Option<Icon> {
    let img = image::load_from_memory(bytes).ok()?;
    // Preserve aspect: favicons are usually square but not always.
    let img = img.resize(ICON, ICON, FilterType::Triangle);
    let mut rgba = img.to_rgba8().into_raw();
    premultiply(&mut rgba);
    let (w, h) = (img.width(), img.height());
    Some(Icon { w, h, rgba })
}

fn decode_svg(bytes: &[u8]) -> Option<Icon> {
    let mut opt = resvg::usvg::Options::default();
    Arc::make_mut(&mut opt.fontdb).load_system_fonts();
    let tree = resvg::usvg::Tree::from_data(bytes, &opt).ok()?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    // Fit into the icon box, centred.
    let scale = (ICON as f32 / sw).min(ICON as f32 / sh);
    let ts = resvg::tiny_skia::Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        (ICON as f32 - sw * scale) / 2.0,
        (ICON as f32 - sh * scale) / 2.0,
    );
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON, ICON)?;
    resvg::render(&tree, ts, &mut pixmap.as_mut());
    // tiny-skia pixels are premultiplied RGBA — exactly what `Icon` stores.
    Some(Icon {
        w: ICON,
        h: ICON,
        rgba: pixmap.data().to_vec(),
    })
}

/// Straight -> premultiplied RGBA in place.
fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        for channel in &mut px[..3] {
            *channel = (u16::from(*channel) * a / 255) as u8;
        }
    }
}
