//! The group's pictures, as the registry keeps them: the same set a computer has of its own (`pictures::Store`, one
//! `pictures.conf` with its `pictures/`), kept in the registry's directory, with a version for the whole and one for
//! each picture file, so that a member copies only what changed.

use crate::pictures::{self, Original, Rule, Save, Store};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub struct GroupPictures {
    store: Store,
    versions: PathBuf,
    version: u64,
    /// The version each picture file was last written at.
    files: BTreeMap<String, u64>,
}

/// What a member asks first: the version, the rules' text and each rule's picture file with its version.
pub struct PicState {
    pub version: u64,
    pub conf: String,
    pub files: Vec<(String, u64)>,
}

impl GroupPictures {
    /// The pictures kept in `dir` (made if it is not there).
    pub fn open(dir: &Path) -> io::Result<GroupPictures> {
        std::fs::create_dir_all(dir)?;
        let versions = dir.join("versions");
        let mut g = GroupPictures { store: Store::at(dir.join("pictures.conf")), versions, version: 0, files: BTreeMap::new() };
        if let Ok(t) = std::fs::read_to_string(&g.versions) {
            for l in t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                match l.split_once(' ') {
                    Some(("version", v)) => g.version = v.trim().parse().unwrap_or(0),
                    Some((file, v)) => {
                        if let Ok(v) = v.trim().parse() {
                            g.files.insert(file.to_string(), v);
                        }
                    }
                    None => {}
                }
            }
        }
        Ok(g)
    }

    fn write_versions(&self) -> io::Result<()> {
        let mut s = format!("# The group's pictures: the version of the set, then of each picture file.\nversion {}\n", self.version);
        for (f, v) in &self.files {
            s += &format!("{f} {v}\n");
        }
        let tmp = self.versions.with_extension("tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(&tmp, &self.versions)
    }

    pub fn state(&self) -> PicState {
        let conf = std::fs::read_to_string(&self.store.conf).unwrap_or_default();
        let mut files: Vec<(String, u64)> = Vec::new();
        for r in pictures::parse(&conf) {
            if !files.iter().any(|(f, _)| *f == r.file) {
                let v = self.files.get(&r.file).copied().unwrap_or(self.version);
                files.push((r.file, v));
            }
        }
        PicState { version: self.version, conf, files }
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.store.load()
    }

    pub fn get(&self, file: &str) -> io::Result<Vec<u8>> {
        self.store.read_png(file)
    }

    pub fn original(&self, file: &str) -> io::Result<Option<Original>> {
        self.store.read_original(file)
    }

    /// Store a picture (any member may); the file it is in.
    pub fn save(&mut self, s: &Save) -> io::Result<String> {
        let file = self.store.save(s)?;
        self.version += 1;
        self.files.insert(file.clone(), self.version);
        self.forget_gone();
        self.write_versions()?;
        Ok(file)
    }

    /// Take the rule for `pattern` out. False if there was none.
    pub fn remove(&mut self, pattern: &str) -> io::Result<bool> {
        if !self.store.remove(pattern)? {
            return Ok(false);
        }
        self.version += 1;
        self.forget_gone();
        self.write_versions()?;
        Ok(true)
    }

    /// The versions of files no rule has any more are dropped.
    fn forget_gone(&mut self) {
        let used: Vec<String> = self.store.load().into_iter().map(|r| r.file).collect();
        self.files.retain(|f, _| used.contains(f));
    }
}
