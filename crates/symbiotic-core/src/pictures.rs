//! The user's pictures of instruments, for the configuration window (the program carries none).
//!
//! `pictures.conf` in the configuration directory (`SYMBIOTIC_PICTURES` names another file; the pictures are in
//! `pictures/` beside it): one rule a line, `PATTERN = file.png [dark=keep]`, `#` comments. PATTERN is a regular
//! expression that the window tries, case-insensitively and anywhere in the text, on what an instrument is called (the
//! model it answers `*IDN?` with, vendor and model, the name the user gave it, its resource name); the first rule in
//! the file that matches is the instrument's picture. The last `=` of a line parts the pattern from the file, so a
//! pattern may have one. This file only reads and writes the lines and the picture files: the regular expressions are
//! the page's (`RegExp`) to compile.
//!
//! A picture is a PNG (black line art on a clear ground, 640 wide, shown at 320): on the dark theme the window inverts
//! it to white, unless the rule says `dark=keep`.

use std::path::PathBuf;

/// A picture file's biggest size: a picture is a few tens of kilobytes; this is only a limit on mistakes.
pub const MAX_PNG: usize = 4 * 1024 * 1024;

/// The original image a picture was made of, kept as it came (a photograph can be a few MiB): this is only a limit on mistakes.
pub const MAX_ORIGINAL: usize = 16 * 1024 * 1024;
/// The edit parameters (a few numbers as JSON).
pub const MAX_EDIT: usize = 64 * 1024;
/// What an original may be (the page reads them all: they are what a browser draws).
pub const ORIGINAL_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const HEADER: &str = "# Pictures of instruments (the configuration window): PATTERN = file.png [dark=keep]\n\
                      # PATTERN is a regular expression tried on an instrument's model, vendor and model, display name and resource name;\n\
                      # the first rule that matches is its picture. The pictures are in the pictures folder beside this file.\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub file: String,
    /// Not inverted on the dark theme.
    pub dark_keep: bool,
}

pub fn conf_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_PICTURES") {
        return Some(PathBuf::from(p));
    }
    crate::paths::config_file("pictures.conf")
}

/// The folder of the picture files: `pictures/` beside the rules.
pub fn dir() -> Option<PathBuf> {
    conf_path().and_then(|p| p.parent().map(|d| d.join("pictures")))
}

/// A name a picture file may have: letters, digits, `.`, `_` and `-`, ending in `.png`, no `..`: never a path.
pub fn valid_file_name(name: &str) -> bool {
    let n = name.len();
    n > 4
        && n <= 80
        && name.to_ascii_lowercase().ends_with(".png")
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The rules in `text`, in file order; a line that does not make a rule is left out.
pub fn parse(text: &str) -> Vec<Rule> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Rule> {
    let mut l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return None;
    }
    // The options at the end come off first: `dark=keep` has an = of its own.
    let mut dark_keep = false;
    while let Some((rest, last)) = l.rsplit_once(char::is_whitespace) {
        if last.eq_ignore_ascii_case("dark=keep") {
            dark_keep = true;
            l = rest.trim_end();
        } else {
            break;
        }
    }
    // Then the last = parts the pattern from the file.
    let (pattern, file) = l.rsplit_once('=')?;
    let (pattern, file) = (pattern.trim(), file.trim());
    if pattern.is_empty() || !valid_file_name(file) {
        return None;
    }
    Some(Rule { pattern: pattern.to_string(), file: file.to_string(), dark_keep })
}

pub fn format_rule(r: &Rule) -> String {
    format!("{} = {}{}", r.pattern, r.file, if r.dark_keep { " dark=keep" } else { "" })
}

pub fn load() -> Vec<Rule> {
    Store::local().map(|s| s.load()).unwrap_or_default()
}

/// A set of pictures: its `pictures.conf` and, beside it, `pictures/`. This computer's own is `Store::local()`; the
/// group's, which the registry keeps and every member holds a copy of, is another.
#[derive(Debug, Clone)]
pub struct Store {
    pub conf: PathBuf,
}

impl Store {
    pub fn at(conf: PathBuf) -> Store {
        Store { conf }
    }

    /// This computer's own (`pictures.conf` in the configuration directory, or where SYMBIOTIC_PICTURES says).
    pub fn local() -> Option<Store> {
        conf_path().map(Store::at)
    }

