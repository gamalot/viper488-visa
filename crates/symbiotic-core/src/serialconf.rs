//! A serial port's default settings: what the daemon gives a port when it opens one nobody has open.
//!
//! Only the user's own configuring writes this file -- Symbiotic Explorer's console and
//! `symbiotic-ctl serial set|reset` -- never a program's VISA attributes: those last while the port
//! is open, so one program changing the rate for a while does not hand the next one, which counts
//! on the default, another rate. The daemon only reads it.
//!
//! One line per port, `<port> baud=115200 data=8 parity=none stop=1 flow=none`, the port named as
//! `serial::port_name` names it (`COM3`, `/dev/ttyUSB0`, `/dev/cu.usbmodem...`). A port at VISA's
//! defaults (9600 8N1, no flow control) has no line. The XON/XOFF characters and DTR/RTS are not kept.

use crate::serial::{Config, FLOW_DTR_DSR, FLOW_RTS_CTS, FLOW_XON_XOFF};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub fn path() -> Option<PathBuf> {
    crate::paths::config_file("serial.conf")
}

const PARITY: [&str; 5] = ["none", "odd", "even", "mark", "space"];

pub fn parity_name(p: u8) -> &'static str {
    PARITY.get(p as usize).copied().unwrap_or("none")
}

pub fn stop_name(s: u8) -> &'static str {
    match s {
        15 => "1.5",
        20 => "2",
        _ => "1",
    }
}

pub fn flow_name(f: u8) -> String {
    let parts: Vec<&str> = [(FLOW_XON_XOFF, "xon"), (FLOW_RTS_CTS, "rts"), (FLOW_DTR_DSR, "dtr")]
        .iter()
        .filter(|(bit, _)| f & bit != 0)
        .map(|(_, n)| *n)
        .collect();
    if parts.is_empty() { "none".into() } else { parts.join("+") }
}

fn parse_flow(v: &str) -> Option<u8> {
    if v.eq_ignore_ascii_case("none") {
        return Some(0);
    }
    v.split('+').try_fold(0u8, |acc, p| {
        Some(acc | match p.to_ascii_lowercase().as_str() {
            "xon" => FLOW_XON_XOFF,
            "rts" => FLOW_RTS_CTS,
            "dtr" => FLOW_DTR_DSR,
            _ => return None,
        })
    })
}

/// One `name=value` onto `c`; false if it is not one this file knows or its value is not one the
/// setting can have (the field is then left as it was).
pub fn apply(c: &mut Config, name: &str, v: &str) -> bool {
    use crate::serial::field;
    let (f, n) = match name.to_ascii_lowercase().as_str() {
        "baud" => (field::BAUD, v.parse().ok()),
        "data" => (field::DATA_BITS, v.parse().ok()),
        "parity" => (field::PARITY, PARITY.iter().position(|p| p.eq_ignore_ascii_case(v)).map(|i| i as u32)),
        "stop" => (field::STOP_BITS, match v {
            "1" => Some(10),
            "1.5" => Some(15),
            "2" => Some(20),
            _ => None,
        }),
        "flow" => (field::FLOW, parse_flow(v).map(u32::from)),
        _ => return false,
    };
    match n.and_then(|n| c.with(f, n).ok()) {
        Some(nc) => {
            *c = nc;
            true
        }
        None => false,
    }
}

/// The settings this file keeps, the rest as VISA's defaults.
fn kept(c: &Config) -> Config {
    let d = Config::default();
    Config { baud: c.baud, data_bits: c.data_bits, parity: c.parity, stop_bits: c.stop_bits, flow: c.flow, ..d }
}

pub fn line(c: &Config) -> String {
    format!("baud={} data={} parity={} stop={} flow={}", c.baud, c.data_bits, parity_name(c.parity), stop_name(c.stop_bits), flow_name(c.flow))
}

/// Lines of `port name=value ...`; blank lines and `#` comments are skipped, and a field that does
/// not parse costs only itself. A port left at the defaults is not listed.
pub fn parse(text: &str) -> BTreeMap<String, Config> {
    let mut out = BTreeMap::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut w = l.split_whitespace();
        let Some(port) = w.next() else { continue };
        let mut c = Config::default();
        for kv in w {
            if let Some((k, v)) = kv.split_once('=') {
                apply(&mut c, k, v);
            }
        }
        if c != Config::default() {
            out.insert(port.to_string(), c);
        }
    }
    out
}

