//! Desktop appearance updates via the XDG Settings portal.
use anyhow::Result;
use calloop::channel::Sender;
use zbus::zvariant::OwnedValue;

const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
    fn read_all(
        &self,
        namespaces: &[&str],
    ) -> zbus::Result<
        std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>>,
    >;
    #[zbus(signal)]
    fn setting_changed(&self, namespace: &str, key: &str, value: OwnedValue) -> zbus::Result<()>;
}

pub fn spawn(sender: Sender<bool>) {
    if let Err(err) = std::thread::Builder::new()
        .name("appearance".into())
        .spawn(move || {
            if let Err(err) = watch(sender) {
                log::warn!(
                    "desktop appearance unavailable; system mode defaults to light: {err:#}"
                );
            }
        })
    {
        log::warn!("could not start desktop appearance listener: {err}");
    }
}

fn watch(sender: Sender<bool>) -> Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = SettingsProxyBlocking::new(&conn)?;
    // Subscribe before reading, so a change during startup is not missed.
    let signals = proxy.receive_setting_changed()?;
    let settings = proxy.read_all(&[NAMESPACE])?;
    let dark = settings
        .get(NAMESPACE)
        .and_then(|s| s.get(KEY))
        .and_then(|v| u32::try_from(v).ok())
        == Some(1);
    if sender.send(dark).is_err() {
        return Ok(());
    }
    for signal in signals {
        let args = signal.args()?;
        if *args.namespace() == NAMESPACE && *args.key() == KEY {
            let dark = u32::try_from(args.value()).ok() == Some(1);
            if sender.send(dark).is_err() {
                break;
            }
        }
    }
    Ok(())
}