    pub fn dir(&self) -> PathBuf {
        self.conf.parent().map(|d| d.join("pictures")).unwrap_or_else(|| PathBuf::from("pictures"))
    }

    pub fn load(&self) -> Vec<Rule> {
        std::fs::read_to_string(&self.conf).map(|t| parse(&t)).unwrap_or_default()
    }
}

fn bad(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg.into())
}

/// A file name for a picture of `pattern`: its letters and digits, the rest turned into `_`.
fn slug(pattern: &str) -> String {
    let mut s = String::new();
    for c in pattern.chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '_' };
        if c == '_' && (s.is_empty() || s.ends_with('_')) {
            continue;
        }
        s.push(c);
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_end_matches('_').to_string();
    if s.is_empty() { "picture".to_string() } else { s }
}

/// `text` without the rules whose pattern is `pattern`, comments and the other lines as they were.
fn without(text: &str, pattern: &str) -> Vec<String> {
    text.lines().filter(|l| parse_line(l).is_none_or(|r| r.pattern != pattern)).map(str::to_string).collect()
}

/// The folder of the originals and edit parameters: `originals/` in the pictures folder (a folder of its own, so that no name of a
/// picture can be one of theirs).
fn originals_dir_in(folder: &std::path::Path) -> PathBuf {
    folder.join("originals")
}

/// A picture file's name without its `.png`.
fn stem(file: &str) -> &str {
    file.get(..file.len().saturating_sub(4)).unwrap_or(file)
}

/// The originals of `file` (an extension each) and its edit parameters taken away.
fn remove_originals(originals: &std::path::Path, file: &str) {
    for ext in ORIGINAL_EXTS.iter().copied().chain(["json"]) {
        let _ = std::fs::remove_file(originals.join(format!("{}.{ext}", stem(file))));
    }
}

/// A picture file with its original and its edit parameters.
fn remove_picture_files(folder: &std::path::Path, file: &str) {
    let _ = std::fs::remove_file(folder.join(file));
    remove_originals(&originals_dir_in(folder), file);
}

/// Make `png` the picture for `pattern`: the file goes into the pictures folder and the rule is written. A rule that has the
/// pattern already is changed where it is, and keeps its file. `replaces` is the pattern of a rule being edited: that rule becomes
/// this one, in its place and with its file, so editing a pattern is not adding a second rule beside the old one. Otherwise the rule
/// goes first (after the comments at the top), where the newest, and likeliest the most particular, is tried first.
/// Returns the file's name.
pub fn add(pattern: &str, dark_keep: bool, png: &[u8], replaces: Option<&str>) -> std::io::Result<String> {
    save(&Save { pattern, dark_keep, png, replaces, original: None, edit: None })
}

/// What `save` is given.
pub struct Save<'a> {
    pub pattern: &'a str,
    pub dark_keep: bool,
    /// The picture as it is shown: black line art on a clear ground, a PNG.
    pub png: &'a [u8],
    /// The pattern of a rule being edited, which this one replaces.
    pub replaces: Option<&'a str>,
    /// The image the picture was made of (its extension, from `ORIGINAL_EXTS`, and its bytes, as they came) and what was done
    /// to it (JSON, the page's own): kept, so that the picture can be made again from the whole image, not from the part that
    /// is shown. None: what is there stays.
    pub original: Option<(&'a str, &'a [u8])>,
    pub edit: Option<&'a str>,
}

/// `add`, and the original and the edit parameters with it. Everything is checked before anything is written.
pub fn save(s: &Save) -> std::io::Result<String> {
    Store::local().ok_or_else(|| bad("no place for pictures.conf (LOCALAPPDATA or HOME is not set)"))?.save(s)
}

