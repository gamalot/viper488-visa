//! The daemon's log and a diagnostics bundle: what a user gives when something is wrong, gathered in one zip. Nothing
//! here starts a daemon (that would take the adapters), opens an instrument or scans the network; private keys and the
//! 82357B's firmware are never put in.

use nusb::MaybeFuture;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The last `n` lines of the daemon's log, the set-aside `.1` before it counted in.
pub fn log_tail(n: usize) -> String {
    let p = crate::paths::log_file();
    let mut lines: Vec<String> = Vec::new();
    for f in [p.with_extension("log.1"), p] {
        if let Ok(t) = std::fs::read(&f) {
            lines.extend(String::from_utf8_lossy(&t).lines().map(str::to_string));
        }
    }
    let from = lines.len().saturating_sub(n);
    lines[from..].iter().map(|l| format!("{l}\n")).collect()
}

/// A log line as a person reads it: its seconds since 1970 as a time on this machine's clock.
pub fn show_log_line(l: &str, offset_s: i64) -> String {
    let Some((t, rest)) = l.split_once(' ') else { return l.to_string() };
    let Ok(t) = t.parse::<i64>() else { return l.to_string() };
    let day = (t + offset_s).rem_euclid(86_400);
    let date = civil((t + offset_s).div_euclid(86_400));
    format!("{date} {:02}:{:02}:{:02} {rest}", day / 3600, day / 60 % 60, day % 60)
}

/// Days since 1970-01-01 as YYYY-MM-DD (Howard Hinnant's civil_from_days).
fn civil(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

/// The files of the bundle: (name in the zip, contents).
pub fn collect() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = vec![
        ("summary.txt".into(), summary().into_bytes()),
        ("usb.txt".into(), usb().into_bytes()),
        ("serial.txt".into(), serial().into_bytes()),
        ("lan.txt".into(), lan().into_bytes()),
        ("share.txt".into(), share().into_bytes()),
    ];
    if cfg!(windows) {
        out.push(("windows-visa.txt".into(), windows_visa().into_bytes()));
    }
    for (name, path) in config_files() {
        if let Some(p) = path {
            if let Ok(b) = std::fs::read(&p) {
                out.push((format!("config/{name}"), b));
            }
        }
    }
    let offset = crate::iotrace::local_offset_s();
    let log: String = log_tail(2000).lines().map(|l| show_log_line(l, offset) + "\n").collect();
    out.push(("symbioticd.log".into(), log.into_bytes()));
    // Who used this machine's shared instruments (share/usage.rs).
    let used: String = crate::share::usage::read_from(&crate::share::usage::path(), 500).iter().map(|r| r.show(offset) + "\n").collect();
    if !used.is_empty() {
        out.push(("share-usage.txt".into(), used.into_bytes()));
    }
    out
}

/// Collect the bundle and write it in `dir` (the Downloads folder when None); its path.
pub fn write_bundle(dir: Option<&Path>) -> io::Result<PathBuf> {
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => crate::paths::downloads().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Downloads folder or home directory"))?,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = dir.join(format!("symbiotic-diag-{now}.zip"));
    let files = collect();
    // A zip's times are the clock's where it was made: this machine's.
    let local = (now as i64 + crate::iotrace::local_offset_s()).max(0) as u64;
    crate::conffile::write(&path, zip(&files, local))?;
    Ok(path)
}

/// The configuration files, by name: every one the user may have, none that holds a secret.
fn config_files() -> Vec<(&'static str, Option<PathBuf>)> {
    vec![
        ("aliases.conf", crate::alias::path()),
        ("identities.conf", crate::identity::path()),
        ("instruments.conf", crate::instruments::path()),
        ("lan.conf", crate::lan::conf::path()),
        ("bridges.conf", crate::prologix::conf_path()),
        ("speed.conf", crate::speedconf::path()),
        ("ren.conf", crate::renconf::path()),
        ("daemon.conf", crate::daemonconf::path()),
        ("features.conf", crate::features::path()),
        ("share.conf", crate::share::conf::ShareConf::path()),
        ("shares.conf", crate::share::conf::shares_path()),
        ("pictures.conf", crate::pictures::conf_path()),
    ]
}

