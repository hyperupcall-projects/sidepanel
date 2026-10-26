//! KWin owns global placement on Wayland. A transient, PID-scoped script
//! records and reapplies only our browser's geometry, without window rules.
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Geometry {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= 100.0
            && self.height >= 100.0
            && self.width <= 32768.0
            && self.height <= 32768.0
    }
}

#[derive(Clone, Default)]
pub struct Placement(pub Arc<Mutex<Option<Geometry>>>);
impl Placement {
    pub fn get(&self) -> Option<Geometry> {
        *self.0.lock().unwrap()
    }
    pub fn set(&self, geometry: Option<Geometry>) {
        *self.0.lock().unwrap() = geometry.filter(|g| g.valid());
    }
}

#[zbus::interface(name = "org.sidepanel.Geometry")]
impl Placement {
    fn read(&self) -> String {
        serde_json::to_string(&self.get()).unwrap()
    }
    fn record(&self, value: &str) {
        if let Ok(geometry) = serde_json::from_str::<Geometry>(value) {
            self.set(Some(geometry));
        }
    }
}

pub struct Bridge {
    connection: zbus::blocking::Connection,
    name: String,
    directory: std::path::PathBuf,
}

impl Bridge {
    pub fn start(placement: Placement) -> anyhow::Result<Self> {
        let pid = std::process::id();
        let service = format!("org.sidepanel.Geometry.p{pid}");
        let connection = zbus::blocking::connection::Builder::session()?
            .method_timeout(std::time::Duration::from_secs(2))
            .name(service.as_str())?
            .serve_at("/Geometry", placement)?
            .build()?;
        let name = format!("sidepanel-geometry-{pid}");
        let directory = std::env::temp_dir().join(&name);
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let bridge = Self {
            connection,
            name,
            directory,
        };
        let path = bridge.directory.join("geometry.js");
        let script = include_str!("geometry.js")
            .replace("__PID__", &pid.to_string())
            .replace("__SERVICE__", &service);
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?
            .write_all(script.as_bytes())?;
        let proxy = zbus::blocking::Proxy::new(
            &bridge.connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )?;
        let id: i32 = proxy.call(
            "loadScript",
            &(path.to_string_lossy().as_ref(), bridge.name.as_str()),
        )?;
        anyhow::ensure!(id >= 0, "KWin rejected geometry script");
        let script_path = format!("/Scripting/Script{id}");
        let script = zbus::blocking::Proxy::new(
            &bridge.connection,
            "org.kde.KWin",
            script_path.as_str(),
            "org.kde.kwin.Script",
        )?;
        let _: () = script.call("run", &())?;
        drop(script);
        drop(proxy);
        Ok(bridge)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Ok(proxy) = zbus::blocking::Proxy::new(
            &self.connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        ) {
            let _ = proxy.call::<_, _, bool>("unloadScript", &(self.name.as_str(),));
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Also called by the parent after a crash/forced exit, when helper RAII cannot run.
pub fn cleanup(pid: u32) {
    if let Ok(connection) = zbus::blocking::connection::Builder::session().and_then(|builder| {
        builder
            .method_timeout(std::time::Duration::from_secs(2))
            .build()
    }) {
        if let Ok(proxy) = zbus::blocking::Proxy::new(
            &connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        ) {
            let _ =
                proxy.call::<_, _, bool>("unloadScript", &(format!("sidepanel-geometry-{pid}"),));
        }
    }
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("sidepanel-geometry-{pid}")));
}
