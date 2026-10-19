//! I/O trace: what each VISA call did, written by the library in every process that uses it, read back by
//! `symbiotic-ctl trace`.
//!
//! Each process writes its own file, `<pid>.trace` in the user's trace directory, one record a line, tab-separated;
//! nothing goes through the daemon. A file named `on` in the directory turns it on; the library looks for it at most
//! once a second, so a trace that is off costs one atomic load a call.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The bytes of a read or a write a record keeps.
pub const DATA_MAX: usize = 64;
/// A process's file is set aside (as `.1`, the one before going) past this.
const FILE_MAX: u64 = 8 << 20;
/// Every process's records together: past this, the oldest files go when a process opens its own (a script
/// run again and again with the trace on is a new file each time).
const TOTAL_MAX: u64 = 64 << 20;
const ON: &str = "on";
const EXT: &str = "trace";

/// The user's own trace directory: `SYMBIOTIC_TRACE_DIR` if set (a test's own); on Windows
/// `%LOCALAPPDATA%\Symbiotic\trace` (the user's even after an all-users install), elsewhere beside the
/// daemon's log (`paths::log_file`: `~/.local/state/symbiotic`, `~/Library/Logs/Symbiotic`).
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SYMBIOTIC_TRACE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        if let Some(d) = crate::paths::user_file("trace") {
            return d;
        }
    }
    let log = crate::paths::log_file();
    log.parent().map(|d| d.join("trace")).unwrap_or_else(|| PathBuf::from("trace"))
}

/// One call, as the library saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// When it started: microseconds since 1970.
    pub at_us: u64,
    pub pid: u32,
    /// 1 for what the application called, more for what that call made (viPrintf's viWrite).
    pub depth: u32,
    pub vi: u32,
    pub op: String,
    pub status: i32,
    pub dur_us: u64,
    /// The resource name ("" when the session has none, or is gone).
    pub resource: String,
    /// Bytes read or written, when the call moves bytes.
    pub count: Option<u32>,
    /// The first `DATA_MAX` of them.
    pub data: Vec<u8>,
}

impl Record {
    /// The record as its line (no line break): fields that cannot hold a tab or a line break.
    pub fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.at_us,
            self.pid,
            self.depth,
            self.vi,
            one_field(&self.op),
            self.status,
            self.dur_us,
            one_field(&self.resource),
            self.count.map(|c| c.to_string()).unwrap_or_default(),
            escape(&self.data)
        )
    }

    pub fn parse(line: &str) -> Option<Record> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 10 {
            return None;
        }
        Some(Record {
            at_us: f[0].parse().ok()?,
            pid: f[1].parse().ok()?,
            depth: f[2].parse().ok()?,
            vi: f[3].parse().ok()?,
            op: f[4].to_string(),
            status: f[5].parse().ok()?,
            dur_us: f[6].parse().ok()?,
            resource: f[7].to_string(),
            count: if f[8].is_empty() { None } else { Some(f[8].parse().ok()?) },
            data: unescape(f[9])?,
        })
    }
}

fn one_field(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Bytes as printable ASCII: `\n`, `\r`, `\t`, `\\` and `\xNN` for the rest.
pub fn escape(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        match c {
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7E => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02X}")),
        }
    }
    s
}

pub fn unescape(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1)? {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'\\' => out.push(b'\\'),
            b'x' => {
                out.push(u8::from_str_radix(std::str::from_utf8(b.get(i + 2..i + 4)?).ok()?, 16).ok()?);
                i += 2;
            }
            _ => return None,
        }
        i += 2;
    }
    Some(out)
}

// ---- the writing side (the library) ------------------------------------------------------------

static CHECKED_S: AtomicU64 = AtomicU64::new(0);
static IS_ON: AtomicBool = AtomicBool::new(false);

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether to trace: the `on` file, looked for at most once a second.
pub fn on() -> bool {
    let now = now_s();
    if CHECKED_S.load(Ordering::Relaxed) != now {
        CHECKED_S.store(now, Ordering::Relaxed);
        IS_ON.store(dir_of().join(ON).exists(), Ordering::Relaxed);
    }
    IS_ON.load(Ordering::Relaxed)
}