impl Store {
    /// `save` on this set.
    pub fn save(&self, s: &Save) -> std::io::Result<String> {
        let (pattern, dark_keep, png, replaces) = (s.pattern.trim(), s.dark_keep, s.png, s.replaces);
        if pattern.is_empty() || pattern.contains(['\n', '\r']) {
            return Err(bad("a pattern is one line of text"));
        }
        if png.len() > MAX_PNG || png.len() < PNG_SIGNATURE.len() || png[..8] != PNG_SIGNATURE {
            return Err(bad("the picture is not a PNG (or it is more than 4 MiB)"));
        }
        if let Some((ext, bytes)) = s.original {
            if !ORIGINAL_EXTS.contains(&ext) || bytes.is_empty() || bytes.len() > MAX_ORIGINAL {
                return Err(bad("the original is not an image of a kind the window reads (or it is more than 16 MiB)"));
            }
        }
        if s.edit.is_some_and(|e| e.is_empty() || e.len() > MAX_EDIT || e.contains('\0')) {
            return Err(bad("the edit parameters are not what the window writes"));
        }
        let conf = self.conf.clone();
        let folder = self.dir();
        let _lock = crate::conffile::lock(&conf)?;
        let text = std::fs::read_to_string(&conf).unwrap_or_else(|_| HEADER.to_string());
        let rules = parse(&text);
        let replaces = replaces.map(str::trim).filter(|r| !r.is_empty() && *r != pattern);
        let old = replaces.and_then(|r| rules.iter().find(|x| x.pattern == r));
        let file = match (rules.iter().find(|r| r.pattern == pattern), old) {
            (Some(r), _) | (None, Some(r)) => r.file.clone(),
            (None, None) => {
                // One that no other rule has and that is not on the disk already (it may be the user's own).
                let base = slug(pattern);
                let mut n = 1;
                loop {
                    let name = if n == 1 { format!("{base}.png") } else { format!("{base}-{n}.png") };
                    if !rules.iter().any(|r| r.file == name) && !folder.join(&name).exists() {
                        break name;
                    }
                    n += 1;
                }
            }
        };
        std::fs::create_dir_all(&folder)?;
        crate::conffile::write(&folder.join(&file), png)?;
        if s.original.is_some() || s.edit.is_some() {
            let originals = originals_dir_in(&folder);
            std::fs::create_dir_all(&originals)?;
            if let Some((ext, bytes)) = s.original {
                remove_originals(&originals, &file);
                std::fs::write(originals.join(format!("{}.{ext}", stem(&file))), bytes)?;
            }
            if let Some(edit) = s.edit {
                std::fs::write(originals.join(format!("{}.json", stem(&file))), edit)?;
            }
        }
        let line = format_rule(&Rule { pattern: pattern.to_string(), file: file.clone(), dark_keep });
        // In the place of the first rule that this one stands for; else first of the rules.
        let mut lines = Vec::new();
        let mut placed = false;
        for l in text.lines() {
            if parse_line(l).is_some_and(|r| r.pattern == pattern || replaces.is_some_and(|x| r.pattern == x)) {
                if !placed {
                    lines.push(line.clone());
                    placed = true;
                }
            } else {
                lines.push(l.to_string());
            }
        }
        if !placed {
            let at = lines.iter().position(|l| parse_line(l).is_some()).unwrap_or(lines.len());
            lines.insert(at, line);
        }
        crate::conffile::write(&conf, lines.join("\n") + "\n")?;
        // The replaced rule's file, if the replacement went to another and nothing else uses it.
        if let Some(r) = old.filter(|r| r.file != file) {
            if !rules.iter().any(|x| x.pattern != r.pattern && x.file == r.file) {
                remove_picture_files(&folder, &r.file);
            }
        }
        Ok(file)
    }
}

/// Take the rule for `pattern` out; its picture file too, unless another rule uses it. False if there was no such rule.
pub fn remove(pattern: &str) -> std::io::Result<bool> {
    Store::local().ok_or_else(|| bad("no place for pictures.conf"))?.remove(pattern)
}

impl Store {
    pub fn remove(&self, pattern: &str) -> std::io::Result<bool> {
        let pattern = pattern.trim();
        let _lock = crate::conffile::lock(&self.conf)?;
        let text = std::fs::read_to_string(&self.conf).unwrap_or_default();
        let rules = parse(&text);
        let Some(gone) = rules.iter().find(|r| r.pattern == pattern) else { return Ok(false) };
        let file = gone.file.clone();
        let lines = without(&text, pattern);
        crate::conffile::write(&self.conf, lines.join("\n") + "\n")?;
        if !rules.iter().any(|r| r.pattern != pattern && r.file == file) {
            remove_picture_files(&self.dir(), &file);
        }
        Ok(true)
    }