pub fn format(ports: &BTreeMap<String, Config>) -> String {
    let mut s = String::from(
        "# A serial port's default settings, set in Symbiotic Explorer's console or with symbiotic-ctl serial set;\n\
         # the daemon gives them to the port whenever it opens it. A port not listed opens at 9600 8N1.\n",
    );
    for (port, c) in ports {
        s.push_str(&format!("{port} {}\n", line(c)));
    }
    s
}

/// Is `key` (as written in the file) the port called `name`? `COM` names in any case.
fn same_port(key: &str, name: &str) -> bool {
    key == name || (name.to_ascii_uppercase().starts_with("COM") && key.eq_ignore_ascii_case(name))
}

pub fn load_from(p: &Path) -> BTreeMap<String, Config> {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

pub fn load() -> BTreeMap<String, Config> {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

pub fn default_in(ports: &BTreeMap<String, Config>, name: &str) -> Config {
    ports.iter().find(|(k, _)| same_port(k, name)).map(|(_, c)| *c).unwrap_or_default()
}

/// What port `n` opens at: its default settings, or VISA's.
pub fn default_for(n: u16) -> Config {
    default_in(&load(), &crate::serial::port_name(n))
}

/// Make `c` the default of the port called `name` (VISA's defaults: forget it). Written whole
/// (conffile.rs), so a reader never sees half a file.
pub fn remember_in(p: &Path, name: &str, c: &Config) -> io::Result<()> {
    let c = kept(c);
    crate::conffile::update(p, |text| {
        let mut ports = parse(text);
        let before = ports.clone();
        ports.retain(|k, _| !same_port(k, name));
        if c != Config::default() {
            ports.insert(name.to_string(), c);
        }
        (ports != before || text.is_empty()).then(|| format(&ports))
    })
    .map(|_| ())
}

pub fn remember(name: &str, c: &Config) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::other("no configuration directory to keep serial.conf in"))?;
    remember_in(&p, name, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reads_back_and_a_bad_field_costs_only_itself() {
        let m = parse("# c\nCOM3 baud=115200 data=7 parity=even stop=2 flow=xon+rts\n/dev/ttyUSB0 baud=19200 parity=weird data=9\nCOM4 baud=9600\n");
        assert_eq!(m.len(), 2, "COM4 is at the defaults: {m:?}");
        let c = m["COM3"];
        assert_eq!((c.baud, c.data_bits, c.parity, c.stop_bits, c.flow), (115200, 7, 2, 20, FLOW_XON_XOFF | FLOW_RTS_CTS));
        let u = m["/dev/ttyUSB0"];
        assert_eq!((u.baud, u.data_bits, u.parity), (19200, 8, 0), "the bad parity and data bits are left at the defaults");
        assert_eq!(parse(&format(&m)), m);
    }

    #[test]
    fn com_names_match_in_any_case_and_a_path_exactly() {
        let m = parse("com3 baud=57600\n/dev/ttyUSB0 baud=38400\n");
        assert_eq!(default_in(&m, "COM3").baud, 57600);
        assert_eq!(default_in(&m, "/dev/ttyUSB0").baud, 38400);
        assert_eq!(default_in(&m, "/dev/ttyusb0").baud, 9600);
        assert_eq!(default_in(&m, "COM5"), Config::default());
    }

    #[test]
    fn remembering_replaces_and_the_defaults_forget() {
        let dir = std::env::temp_dir().join(format!("symbiotic-serialconf-{}", std::process::id()));
        let p = dir.join("serial.conf");
        let _ = std::fs::remove_dir_all(&dir);
        let fast = Config { baud: 115200, dtr: false, xon: 1, ..Config::default() };
        remember_in(&p, "COM3", &fast).unwrap();
        remember_in(&p, "/dev/ttyACM0", &Config { parity: 1, ..Config::default() }).unwrap();
        let m = load_from(&p);
        assert_eq!(m.len(), 2);
        assert_eq!(m["COM3"], Config { baud: 115200, ..Config::default() }, "DTR and XON are not kept");
        remember_in(&p, "com3", &Config::default()).unwrap();
        let m = load_from(&p);
        assert_eq!(m.keys().collect::<Vec<_>>(), ["/dev/ttyACM0"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