fn summary() -> String {
    let mut s = String::new();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let offset = crate::iotrace::local_offset_s();
    let _ = writeln!(s, "Symbiotic VISA diagnostics, {} (UTC{:+})", show_log_line(&format!("{now} -"), offset).trim_end_matches(" -"), offset as f64 / 3600.0);
    let _ = writeln!(s, "build {} (IPC v{})", crate::ipc::BUILD, crate::ipc::VERSION);
    let _ = writeln!(s, "system {} {}{}", std::env::consts::OS, std::env::consts::ARCH, os_version().map(|v| format!(", {v}")).unwrap_or_default());
    let _ = writeln!(s, "program {}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    let _ = writeln!(s, "\npaths");
    let _ = writeln!(s, "  configuration {}", crate::paths::config_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()));
    let _ = writeln!(s, "  this user's    {}", crate::paths::user_file("").map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()));
    let _ = writeln!(s, "  log           {}", crate::paths::log_file().display());
    let _ = writeln!(s, "  I/O trace     {} ({})", crate::iotrace::dir().display(), if crate::iotrace::is_on(&crate::iotrace::dir()) { "on" } else { "off" });
    for d in crate::paths::install_dirs() {
        let _ = writeln!(s, "  installed in  {}", d.display());
    }
    let vars: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k.starts_with("SYMBIOTIC_")).collect();
    if !vars.is_empty() {
        let _ = writeln!(s, "\nenvironment");
        for (k, v) in vars {
            let _ = writeln!(s, "  {k}={v}");
        }
    }
    let _ = writeln!(s, "\ndaemon");
    match crate::client::Client::connect_existing() {
        Err(_) => {
            let _ = writeln!(s, "  not running (not started for this: it would take the adapters)");
        }
        Ok(mut c) => {
            match c.daemon_version() {
                Ok((v, b)) => {
                    let _ = writeln!(s, "  build {} (IPC v{v})", if b.is_empty() { "(older than saying it)" } else { &b });
                }
                Err(e) => {
                    let _ = writeln!(s, "  does not answer: {e}");
                }
            }
            match c.list() {
                Ok(l) => {
                    for (i, x) in l.iter().enumerate() {
                        let _ = writeln!(s, "  board {i}: {} serial {} pid {:#06x}{}", x.entry.kind.name(), x.entry.serial, x.entry.pid, if x.present { "" } else { " (not attached)" });
                    }
                    if l.is_empty() {
                        let _ = writeln!(s, "  no GPIB adapter");
                    }
                }
                Err(e) => {
                    let _ = writeln!(s, "  adapters: {e}");
                }
            }
            if let Ok(l) = c.usb_list() {
                for (i, u) in l.iter().enumerate() {
                    let _ = writeln!(s, "  USB{i}: {:04X}:{:04X} {} {}{}", u.vid, u.pid, u.serial, u.product, if u.present { "" } else { " (not attached)" });
                }
            }
            match c.clients() {
                Ok(l) if l.is_empty() => {
                    let _ = writeln!(s, "  no other process connected");
                }
                Ok(l) => {
                    let who: Vec<String> = l.iter().map(|c| c.who()).collect();
                    let _ = writeln!(s, "  connected processes: {}", who.join(", "));
                }
                Err(e) => {
                    let _ = writeln!(s, "  connected processes: {e}");
                }
            }
        }
    }
    s
}

