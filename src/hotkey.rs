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

/// Messages sent from the D-Bus thread to the Wayland event loop.
#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Toggle,
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

    #[zbus(name = "getComponent")]
    fn get_component(&self, component_unique: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
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
pub fn spawn(sender: Sender<HotkeyEvent>, config: Config) -> Result<()> {
    std::thread::Builder::new()
        .name("kglobalaccel".into())
        .spawn(move || {
            if let Err(err) = run(sender, &config) {
                log::error!("global-shortcut listener stopped: {err:#}");
            }
        })
        .context("failed to spawn the global-shortcut thread")?;
    Ok(())
}

fn run(sender: Sender<HotkeyEvent>, config: &Config) -> Result<()> {
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
    for signal in signals {
        let args = signal.args().context("failed to decode the signal")?;
        if *args.action_unique() == ACTION && sender.send(HotkeyEvent::Toggle).is_err() {
            break;
        }
    }
    Ok(())
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
                key = Some(key_code(other).ok_or_else(|| {
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
