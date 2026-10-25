//! Runtime configuration, loaded from `$XDG_CONFIG_HOME/sidepanel/config.toml`.
//!
//! Every field is optional; missing files or fields fall back to the defaults
//! below, so the app runs out of the box. The panel's Settings UI writes
//! changes back with [`Config::save`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

/// Saved appearance preference. System follows the desktop portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

impl Theme {
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::Light => false,
            Self::Dark => true,
            Self::System => system_dark,
        }
    }
}

/// One entry of the panel's "Websites" drop-down.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Website {
    /// URL opened in the browser window.
    pub url: String,
    /// Optional label shown in the panel; falls back to the URL's host
    /// (plus explicit port), e.g. `localhost:6005`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl Website {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            title: None,
        }
    }

    /// Text shown next to the favicon in the panel.
    pub fn label(&self) -> String {
        if let Some(title) = &self.title {
            if !title.is_empty() {
                return title.clone();
            }
        }
        match Url::parse(&self.url) {
            Ok(url) => {
                let host = url.host_str().unwrap_or(&self.url).to_string();
                match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host,
                }
            }
            Err(_) => self.url.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Independent edge thicknesses, in logical pixels. The old `width` key
    /// remains readable as the left panel width.
    #[serde(alias = "width")]
    pub left_width: u32,
    pub right_width: u32,
    pub top_height: u32,
    pub bottom_height: u32,
    /// Shared gap between every shape and between shapes and the screen edges.
    pub margin: u32,
    /// Corner radius of every frame piece and the settings card.
    pub border_radius: u32,
    /// Panel opacity as a percentage (100 is fully opaque).
    pub opacity: u32,
    /// Black backdrop strength while the launcher is visible (0 disables it).
    pub desktop_dim: u32,
    /// Global shortcut, in a `Modifier+Key` form, e.g. `"Meta+F"`.
    pub shortcut: String,
    /// Default browser window width (see the panel's Settings UI).
    pub browser_width: u32,
    /// Default browser window height.
    pub browser_height: u32,
    /// Appearance: light, dark, or system.
    pub theme: Theme,
    /// Sites listed in the "Websites" drop-down, in order.
    ///
    /// Declared last: TOML requires scalar values to precede tables when the
    /// config is written back out.
    pub websites: Vec<Website>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Slim rails and square corner boxes, matching the frame layout.
            left_width: 64,
            right_width: 64,
            top_height: 64,
            bottom_height: 64,
            margin: 12,
            border_radius: 16,
            opacity: 100,
            desktop_dim: 20,
            shortcut: "Meta+F".to_string(),
            browser_width: 1280,
            browser_height: 800,
            theme: Theme::System,
            websites: vec![
                Website::new("http://localhost:6005/"),
                Website::new("http://localhost:6006/"),
            ],
        }
    }
}

/// Bounds for the browser window size (the Settings UI steps within
/// these; the helper clamps with them too).
pub fn clamp_browser_width(v: u32) -> u32 {
    v.clamp(640, 3840)
}

pub fn clamp_browser_height(v: u32) -> u32 {
    v.clamp(400, 2160)
}

/// Apply a Settings step (signed) with clamping.
pub fn step_browser_width(current: u32, delta: i32) -> u32 {
    (i64::from(current) + i64::from(delta)).clamp(640, 3840) as u32
}

pub fn step_browser_height(current: u32, delta: i32) -> u32 {
    (i64::from(current) + i64::from(delta)).clamp(400, 2160) as u32
}

