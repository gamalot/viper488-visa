//! An alias for one particular instrument. The alias stays `ALIAS = RESOURCE` in aliases.conf; `identities.conf` says
//! which unit it is, by `*IDN?`'s manufacturer, model and serial number (and, for a USBTMC instrument, its descriptor's
//! VID:PID:serial, which needs no command). An alias with an identity is checked when it is opened and, if its resource
//! is no longer that unit, looked for where the unit is now. Never "any unit of this model": a different unit is a
//! different instrument and takes a new binding.
//!
//! `*IDN?` is sent only on the user's word: when the alias is bound, and when a bound alias has to be checked or looked
//! for -- and never to an instrument the user turned `*IDN?` off for.

use crate::resource;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a successful check of a resource is believed, per process.
const VERIFIED_FOR: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbIds {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub usb: Option<UsbIds>,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A serial number that tells units apart: not empty, and not one character over and over (`0`, `000000`).
pub fn usable_serial(s: &str) -> bool {
    let s = s.trim();
    let mut c = s.chars();
    c.next().is_some_and(|first| !s.chars().all(|x| x == first))
}

impl Identity {
    /// The first three fields of an `*IDN?` answer. An answer without a usable serial number cannot bind.
    pub fn from_idn(text: &str) -> Result<Identity, String> {
        let f: Vec<&str> = text.trim().splitn(4, ',').map(str::trim).collect();
        if f.len() < 3 || f[0].is_empty() || f[1].is_empty() {
            return Err(format!("{:?} is not an *IDN? answer (manufacturer,model,serial,firmware)", text.trim()));
        }
        if !usable_serial(f[2]) {
            return Err("this instrument does not report a serial number, so it cannot be told from another of its kind".to_string());
        }
        Ok(Identity { vendor: clean(f[0]), model: clean(f[1]), serial: clean(f[2]), usb: None })
    }

    /// The same unit: manufacturer, model and serial, without regard to case or spacing.
    pub fn same_unit(&self, other: &Identity) -> bool {
        norm(&self.vendor) == norm(&other.vendor) && norm(&self.model) == norm(&other.model) && norm(&self.serial) == norm(&other.serial)
    }

    pub fn matches_idn(&self, text: &str) -> bool {
        Identity::from_idn(text).is_ok_and(|o| self.same_unit(&o))
    }

    pub fn line(&self) -> String {
        let mut s = format!("{} | {} | {}", self.vendor, self.model, self.serial);
        if let Some(u) = &self.usb {
            s += &format!(" | usb:{:04X}:{:04X}:{}", u.vid, u.pid, u.serial);
        }
        s
    }

    /// A unit as `line` writes it. Text with a control character is not one: a line break inside a field would end up as a
    /// line of its own in whatever file the identity is written to (identities.conf, the registry's state).
    pub fn parse_line(text: &str) -> Option<Identity> {
        if text.chars().any(char::is_control) {
            return None;
        }
        let f: Vec<&str> = text.split('|').map(str::trim).collect();
        if f.len() < 3 || f[0].is_empty() || f[1].is_empty() || !usable_serial(f[2]) {
            return None;
        }
        let usb = f.get(3).and_then(|u| {
            let u = u.strip_prefix("usb:")?;
            let mut p = u.splitn(3, ':');
            Some(UsbIds { vid: u16::from_str_radix(p.next()?, 16).ok()?, pid: u16::from_str_radix(p.next()?, 16).ok()?, serial: p.next()?.to_string() })
        });
        Some(Identity { vendor: f[0].to_string(), model: f[1].to_string(), serial: f[2].to_string(), usb })
    }
}

/// A field as one line of identities.conf can hold it.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '|' || c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

// ---- the file ----------------------------------------------------------------------------------

pub fn path() -> Option<PathBuf> {
    std::env::var_os("SYMBIOTIC_IDENTITIES").map(PathBuf::from).or_else(|| crate::paths::config_file("identities.conf"))
}

pub fn parse(text: &str) -> Vec<(String, Identity)> {
    let mut out: Vec<(String, Identity)> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((a, rest)) = l.split_once('=') else { continue };
        let a = a.trim();
        let Some(id) = Identity::parse_line(rest) else { continue };
        if !out.iter().any(|(o, _)| o.eq_ignore_ascii_case(a)) {
            out.push((a.to_string(), id));
        }
    }
    out
}

pub fn load() -> Vec<(String, Identity)> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

/// The identity bound to `alias`, if any.
pub fn get(alias: &str) -> Option<Identity> {
    load().into_iter().find(|(a, _)| a.eq_ignore_ascii_case(alias.trim())).map(|(_, i)| i)
}