#[cfg(windows)]
fn os_version() -> Option<String> {
    run("cmd", &["/C", "ver"]).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

#[cfg(not(windows))]
fn os_version() -> Option<String> {
    let t = std::fs::read_to_string("/etc/os-release").ok();
    let pretty = t.as_deref().and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string()));
    let kernel = run("uname", &["-sr"]).map(|k| k.trim().to_string());
    match (pretty, kernel) {
        (Some(p), Some(k)) => Some(format!("{p}, {k}")),
        (p, k) => p.or(k).or_else(|| run("sw_vers", &["-productVersion"]).map(|v| format!("macOS {}", v.trim()))),
    }
}

/// A command's output, or None if it could not be run. On Windows without a console window: the configuration window
/// makes the bundle too, and each command would flash one up.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new(cmd);
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let o = c.output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
}

fn usb() -> String {
    let mut s = String::new();
    let _ = writeln!(s, "USB devices the system lists (descriptors only: nothing is opened)");
    match nusb::list_devices().wait() {
        Ok(all) => {
            for d in all {
                let _ = writeln!(
                    s,
                    "  {:04X}:{:04X}  {} | {} | serial {}",
                    d.vendor_id(),
                    d.product_id(),
                    d.manufacturer_string().unwrap_or(""),
                    d.product_string().unwrap_or(""),
                    d.serial_number().unwrap_or("")
                );
            }
        }
        Err(e) => {
            let _ = writeln!(s, "  cannot enumerate: {e}");
        }
    }
    let _ = writeln!(s, "\nGPIB adapters, by kind");
    let mut kind = |name: &str, r: Result<Vec<String>, crate::adapter::Error>| {
        match r {
            Ok(v) if v.is_empty() => {}
            Ok(v) => v.iter().for_each(|x| {
                let _ = writeln!(s, "  {name}: {x}");
            }),
            Err(e) => {
                let _ = writeln!(s, "  {name}: {e}");
            }
        }
    };
    kind("viper488", crate::adapter::list().map(|v| v.into_iter().map(|d| format!("serial {} pid {:#06x}", d.serial, d.pid)).collect()));
    let entries = |v: Vec<crate::backend::Entry>| v.into_iter().map(|d| format!("serial {} pid {:#06x}", d.serial, d.pid)).collect();
    kind("ni", crate::ni::list().map(entries));
    kind("keysight", crate::ks::list().map(entries));
    kind("contec", crate::contec::list().map(entries));
    let _ = writeln!(s, "\nUSBTMC instruments");
    match crate::usbtmc::list_all() {
        Ok(all) if all.is_empty() => {
            let _ = writeln!(s, "  none");
        }
        Ok(all) => {
            for f in all {
                let why = f.not_ours.as_deref().map(|w| format!("not used: {w}")).unwrap_or_else(|| if f.driver.is_empty() { "ours".into() } else { format!("ours ({})", f.driver) });
                let _ = writeln!(s, "  USB0::0x{:04X}::0x{:04X}::{}::INSTR  {} {}{}  {why}", f.vid, f.pid, f.serial, f.manufacturer, f.product, if f.usb488 { ", USB488" } else { "" });
            }
        }
        Err(e) => {
            let _ = writeln!(s, "  {e}");
        }
    }
    s
}

fn serial() -> String {
    let mut s = String::from("serial ports (nothing is opened)\n");
    let bridges = crate::prologix::configured();
    match crate::serial::list() {
        Ok(l) if l.is_empty() => s.push_str("  none\n"),
        Ok(l) => {
            for f in l {
                let bridge = bridges.iter().find(|(b, _)| *b == f.n).map(|(_, d)| format!("  bridge ({})", d.map_or("detected", |d| d.name()))).unwrap_or_default();
                let _ = writeln!(s, "  {} = {} ({}){bridge}", crate::resource::format_asrl(f.n), crate::serial::port_name(f.n), f.device);
            }
        }
        Err(e) => {
            let _ = writeln!(s, "  {e}");
        }
    }
    s
}

