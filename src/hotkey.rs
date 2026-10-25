//! Global shortcut registration via KDE's KGlobalAccel D-Bus service.
//!
//! On Wayland clients cannot grab global keys themselves — the compositor owns
//! the keyboard. KGlobalAccel is KDE's broker: we register a *component* and an
//! *action*, KWin performs the actual key grab, and the daemon emits
//! `globalShortcutPressed` when the user presses the key. This is the native
//! KDE mechanism and it works on Wayland.
//!
//! The registered shortcut also shows up in System Settings → Shortcuts, so the
//! user can rebind it without touching our config file.

use anyhow::{anyhow, Context, Result};
use calloop::channel::Sender;
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers};
use std::sync::mpsc;

use crate::config::Config;

/// Component (application) id used with KGlobalAccel.
pub const COMPONENT: &str = "sidepanel";
/// Action id used with KGlobalAccel.
pub const ACTION: &str = "toggle";
const COMPONENT_FRIENDLY: &str = "Side Panel";
const ACTION_FRIENDLY: &str = "Toggle Side Panel";

/// `KGlobalAccelD::SetShortcutFlag` values (see `kglobalacceld.h`).
///
/// `SetPresent` marks the shortcut as active. `IsDefault` records it as the
/// action's default binding (what "Reset" in System Settings restores).
const SET_PRESENT: u32 = 0x2;
const IS_DEFAULT: u32 = 0x8;
const NO_AUTOLOADING: u32 = 0x4;

/// Messages sent from the D-Bus thread to the Wayland event loop.
#[derive(Debug)]
pub enum HotkeyEvent {
    Toggle,
    Registered(String),
    Rebound(std::result::Result<String, String>),
}

#[zbus::proxy(
    interface = "org.kde.KGlobalAccel",
    default_service = "org.kde.kglobalaccel",
    default_path = "/kglobalaccel"
)]
trait KGlobalAccel {
    // KGlobalAccel uses camelCase D-Bus names; zbus would otherwise PascalCase
    // the Rust method names, so spell them out explicitly.
    #[zbus(name = "doRegister")]
    fn do_register(&self, action_id: &[&str]) -> zbus::Result<()>;

    // The D-Bus signature is `a(ai)` — an array of structs each holding one
    // `QKeySequence`. A 1-tuple gives us that struct wrapper; `Vec<Vec<i32>>`
    // would serialise as `aai` and be rejected. Each inner array must hold
    // exactly four ints; see `encode_sequence`.
    #[zbus(name = "setShortcutKeys")]
    fn set_shortcut_keys(
        &self,
        action_id: &[&str],
        keys: &[(Vec<i32>,)],
        flags: u32,
    ) -> zbus::Result<Vec<(Vec<i32>,)>>;

    #[zbus(name = "globalShortcutAvailable")]
    fn shortcut_available(&self, key: &(Vec<i32>,), component: &str) -> zbus::Result<bool>;

    #[zbus(name = "shortcutKeys")]
    fn shortcut_keys(&self, action_id: &[&str]) -> zbus::Result<Vec<(Vec<i32>,)>>;

