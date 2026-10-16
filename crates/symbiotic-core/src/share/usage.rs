//! The owner's record of who used the instruments it shares: one line per session another member opened through a
//! shared alias, appended when it ends, in `usage.log` beside the daemon's log. `symbiotic-ctl share usage` and the
//! configuration window's sharing page read it; the diagnostics bundle takes it.
//!
//! A line is tab-separated: when it began (Unix seconds), how long it lasted (s), the alias, the member's name, the
//! first bytes of its key (hex), the address it came from, the requests it made, and how many of those were refused
//! (operations not offered over an alias).

use std::io::Write;
use std::path::{Path, PathBuf};

/// The file is started again from empty, the old one kept as `usage.log.1`, past this size.
const MAX: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub began: u64,
    pub secs: f64,
    pub alias: String,
    pub member: String,
    pub key: String,
    pub from: String,
    pub requests: u64,
    pub refused: u64,
}

/// A value as one field: no tab or other control character (a name came from another machine).
fn field(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl Record {
    pub fn line(&self) -> String {
        format!(
            "{}\t{:.1}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.began,
            self.secs,
            field(&self.alias),
            field(&self.member),
            field(&self.key),
            field(&self.from),
            self.requests,
            self.refused
        )
    }

    pub fn parse(line: &str) -> Option<Record> {
        let f: Vec<&str> = line.split('\t').collect();
        let [began, secs, alias, member, key, from, requests, refused] = f[..] else { return None };
        Some(Record {
            began: began.parse().ok()?,
            secs: secs.parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0)?,
            alias: alias.into(),
            member: member.into(),
            key: key.into(),
            from: from.into(),
            requests: requests.parse().ok()?,
            refused: refused.parse().ok()?,
        })
    }
}

impl Record {
    /// As a person reads it, the time on this machine's clock (`offset_s`: iotrace::local_offset_s).
    pub fn show(&self, offset_s: i64) -> String {
        let refused = if self.refused > 0 { format!(", {} refused", self.refused) } else { String::new() };
        crate::diag::show_log_line(
            &format!("{} {}  {} ({}) from {}  {:.1} s  {} request(s){refused}", self.began, self.alias, self.member, self.key, self.from, self.secs, self.requests),
            offset_s,
        )
    }
}

pub fn path() -> PathBuf {
    crate::paths::log_file().with_file_name("usage.log")
}

/// Add `r` to the file at `p` (the older half kept as `.1` when it is full).
pub fn append_to(p: &Path, r: &Record) -> std::io::Result<()> {
    if std::fs::metadata(p).is_ok_and(|m| m.len() > MAX) {
        let _ = std::fs::rename(p, p.with_extension("log.1"));
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    writeln!(o.open(p)?, "{}", r.line())
}

/// The last `n` records in the file at `p` (and its `.1`), oldest first.
pub fn read_from(p: &Path, n: usize) -> Vec<Record> {
    let mut v: Vec<Record> = [p.with_extension("log.1"), p.to_path_buf()]
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .flat_map(|t| t.lines().filter_map(Record::parse).collect::<Vec<_>>())
        .collect();
    let drop = v.len().saturating_sub(n);
    v.drain(..drop);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_a_name_cannot_break_the_line() {
        let r = Record { began: 1_791_000_000, secs: 12.5, alias: "K2015".into(), member: "wsl\tubuntu\nx".into(), key: "a1b2c3d4".into(), from: "172.20.0.2:51234".into(), requests: 40, refused: 1 };
        let l = r.line();
        assert!(!l.contains('\n') && l.split('\t').count() == 8, "{l:?}");
        assert_eq!(Record::parse(&l), Some(Record { member: "wsl ubuntu x".into(), ..r }));
        assert_eq!(Record::parse("1\t2\tonly three"), None);
        assert_eq!(Record::parse("1\tNaN\ta\tb\tc\td\t1\t0"), None);
    }

    #[test]
    fn the_file_keeps_the_newest_and_turns_over() {
        let dir = std::env::temp_dir().join(format!("symbiotic-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("usage.log");
        let rec = |i: u64| Record { began: i, secs: 1.0, alias: "A".into(), member: "m".into(), key: "k".into(), from: "f".into(), requests: i, refused: 0 };
        for i in 0..20_000 {
            append_to(&p, &rec(i)).unwrap();
        }
        assert!(p.with_extension("log.1").exists(), "turned over");
        assert!(std::fs::metadata(&p).unwrap().len() <= MAX + 100);
        let last = read_from(&p, 3);
        assert_eq!(last.iter().map(|r| r.began).collect::<Vec<_>>(), [19_997, 19_998, 19_999]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