fn lan() -> String {
    let mut s = String::from("LAN instruments (configured; the network is not scanned)\n");
    match crate::lan::conf::path() {
        Some(p) => {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let _ = writeln!(s, "  {} ({} configured, discovery {})", p.display(), crate::lan::conf::parse(&text).len(), if crate::lan::conf::discover_in(&text) { "on" } else { "off" });
        }
        None => s.push_str("  no lan.conf\n"),
    }
    s
}

fn share() -> String {
    use crate::share::{conf, keys, member::Member};
    let mut s = String::new();
    let mut c = conf::ShareConf::load();
    let _ = writeln!(s, "sharing {}, group {}, name {}, serves on {}, follow {}", if c.share { "on" } else { "off" }, if c.joined() { c.registry.as_str() } else { "(none)" }, c.name, c.listen, if c.follow { "on" } else { "off" });
    if let Some(r) = &c.registry_listen {
        let _ = writeln!(s, "registry in this daemon, on {r}");
    }
    for a in conf::load_shares() {
        let _ = writeln!(s, "shares {a}");
    }
    if !c.joined() {
        return s;
    }
    // Read only: the directory and who is online. Nothing is claimed and no heartbeat is sent.
    c.share = false;
    let Some(key) = crate::paths::user_file("share.key").and_then(|p| keys::load(&p)) else {
        let _ = writeln!(s, "no share.key");
        return s;
    };
    let m = Member::new(c, key.noise, crate::paths::user_file("share.dir"), crate::alias::load, conf::load_shares);
    match m.sync() {
        Ok(_) => {
            let _ = writeln!(s, "registry answers");
        }
        Err(e) => {
            let _ = writeln!(s, "registry: {e} (the cached directory below, if any)");
        }
    }
    if let Some(d) = m.directory() {
        let presence = m.presence();
        let _ = writeln!(s, "directory version {}", d.version);
        for mem in &d.members {
            let p = presence.iter().find(|p| p.key == mem.key);
            let _ = writeln!(s, "  member {}{}{}", mem.name, if mem.key == m.public_key() { " (this machine)" } else { "" }, match p {
                Some(p) if p.online => format!("  online {}", p.addrs.join(" ")),
                Some(_) => "  offline".to_string(),
                None => String::new(),
            });
        }
        for cl in &d.claims {
            let owner = if cl.owner == crate::share::registry::GROUP_OWNER { "(group)" } else { d.member(&cl.owner).map_or("?", |m| m.name.as_str()) };
            let _ = writeln!(s, "  alias {} -> {owner} {}", cl.alias, if cl.resource.is_empty() { "relay" } else { &cl.resource });
        }
    }
    s
}

#[cfg(windows)]
fn windows_visa() -> String {
    let mut s = String::from("The IVI VISA router's installations (who answers viOpenDefaultRM through it)\n");
    for key in [r"HKLM\SOFTWARE\VXIPNP_Alliance\VISA_Installs", r"HKLM\SOFTWARE\WOW6432Node\VXIPNP_Alliance\VISA_Installs"] {
        let _ = writeln!(s, "\n> reg query {key} /s");
        s += &run("reg", &["query", key, "/s"]).unwrap_or_else(|| "  (reg could not be run)\n".into());
    }
    s
}

#[cfg(not(windows))]
fn windows_visa() -> String {
    String::new()
}

// ---- a zip, stored (not compressed): enough for a bundle of text, and every unzip opens it -------------------

fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// Seconds since 1970 as MS-DOS date and time (a zip has no zone: the caller gives the local clock's).
fn dos_time(t: u64) -> (u16, u16) {
    let days = (t / 86_400) as i64;
    let date = civil(days);
    let (y, m, d): (u16, u16, u16) = (date[0..4].parse().unwrap_or(1980), date[5..7].parse().unwrap_or(1), date[8..10].parse().unwrap_or(1));
    let s = t % 86_400;
    let time = ((s / 3600) << 11 | (s / 60 % 60) << 5 | ((s % 60) / 2)) as u16;
    (time, (y.saturating_sub(1980)) << 9 | m << 5 | d)
}