    #[zbus(name = "getComponent")]
    fn get_component(
        &self,
        component_unique: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.kde.kglobalaccel.Component",
    default_service = "org.kde.kglobalaccel"
)]
trait Component {
    #[zbus(name = "globalShortcutPressed", signal)]
    fn global_shortcut_pressed(
        &self,
        component_unique: &str,
        action_unique: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;
}

/// Spawn the background thread that registers the shortcut and forwards presses
/// to `sender` (which feeds the Wayland event loop).
pub fn spawn(sender: Sender<HotkeyEvent>, config: Config) -> Result<mpsc::Sender<String>> {
    let (requests, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("kglobalaccel".into())
        .spawn(move || {
            if let Err(err) = run(sender, &config, receiver) {
                log::error!("global-shortcut listener stopped: {err:#}");
            }
        })
        .context("failed to spawn the global-shortcut thread")?;
    Ok(requests)
}

fn run(
    sender: Sender<HotkeyEvent>,
    config: &Config,
    receiver: mpsc::Receiver<String>,
) -> Result<()> {
    let conn =
        zbus::blocking::Connection::session().context("failed to connect to the session bus")?;
    let kga = KGlobalAccelProxyBlocking::new(&conn)
        .context("KGlobalAccel is unavailable (is kglobalacceld running?)")?;

    let action_id = [COMPONENT, ACTION, COMPONENT_FRIENDLY, ACTION_FRIENDLY];
    kga.do_register(&action_id)
        .context("failed to register the component with KGlobalAccel")?;

    let combined = parse_shortcut(&config.shortcut)?;
    let keys = [(encode_sequence(combined),)];

    // `SetPresent` marks the shortcut active. We deliberately do *not* pass
    // `NoAutoloading`: with autoloading the daemon keeps a binding the user has
    // customised in System Settings and only installs ours when the action has
    // none yet. Either way it returns the binding actually in effect.
    let applied = kga
        .set_shortcut_keys(&action_id, &keys, SET_PRESENT)
        .context("failed to set the global shortcut")?;

    // Also record our binding as the action's default, so it shows up in System
    // Settings → Shortcuts and can be restored there.
    kga.set_shortcut_keys(&action_id, &keys, IS_DEFAULT)
        .context("failed to set the default shortcut")?;

    let path = kga
        .get_component(COMPONENT)
        .context("failed to resolve the component object")?;
    let component = ComponentProxyBlocking::builder(&conn)
        .path(path.as_str())
        .context("invalid component object path")?
        .build()
        .context("failed to build the component proxy")?;

    log::info!(
        "global shortcut '{}' registered (active binding: {})",
        config.shortcut,
        describe(&applied)
    );

    let signals = component
        .receive_global_shortcut_pressed()
        .context("failed to subscribe to globalShortcutPressed")?;
    if let Some(shortcut) = applied.first().and_then(|s| format_sequence(&s.0)) {
        let _ = sender.send(HotkeyEvent::Registered(shortcut));
    }
    // Reuse the registering connection so KGlobalAccel keeps the same owner.
    let worker_conn = conn.clone();
    let worker_sender = sender.clone();
    std::thread::Builder::new()
        .name("shortcut-rebind".into())
        .spawn(move || {
            for shortcut in receiver {
                let result = rebind(&worker_conn, &shortcut)
                    .map(|()| shortcut)
                    .map_err(|err| format!("{err:#}"));
                if worker_sender.send(HotkeyEvent::Rebound(result)).is_err() {
                    break;
                }
            }
        })
        .context("failed to start shortcut updater")?;
    for signal in signals {
        let args = signal.args().context("failed to decode the signal")?;
        if *args.action_unique() == ACTION && sender.send(HotkeyEvent::Toggle).is_err() {
            break;
        }
    }
    Ok(())
}

fn rebind(conn: &zbus::blocking::Connection, shortcut: &str) -> Result<()> {
    let combined = parse_shortcut(shortcut)?;
    let proxy = KGlobalAccelProxyBlocking::new(conn)?;
    let action = [COMPONENT, ACTION, COMPONENT_FRIENDLY, ACTION_FRIENDLY];
    let previous = proxy.shortcut_keys(&action)?;
    let keys = vec![(encode_sequence(combined),)];
    if previous == keys {
        return Ok(());
    }
    anyhow::ensure!(
        proxy.shortcut_available(&keys[0], COMPONENT)?,
        "Shortcut already in use; choose another."
    );
    let applied = proxy.set_shortcut_keys(&action, &keys, SET_PRESENT | NO_AUTOLOADING)?;
    if applied != keys {
        proxy
            .set_shortcut_keys(&action, &previous, SET_PRESENT | NO_AUTOLOADING)
            .context("Shortcut rejected and previous shortcut could not be restored")?;
        anyhow::bail!("Shortcut rejected; previous shortcut restored.");
    }
    Ok(())
}

/// Convert a captured key using the compositor's active layout.
/// Modifier-only presses wait for a key; unsupported/plain keys report an error.
pub fn captured_shortcut(key: Keysym, modifiers: Modifiers) -> Result<Option<String>> {
    let raw = key.raw();
    if (0xffe1..=0xffee).contains(&raw) || raw == 0xff7f {
        return Ok(None);
    }
    anyhow::ensure!(
        modifiers.ctrl || modifiers.alt || modifiers.logo,
        "Use Ctrl, Alt or Meta with a key."
    );
    let name = match raw {
        0x41..=0x5a | 0x61..=0x7a | 0x30..=0x39 => {
            (char::from_u32(raw).unwrap().to_ascii_uppercase()).to_string()
        }
        0x20 => "Space".into(),
        0xff1b => "Escape".into(),
        0xff09 | 0xfe20 => "Tab".into(),
        0xff08 => "Backspace".into(),
        0xff0d => "Return".into(),
        0xffff => "Delete".into(),
        0xff50 => "Home".into(),
        0xff57 => "End".into(),
        0xff51 => "Left".into(),
        0xff52 => "Up".into(),
        0xff53 => "Right".into(),
        0xff54 => "Down".into(),
        0xff55 => "PageUp".into(),
        0xff56 => "PageDown".into(),
        0xffbe..=0xffc9 => format!("F{}", raw - 0xffbe + 1),
        _ => anyhow::bail!("Choose a letter, digit, F1–F12 or navigation key."),
    };
    let mut parts = Vec::new();
    if modifiers.logo {
        parts.push("Meta".to_string());
    }
    if modifiers.ctrl {
        parts.push("Ctrl".to_string());
    }
    if modifiers.alt {
        parts.push("Alt".to_string());
    }
    if modifiers.shift {
        parts.push("Shift".to_string());
    }
    parts.push(name);
    let shortcut = parts.join("+");
    parse_shortcut(&shortcut)?;
    Ok(Some(shortcut))
}

/// Display the actual startup binding returned by KDE, including custom ones.
fn format_sequence(sequence: &[i32]) -> Option<String> {
    let mut chords = Vec::new();
    for &combined in sequence.iter().filter(|&&v| v != 0) {
        let key = combined & 0x01ff_ffff;
        let name = match key {
            0x30..=0x39 | 0x41..=0x5a => char::from_u32(key as u32)?.to_string(),
            0x20 => "Space".into(),
            0x0100_0030..=0x0100_003b => format!("F{}", key - 0x0100_0030 + 1),
            _ => [
                "Escape",
                "Tab",
                "Backspace",
                "Return",
                "Delete",
                "Home",
                "End",
                "Left",
                "Up",
                "Right",
                "Down",
                "PageUp",
                "PageDown",
            ]
            .into_iter()
            .find(|name| key_code(&name.to_ascii_lowercase()) == Some(key))?
            .to_string(),
        };
        let mut parts = Vec::new();
        for (flag, label) in [
            (0x1000_0000, "Meta"),
            (0x0400_0000, "Ctrl"),
            (0x0800_0000, "Alt"),
            (0x0200_0000, "Shift"),
        ] {
            if combined & flag != 0 {
                parts.push(label.to_string());
            }
        }
        parts.push(name);
        chords.push(parts.join("+"));
    }
    if chords.is_empty() {
        None
    } else {
        Some(chords.join(", "))
    }
}

/// Parse a `Modifier+Key` shortcut into a Qt "combined" key code.
///
/// KGlobalAccel speaks Qt's `QKeySequence` encoding: the modifier flags live in
/// the high bits and the key code in the low bits, OR-ed together. See
/// `sequencehelpers_p.cpp` in the kglobalaccel sources.
fn parse_shortcut(spec: &str) -> Result<i32> {
    const SHIFT: i32 = 0x0200_0000;
    const CTRL: i32 = 0x0400_0000;
    const ALT: i32 = 0x0800_0000;
    const META: i32 = 0x1000_0000;

    let mut modifiers = 0i32;
    let mut key: Option<i32> = None;

    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "meta" | "super" | "logo" | "win" => modifiers |= META,
            "ctrl" | "control" => modifiers |= CTRL,
            "alt" => modifiers |= ALT,
            "shift" => modifiers |= SHIFT,
            other => {
                anyhow::ensure!(
                    key.is_none(),
                    "shortcut must contain exactly one non-modifier key"
                );
                key =
                    Some(key_code(other).ok_or_else(|| {
                        anyhow!("unsupported key '{other}' in shortcut '{spec}'")
                    })?);
            }
        }
    }