/// Bind `alias` to `id`, or with None unbind it. Other lines and comments stay.
pub fn set(alias: &str, id: Option<&Identity>) -> std::io::Result<()> {
    let Some(p) = path() else { return Ok(()) };
    crate::conffile::update(&p, |text| {
        if id.is_none() && !text.lines().any(|l| l.split_once('=').is_some_and(|(a, _)| a.trim().eq_ignore_ascii_case(alias.trim()))) {
            return None; // nothing to take out: do not make the file
        }
        let mut lines: Vec<String> = if text.is_empty() {
            vec!["# Which unit an alias is (symbiotic-ctl alias bind): ALIAS = MANUFACTURER | MODEL | SERIAL [| usb:VID:PID:SERIAL]".to_string()]
        } else {
            text.lines()
                .filter(|l| {
                    let t = l.trim();
                    t.starts_with('#') || t.split_once('=').is_none_or(|(a, _)| !a.trim().eq_ignore_ascii_case(alias.trim()))
                })
                .map(str::to_string)
                .collect()
        };
        if let Some(id) = id {
            lines.push(format!("{} = {}", alias.trim(), id.line()));
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// Bind `alias` to the unit that is at its resource now: ask it `*IDN?` once (the user's word for it: this is what binding is) and
/// remember the answer's manufacturer, model and serial number, and for a USBTMC instrument its descriptor's ids. Refused
/// for an instrument set not to be sent `*IDN?`, and for one that reports no serial number.
pub fn bind(alias: &str) -> Result<Identity, String> {
    let (_, res) = crate::alias::resolve(alias).ok_or_else(|| format!("{alias:?} is not an alias"))?;
    if !crate::instruments::get(&res).idn {
        return Err(format!("{res} is set not to be sent *IDN? (the configuration window), so it cannot be bound"));
    }
    let text = ask_idn(&res, Duration::from_millis(3000))?;
    let mut id = Identity::from_idn(&text)?;
    if let Some(u) = resource::parse_usb(&res) {
        id.usb = Some(UsbIds { vid: u.vid, pid: u.pid, serial: u.serial });
    }
    set(alias, Some(&id)).map_err(|e| e.to_string())?;
    Ok(id)
}

// ---- finding the unit ---------------------------------------------------------------------------

/// What a search needs from the world, so that it can be run against a pretend one.
pub trait World {
    /// The USBTMC instruments that are ours, as VID:PID:serial.
    fn usb(&self) -> Vec<UsbIds>;
    /// The LAN resources worth asking: the configured ones, and (when discovery is on) the found ones.
    fn lan(&self) -> Vec<String>;
    /// The GPIB instruments that answer on `board`; Err (why) when the board cannot be looked at now.
    fn gpib(&self, board: u16) -> Result<Vec<String>, String>;
    /// What `resource` is known to answer to `*IDN?` without asking it (the daemon's sharing scan).
    fn known(&self, _resource: &str) -> Option<String> {
        None
    }
    /// The GPIB boards there are now (their adapters attached).
    fn gpib_boards(&self) -> Vec<u16>;
    /// May this resource be sent `*IDN?` (the user's per-instrument switch).
    fn idn_allowed(&self, resource: &str) -> bool;
    /// `*IDN?` of one resource. `quiet`: not if another program is using it now (the search; the
    /// check of where the alias points is the opening application's own business).
    fn ask(&self, resource: &str, quiet: bool) -> Result<String, String>;
    /// A LAN resource's host by its MAC (lanunits.rs): Some(true) it has the MAC recorded for it, Some(false) another, None
    /// nothing to tell by (no record, no MAC to be had, not a LAN resource). No command to the instrument.
    fn mac_says(&self, _resource: &str) -> Option<bool> {
        None
    }
    /// Where the MAC recorded for a LAN resource's host has gone, with what the old host kept moved there (lanunits::reconcile):
    /// the resource at its new host. No command to any instrument.
    fn mac_moved(&self, _resource: &str) -> Option<String> {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Located {
    /// The resource it has is the unit.
    Here,
    /// The unit is at another resource now.
    Moved(String),
    /// It is nowhere we may look (and why).
    NotFound(String),
    /// More than one resource answers as that unit: none is chosen.
    Ambiguous(Vec<String>),
}

fn verified() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn usb_name(u: &UsbIds) -> String {
    resource::format_usb(&resource::UsbName { board: 0, vid: u.vid, pid: u.pid, serial: u.serial.clone(), iface: None })
}

enum Check {
    Yes,
    No,
    /// The user turned `*IDN?` off for it: it cannot be checked, so it is not trusted.
    NotAllowed,
    /// It did not answer.
    Silent,
}

/// Is `resource` the unit `id`?
fn is_unit(w: &dyn World, resource: &str, id: &Identity) -> Check {
    if let (Some(want), Some(u)) = (&id.usb, resource::parse_usb(resource)) {
        // A USBTMC resource name carries the serial number: no command needed, only that it is plugged in.
        let same = u.vid == want.vid && u.pid == want.pid && u.serial.eq_ignore_ascii_case(&want.serial);
        return if same && w.usb().iter().any(|x| x == want) { Check::Yes } else { Check::No };
    }
    if !w.idn_allowed(resource) {
        return Check::NotAllowed;
    }
    match w.ask(resource, false) {
        Ok(t) if id.matches_idn(&t) => Check::Yes,
        Ok(_) => Check::No,
        Err(_) => Check::Silent,
    }
}

/// For a LAN resource: which instrument it names (its host, and behind a LAN/GPIB gateway the `gpib0,N` it asks for) and
/// how much it is preferred as its name (HiSLIP, then VXI-11, then a socket: the order the window uses for its representative).
fn lan_unit(resource: &str) -> Option<(String, u8)> {
    use crate::lan::name::{parse, Proto};
    let t = parse(resource)?;
    let (device, rank) = match &t.proto {
        Proto::Hislip { .. } => (String::new(), 0),
        Proto::Vxi11 { device, .. } => (if device.to_ascii_lowercase().starts_with("gpib") { device.to_ascii_lowercase() } else { String::new() }, 1),
        Proto::Socket { .. } => (String::new(), 2),
    };
    Some((format!("{}|{}", t.host.to_ascii_lowercase(), device), rank))
}

/// One instrument is often several resource names (`inst0`, `hislip0`, a socket): keep the preferred one of each, or the one
/// that is the same kind as the alias's own resource.
fn one_per_instrument(found: Vec<String>, current: &str) -> Vec<String> {
    let kind = |r: &str| lan_unit(r).map(|(_, rank)| rank);
    let mine = kind(current);
    let mut out: Vec<(String, String, u8)> = Vec::new(); // (unit, resource, preference: lower is better)
    for r in found {
        let Some((unit, rank)) = lan_unit(&r) else {
            out.push((r.clone(), r, 0)); // not LAN: its own instrument
            continue;
        };
        let pref = if Some(rank) == mine { 0 } else { rank + 1 };
        match out.iter_mut().find(|(u, _, _)| *u == unit) {
            Some(e) if pref < e.2 => *e = (unit, r, pref),
            Some(_) => {}
            None => out.push((unit, r, pref)),
        }
    }
    out.into_iter().map(|(_, r, _)| r).collect()
}

/// How long a search for a unit may take in all: each instrument asked takes up to two seconds, and
/// this is inside an application's viOpen.
pub const LOCATE_WITHIN: Duration = Duration::from_secs(15);

/// Where is the unit `alias` is, given the resource the alias has? Asks nothing the user turned off, nothing another
/// program is using (ipc.rs, `F_IF_QUIET`: our `*IDN?` would make it drop the answer that program has not read), and
/// takes `LOCATE_WITHIN` at most.
pub fn locate(alias: &str, current: &str, id: &Identity, w: &dyn World) -> Located {
    locate_within(alias, current, id, w, LOCATE_WITHIN)
}

fn locate_within(alias: &str, current: &str, id: &Identity, w: &dyn World, budget: Duration) -> Located {
    let end = Instant::now() + budget;
    let cur = resource::canonical(current).unwrap_or_else(|| current.to_string());
    let key = format!("{}|{}", alias.to_ascii_lowercase(), cur);
    if verified().lock().unwrap().get(&key).is_some_and(|t| t.elapsed() < VERIFIED_FOR) {
        return Located::Here;
    }
    // A LAN host with the MAC on record is the unit: no command.
    if w.mac_says(&cur) == Some(true) {
        verified().lock().unwrap().insert(key, Instant::now());
        return Located::Here;
    }
    match is_unit(w, &cur, id) {
        Check::Yes => {
            verified().lock().unwrap().insert(key, Instant::now());
            return Located::Here;
        }
        Check::NotAllowed => {
            return Located::NotFound(format!("{cur} is not to be sent *IDN? (the setting for that instrument), so it cannot be checked"));
        }
        Check::No | Check::Silent => {}
    }
    // Its MAC turned up at another host: there, without asking anyone `*IDN?`.
    if let Some(r) = w.mac_moved(&cur) {
        verified().lock().unwrap().insert(format!("{}|{}", alias.to_ascii_lowercase(), r), Instant::now());
        return Located::Moved(r);
    }
    // Not the unit, or not answering: look.
    let mut found: Vec<String> = Vec::new();
    let mut add = |r: String| {
        if !found.iter().any(|f| f.eq_ignore_ascii_case(&r)) {
            found.push(r);
        }
    };
    // What could not be looked at, to say so if the unit is not found.
    let mut unseen: Vec<String> = Vec::new();
    let mut late = false;
    let mut is_it = |r: &str, unseen: &mut Vec<String>| -> bool {
        if let Some(t) = w.known(r) {
            return id.matches_idn(&t);
        }
        if Instant::now() >= end {
            late = true;
            return false;
        }
        match w.ask(r, true) {
            Ok(t) => id.matches_idn(&t),
            Err(e) if e.contains(IN_USE) => {
                unseen.push(format!("{r} (in use)"));
                false
            }
            Err(_) => false,
        }
    };
    if let Some(want) = &id.usb {
        for u in w.usb() {
            if &u == want {
                add(usb_name(&u));
            }
        }
    }
    for r in w.lan() {
        if r.eq_ignore_ascii_case(&cur) || !w.idn_allowed(&r) {
            continue;
        }
        if is_it(&r, &mut unseen) {
            add(r);
        }
    }
    if let Some(i) = resource::parse(&cur) {
        // The board it was on first, and the others only if it is not there: a board number is the
        // order the daemon found the adapters in when it started, so the same adapter can be
        // another number after one was plugged or unplugged.
        let mut gpib_found = false;
        let others = w.gpib_boards().into_iter().filter(|b| *b != i.board);
        for board in std::iter::once(i.board).chain(others) {
            if gpib_found {
                break;
            }
            let listeners = match w.gpib(board) {
                Ok(l) => l,
                Err(why) => {
                    unseen.push(format!("GPIB{board} ({why})"));
                    continue;
                }
            };
            for r in listeners {
                if r.eq_ignore_ascii_case(&cur) || !w.idn_allowed(&r) {
                    continue;
                }
                if is_it(&r, &mut unseen) {
                    add(r);
                    gpib_found = true;
                }
            }
        }
    }
    // A serial port is not searched: opening one raises DTR, and nothing says which port it would be.
    let mut found = one_per_instrument(found, &cur);
    if late {
        unseen.push(format!("the rest: the search stops after {} s", budget.as_secs()));
    }
    match found.len() {
        0 if unseen.is_empty() => {
            Located::NotFound(format!("{} {} ({}) is not at {cur} and was not found elsewhere", id.vendor, id.model, id.serial))
        }
        0 => Located::NotFound(format!(
            "{} {} ({}) is not at {cur} and was not found elsewhere; not looked at: {}",
            id.vendor,
            id.model,
            id.serial,
            unseen.join(", ")
        )),
        1 => {
            let r = found.remove(0);
            verified().lock().unwrap().insert(format!("{}|{}", alias.to_ascii_lowercase(), r), Instant::now());
            Located::Moved(r)
        }
        _ => Located::Ambiguous(found),
    }
}

// ---- the real world ----------------------------------------------------------------------------

/// What `ask_idn` says, among other things, when the instrument is in use: another program used it in
/// the last `ipc::QUIET_FOR`, and a `*IDN?` now could take the answer it is waiting for.
pub const IN_USE: &str = "another program is using it";

/// `*IDN?` of one resource, through the daemon (GPIB, USBTMC, serial) or a connection of our own (LAN). The
/// answer, without its line end.
pub fn ask_idn(resource: &str, tmo: Duration) -> Result<String, String> {
    ask_idn_as(resource, tmo, false)
}

/// `ask_idn`; `quiet`: refused (`IN_USE`) if another program is using the instrument now (ipc.rs, `F_IF_QUIET`).
pub fn ask_idn_as(resource: &str, tmo: Duration, quiet: bool) -> Result<String, String> {
    use crate::client::Client;
    use crate::locks::Target;
    let ms = tmo.as_millis().min(60_000) as u32;
    let text = |d: Vec<u8>| String::from_utf8_lossy(&d).trim().to_string();
    let e = |x: crate::adapter::Error| x.to_string();
    let w = |x: crate::adapter::Error| if matches!(x, crate::adapter::Error::Busy) { IN_USE.to_string() } else { x.to_string() };
    if let Some(t) = crate::lan::name::parse(resource) {
        let mut s = crate::lan::Session::open(&t, Duration::from_secs(3), None).map_err(|x| x.to_string())?;
        let socket = t.class() == "SOCKET";
        s.write(b"*IDN?\n", true, Some(tmo)).map_err(|(_, x)| x.to_string())?;
        let r = s.read(4096, socket.then_some(b'\n'), socket, Some(tmo)).map_err(|x| x.to_string())?;
        return Ok(text(r.data));
    }
    let mut c = Client::connect(None).map_err(e)?;
    let write = |c: &mut Client, t: Target, d: &[u8], eoi: bool, ms: u32, quiet: bool| {
        if quiet { c.write_if_quiet(t, d, eoi, ms) } else { c.write(t, d, eoi, ms) }
    };
    if let Some(i) = resource::parse(resource) {
        let t = Target::Gpib(i.board, i.pad, i.sad);
        write(&mut c, t, b"*IDN?\n", true, ms, quiet).map_err(|(_, x)| w(x))?;
        return c.read(t, 4096, None, ms).map(|r| text(r.data)).map_err(e);
    }
    if let Some(u) = resource::parse_usb(resource) {
        let key = crate::usbtmc::key(u.vid, u.pid, &u.serial);
        let n = c.usb_list().map_err(e)?.iter().position(|x| x.present && x.key() == key).ok_or("that USB instrument is not plugged in")? as u16;
        c.usb_probe(n).map_err(e)?;
        let t = Target::Usb(n);
        write(&mut c, t, b"*IDN?\n", true, ms, quiet).map_err(|(_, x)| w(x))?;
        return c.read(t, 4096, None, ms).map(|r| text(r.data)).map_err(e);
    }
    if let Some((n, _)) = resource::parse_asrl(resource) {
        c.asrl_open(n).map_err(e)?;
        let t = Target::Asrl(n);
        write(&mut c, t, b"*IDN?\n", false, ms, quiet).map_err(|(_, x)| w(x))?;
        return c.read(t, 4096, Some(b'\n'), ms).map(|r| text(r.data)).map_err(e);
    }
    Err(format!("{resource} is not an instrument that can be asked"))
}

/// The world as it is: USB descriptors, `lan.conf` and discovery, the GPIB bus, `instruments.conf`.
#[derive(Default)]
pub struct Real {
    /// What the daemon's scans said the GPIB listeners are, by resource name.
    known: Mutex<HashMap<String, String>>,
}

impl World for Real {
    fn usb(&self) -> Vec<UsbIds> {
        crate::usbtmc::list().unwrap_or_default().into_iter().map(|f| UsbIds { vid: f.vid, pid: f.pid, serial: f.serial }).collect()
    }

    fn lan(&self) -> Vec<String> {
        let mut v: Vec<String> = crate::lan::conf::load().iter().map(|t| t.name()).collect();
        if crate::lan::conf::discover_enabled() {
            for t in crate::lan::discover::discover(Duration::from_millis(1200)) {
                v.push(t.name());
            }
        }
        v.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        v
    }

    fn gpib_boards(&self) -> Vec<u16> {
        let Ok(mut c) = crate::client::Client::connect(None) else { return Vec::new() };
        c.list().map(|l| l.iter().enumerate().filter(|(_, b)| b.present).map(|(n, _)| n as u16).collect()).unwrap_or_default()
    }

    fn gpib(&self, board: u16) -> Result<Vec<String>, String> {
        let mut c = crate::client::Client::connect(None).map_err(|e| e.to_string())?;
        let boards = c.list().map_err(|e| e.to_string())?;
        if !boards.get(board as usize).is_some_and(|b| b.present) {
            return Ok(Vec::new());
        }
        let name = |pad: u8| resource::format(resource::Instr::primary(board, pad));
        // The daemon's scan of the whole board (a recent one will do), with what it knows each listener to be. It is
        // refused while another program uses the daemon, and the board is then not looked at.
        match c.bus_scan_units(board, 10) {
            Ok(s) => {
                let mut known = self.known.lock().unwrap();
                for (pad, idn) in s.units {
                    known.insert(name(pad), idn);
                }
                Ok(s.pads.into_iter().map(name).collect())
            }
            Err(crate::adapter::Error::NoDevice) => Ok(Vec::new()),
            Err(crate::adapter::Error::Busy) => Err("another program is using the daemon".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn known(&self, r: &str) -> Option<String> {
        self.known.lock().unwrap().get(r).cloned()
    }

    fn idn_allowed(&self, r: &str) -> bool {
        crate::instruments::get(r).idn
    }

    fn ask(&self, r: &str, quiet: bool) -> Result<String, String> {
        ask_idn_as(r, Duration::from_millis(2000), quiet)
    }

    fn mac_says(&self, r: &str) -> Option<bool> {
        let h = crate::lanunits::host_of(r)?;
        let rec = crate::lanunits::path().map(|p| crate::lanunits::macs_from(&p)).unwrap_or_default().into_iter().find(|(x, _)| *x == h)?.1;
        let a = crate::neighbor::ipv4_of(h.trim_start_matches('[').trim_end_matches(']'))?;
        let now = crate::neighbor::macs(&[a]).remove(&a)?;
        Some(now == rec)
    }

    fn mac_moved(&self, r: &str) -> Option<String> {
        let h = crate::lanunits::host_of(r)?;
        let rec = crate::lanunits::path().map(|p| crate::lanunits::macs_from(&p)).unwrap_or_default();
        rec.iter().find(|(x, _)| *x == h)?;
        // Only the hosts that answer now: an address nobody has costs seconds of ARP before the system gives up.
        let found = crate::lan::conf::discover_enabled().then(|| std::thread::spawn(|| crate::lan::discover::discover(Duration::from_millis(1200))));
        let mut answered: Vec<String> = crate::lan::conf::check_all(crate::lan::conf::load(), Duration::from_millis(1500))
            .into_iter()
            .filter(|r| r.result.is_ok())
            .map(|r| r.target.name())
            .collect();
        answered.extend(found.and_then(|h| h.join().ok()).unwrap_or_default().into_iter().map(|t| t.name()));
        let done = crate::lanunits::reconcile(&answered);
        let (_, to) = done.moved.iter().find(|(from, _)| *from == h)?;
        crate::lanunits::moved_name(r, to)
    }
}

#[cfg(test)]
pub fn forget_verified() {
    verified().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache of checked units is the process's, and tests run at once: one that clears it would
    /// clear it under another. Each test that uses it holds this, and starts from an empty cache.
    fn fresh_cache() -> std::sync::MutexGuard<'static, ()> {
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        forget_verified();
        g
    }
    use std::cell::RefCell;

    const SDG: &str = "Siglent Technologies,SDG1022X Plus,SDG1EXAMPLE001,V1P.1.1.1.43R8";
    const K2015: &str = "KEITHLEY INSTRUMENTS INC.,MODEL 2015,0863839,B11  /A02";

    /// A pretend bench: what each resource answers to *IDN? (None: nothing there), what is plugged in, and who has been asked.
    #[derive(Default)]
    struct Bench {
        idn: Vec<(&'static str, Option<&'static str>)>,
        usb: Vec<UsbIds>,
        lan: Vec<&'static str>,
        gpib: Vec<&'static str>,
        off: Vec<&'static str>,
        asked: RefCell<Vec<String>>,
        busy_boards: Vec<u16>,
        /// Resources another program is using: asking them is refused.
        in_use: Vec<&'static str>,
        known: Vec<(&'static str, &'static str)>,
        /// How long each ask takes.
        slow: Duration,
        /// What the MACs say: the resources whose host has its recorded MAC, and where a recorded MAC went.
        mac_same: Vec<&'static str>,
        mac_went: Vec<(&'static str, &'static str)>,
    }

    impl World for Bench {
        fn usb(&self) -> Vec<UsbIds> {
            self.usb.clone()
        }
        fn lan(&self) -> Vec<String> {
            self.lan.iter().map(|s| s.to_string()).collect()
        }
        fn gpib(&self, board: u16) -> Result<Vec<String>, String> {
            if self.busy_boards.contains(&board) {
                return Err("another program is using the daemon".into());
            }
            Ok(self.gpib.iter().filter(|s| resource::parse(s).is_some_and(|i| i.board == board)).map(|s| s.to_string()).collect())
        }
        fn known(&self, r: &str) -> Option<String> {
            self.known.iter().find(|(x, _)| x.eq_ignore_ascii_case(r)).map(|(_, t)| t.to_string())
        }
        fn gpib_boards(&self) -> Vec<u16> {
            let mut v: Vec<u16> = self.gpib.iter().filter_map(|s| resource::parse(s)).map(|i| i.board).collect();
            v.sort();
            v.dedup();
            v
        }
        fn idn_allowed(&self, r: &str) -> bool {
            !self.off.iter().any(|o| o.eq_ignore_ascii_case(r))
        }
        fn ask(&self, r: &str, quiet: bool) -> Result<String, String> {
            self.asked.borrow_mut().push(r.to_string());
            std::thread::sleep(self.slow);
            if quiet && self.in_use.iter().any(|x| x.eq_ignore_ascii_case(r)) {
                return Err(IN_USE.to_string());
            }
            self.idn.iter().find(|(x, _)| x.eq_ignore_ascii_case(r)).and_then(|(_, a)| *a).map(str::to_string).ok_or_else(|| "no answer".to_string())
        }
        fn mac_says(&self, r: &str) -> Option<bool> {
            self.mac_same.iter().any(|x| x.eq_ignore_ascii_case(r)).then_some(true)
        }
        fn mac_moved(&self, r: &str) -> Option<String> {
            self.mac_went.iter().find(|(x, _)| x.eq_ignore_ascii_case(r)).map(|(_, to)| to.to_string())
        }
    }

    #[test]
    fn a_lan_unit_is_told_and_found_by_its_mac_without_a_command() {
        let _g = fresh_cache();
        let ag = id("Agilent Technologies,33210A,MY20000002,1.04");
        // Its MAC where the alias points: here, nobody asked.
        let b = Bench { mac_same: vec!["TCPIP0::198.18.0.21::inst0::INSTR"], ..Default::default() };
        assert_eq!(locate("AG", "TCPIP0::198.18.0.21::inst0::INSTR", &ag, &b), Located::Here);
        assert!(b.asked.borrow().is_empty());
        // Gone from there, its MAC at .21: moved there after the one check of where it was, the search not asked.
        let b = Bench { mac_went: vec![("TCPIP0::198.18.0.210::inst0::INSTR", "TCPIP0::198.18.0.21::inst0::INSTR")], lan: vec!["TCPIP0::198.18.0.99::inst0::INSTR"], ..Default::default() };
        assert_eq!(locate("AG", "TCPIP0::198.18.0.210::inst0::INSTR", &ag, &b), Located::Moved("TCPIP0::198.18.0.21::inst0::INSTR".into()));
        assert_eq!(*b.asked.borrow(), ["TCPIP0::198.18.0.210::inst0::INSTR"], "no host the search would ask");
    }

    fn id(text: &str) -> Identity {
        Identity::from_idn(text).unwrap()
    }

    #[test]
    fn what_an_idn_answer_gives_and_what_it_must_not() {
        let i = id(K2015);
        assert_eq!((i.vendor.as_str(), i.model.as_str(), i.serial.as_str()), ("KEITHLEY INSTRUMENTS INC.", "MODEL 2015", "0863839"));
        assert!(i.same_unit(&id("keithley  instruments inc.,model 2015,0863839,A99")), "case, spacing and the firmware do not matter");
        assert!(!i.same_unit(&id("KEITHLEY INSTRUMENTS INC.,MODEL 2015,0863840,B11")), "another serial is another unit");
        assert!(!i.same_unit(&id("KEITHLEY INSTRUMENTS INC.,MODEL 2000,0863839,B11")), "another model is another instrument");
        for bad in ["", "garbage", "A,B", "A,B,,fw", "A,B,0,fw", "A,B,0000000,fw", "A,B,x,fw"] {
            assert!(Identity::from_idn(bad).is_err(), "{bad:?}");
        }
        assert!(Identity::from_idn("A,B,1A,fw").is_ok());
        assert!(!usable_serial("  ") && !usable_serial("0") && usable_serial("A0"));
    }

    #[test]
    fn the_file_round_trips() {
        let mut a = id(SDG);
        a.usb = Some(UsbIds { vid: 0xF4EC, pid: 0x1103, serial: "SDG1EXAMPLE001".into() });
        let text = format!("# c
SDG = {}
DMM = {}
bad = only | two
SDG = {}
", a.line(), id(K2015).line(), id(K2015).line());
        let list = parse(&text);
        assert_eq!(list.len(), 2, "a bad line and the second of a name are left out");
        assert_eq!(list[0], ("SDG".to_string(), a));
        assert_eq!(list[1].1, id(K2015));
        let weird = Identity { vendor: "A|B".into(), model: "M
".into(), serial: "S1".into(), usb: None };
        assert_eq!(Identity::parse_line(&clean_for_test(&weird)).unwrap().vendor, "A B");
        assert_eq!(Identity::parse_line(&weird.line()), None, "a line break inside a field is not a unit");
        assert_eq!(Identity::parse_line("A | M | S1 | usb:0001:0002:X\0"), None);
    }

    fn clean_for_test(i: &Identity) -> String {
        Identity { vendor: clean(&i.vendor), model: clean(&i.model), serial: clean(&i.serial), usb: None }.line()
    }

    #[test]
    fn what_is_in_use_or_cannot_be_scanned_is_not_asked_and_the_answer_says_so() {
        let _cache = fresh_cache();
        let b = Bench {
            gpib: vec!["GPIB0::15::INSTR", "GPIB0::16::INSTR", "GPIB1::3::INSTR"],
            idn: vec![("GPIB0::16::INSTR", Some(SDG))],
            in_use: vec!["GPIB0::15::INSTR"],
            busy_boards: vec![1],
            ..Default::default()
        };
        let Located::NotFound(why) = locate("DMM", "GPIB0::4::INSTR", &id(K2015), &b) else { panic!() };
        assert!(why.contains("GPIB0::15::INSTR (in use)") && why.contains("GPIB1 (another program"), "{why}");
        assert!(!b.asked.borrow().iter().any(|r| r.starts_with("GPIB1")), "a board that could not be scanned is not probed");
    }

    #[test]
    fn a_listener_the_daemon_knows_is_not_asked() {
        let _cache = fresh_cache();
        let b = Bench {
            gpib: vec!["GPIB0::15::INSTR", "GPIB0::16::INSTR"],
            known: vec![("GPIB0::16::INSTR", K2015), ("GPIB0::15::INSTR", SDG)],
            ..Default::default()
        };
        assert_eq!(locate("DMM", "GPIB0::4::INSTR", &id(K2015), &b), Located::Moved("GPIB0::16::INSTR".into()));
        assert_eq!(b.asked.borrow().as_slice(), ["GPIB0::4::INSTR"], "only where it was, which the daemon's knowledge does not vouch for");
    }

    #[test]
    fn the_search_stops_at_its_time_and_says_so() {
        let _cache = fresh_cache();
        let b = Bench {
            gpib: (1..=10).map(|p| &*Box::leak(format!("GPIB0::{p}::INSTR").into_boxed_str())).collect(),
            slow: Duration::from_millis(20),
            ..Default::default()
        };
        let Located::NotFound(why) = locate_within("DMM", "GPIB0::20::INSTR", &id(K2015), &b, Duration::from_millis(50)) else { panic!() };
        assert!(why.contains("the search stops"), "{why}");
        assert!(b.asked.borrow().len() < 6, "not every one was asked: {:?}", b.asked.borrow());
    }

    #[test]
    fn the_unit_is_where_it_was() {
        let _cache = fresh_cache();
        let b = Bench { idn: vec![("GPIB0::15::INSTR", Some(K2015))], ..Default::default() };
        assert_eq!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::Here);
        assert_eq!(b.asked.borrow().len(), 1);
        // Believed for a minute: not asked again.
        assert_eq!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::Here);
        assert_eq!(b.asked.borrow().len(), 1, "a check is cached");
    }

    #[test]
    fn another_unit_at_the_address_is_not_taken_for_it() {
        let _cache = fresh_cache();
        // The address now holds a different instrument; the unit itself is on the LAN.
        let b = Bench {
            idn: vec![("GPIB0::15::INSTR", Some(SDG)), ("TCPIP0::192.0.2.7::inst0::INSTR", Some(K2015))],
            lan: vec!["TCPIP0::192.0.2.7::inst0::INSTR"],
            ..Default::default()
        };
        assert_eq!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::Moved("TCPIP0::192.0.2.7::inst0::INSTR".to_string()));
    }

    #[test]
    fn a_lan_unit_with_a_new_address_is_found_and_nothing_else_is_asked() {
        let _cache = fresh_cache();
        let b = Bench {
            idn: vec![
                ("TCPIP0::192.0.2.5::inst0::INSTR", None),
                ("TCPIP0::192.0.2.8::inst0::INSTR", Some(K2015)),
                ("TCPIP0::192.0.2.9::inst0::INSTR", Some(SDG)),
                ("TCPIP0::192.0.2.10::inst0::INSTR", Some(K2015)),
            ],
            lan: vec!["TCPIP0::192.0.2.8::inst0::INSTR", "TCPIP0::192.0.2.9::inst0::INSTR", "TCPIP0::192.0.2.10::inst0::INSTR"],
            off: vec!["TCPIP0::192.0.2.10::inst0::INSTR"],
            ..Default::default()
        };
        // .10 also answers as the unit but the user turned *IDN? off for it: it is not asked, so there is one match.
        assert_eq!(locate("DMM", "TCPIP0::192.0.2.5::inst0::INSTR", &id(K2015), &b), Located::Moved("TCPIP0::192.0.2.8::inst0::INSTR".to_string()));
        assert!(!b.asked.borrow().iter().any(|a| a.ends_with(".10::inst0::INSTR")), "{:?}", b.asked.borrow());
    }

    #[test]
    fn two_that_answer_as_the_unit_are_not_chosen_between() {
        let _cache = fresh_cache();
        let b = Bench {
            idn: vec![("TCPIP0::192.0.2.8::inst0::INSTR", Some(K2015)), ("TCPIP0::192.0.2.10::inst0::INSTR", Some(K2015))],
            lan: vec!["TCPIP0::192.0.2.8::inst0::INSTR", "TCPIP0::192.0.2.10::inst0::INSTR"],
            ..Default::default()
        };
        match locate("DMM", "TCPIP0::192.0.2.5::inst0::INSTR", &id(K2015), &b) {
            Located::Ambiguous(v) => assert_eq!(v.len(), 2),
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn nowhere_is_not_found_and_the_old_resource_is_not_a_fallback() {
        let _cache = fresh_cache();
        let b = Bench { idn: vec![("GPIB0::15::INSTR", Some(SDG))], gpib: vec!["GPIB0::15::INSTR"], ..Default::default() };
        assert!(matches!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::NotFound(_)), "the address holds another unit");
        let silent = Bench::default();
        assert!(matches!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &silent), Located::NotFound(_)));
    }

    #[test]
    fn gpib_is_searched_on_its_own_board_first_and_serial_ports_not_at_all() {
        let _cache = fresh_cache();
        let b = Bench {
            idn: vec![("GPIB0::15::INSTR", None), ("GPIB0::9::INSTR", Some(K2015)), ("ASRL6::INSTR", Some(K2015))],
            gpib: vec!["GPIB0::9::INSTR"],
            ..Default::default()
        };
        assert_eq!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::Moved("GPIB0::9::INSTR".to_string()));
        forget_verified();
        // Not on its own board: the others are searched, as when the adapter it is on took another
        // number (the daemon numbers them in the order it found them when it started).
        let moved = Bench {
            idn: vec![("GPIB1::15::INSTR", None), ("GPIB0::15::INSTR", Some(K2015)), ("GPIB2::4::INSTR", Some(SDG))],
            gpib: vec!["GPIB0::15::INSTR", "GPIB2::4::INSTR"],
            ..Default::default()
        };
        assert_eq!(locate("DMM", "GPIB1::15::INSTR", &id(K2015), &moved), Located::Moved("GPIB0::15::INSTR".to_string()));
        forget_verified();
        // Found on its own board: no other board is asked anything.
        let home = Bench {
            idn: vec![("GPIB1::15::INSTR", None), ("GPIB1::16::INSTR", Some(K2015)), ("GPIB0::3::INSTR", Some(SDG))],
            gpib: vec!["GPIB1::16::INSTR", "GPIB0::3::INSTR"],
            ..Default::default()
        };
        assert_eq!(locate("DMM", "GPIB1::15::INSTR", &id(K2015), &home), Located::Moved("GPIB1::16::INSTR".to_string()));
        assert!(!home.asked.borrow().iter().any(|a| a.starts_with("GPIB0")), "{:?}", home.asked.borrow());
        forget_verified();
        // A serial alias is checked where it is and never searched for.
        let s = Bench { idn: vec![("ASRL6::INSTR", Some(SDG)), ("ASRL7::INSTR", Some(K2015))], ..Default::default() };
        assert!(matches!(locate("DMM", "ASRL6::INSTR", &id(K2015), &s), Located::NotFound(_)));
        assert!(!s.asked.borrow().iter().any(|a| a == "ASRL7::INSTR"), "no other port is opened");
    }

    #[test]
    fn a_resource_that_may_not_be_asked_cannot_vouch_for_the_alias() {
        let _cache = fresh_cache();
        let b = Bench { idn: vec![("GPIB0::15::INSTR", Some(K2015))], off: vec!["GPIB0::15::INSTR"], ..Default::default() };
        match locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b) {
            Located::NotFound(why) => assert!(why.contains("*IDN?"), "{why}"),
            x => panic!("{x:?}"),
        }
        assert!(b.asked.borrow().is_empty(), "nothing was sent");
    }

    #[test]
    fn a_usb_unit_is_known_by_its_descriptor_without_a_command() {
        let _cache = fresh_cache();
        let ids = UsbIds { vid: 0xF4EC, pid: 0x1103, serial: "SDG1EXAMPLE001".into() };
        let mut i = id(SDG);
        i.usb = Some(ids.clone());
        let here = Bench { usb: vec![ids.clone()], ..Default::default() };
        assert_eq!(locate("SDG", "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR", &i, &here), Located::Here);
        assert!(here.asked.borrow().is_empty(), "a USB instrument is not sent *IDN? to be recognised");
        // Unplugged: not found (and no LAN instrument was asked in the hope of it).
        forget_verified();
        let gone = Bench::default();
        assert!(matches!(locate("SDG", "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR", &i, &gone), Located::NotFound(_)));
    }

    #[test]
    fn one_instrument_under_several_names_is_one_match_but_two_instruments_are_two() {
        let _cache = fresh_cache();
        let b = Bench {
            idn: vec![
                ("TCPIP0::192.0.2.8::inst0::INSTR", Some(K2015)),
                ("TCPIP0::192.0.2.8::hislip0::INSTR", Some(K2015)),
                ("TCPIP0::192.0.2.8::5025::SOCKET", Some(K2015)),
            ],
            lan: vec!["TCPIP0::192.0.2.8::inst0::INSTR", "TCPIP0::192.0.2.8::5025::SOCKET", "TCPIP0::192.0.2.8::hislip0::INSTR"],
            ..Default::default()
        };
        // Moved from somewhere else: the preferred name, HiSLIP.
        assert_eq!(locate("DMM", "TCPIP0::192.0.2.5::5025::SOCKET", &id(K2015), &b), Located::Moved("TCPIP0::192.0.2.8::5025::SOCKET".to_string()), "the kind the alias had");
        forget_verified();
        assert_eq!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &b), Located::Moved("TCPIP0::192.0.2.8::hislip0::INSTR".to_string()));
        // Behind a gateway the device name tells the instruments apart.
        forget_verified();
        let g = Bench {
            idn: vec![("TCPIP0::192.0.2.9::gpib0,5::INSTR", Some(K2015)), ("TCPIP0::192.0.2.9::gpib0,6::INSTR", Some(K2015))],
            lan: vec!["TCPIP0::192.0.2.9::gpib0,5::INSTR", "TCPIP0::192.0.2.9::gpib0,6::INSTR"],
            ..Default::default()
        };
        assert!(matches!(locate("DMM", "GPIB0::15::INSTR", &id(K2015), &g), Located::Ambiguous(v) if v.len() == 2));
    }

    /// A raw-socket "instrument" that answers `*IDN?` with `answer`.
    fn instrument(answer: &'static str) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    use std::io::{BufRead, BufReader, Write};
                    let mut w = c.try_clone().unwrap();
                    for line in BufReader::new(c).lines().map_while(Result::ok) {
                        if line.trim() == "*IDN?" {
                            let _ = w.write_all(format!("{answer}\n").as_bytes());
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn over_a_real_connection_and_the_real_lan_list() {
        let _cache = fresh_cache();
        let live = instrument(K2015);
        let other = instrument(SDG);
        let name = |p: u16| format!("TCPIP0::127.0.0.1::{p}::SOCKET");
        assert_eq!(ask_idn(&name(live), Duration::from_secs(2)).unwrap(), K2015);
        // A port nobody listens on, and the unit on `live`, which lan.conf lists next to another instrument.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let conf = std::env::temp_dir().join(format!("sym-id-lan-{}.conf", std::process::id()));
        std::fs::write(&conf, format!("{}\n{}\n", name(other), name(live))).unwrap();
        std::env::set_var("SYMBIOTIC_LAN_CONF", &conf);
        let found = locate("DMM", &name(closed), &id(K2015), &Real::default());
        std::env::remove_var("SYMBIOTIC_LAN_CONF");
        std::fs::remove_file(&conf).ok();
        assert_eq!(found, Located::Moved(name(live)));
    }
}
