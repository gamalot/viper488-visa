//! Remembered T1 levels: the level set on an adapter is the level it has, until it is set again.
//!
//! The NI and the 82357B are programmed by the daemon when it opens them, and a new daemon starts them at normal; a
//! Viper488 keeps its level only while powered. So the daemon writes each level it is asked to SET here, and sets it
//! again whenever it opens that adapter -- one rule for all three kinds.
//!
//! One line per adapter, `kind:serial level` (the adapter's key, so a level follows the adapter to another USB port and
//! another board number). Normal is the default and is not written: setting an adapter back to normal removes its line,
//! so the file lists exactly the adapters that are not at the default. It lives in the configuration directory
//! (`paths::config_file`).

use crate::proto::{speed_from_name, speed_name, SPEED_NORMAL};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub fn path() -> Option<PathBuf> {
    // The configuration directory: shared by all users after an all-users install (paths.rs).
    crate::paths::config_file("speed.conf")
}

/// Lines of `key level`; blank lines, `#` comments and lines that do not parse are ignored (a hand
/// edit gone wrong costs that one line, not the rest).
pub fn parse(text: &str) -> BTreeMap<String, u8> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (key, level) = (w.next()?, speed_from_name(w.next()?)?);
            (level != SPEED_NORMAL).then(|| (key.to_string(), level))
        })
        .collect()
}

pub fn format(levels: &BTreeMap<String, u8>) -> String {
    let mut s = String::from("# T1 levels set with symbiotic-ctl speed / symbioticSpeed; set by the daemon whenever it opens the adapter.\n");
    for (key, level) in levels {
        s.push_str(&format!("{key} {}\n", speed_name(*level)));
    }
    s
}

pub fn load_from(p: &Path) -> BTreeMap<String, u8> {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

pub fn load() -> BTreeMap<String, u8> {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

/// The level remembered for `key`, if it is not normal.
pub fn remembered(key: &str) -> Option<u8> {
    load().get(key).copied()
}

/// Record `level` for `key` (normal: forget it). Written whole (conffile.rs), so a reader never
/// sees half a file.
pub fn remember_in(p: &Path, key: &str, level: u8) -> io::Result<()> {
    crate::conffile::update(p, |text| {
        let mut levels = parse(text);
        let before = levels.clone();
        if level == SPEED_NORMAL {
            levels.remove(key);
        } else {
            levels.insert(key.to_string(), level);
        }
        (levels != before || text.is_empty()).then(|| format(&levels))
    })
    .map(|_| ())
}

pub fn remember(key: &str, level: u8) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::other("no configuration directory to keep speed.conf in"))?;
    remember_in(&p, key, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{SPEED_HIGH, SPEED_LOW};

    #[test]
    fn a_file_is_read_line_by_line_and_a_bad_line_costs_only_itself() {
        let m = parse("# comment\nni:01E8E8A6 high\n\nkeysight:MY1 low\nviper:ABC fastest\nviper:DEF normal\n");
        assert_eq!(m.len(), 2, "the bad level and the default are both left out: {m:?}");
        assert_eq!(m["ni:01E8E8A6"], SPEED_HIGH);
        assert_eq!(m["keysight:MY1"], SPEED_LOW);
    }

    #[test]
    fn setting_normal_forgets_and_the_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("symbiotic-speedconf-{}", std::process::id()));
        let p = dir.join("speed.conf");
        let _ = std::fs::remove_dir_all(&dir);
        remember_in(&p, "ni:A", SPEED_HIGH).unwrap();
        remember_in(&p, "keysight:B", SPEED_LOW).unwrap();
        assert_eq!(load_from(&p).len(), 2);
        remember_in(&p, "ni:A", SPEED_NORMAL).unwrap();
        let m = load_from(&p);
        assert_eq!(m.len(), 1);
        assert_eq!(m["keysight:B"], SPEED_LOW);
        assert_eq!(parse(&format(&m)), m);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