fn dir_of() -> &'static Path {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(dir)
}

/// This process's file, open while tracing.
static FILE: Mutex<Option<(PathBuf, File)>> = Mutex::new(None);

/// Write `r` to this process's file. Failing to is not the application's problem: nothing is said.
pub fn write(r: &Record) {
    let _ = write_in(dir_of(), r);
}

fn write_in(dir: &Path, r: &Record) -> io::Result<()> {
    let path = dir.join(format!("{}.{EXT}", std::process::id()));
    let mut g = FILE.lock().unwrap_or_else(|e| e.into_inner());
    // A file taken away (`trace clear`, or `on` again) or grown past its size is begun again.
    let size = std::fs::metadata(&path).map(|m| m.len()).ok();
    if size.is_none() || size.is_some_and(|s| s > FILE_MAX) || g.as_ref().is_some_and(|(p, _)| *p != path) {
        *g = None;
        if size.is_some() {
            let _ = std::fs::rename(&path, path.with_extension(format!("{EXT}.1")));
        }
    }
    if g.is_none() {
        std::fs::create_dir_all(dir)?;
        prune(dir, &path, TOTAL_MAX);
        let mut o = OpenOptions::new();
        o.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600); // an instrument's data is the user's
        let mut f = o.open(&path)?;
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        writeln!(f, "# pid {} {}", std::process::id(), one_field(&exe))?;
        *g = Some((path, f));
    }
    // One write per record, so a reader never sees half a line it would then take for whole.
    let (_, f) = g.as_mut().unwrap();
    f.write_all(format!("{}\n", r.line()).as_bytes())
}

/// Take the oldest record files away, by when they were last written, until those left come to `max` at most;
/// never `keep`. Another process whose file goes begins a new one at its next record, as after `clear`.
fn prune(dir: &Path, keep: &Path, max: u64) {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = trace_files(dir)
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (m.modified().unwrap_or(UNIX_EPOCH), m.len(), p)))
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, p) in files {
        if total <= max {
            break;
        }
        if p != keep && std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

// ---- the reading side (symbiotic-ctl) ----------------------------------------------------------

/// Turn tracing on in `dir` (the records there before go: a new capture), or off.
pub fn set_on(dir: &Path, on: bool) -> io::Result<()> {
    if on {
        clear(dir)?;
        std::fs::create_dir_all(dir)?;
        File::create(dir.join(ON)).map(|_| ())
    } else {
        match std::fs::remove_file(dir.join(ON)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

pub fn is_on(dir: &Path) -> bool {
    dir.join(ON).exists()
}

/// Take the records away; whether tracing is on stays as it is.
pub fn clear(dir: &Path) -> io::Result<()> {
    for f in trace_files(dir) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}

fn trace_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".trace") || n.ends_with(".trace.1")))
        .collect()
}

/// The programs that wrote: pid to its path, from the files' first lines.
pub fn programs(dir: &Path) -> std::collections::HashMap<u32, String> {
    let mut m = std::collections::HashMap::new();
    for p in trace_files(dir) {
        if let Some(Ok(l)) = File::open(&p).ok().and_then(|f| io::BufReader::new(f).lines().next()) {
            if let Some((pid, exe)) = l.strip_prefix("# pid ").and_then(|r| r.split_once(' ')) {
                if let Ok(pid) = pid.parse() {
                    m.insert(pid, exe.to_string());
                }
            }
        }
    }
    m
}

/// Where a reader is in each file, so `follow` gives only what is new.
#[derive(Default)]
pub struct Cursor {
    at: std::collections::HashMap<PathBuf, u64>,
}

impl Cursor {
    /// The records written since the last call (all of them the first time), in time order.
    pub fn next(&mut self, dir: &Path) -> Vec<Record> {
        use std::io::{Read, Seek, SeekFrom};
        let mut out = Vec::new();
        for p in trace_files(dir) {
            let Ok(mut f) = File::open(&p) else { continue };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let from = self.at.get(&p).copied().unwrap_or(0);
            // A file shorter than where we were has been begun again.
            let from = if from > len { 0 } else { from };
            if f.seek(SeekFrom::Start(from)).is_err() {
                continue;
            }
            let mut text = String::new();
            if f.read_to_string(&mut text).is_err() {
                continue;
            }
            // Only whole lines; a line still being written is read next time.
            let whole = text.rfind('\n').map_or(0, |i| i + 1);
            for l in text[..whole].lines() {
                if let Some(r) = Record::parse(l) {
                    out.push(r);
                }
            }
            self.at.insert(p, from + whole as u64);
        }
        out.sort_by_key(|r| (r.at_us, r.pid, r.depth));
        out
    }
}