impl Config {
    /// Load the config, falling back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut config) => {
                config.left_width = config.left_width.clamp(48, 800);
                config.right_width = config.right_width.clamp(48, 800);
                config.top_height = config.top_height.clamp(40, 400);
                config.bottom_height = config.bottom_height.clamp(40, 400);
                config.margin = config.margin.min(80);
                config.border_radius = config.border_radius.min(48);
                config.opacity = config.opacity.clamp(20, 100);
                config.desktop_dim = config.desktop_dim.min(100);
                config.browser_width = clamp_browser_width(config.browser_width);
                config.browser_height = clamp_browser_height(config.browser_height);
                config
            }
            Err(err) => {
                log::warn!("ignoring invalid config at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Persist the current configuration (creating the directory if needed).
    ///
    /// Note: this rewrites the whole file, so hand-written comments in it are
    /// not preserved.
    pub fn save(&self) -> Result<()> {
        let path = config_path().context("could not determine the config path")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("could not serialize the config")?;
        std::fs::write(&path, text)
            .with_context(|| format!("could not write {}", path.display()))?;
        log::debug!("config written to {}", path.display());
        Ok(())
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("sidepanel").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Settings UI round-trips the config through TOML; this pins the
    /// serialization (field order, skipped `None`s, array of tables).
    #[test]
    fn default_config_round_trips() {
        let original = Config::default();
        let text = toml::to_string_pretty(&original).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("parse");

        assert_eq!(parsed.left_width, original.left_width);
        assert_eq!(parsed.right_width, original.right_width);
        assert_eq!(parsed.top_height, original.top_height);
        assert_eq!(parsed.bottom_height, original.bottom_height);
        assert_eq!(parsed.desktop_dim, original.desktop_dim);
        assert_eq!(parsed.shortcut, original.shortcut);
        assert_eq!(parsed.browser_width, 1280);
        assert_eq!(parsed.browser_height, 800);
        assert_eq!(parsed.websites.len(), 2);
        assert_eq!(parsed.websites[0].url, "http://localhost:6005/");
        assert_eq!(parsed.websites[1].url, "http://localhost:6006/");
        assert_eq!(parsed.websites[0].title, None);
    }

    #[test]
    fn appearance_defaults_and_custom_values_round_trip() {
        let default: Config = toml::from_str("").unwrap();
        assert_eq!(default.left_width, 64);
        assert_eq!(default.desktop_dim, 20);
        assert_eq!(
            (default.margin, default.border_radius, default.opacity),
            (12, 16, 100)
        );
        let custom: Config =
            toml::from_str("margin = 24\nborder_radius = 32\nopacity = 75\ndesktop_dim = 45")
                .unwrap();
        let parsed: Config = toml::from_str(&toml::to_string(&custom).unwrap()).unwrap();
        assert_eq!(
            (parsed.margin, parsed.border_radius, parsed.opacity),
            (24, 32, 75)
        );
        assert_eq!(parsed.desktop_dim, 45);
    }

    #[test]
    fn independent_edge_sizes_round_trip_and_accept_legacy_width() {
        let old: Config = toml::from_str("side = 'right'\nwidth = 260").unwrap();
        assert_eq!(old.left_width, 260);
        assert_eq!(old.right_width, 64);
        let config: Config = toml::from_str(
            "left_width = 72\nright_width = 96\ntop_height = 48\nbottom_height = 60",
        )
        .unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("\nwidth ="));
        assert!(!text.contains("side ="));
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            (
                parsed.left_width,
                parsed.right_width,
                parsed.top_height,
                parsed.bottom_height
            ),
            (72, 96, 48, 60)
        );
    }

    #[test]
    fn themes_round_trip_and_old_configs_follow_system() {
        assert_eq!(
            toml::from_str::<Config>("width = 260").unwrap().theme,
            Theme::System
        );
        for theme in [Theme::Light, Theme::Dark, Theme::System] {
            let config = Config {
                theme,
                ..Config::default()
            };
            let parsed: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(parsed.theme, theme);
        }
        assert!(!Theme::Light.is_dark(true));
        assert!(Theme::Dark.is_dark(false));
        assert!(Theme::System.is_dark(true));
        assert!(!Theme::System.is_dark(false));
    }

    #[test]
    fn browser_size_is_clamped_and_stepped() {
        assert_eq!(clamp_browser_width(10), 640);
        assert_eq!(clamp_browser_width(99_999), 3840);
        assert_eq!(clamp_browser_height(0), 400);
        assert_eq!(step_browser_width(1280, -40), 1240);
        assert_eq!(step_browser_width(640, -40), 640);
        assert_eq!(step_browser_height(400, -40), 400);
        assert_eq!(step_browser_height(2160, 40), 2160);
        assert_eq!(step_browser_width(1280, -5000), 640);
    }
}