    let key = key.ok_or_else(|| anyhow!("shortcut '{spec}' does not contain a key"))?;
    Ok(modifiers | key)
}

/// Map a key name to its Qt `Qt::Key` code.
fn key_code(name: &str) -> Option<i32> {
    // Single ASCII letter or digit.
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as i32);
        }
        if c.is_ascii_digit() {
            return Some(c as i32);
        }
    }

    Some(match name {
        "space" => 0x20,
        "escape" | "esc" => 0x0100_0000,
        "tab" => 0x0100_0001,
        "backspace" => 0x0100_0003,
        "return" | "enter" => 0x0100_0004,
        "delete" | "del" => 0x0100_0007,
        "home" => 0x0100_0010,
        "end" => 0x0100_0011,
        "left" => 0x0100_0012,
        "up" => 0x0100_0013,
        "right" => 0x0100_0014,
        "down" => 0x0100_0015,
        "pageup" => 0x0100_0016,
        "pagedown" => 0x0100_0017,
        "f1" => 0x0100_0030,
        "f2" => 0x0100_0031,
        "f3" => 0x0100_0032,
        "f4" => 0x0100_0033,
        "f5" => 0x0100_0034,
        "f6" => 0x0100_0035,
        "f7" => 0x0100_0036,
        "f8" => 0x0100_0037,
        "f9" => 0x0100_0038,
        "f10" => 0x0100_0039,
        "f11" => 0x0100_003a,
        "f12" => 0x0100_003b,
        _ => return None,
    })
}