    /// The bytes of the picture file `file` (a name from a rule, never a path).
    pub fn read_png(&self, file: &str) -> std::io::Result<Vec<u8>> {
        if !valid_file_name(file) {
            return Err(bad(format!("{file:?} is not a picture file name")));
        }
        std::fs::read(self.dir().join(file))
    }

    /// The original of the picture `file`, if it has one.
    pub fn read_original(&self, file: &str) -> std::io::Result<Option<Original>> {
        if !valid_file_name(file) {
            return Err(bad(format!("{file:?} is not a picture file name")));
        }
        let originals = originals_dir_in(&self.dir());
        for ext in ORIGINAL_EXTS {
            if let Ok(bytes) = std::fs::read(originals.join(format!("{}.{ext}", stem(file)))) {
                let edit = std::fs::read_to_string(originals.join(format!("{}.json", stem(file)))).ok();
                return Ok(Some(Original { ext: ext.to_string(), bytes, edit }));
            }
        }
        Ok(None)
    }
}

/// The bytes of the picture file `file` (a name from a rule, never a path).
pub fn read_png(file: &str) -> std::io::Result<Vec<u8>> {
    Store::local().ok_or_else(|| bad("no place for the pictures"))?.read_png(file)
}

/// What a picture was made of: the original image as it came (its extension and bytes) and the edit parameters that were written.
pub struct Original {
    pub ext: String,
    pub bytes: Vec<u8>,
    pub edit: Option<String>,
}

