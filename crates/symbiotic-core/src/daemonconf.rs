//! Settings of the daemon itself, `daemon.conf` in the configuration directory. One `key = value` per line; `#`
//! comments and lines that do not parse are ignored.
//!
//! `release_idle = on`: a daemon that stays (it shares aliases, or runs the registry, so nobody can reach it once it
//! has exited) lets go of every adapter, USB instrument and serial port it has open when nothing has been connected for
//! ten seconds -- so that `usbipd` or another vendor's tool can have the device, which WinUSB gives to one process at a
//! time. Off by default: the daemon keeps what it opened, which is what a probe of the bus needs, and costs nothing
//! until something else wants the device. A daemon that does not stay exits when idle, and so gives everything back,
//! whatever this says.

use std::io;
use std::path::{Path, PathBuf};

pub fn path() -> Option<PathBuf> {
    crate::paths::config_file("daemon.conf")
}

/// `release_idle` in `text`: on only for `on`, `true`, `yes` or `1`.
pub fn parse_release_idle(text: &str) -> bool {
    text.lines().map(str::trim).filter(|l| !l.starts_with('#')).any(|l| {
        l.split_once('=').is_some_and(|(k, v)| k.trim() == "release_idle" && matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1"))
    })
}

pub fn release_idle_in(p: &Path) -> bool {
    std::fs::read_to_string(p).map(|t| parse_release_idle(&t)).unwrap_or(false)
}

/// Whether the daemon that stays lets go of the hardware when idle. Read each time it is asked, so the setting takes effect without a restart.
pub fn release_idle() -> bool {
    path().is_some_and(|p| release_idle_in(&p))
}

pub fn set_release_idle_in(p: &Path, on: bool) -> io::Result<()> {
    // The line is replaced and the rest of the file kept, comments and keys a newer build may add.
    crate::conffile::update(p, |old| {
        let mut text: String = old.lines().filter(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != "release_idle")).map(|l| format!("{l}\n")).collect();
        if on {
            text.push_str("release_idle = on\n");
        }
        Some(text)
    })
    .map(|_| ())
}

pub fn set_release_idle(on: bool) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no configuration directory"))?;
    set_release_idle_in(&p, on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_said_on_and_the_rest_of_the_file_stays() {
        assert!(!parse_release_idle(""));
        assert!(!parse_release_idle("# release_idle = on\n"));
        assert!(parse_release_idle("x = 1\nrelease_idle = ON\n"));
        assert!(!parse_release_idle("release_idle = off"));
        let dir = std::env::temp_dir().join(format!("symbiotic-daemonconf-{}", std::process::id()));
        let p = dir.join("daemon.conf");
        assert!(!release_idle_in(&p));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, "# mine\nother = 2\n").unwrap();
        set_release_idle_in(&p, true).unwrap();
        assert!(release_idle_in(&p));
        set_release_idle_in(&p, false).unwrap();
        assert!(!release_idle_in(&p));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# mine\nother = 2\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