/// The files as a zip archive, every one stored, dated `now`.
pub fn zip(files: &[(String, Vec<u8>)], now: u64) -> Vec<u8> {
    let (time, date) = dos_time(now);
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let (crc, size, at) = (crc32(data), data.len() as u32, out.len() as u32);
        // Local header: version 2.0, flag 0x800 (the name is UTF-8), method 0 (stored).
        let head = |sig: u32, w: &mut Vec<u8>, central: bool| {
            w.extend_from_slice(&sig.to_le_bytes());
            if central {
                w.extend_from_slice(&20u16.to_le_bytes()); // made by
            }
            for v in [20u16, 0x0800, 0, time, date] {
                w.extend_from_slice(&v.to_le_bytes());
            }
            for v in [crc, size, size] {
                w.extend_from_slice(&v.to_le_bytes());
            }
            w.extend_from_slice(&(name.len() as u16).to_le_bytes());
            w.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                for v in [0u16, 0, 0] {
                    w.extend_from_slice(&v.to_le_bytes()); // comment, disk, internal attributes
                }
                w.extend_from_slice(&0u32.to_le_bytes()); // external attributes
                w.extend_from_slice(&at.to_le_bytes());
            }
            w.extend_from_slice(name.as_bytes());
        };
        head(0x0403_4b50, &mut out, false);
        out.extend_from_slice(data);
        head(0x0201_4b50, &mut central, true);
    }
    let (cd_at, cd_len, n) = (out.len() as u32, central.len() as u32, files.len() as u16);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, n, n] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Write `text` to `w` (the command line's `log`), each line on this machine's clock.
pub fn print_log(w: &mut impl Write, text: &str) -> io::Result<()> {
    let offset = crate::iotrace::local_offset_s();
    for l in text.lines() {
        writeln!(w, "{}", show_log_line(l, offset))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_dates() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "the check value of CRC-32/ISO-HDLC");
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(20_368), "2025-10-07");
        assert_eq!(show_log_line("1791351941 pid 7 started", 8 * 3600), "2026-10-07 13:45:41 pid 7 started");
        assert_eq!(show_log_line("not a log line", 0), "not a log line");
        assert_eq!(dos_time(0).1 >> 9, 0, "1970 is before the DOS epoch: clamped to 1980");
    }

    /// The archive read back field by field, as an unzip would: every name and every byte.
    #[test]
    fn a_zip_any_unzip_reads() {
        let files = vec![("summary.txt".to_string(), b"hello\n".to_vec()), ("config/aliases.conf".to_string(), (0..=255u8).collect()), ("empty".to_string(), vec![])];
        let z = zip(&files, 1_791_351_941);
        let u16_at = |i: usize| u16::from_le_bytes([z[i], z[i + 1]]) as usize;
        let u32_at = |i: usize| u32::from_le_bytes(z[i..i + 4].try_into().unwrap()) as usize;
        let end = z.len() - 22;
        assert_eq!(u32_at(end), 0x0605_4b50);
        assert_eq!(u16_at(end + 10), 3);
        let mut at = u32_at(end + 16);
        for (name, data) in &files {
            assert_eq!(u32_at(at), 0x0201_4b50);
            let (crc, size, nlen, local) = (u32_at(at + 16), u32_at(at + 24), u16_at(at + 28), u32_at(at + 42));
            assert_eq!(&z[at + 46..at + 46 + nlen], name.as_bytes());
            assert_eq!((crc as u32, size), (crc32(data), data.len()));
            assert_eq!(u32_at(local), 0x0403_4b50);
            let body = local + 30 + u16_at(local + 26);
            assert_eq!(&z[body..body + size], data.as_slice());
            at += 46 + nlen;
        }
    }

    #[test]
    fn a_bundle_holds_no_secret() {
        let names: Vec<&str> = config_files().iter().map(|(n, _)| *n).collect();
        assert!(!names.iter().any(|n| n.contains("key") || n.contains("firmware")), "{names:?}");
    }
}
