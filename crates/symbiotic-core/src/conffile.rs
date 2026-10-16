//! Writing the configuration files (aliases.conf, identities.conf, instruments.conf, lan.conf, ...) atomically, one
//! change at a time.
//!
//! New text is written to a file beside the target, which then replaces it, so a reader sees either the old file or the
//! new one. `update` reads, edits and writes the file while holding a lock on a `.lock` file beside it, so concurrent
//! changes from several processes are serialized.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// A value for one line of a configuration file. `Field::new` refuses control characters and the file's own separators,
/// so a value can neither add a line nor split one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field(String);

impl Field {
    /// `what` names the value in the error, as the user knows it ("an alias", "this machine's name").
    pub fn new(s: &str, separators: &[char], what: &str) -> io::Result<Field> {
        let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("{what} {why}: {s:?}"));
        if s.chars().any(char::is_control) {
            return Err(bad("cannot hold a line break or another control character"));
        }
        if let Some(c) = s.chars().find(|c| separators.contains(c)) {
            return Err(bad(&format!("cannot hold {c:?} here")));
        }
        Ok(Field(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Put `text` in `path` whole: written beside it, flushed, then renamed over it. The folder is
/// made if it is not there.
pub fn write(path: &Path, text: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    // A unique temporary name for each write, also across threads of one process.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = beside(path, &format!("{}.{n}.tmp", std::process::id()));
    let r = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_ref())?;
        f.sync_all()?;
        drop(f);
        replace(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Rename `from` over `to`, retrying for a while if Windows refuses because another program has `to` open.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// Change `path`: `edit` gets its text ("" when there is none) and returns the new text, or None to leave the file as
/// it is. Other `update`s of the same file wait meanwhile. Returns whether it was written.
pub fn update(path: &Path, edit: impl FnOnce(&str) -> Option<String>) -> io::Result<bool> {
    let _lock = lock(path)?;
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e), // an unreadable file is an error, not an empty one
    };
    let Some(new) = edit(&text) else { return Ok(false) };
    write(path, new)?;
    Ok(true)
}

/// Hold off other changes of `path` (`update`, or another `lock`) until the returned file is dropped.
pub fn lock(path: &Path) -> io::Result<File> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(beside(path, "lock"))?;
    f.lock()?;
    Ok(f)
}

/// `path` with `.suffix` added to its name: in the same folder, so a rename is not across disks.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_one_line_without_the_files_separators() {
        for ok in ["DMM", "Left bench generator", "x = y", "\u{e9}t\u{e9}", "BENCH-PC"] {
            assert_eq!(Field::new(ok, &[], "it").unwrap().as_str(), ok);
        }
        // Every control character, not only the line breaks: \u{85} (NEL) and \u{1b} (ESC) too.
        for bad in ["a\nb", "a\rb", "a\0b", "a\tb", "a\u{1b}b", "a\u{85}b", "\n"] {
            let e = Field::new(bad, &[], "this machine's name").unwrap_err();
            assert!(e.to_string().starts_with("this machine's name cannot hold a line break"), "{bad:?}: {e}");
        }
        assert!(Field::new("a=b", &['=', '#'], "an alias").unwrap_err().to_string().contains("'='"));
        assert!(Field::new("a#b", &['=', '#'], "an alias").is_err());
    }
    use std::sync::{Arc, Barrier};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("symbiotic-conffile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn written_whole_and_nothing_left_beside_it() {
        let d = dir("whole");
        let p = d.join("sub").join("a.conf");
        write(&p, "one\n").unwrap();
        write(&p, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two\n");
        let names: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.conf")], "no temporary file stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn threads_writing_one_file_at_once_each_leave_it_whole() {
        let d = dir("threads");
        let p = d.join("same.conf");
        let n = 16;
        let start = Arc::new(Barrier::new(n));
        let hands: Vec<_> = (0..n)
            .map(|i| {
                let (p, start) = (p.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    write(&p, format!("{}\n", i.to_string().repeat(4096)))
                })
            })
            .collect();
        for h in hands {
            h.join().unwrap().unwrap();
        }
        let t = std::fs::read_to_string(&p).unwrap();
        let line = t.trim_end();
        assert!(line.len().is_multiple_of(4096) && t.ends_with('\n'), "one writer's text, whole");
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("same.conf")], "no temporary file stays");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_change_is_lost_to_another_at_the_same_time() {
        let d = dir("race");
        let p = d.join("n.conf");
        let n = 16;
        let start = Arc::new(Barrier::new(n));
        let hands: Vec<_> = (0..n)
            .map(|i| {
                let (p, start) = (p.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    update(&p, |t| {
                        std::thread::sleep(std::time::Duration::from_millis(2)); // a slow editor, to make the race likely
                        Some(format!("{t}line {i}\n"))
                    })
                    .unwrap()
                })
            })
            .collect();
        for h in hands {
            assert!(h.join().unwrap());
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), n, "every change is in the file");
        assert!(!update(&p, |_| None).unwrap(), "no new text: not written");
        let _ = std::fs::remove_dir_all(&d);
    }
}