/// The original of the picture `file`, if it has one (a picture put there by hand, or made before the originals were kept, has not).
pub fn read_original(file: &str) -> std::io::Result<Option<Original>> {
    Store::local().ok_or_else(|| bad("no place for the pictures"))?.read_original(file)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Standard base64 with padding; white space is ignored. None for anything else.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for ch in text.bytes().filter(|b| !b.is_ascii_whitespace()) {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            _ => return None,
        };
        if pad > 0 {
            return None; // data after the padding
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (pad <= 2 && (bits == 0 || pad > 0)).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that point SYMBIOTIC_PICTURES at a file of their own take turns.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn png() -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend_from_slice(b"not really a picture, but it has the signature");
        v
    }

    #[test]
    fn a_line_is_a_pattern_a_file_and_an_option() {
        let r = parse(
            "# c\n\
             33220A = 33220A.png\n\
             MODEL 2015|KEITHLEY 2015|K2015 = k2015.png dark=keep\n\
             a=b = with-equals.png\n\
             ^GPIB0::15:: = one.PNG\n\
             no file =\n\
             = nopattern.png\n\
             SFU = ../escape.png\n\
             HP = has space.png\n\
             not a rule\n",
        );
        let got: Vec<(&str, &str, bool)> = r.iter().map(|r| (r.pattern.as_str(), r.file.as_str(), r.dark_keep)).collect();
        assert_eq!(
            got,
            [
                ("33220A", "33220A.png", false),
                ("MODEL 2015|KEITHLEY 2015|K2015", "k2015.png", true),
                ("a=b", "with-equals.png", false),
                ("^GPIB0::15::", "one.PNG", false),
                // "has space.png" reads as the file "has" with a word after it: not a picture file name.
            ],
            "the last = parts a line; a name that is a path, a missing file or pattern, a line with none of it: no rule"
        );
        for r in &r {
            assert_eq!(parse(&format_rule(r)), vec![r.clone()], "a rule reads back as it was written");
        }
    }

    #[test]
    fn file_names_are_never_paths() {
        assert!(valid_file_name("34465A.png") && valid_file_name("a_b-c.2.PNG"));
        for n in ["", ".png", "a.jpg", "a/b.png", "a\\b.png", "..png", "a..b.png", "C:x.png", "a b.png", &format!("{}.png", "x".repeat(80))] {
            assert!(!valid_file_name(n), "{n:?}");
        }
    }

    #[test]
    fn base64_goes_both_ways() {
        for (plain, coded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("Man", "TWFu")] {
            assert_eq!(base64_encode(plain.as_bytes()), coded);
            assert_eq!(base64_decode(coded).as_deref(), Some(plain.as_bytes()));
        }
        assert_eq!(base64_decode("Zm9v\nYg==\n").as_deref(), Some(&b"foob"[..]), "white space is ignored");
        for bad in ["Zm9", "Zm9v=Yg", "Zm9v!", "Z", "Zg==Zg=="] {
            assert_eq!(base64_decode(bad), None, "{bad:?}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)), Some(all));
    }

    #[test]
    fn adding_replacing_and_removing_pictures() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-pictures-{}", std::process::id()));
        let conf = dir.join("pictures.conf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&conf, "# mine\nOLD = old.png\n").unwrap();
        std::fs::create_dir_all(dir.join("pictures")).unwrap();
        // A file of the user's own, with the name a picture of 33220A would be given.
        std::fs::write(dir.join("pictures").join("33220A.png"), b"somebody else's file").unwrap();
        std::env::set_var("SYMBIOTIC_PICTURES", &conf);

        let a = add("33220A", false, &png(), None).unwrap();
        // The same letters and digits, another pattern: another name still.
        let b = add("33220A!", true, &png(), None).unwrap();
        // The same pattern again: its rule is changed where it is and keeps its file.
        let again = add("33220A", true, &png(), None).unwrap();
        let rules = load();
        let wrong = (add("", false, &png(), None).is_err(), add("x\ny", false, &png(), None).is_err(), add("X", false, b"GIF89a", None).is_err());
        let kept = std::fs::read(dir.join("pictures").join("33220A.png")).unwrap();
        let text = std::fs::read_to_string(&conf).unwrap();
        let had = (dir.join("pictures").join(&a).exists(), dir.join("pictures").join(&b).exists());
        let removed = (remove("33220A").unwrap(), remove("33220A").unwrap(), remove("nothing").unwrap());
        let after = (dir.join("pictures").join(&a).exists(), dir.join("pictures").join(&b).exists(), load().len());
        let text_after = std::fs::read_to_string(&conf).unwrap();
        std::env::remove_var("SYMBIOTIC_PICTURES");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(a, "33220A-2.png", "{a}");
        assert_eq!(b, "33220A-3.png", "{b}");
        assert_eq!(again, a, "the same pattern keeps its file");
        assert_eq!(wrong, (true, true, true));
        assert_eq!(kept, b"somebody else's file", "the user's own file is not touched");
        let order: Vec<(&str, bool)> = rules.iter().map(|r| (r.pattern.as_str(), r.dark_keep)).collect();
        // The newest first, after the comment and before the rule that was there; the one added again stayed where it was.
        assert_eq!(order, [("33220A!", true), ("33220A", true), ("OLD", false)], "{order:?}");
        assert!(text.starts_with("# mine\n") && text.contains("OLD = old.png"), "{text}");
        assert_eq!(had, (true, true));
        assert_eq!(removed, (true, false, false));
        assert_eq!(after, (false, true, 2), "its file goes with it; the other pattern's stays");
        assert!(text_after.contains("# mine") && !text_after.contains("33220A ="), "{text_after}");
    }

    #[test]
    fn editing_a_pattern_replaces_the_rule_in_its_place() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-pictures-edit-{}", std::process::id()));
        let conf = dir.join("pictures.conf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&conf, "# mine\nFIRST = first.png\nSECOND = second.png dark=keep\nTHIRD = third.png\n").unwrap();
        std::fs::create_dir_all(dir.join("pictures")).unwrap();
        for f in ["first.png", "second.png", "third.png"] {
            std::fs::write(dir.join("pictures").join(f), b"old").unwrap();
        }
        std::env::set_var("SYMBIOTIC_PICTURES", &conf);

        // SECOND is edited to SECOND|2nd: the same place, the same file, no second rule.
        let renamed = add("SECOND|2nd", false, &png(), Some("SECOND")).unwrap();
        let after_rename: Vec<(String, String, bool)> = load().into_iter().map(|r| (r.pattern, r.file, r.dark_keep)).collect();
        let bytes = std::fs::read(dir.join("pictures").join("second.png")).unwrap();
        // The pattern of a rule that is gone is no reason to fail: it is a new rule.
        let fresh = add("NEW", false, &png(), Some("NOT THERE")).unwrap();
        // An edit into the pattern of another rule: the other's file is written, the edited rule's own file goes (nothing else uses it).
        let merged = add("THIRD", true, &png(), Some("SECOND|2nd")).unwrap();
        let after_merge: Vec<(String, String)> = load().into_iter().map(|r| (r.pattern, r.file)).collect();
        let second_gone = !dir.join("pictures").join("second.png").exists();
        let third_new = std::fs::read(dir.join("pictures").join("third.png")).unwrap() != b"old";
        std::env::remove_var("SYMBIOTIC_PICTURES");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(renamed, "second.png");
        assert_eq!(
            after_rename,
            [("FIRST".into(), "first.png".into(), false), ("SECOND|2nd".into(), "second.png".into(), false), ("THIRD".into(), "third.png".into(), false)],
            "one rule fewer than a new one would make: the edited one, in its place"
        );
        assert_ne!(bytes, b"old", "its picture is the new one");
        assert_eq!(fresh, "NEW.png");
        assert_eq!(merged, "third.png");
        assert_eq!(
            after_merge,
            [("NEW".to_string(), "NEW.png".to_string()), ("FIRST".to_string(), "first.png".to_string()), ("THIRD".to_string(), "third.png".to_string())],
            "{after_merge:?}"
        );
        assert!(second_gone && third_new);
    }

    #[test]
    fn the_original_and_the_edit_are_kept_with_the_picture() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-pictures-orig-{}", std::process::id()));
        let conf = dir.join("pictures.conf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("SYMBIOTIC_PICTURES", &conf);
        let jpg = b"\xff\xd8\xff the whole photograph".to_vec();
        let edit = r#"{"zoom":250,"panX":-120.5,"panY":-40,"height":279,"lineArt":true,"threshold":192,"keep":false}"#;

        let file = save(&Save { pattern: "34465A", dark_keep: false, png: &png(), replaces: None, original: Some(("jpg", &jpg)), edit: Some(edit) }).unwrap();
        let first = read_original(&file).unwrap().map(|o| (o.ext, o.bytes, o.edit));
        // Edited again from the original, with other parameters, and a different kind of file: one original, the latest.
        let png_original = png();
        let edit2 = r#"{"zoom":100}"#;
        let again = save(&Save { pattern: "34465A", dark_keep: false, png: &png(), replaces: None, original: Some(("png", &png_original)), edit: Some(edit2) }).unwrap();
        let second = read_original(&again).unwrap().map(|o| (o.ext, o.edit));
        let jpg_left = dir.join("pictures").join("originals").join("34465A.jpg").exists();
        // Saved without them (a caller that has none): what is there stays.
        save(&Save { pattern: "34465A", dark_keep: true, png: &png(), replaces: None, original: None, edit: None }).unwrap();
        let kept = read_original(&file).unwrap().map(|o| (o.ext, o.edit));
        // A picture with none: nothing, and no error.
        let hand = add("HAND", false, &png(), None).unwrap();
        let none = read_original(&hand).unwrap().is_none();
        // Nothing is written when something is wrong with the original or the edit.
        let before = std::fs::read_dir(dir.join("pictures")).unwrap().count();
        let wrong = (
            save(&Save { pattern: "X", dark_keep: false, png: &png(), replaces: None, original: Some(("exe", b"MZ")), edit: None }).is_err(),
            save(&Save { pattern: "X", dark_keep: false, png: &png(), replaces: None, original: Some(("jpg", b"")), edit: None }).is_err(),
            save(&Save { pattern: "X", dark_keep: false, png: &png(), replaces: None, original: None, edit: Some("") }).is_err(),
            read_original("../x.png").is_err(),
        );
        let unchanged = std::fs::read_dir(dir.join("pictures")).unwrap().count() == before;
        // Removing the picture takes the original and the edit with it.
        let removed = remove("34465A").unwrap();
        let left: Vec<String> = std::fs::read_dir(dir.join("pictures").join("originals")).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        std::env::remove_var("SYMBIOTIC_PICTURES");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(first, Some(("jpg".to_string(), jpg, Some(edit.to_string()))));
        assert_eq!(again, file, "the same picture");
        assert_eq!(second, Some(("png".to_string(), Some(edit2.to_string()))));
        assert!(!jpg_left, "the old original is replaced, not kept beside");
        assert_eq!(kept, Some(("png".to_string(), Some(edit2.to_string()))));
        assert!(none);
        assert_eq!(wrong, (true, true, true, true));
        assert!(unchanged, "nothing was written");
        assert!(removed && left.is_empty(), "{left:?}");
    }
}