/// A status's name (`VI_ERROR_TMO`), from the C header's definitions: every one the library can return.
pub fn status_name(st: i32) -> Option<&'static str> {
    static NAMES: OnceLock<std::collections::HashMap<i32, &'static str>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            let mut m = std::collections::HashMap::new();
            for l in include_str!("../../../include/visa.h").lines() {
                let mut w = l.split_whitespace();
                let (Some("#define"), Some(name), Some(v)) = (w.next(), w.next(), w.next()) else { continue };
                if !(name.starts_with("VI_SUCCESS") || name.starts_with("VI_WARN_") || name.starts_with("VI_ERROR_")) {
                    continue;
                }
                let v = v.trim_start_matches('(').trim_end_matches(')').trim_end_matches('L');
                let (err, hex) = match v.strip_prefix("_VI_ERROR+") {
                    Some(h) => (true, h),
                    None => (false, v),
                };
                let n = if let Some(h) = hex.strip_prefix("0x") { u32::from_str_radix(h, 16).ok() } else { hex.parse().ok() };
                if let Some(n) = n {
                    m.entry((if err { n | 0x8000_0000 } else { n }) as i32).or_insert(name);
                }
            }
            m
        })
        .get(&st)
        .copied()
}

/// How far this machine's clock is ahead of UTC now, in seconds (for showing times as the user reads a clock).
pub fn local_offset_s() -> i64 {
    #[cfg(unix)]
    {
        let t = now_s() as libc::time_t;
        // SAFETY: a time and a tm to fill; localtime_r is the reentrant one.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        // SAFETY: a structure for the call to fill.
        let mut z: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        let which = unsafe { GetTimeZoneInformation(&mut z) };
        // Bias is minutes to add to local time to get UTC; 2 is TIME_ZONE_ID_DAYLIGHT.
        let bias = z.Bias + if which == 2 { z.DaylightBias } else { z.StandardBias };
        -(bias as i64) * 60
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

/// A record as a person reads it: time of day (on a clock `offset_s` ahead of UTC), process, call,
/// resource, status, time taken, bytes.
pub fn show(r: &Record, offset_s: i64, status_name: impl Fn(i32) -> Option<&'static str>) -> String {
    let t = Duration::from_micros(r.at_us);
    let day = (t.as_secs() as i64 + offset_s).rem_euclid(86_400) as u64;
    let clock = format!("{:02}:{:02}:{:02}.{:06}", day / 3600, day / 60 % 60, day % 60, t.subsec_micros());
    let st = status_name(r.status).map(str::to_string).unwrap_or_else(|| format!("0x{:08X}", r.status as u32));
    let indent = "  ".repeat(r.depth.saturating_sub(1) as usize);
    let mut s = format!("{clock} {:>6} {indent}{} ({}{}) {st} {:.3} ms", r.pid, r.op, r.vi, if r.resource.is_empty() { String::new() } else { format!(" {}", r.resource) }, r.dur_us as f64 / 1000.0);
    if let Some(c) = r.count {
        s += &format!(" {c} B");
        if !r.data.is_empty() {
            s += &format!(" \"{}{}\"", escape(&r.data), if c as usize > r.data.len() { "..." } else { "" });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(at: u64, op: &str, data: &[u8]) -> Record {
        Record { at_us: at, pid: 7, depth: 1, vi: 3, op: op.into(), status: 0, dur_us: 1500, resource: "GPIB0::15::INSTR".into(), count: Some(data.len() as u32), data: data.to_vec() }
    }

    #[test]
    fn a_record_round_trips_whatever_its_bytes() {
        let all: Vec<u8> = (0..=255u8).collect();
        for d in [&b"*IDN?\n"[..], b"", b"a\tb\\c\r\n", &all] {
            let r = rec(1, "viWrite", d);
            assert!(!r.line().contains('\n') && r.line().split('\t').count() == 10);
            assert_eq!(Record::parse(&r.line()), Some(r));
        }
        let mut odd = rec(1, "vi\tRead", b"x");
        odd.resource = "a\nb".into();
        let back = Record::parse(&odd.line()).unwrap();
        assert_eq!((back.op.as_str(), back.resource.as_str()), ("vi Read", "a b"), "a tab or a line break in a name does not break the line");
        assert_eq!(Record::parse("garbage"), None);
    }

    #[test]
    fn written_by_one_process_read_in_order_and_only_what_is_new() {
        let d = std::env::temp_dir().join(format!("symbiotic-iotrace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        set_on(&d, true).unwrap();
        assert!(is_on(&d));
        write_in(&d, &rec(20, "viRead", b"KEITHLEY\n")).unwrap();
        write_in(&d, &rec(10, "viWrite", b"*IDN?\n")).unwrap();
        // Another process's file, older and newer records among ours.
        std::fs::write(d.join("99.trace"), format!("# pid 99 /bin/other\n{}\n{}\n", Record { pid: 99, ..rec(15, "viClear", b"") }.line(), Record { pid: 99, ..rec(30, "viClose", b"") }.line())).unwrap();
        let mut c = Cursor::default();
        let ops: Vec<String> = c.next(&d).into_iter().map(|r| r.op).collect();
        assert_eq!(ops, ["viWrite", "viClear", "viRead", "viClose"], "merged by time");
        assert!(c.next(&d).is_empty(), "nothing new");
        write_in(&d, &rec(40, "viWrite", b"x")).unwrap();
        assert_eq!(c.next(&d).len(), 1, "only the new one");
        assert_eq!(programs(&d).get(&99).map(String::as_str), Some("/bin/other"));
        clear(&d).unwrap();
        assert!(c.next(&d).is_empty() && is_on(&d), "cleared, still on");
        write_in(&d, &rec(50, "viRead", b"y")).unwrap();
        assert_eq!(c.next(&d).len(), 1, "a file begun again after a clear is read from its start");
        set_on(&d, false).unwrap();
        assert!(!is_on(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn past_the_total_the_oldest_files_go_but_never_the_one_being_begun() {
        let d = std::env::temp_dir().join(format!("symbiotic-iotrace-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let t0 = SystemTime::now() - Duration::from_secs(100);
        for (i, name) in ["1.trace.1", "1.trace", "2.trace", "3.trace", "4.trace"].iter().enumerate() {
            let f = File::create(d.join(name)).unwrap();
            f.set_len(100).unwrap();
            f.set_modified(t0 + Duration::from_secs(i as u64)).unwrap();
        }
        std::fs::write(d.join("notes.txt"), "not a record").unwrap();
        prune(&d, &d.join("1.trace"), 300);
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["1.trace", "3.trace", "4.trace", "notes.txt"], "the oldest went, the one kept stays though it is old");
        prune(&d, &d.join("1.trace"), 300);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 4, "under the total, nothing goes");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn statuses_by_name() {
        assert_eq!(status_name(0), Some("VI_SUCCESS"));
        assert_eq!(status_name(0x3FFF0005), Some("VI_SUCCESS_TERM_CHAR"));
        assert_eq!(status_name(0xBFFF0015u32 as i32), Some("VI_ERROR_TMO"));
        assert_eq!(status_name(12345), None);
    }

    #[test]
    fn shown_for_a_person() {
        let r = Record { depth: 2, ..rec(3_600_000_123, "viWrite", b"*IDN?\n") };
        let s = show(&r, 0, |st| (st == 0).then_some("VI_SUCCESS"));
        assert!(s.starts_with("01:00:00.000123 "), "{s}");
        assert!(show(&r, 8 * 3600, |_| None).starts_with("09:00:00.000123 "), "on the local clock");
        assert!(show(&r, -2 * 3600, |_| None).starts_with("23:00:00.000123 "), "behind UTC, the day before");
        assert!(s.contains("  viWrite (3 GPIB0::15::INSTR) VI_SUCCESS 1.500 ms 6 B \"*IDN?\\n\""), "{s}");
    }
}
