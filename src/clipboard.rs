//! Optional wl-clipboard integration; clipboard owners never block Wayland input.
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::editor::Field;

pub struct Event {
    pub revision: u64,
    pub field: Field,
    pub text: Result<String, String>,
}

pub fn paste(revision: u64, field: Field, tx: calloop::channel::Sender<Event>) {
    std::thread::spawn(move || {
        let text = read_text();
        let _ = tx.send(Event {
            revision,
            field,
            text,
        });
    });
}

fn read_text() -> Result<String, String> {
    let mut child = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Paste requires wl-paste (wl-clipboard).".to_string())?;
    let stdout = child.stdout.take().ok_or("Clipboard unavailable.")?;
    // Bound data from a clipboard owner and close the pipe on oversized input.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Could not read clipboard.")?
        .map_err(|_| "Could not read clipboard.")?;
    if !status.is_some_and(|status| status.success()) {
        return Err("Clipboard has no text or did not respond.".into());
    }
    String::from_utf8(bytes).map_err(|_| "Clipboard text is not UTF-8.".into())
}