/// Encode a Qt "combined" key code as a `QKeySequence` for the D-Bus wire.
///
/// KGlobalAccel marshals a `QKeySequence` as a struct wrapping an array of
/// **exactly four** int32s — a chord of up to four keys, zero-padded. Qt's
/// demarshaller reads all four unconditionally (it never checks for the end of
/// the array), so a shorter array makes the receiver read past the end and abort
/// the whole process on a libdbus assertion. Always pad to four.
fn encode_sequence(combined: i32) -> Vec<i32> {
    vec![combined, 0, 0, 0]
}

/// Render the keys returned by `setShortcutKeys` for logging.
fn describe(applied: &[(Vec<i32>,)]) -> String {
    if applied.is_empty() {
        return "none".to_string();
    }
    applied
        .iter()
        .map(|(seq,)| match seq.first() {
            Some(code) => format!("0x{code:08x}"),
            None => "none".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{serialized::Context, to_bytes, Endian, Type};

    #[test]
    fn meta_f_parses_to_the_expected_combined_code() {
        assert_eq!(parse_shortcut("Meta+F").unwrap(), 0x1000_0046);
    }

    #[test]
    fn captures_and_formats_supported_shortcuts() {
        let modifiers = Modifiers {
            logo: true,
            ctrl: true,
            alt: true,
            shift: true,
            ..Modifiers::default()
        };
        for raw in [
            0x61, 0x5a, 0x31, 0x20, 0xff09, 0xfe20, 0xff08, 0xff0d, 0xffff, 0xff50, 0xff57, 0xff51,
            0xff52, 0xff53, 0xff54, 0xff55, 0xff56, 0xffbe, 0xffc9, 0xff1b,
        ] {
            let captured = captured_shortcut(Keysym::new(raw), modifiers)
                .unwrap()
                .unwrap();
            let combined = parse_shortcut(&captured).unwrap();
            assert_eq!(format_sequence(&encode_sequence(combined)), Some(captured));
        }
        let captured = captured_shortcut(
            Keysym::new(0x66),
            Modifiers {
                logo: true,
                ..Modifiers::default()
            },
        )
        .unwrap();
        assert_eq!(captured.as_deref(), Some("Meta+F"));
    }

    #[test]
    fn recorder_waits_for_modifiers_and_rejects_plain_or_unsupported_keys() {
        for raw in 0xffe1..=0xffee {
            assert_eq!(
                captured_shortcut(Keysym::new(raw), Modifiers::default()).unwrap(),
                None
            );
        }
        assert!(captured_shortcut(Keysym::new(0x61), Modifiers::default()).is_err());
        assert!(captured_shortcut(
            Keysym::new(0x61),
            Modifiers {
                shift: true,
                ..Modifiers::default()
            }
        )
        .is_err());
        assert!(captured_shortcut(
            Keysym::new(0x1008ff12),
            Modifiers {
                logo: true,
                ..Modifiers::default()
            }
        )
        .is_err());
        assert!(parse_shortcut("Meta+A+B").is_err());
        assert!(parse_shortcut("Meta").is_err());
    }

    #[test]
    fn sequence_is_padded_to_four_ints() {
        assert_eq!(encode_sequence(0x1000_0046), vec![0x1000_0046, 0, 0, 0]);
    }

    #[test]
    fn keys_argument_has_the_a_ai_signature() {
        assert_eq!(<(Vec<i32>,) as Type>::SIGNATURE, "(ai)");
        assert_eq!(<Vec<(Vec<i32>,)> as Type>::SIGNATURE, "a(ai)");
    }

    /// Regression test for the KWin crash: Qt's `QKeySequence` demarshaller reads
    /// exactly four ints, so the wire form must always carry four.
    #[test]
    fn keys_round_trip_with_four_ints() {
        let keys: Vec<(Vec<i32>,)> = vec![(encode_sequence(0x1000_0046),)];
        let ctxt = Context::new_dbus(Endian::Little, 0);
        let data = to_bytes(ctxt, &keys).unwrap();
        let (decoded, _): (Vec<(Vec<i32>,)>, usize) = data.deserialize().unwrap();
        assert_eq!(decoded, keys);
        assert_eq!(decoded[0].0.len(), 4);
    }
}
