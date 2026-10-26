//! Expiring session cache. Preferences/bookmarks live separately in config.toml.
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{browser::Snapshot, config::Persistence, panel::Ui};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|path| path.join("sidepanel"))
}

pub fn browser_data_path() -> Option<PathBuf> {
    state_directory().map(|path| path.join("browser-data"))
}

/// Called before starting a helper or after its old WebContext is gone.
pub fn clear_browser_data() {
    if let Some(path) = browser_data_path() {
        remove_browser_data(&path);
    }
}

fn remove_browser_data(path: &std::path::Path) {
    if let Err(err) = std::fs::remove_dir_all(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            log::warn!("could not clear browser session data: {err}");
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Identity {
    pub boot: Option<String>,
    pub login: Option<String>,
}

impl Identity {
    pub fn current() -> Self {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string());
        let login = login_identity()
            .map_err(|err| {
                log::warn!("could not identify desktop login: {err:#}");
            })
            .ok();
        Self { boot, login }
    }
}

/// logind session IDs/timestamps survive screen lock and suspend. Neither
/// LockedHint nor screensaver/idle notifications participate in expiration.
fn login_identity() -> Result<String> {
    let connection = zbus::blocking::connection::Builder::system()?
        .method_timeout(Duration::from_secs(2))
        .build()?;
    let manager = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    let sessions: Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)> =
        manager.call("ListSessions", &())?;
    let preferred = std::env::var("XDG_SESSION_ID").ok();
    let mut candidates = Vec::new();
    for (id, owner, _, _, path) in sessions {
        if owner != uid {
            continue;
        }
        let session = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )?;
        let kind: String = session.get_property("Type")?;
        let state: String = session.get_property("State")?;
        if kind != "wayland" || !matches!(state.as_str(), "active" | "online") {
            continue;
        }
        let timestamp: u64 = session.get_property("Timestamp")?;
        candidates.push((
            preferred.as_ref() == Some(&id),
            state == "active",
            format!("{id}:{timestamp}"),
        ));
    }
    candidates.sort();
    candidates
        .pop()
        .map(|(_, _, id)| id)
        .context("no graphical login session")
}

pub fn watch_identity(sender: calloop::channel::Sender<Identity>) {
    std::thread::spawn(move || loop {
        let identity = Identity::current();
        if sender.send(identity).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub saved_at: u64,
    pub hidden_since: Option<u64>,
    pub identity: Identity,
    pub ui: Ui,
    pub browser: Snapshot,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: 1,
            saved_at: now(),
            hidden_since: None,
            identity: Identity::default(),
            ui: Ui::default(),
            browser: Snapshot::default(),
        }
    }
}

impl Session {
    pub fn expired(&self, policy: Persistence, time: u64, identity: &Identity) -> bool {
        match policy {
            Persistence::Always => false,
            // Writes while hidden must never extend the original deadline.
            Persistence::Minutes(minutes) => {
                time.saturating_sub(self.hidden_since.unwrap_or(self.saved_at))
                    >= u64::from(minutes) * 60
            }
            Persistence::Login => {
                identity.boot.is_none()
                    || identity.login.is_none()
                    || self.identity.boot != identity.boot
                    || self.identity.login != identity.login
            }
            Persistence::Restart => identity.boot.is_none() || self.identity.boot != identity.boot,
        }
    }

    pub fn sanitize(&mut self, config: &crate::config::Config) {
        self.ui.collapsed.retain(|&i| i < config.sections.len());
        self.ui.selected = self.ui.selected.filter(|&i| i < config.websites().count());
        let editor = &mut self.ui.editor;
        editor.section = editor.section.filter(|&i| i < config.sections.len());
        editor.website = editor.website.filter(|&i| {
            editor
                .section
                .is_some_and(|s| i < config.sections[s].websites.len())
        });
        editor.section_page = editor
            .section_page
            .min(editor.sections(config).len().saturating_sub(1) / crate::editor::PAGE_SIZE);
        let website_count = editor
            .section
            .map_or(0, |s| config.sections[s].websites.len());
        editor.website_page = editor
            .website_page
            .min(website_count.saturating_sub(1) / crate::editor::PAGE_SIZE);
        for input in [&mut editor.name, &mut editor.title, &mut editor.url] {
            input.cursor = input.cursor.min(input.text.len());
            while !input.text.is_char_boundary(input.cursor) {
                input.cursor -= 1;
            }
        }
        // WebKit's serialized history is an opaque cache, not an unbounded log.
        if self.browser.history.len() > 8 * 1024 * 1024 {
            self.browser.history.clear();
        }
        self.browser.width = crate::config::clamp_browser_width(self.browser.width);
        self.browser.height = crate::config::clamp_browser_height(self.browser.height);
    }
}

pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            path: state_directory().map(|p| p.join("session.json")),
        }
    }

    pub fn load(&self, policy: Persistence, identity: &Identity) -> Option<Session> {
        let path = self.path.as_ref()?;
        if !path.exists() {
            // A profile without lifetime metadata cannot be safely retained.
            remove_browser_data(&path.parent()?.join("browser-data"));
            return None;
        }
        let loaded = (|| -> Result<Session> {
            anyhow::ensure!(
                std::fs::metadata(path)?.len() <= 32 * 1024 * 1024,
                "session cache too large"
            );
            Ok(serde_json::from_slice(&std::fs::read(path)?)?)
        })();
        match loaded {
            Ok(session) if session.version == 1 && !session.expired(policy, now(), identity) => {
                Some(session)
            }
            Ok(_) => {
                self.clear();
                remove_browser_data(&path.parent()?.join("browser-data"));
                None
            }
            Err(err) => {
                log::warn!("discarding invalid panel session: {err:#}");
                self.clear();
                remove_browser_data(&path.parent()?.join("browser-data"));
                None
            }
        }
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("could not determine session path")?;
        let dir = path.parent().unwrap();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, session)?;
            file.flush()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    pub fn clear(&self) {
        if let Some(path) = &self.path {
            if let Err(err) = std::fs::remove_file(path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("could not clear panel session: {err}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(boot: &str, login: &str) -> Identity {
        Identity {
            boot: Some(boot.into()),
            login: Some(login.into()),
        }
    }

    #[test]
    fn duration_deadline_is_from_hide_and_does_not_slide_with_saves() {
        let mut s = Session {
            saved_at: 100,
            hidden_since: Some(100),
            ..Session::default()
        };
        for policy in Persistence::choices() {
            if let Persistence::Minutes(minutes) = policy {
                let deadline = 100 + u64::from(*minutes) * 60;
                s.saved_at = deadline - 1;
                assert!(!s.expired(*policy, deadline - 1, &Identity::default()));
                assert!(s.expired(*policy, deadline, &Identity::default()));
            }
        }
        assert!(!s.expired(Persistence::Always, u64::MAX, &Identity::default()));
        assert!(!s.expired(Persistence::Minutes(1), 10, &Identity::default()));
    }

    #[test]
    fn login_and_reboot_boundaries_are_distinct_from_same_session_and_process_restart() {
        let original = identity("boot-a", "session-3:100");
        let s = Session {
            identity: original.clone(),
            ..Session::default()
        };
        for policy in [Persistence::Login, Persistence::Restart] {
            // Lock/unlock and daemon restarts retain these identifiers.
            assert!(!s.expired(policy, 5000, &original));
            assert!(s.expired(policy, 5000, &identity("boot-b", "session-3:100")));
        }
        assert!(s.expired(
            Persistence::Login,
            5000,
            &identity("boot-a", "session-4:200")
        ));
        assert!(!s.expired(
            Persistence::Restart,
            5000,
            &identity("boot-a", "session-4:200")
        ));
    }

    #[test]
    fn cache_roundtrip_expiry_and_corruption() {
        let dir =
            std::env::temp_dir().join(format!("sidepanel-session-test-{}", std::process::id()));
        let store = Store {
            path: Some(dir.join("session.json")),
        };
        let mut session = Session {
            identity: identity("boot", "login"),
            ..Session::default()
        };
        session.ui.settings_open = true;
        session.ui.contents_tab = true;
        session.ui.collapsed = vec![1, 2];
        session.ui.rectangle_scroll = [4, 3, 2, 1];
        session.ui.editor.url = crate::editor::TextInput::new("https://example.com/draft".into());
        session.browser.open = true;
        session.browser.url = Some("https://example.com/current".into());
        store.save(&session).unwrap();
        assert_eq!(
            std::fs::metadata(store.path.as_ref().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let loaded = store.load(Persistence::Always, &session.identity).unwrap();
        assert!(loaded.ui.settings_open && loaded.ui.contents_tab && loaded.browser.open);
        assert_eq!(loaded.ui.rectangle_scroll, [4, 3, 2, 1]);
        assert_eq!(loaded.ui.editor.url.text, session.ui.editor.url.text);
        assert_eq!(loaded.browser.url, session.browser.url);
        let profile = dir.join("browser-data");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join("site-storage"), "saved site data").unwrap();
        let preferences = dir.join("config.toml");
        std::fs::write(&preferences, "saved preferences").unwrap();
        assert!(store
            .load(Persistence::Restart, &identity("new-boot", "login"))
            .is_none());
        assert!(!store.path.as_ref().unwrap().exists());
        assert!(!profile.exists(), "expired profiles must be removed");
        assert!(
            preferences.exists(),
            "expiry must retain configured preferences"
        );
        std::fs::write(store.path.as_ref().unwrap(), b"broken").unwrap();
        assert!(store.load(Persistence::Always, &session.identity).is_none());
        assert!(!store.path.as_ref().unwrap().exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
