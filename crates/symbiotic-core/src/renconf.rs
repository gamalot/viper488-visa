//! Which adapters hold REN asserted: a setting per adapter.
//!
//! The daemon sets each adapter's REN line when it opens it: released unless the adapter is listed here. An instrument
//! is put in Remote by being addressed while REN is asserted, and some instruments beep when they go back to Local (REN
//! released or GTL), so a controller that holds REN makes every later probe of the bus audible. Released is the
//! default; an instrument that must be in Remote to take a command wants its adapter listed.
//!
//! One line per adapter, `kind:serial on` (the adapter's key, as speed.conf). Only adapters that hold REN are written,
//! so the file lists exactly the ones that are not at the default. `ren.conf` in the configuration directory.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

pub fn path() -> Option<PathBuf> {
    crate::paths::config_file("ren.conf")
}

/// Lines of `key on`; blank lines, `#` comments, `off` and lines that do not parse are ignored.
pub fn parse(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let key = w.next()?;
            (w.next()?.eq_ignore_ascii_case("on")).then(|| key.to_string())
        })
        .collect()
}

pub fn format(keys: &BTreeSet<String>) -> String {
    let mut s = String::from("# Adapters that hold REN asserted (the default is released); set with the configuration window or symbiotic-ctl ren.\n");
    for k in keys {
        s.push_str(&format!("{k} on\n"));
    }
    s
}

pub fn load_from(p: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

pub fn load() -> BTreeSet<String> {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

/// Whether the adapter `key` holds REN asserted.
pub fn asserted(key: &str) -> bool {
    load().contains(key)
}

pub fn set_in(p: &Path, key: &str, on: bool) -> io::Result<()> {
    crate::conffile::update(p, |text| {
        let mut keys = parse(text);
        let before = keys.clone();
        if on {
            keys.insert(key.to_string());
        } else {
            keys.remove(key);
        }
        (keys != before || (text.is_empty() && on)).then(|| format(&keys))
    })
    .map(|_| ())
}

pub fn set(key: &str, on: bool) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no configuration directory"))?;
    set_in(&p, key, on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_adapters_that_hold_ren_are_listed() {
        let dir = std::env::temp_dir().join(format!("symbiotic-renconf-{}", std::process::id()));
        let p = dir.join("ren.conf");
        assert!(load_from(&p).is_empty(), "no file: released everywhere");
        set_in(&p, "viper:A1", true).unwrap();
        set_in(&p, "ni:B2", true).unwrap();
        assert_eq!(load_from(&p).into_iter().collect::<Vec<_>>(), ["ni:B2", "viper:A1"]);
        set_in(&p, "ni:B2", false).unwrap();
        assert_eq!(load_from(&p).into_iter().collect::<Vec<_>>(), ["viper:A1"]);
        assert_eq!(parse("# c\nviper:A1 on\nni:B2 off\nbad\n\nks:C3 ON"), ["ks:C3", "viper:A1"].iter().map(|s| s.to_string()).collect());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
