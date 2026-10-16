//! Which features the configuration window shows, read from `features.conf` in the configuration directory.
//!
//! Keys: `sharing` (the Sharing page and shared instruments in the tree), `service` (the Service and diagnostics
//! page), `trace` (the I/O trace window), `display_name` (an instrument's display name and Rename), `alias_binding`
//! (binding an alias to one unit), `version` (the version in the status bar) and `tree_counts` (instrument counts in
//! the tree). One `key = value` per line; `on`, `true`, `yes` or `1` is on, anything else off; `#` starts a comment.
//! A key the file does not name has its default (`KEYS`). The switches affect only the window, not the daemon, the
//! library or `symbiotic-ctl`. The window writes the file with every key when it is missing and adds any key it lacks.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub sharing: bool,
    pub service: bool,
    pub trace: bool,
    pub display_name: bool,
    pub alias_binding: bool,
    pub version: bool,
    pub tree_counts: bool,
}

/// Each key and what it is when the file does not say, in the order the file is written.
const KEYS: [(&str, bool); 7] = [
    ("sharing", false),
    ("service", false),
    ("trace", false),
    ("display_name", false),
    ("alias_binding", false),
    ("version", true),
    ("tree_counts", false),
];

impl Default for Features {
    fn default() -> Self {
        let on = |k: &str| KEYS.iter().any(|&(x, d)| x == k && d);
        Features {
            sharing: on("sharing"),
            service: on("service"),
            trace: on("trace"),
            display_name: on("display_name"),
            alias_binding: on("alias_binding"),
            version: on("version"),
            tree_counts: on("tree_counts"),
        }
    }
}

pub fn path() -> Option<PathBuf> {
    crate::paths::config_file("features.conf")
}

const HEAD: &str = "\
# Which features the configuration window shows: on or off.
# The window reads this when it starts.
";

fn line(k: &str, on: bool) -> String {
    format!("{k} = {}\n", if on { "on" } else { "off" })
}

/// The file as the window first writes it: every key at its default.
pub fn template() -> String {
    HEAD.to_string() + &KEYS.iter().map(|&(k, d)| line(k, d)).collect::<String>()
}

/// The switches in `text`; a key it does not name has its default.
pub fn parse(text: &str) -> Features {
    let mut f = Features::default();
    for l in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let Some((k, v)) = l.split_once('=') else { continue };
        let on = matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1");
        match k.trim() {
            "sharing" => f.sharing = on,
            "service" => f.service = on,
            "trace" => f.trace = on,
            "display_name" => f.display_name = on,
            "alias_binding" => f.alias_binding = on,
            "version" => f.version = on,
            "tree_counts" => f.tree_counts = on,
            _ => {}
        }
    }
    f
}

pub fn load_from(p: &Path) -> Features {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

pub fn load() -> Features {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

/// Write the file, every key at its default, when there is none; to one that is there, add each key it does not name, at
/// its default. What it says already, and its comments, stay.
pub fn create_in(p: &Path) -> io::Result<bool> {
    let there = p.exists();
    crate::conffile::update(p, |old| {
        if !there && old.is_empty() {
            return Some(template());
        }
        let named = |k: &str| old.lines().map(str::trim).filter(|l| !l.starts_with('#')).any(|l| l.split_once('=').is_some_and(|(x, _)| x.trim() == k));
        let missing: Vec<(&str, bool)> = KEYS.into_iter().filter(|&(k, _)| !named(k)).collect();
        if missing.is_empty() {
            return None;
        }
        let mut text = old.to_string();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for (k, d) in missing {
            text.push_str(&line(k, d));
        }
        Some(text)
    })
}

pub fn create() -> io::Result<bool> {
    let p = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no configuration directory"))?;
    create_in(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_at_its_default_unless_said_and_a_file_there_only_gains_what_it_lacks() {
        let d = Features::default();
        assert!(!d.sharing && !d.service && !d.trace && !d.display_name && !d.alias_binding && d.version && !d.tree_counts);
        assert_eq!(parse(""), d);
        assert_eq!(parse(&template()), d);
        assert_eq!(parse("# sharing = on\ntrace = ON\nservice=yes\nother = on\n"), Features { service: true, trace: true, ..d });
        assert!(parse("alias_binding = 1").alias_binding);
        assert!(parse("display_name = on").display_name);
        assert!(!parse("version = off").version);
        let dir = std::env::temp_dir().join(format!("symbiotic-features-{}", std::process::id()));
        let p = dir.join("features.conf");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_from(&p), d);
        assert!(create_in(&p).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), template());
        assert!(template().ends_with("alias_binding = off\nversion = on\ntree_counts = off\n"));
        assert!(!create_in(&p).unwrap());
        std::fs::write(&p, "# mine\nsharing = on\ntrace = off").unwrap();
        assert!(create_in(&p).unwrap());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "# mine\nsharing = on\ntrace = off\nservice = off\ndisplay_name = off\nalias_binding = off\nversion = on\ntree_counts = off\n"
        );
        assert_eq!(load_from(&p), Features { sharing: true, ..d });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
