//! Runtime configuration, loaded from `$XDG_CONFIG_HOME/sidepanel/config.toml`.
//!
//! Every field is optional; missing files or fields fall back to the defaults
//! below, so the app runs out of the box.

use std::path::PathBuf;

use serde::Deserialize;

/// Which screen edge the panel is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Screen edge to anchor the panel to.
    pub side: Side,
    /// Panel width in logical pixels. Height always fills the screen.
    pub width: u32,
    /// Global shortcut, in a `Modifier+Key` form, e.g. `"Meta+F"`.
    pub shortcut: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            side: Side::Left,
            width: 150,
            shortcut: "Meta+F".to_string(),
        }
    }
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
        match toml::from_str(&text) {
            Ok(config) => config,
            Err(err) => {
                log::warn!("ignoring invalid config at {}: {err}", path.display());
                Self::default()
            }
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("sidepanel").join("config.toml"))
}
