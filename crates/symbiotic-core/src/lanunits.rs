//! Which instrument each address with settings had, so that an instrument's settings are its own and not its address's.
//! `instruments.conf` keeps them by resource name, which for a LAN instrument is its address: without this the next
//! instrument DHCP gives the address to inherits them.
//!
//! `units.conf` in the configuration directory (`SYMBIOTIC_UNITS` names another file; `lan-units.conf`, an earlier
//! name, is taken over): `KEY = MANUFACTURER | MODEL | SERIAL` lines, the unit an `*IDN?` answer named, and `mac HOST =
//! aa:bb:cc:dd:ee:ff` lines, the MAC a LAN host had. A LAN key is one instrument as the configuration window takes it
//! (`mergeLan` in app.js): `inst<k>`, `hislip<k>` on a host are instrument k, a socket is instrument 0, any other
//! device name (a gateway's `gpib0,6`) one of its own; a GPIB or serial resource is its own key.
//!
//! A LAN host's settings count only while its MAC is the one recorded (`reconcile`, asked at every `viFindRsrc` and
//! window refresh; the MAC from the neighbor table, `neighbor.rs`, without a command to the instrument), and go with
//! that MAC to the host it turns up at. GPIB and serial settings always count (their addresses are the user's); the
//! window says when an `*IDN?` answer there is another unit's. Nothing here sends a command to an instrument.

use crate::instruments::Settings;
use std::path::{Path, PathBuf};

/// A unit as `*IDN?` names it. The serial may be one that tells nothing (`0`, empty): then only the manufacturer
/// and model are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub vendor: String,
    pub model: String,
    pub serial: String,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A field as one line of the file can hold it.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '|' || c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

impl Unit {
    /// The first three fields of an `*IDN?` answer; a manufacturer and a model are needed, a serial is not.
    pub fn from_idn(text: &str) -> Option<Unit> {
        let f: Vec<&str> = text.trim().splitn(4, ',').map(str::trim).collect();
        let (vendor, model) = (clean(f.first()?), clean(f.get(1)?));
        if vendor.is_empty() || model.is_empty() {
            return None;
        }
        Some(Unit { vendor, model, serial: f.get(2).map(|s| clean(s)).unwrap_or_default() })
    }

    /// The same unit: manufacturer and model, and the serial too when both have one that tells units apart.
    pub fn same(&self, other: &Unit) -> bool {
        let serials = crate::identity::usable_serial(&self.serial) && crate::identity::usable_serial(&other.serial);
        norm(&self.vendor) == norm(&other.vendor) && norm(&self.model) == norm(&other.model) && (!serials || norm(&self.serial) == norm(&other.serial))
    }

    fn line(&self) -> String {
        format!("{} | {} | {}", self.vendor, self.model, self.serial)
    }

    fn parse_line(text: &str) -> Option<Unit> {
        let f: Vec<&str> = text.split('|').map(str::trim).collect();
        (f.len() == 3 && !f[0].is_empty() && !f[1].is_empty()).then(|| Unit { vendor: f[0].to_string(), model: f[1].to_string(), serial: f[2].to_string() })
    }
}

/// The instrument a resource name is one protocol of, as a key of the file: a LAN one's `host#index`, a GPIB or serial one its own
/// name; None for a USB one (its name is already the unit) and anything else.
pub fn key_of(resource: &str) -> Option<String> {
    use crate::lan::name::Proto;
    let Some(t) = crate::lan::name::parse(resource) else {
        let c = crate::resource::canonical(resource)?;
        let ok = crate::resource::parse(&c).is_some() || crate::resource::parse_asrl(&c).is_some();
        return ok.then(|| c.to_ascii_lowercase());
    };
    let host = t.host.to_ascii_lowercase();
    let index = |dev: &str, prefix: &str| crate::resource::strip_prefix_ci(dev, prefix).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).map(str::to_string);
    let dev = t.device.split(',').next().unwrap_or("").to_string();
    let which = match &t.proto {
        Proto::Socket { .. } => Some("0".to_string()),
        Proto::Hislip { .. } => index(&dev, "hislip"),
        Proto::Vxi11 { .. } if crate::lan::name::gpib_of(&t.device).is_none() => index(&dev, "inst"),
        Proto::Vxi11 { .. } => None,
    };
    Some(format!("{host}#{}", which.unwrap_or_else(|| t.device.to_ascii_lowercase())))
}

/// `resource` with its host replaced by `host`: protocol, device and port as they were. None if `host` is not one a name can hold.
pub fn moved_name(resource: &str, host: &str) -> Option<String> {
    let mut t = crate::lan::name::parse(resource)?;
    t.host = host.trim().to_string();
    let n = t.name();
    crate::lan::name::parse(&n).filter(|x| x.host == t.host && x.proto == t.proto).map(|_| n)
}

/// The host of a LAN resource name, as the file keys it (lower case, brackets of an IPv6 address kept); None for any other.
pub fn host_of(resource: &str) -> Option<String> {
    crate::lan::name::parse(resource).map(|t| t.host.to_ascii_lowercase())
}

pub fn path() -> Option<PathBuf> {
    for v in ["SYMBIOTIC_UNITS", "SYMBIOTIC_LAN_UNITS"] {
        if let Some(p) = std::env::var_os(v) {
            return Some(PathBuf::from(p));
        }
    }
    crate::paths::config_file("units.conf")
}

pub fn parse(text: &str) -> Vec<(String, Unit)> {
    let mut out: Vec<(String, Unit)> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("mac ")) {
        let Some((k, v)) = l.split_once('=') else { continue };
        let k = k.trim().to_ascii_lowercase();
        if let (false, Some(u)) = (out.iter().any(|(x, _)| *x == k), Unit::parse_line(v)) {
            out.push((k, u));
        }
    }
    out
}

pub fn load_from(p: &Path) -> Vec<(String, Unit)> {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

/// Each LAN host's recorded MAC, `mac HOST = aa:bb:...` lines.
pub fn parse_macs(text: &str) -> Vec<(String, crate::neighbor::Mac)> {
    let mut out: Vec<(String, crate::neighbor::Mac)> = Vec::new();
    for l in text.lines().map(str::trim) {
        let Some(rest) = l.strip_prefix("mac ") else { continue };
        let Some((h, m)) = rest.split_once('=') else { continue };
        let h = h.trim().to_ascii_lowercase();
        if let (false, Some(m)) = (out.iter().any(|(x, _)| *x == h), crate::neighbor::parse(m)) {
            out.push((h, m));
        }
    }
    out
}

pub fn macs_from(p: &Path) -> Vec<(String, crate::neighbor::Mac)> {
    std::fs::read_to_string(p).map(|t| parse_macs(&t)).unwrap_or_default()
}

const HEADER: &str = "# Which instrument each address with settings had (VISA and the configuration window): KEY = MANUFACTURER | MODEL | SERIAL, mac HOST = MAC\n";

/// Replace the line whose key (`KEY` or `mac HOST`) is `key` with `line`, or with None take it out.
fn write_line(p: &Path, key: &str, line: Option<String>) -> std::io::Result<()> {
    let key = key.to_ascii_lowercase();
    crate::conffile::update(p, |text| {
        let text = if text.is_empty() { HEADER } else { text };
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('#') || t.split_once('=').is_none_or(|(k, _)| k.trim().to_ascii_lowercase() != key)
            })
            .map(str::to_string)
            .collect();
        if let Some(l) = line {
            lines.push(l);
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// Record `unit` for `key` (None: forget it).
pub fn record_in(p: &Path, key: &str, unit: Option<&Unit>) -> std::io::Result<()> {
    let k = key.to_ascii_lowercase();
    write_line(p, &k, unit.map(|u| format!("{k} = {}", u.line())))
}

/// Record `mac` for `host` (None: forget it).
pub fn record_mac_in(p: &Path, host: &str, mac: Option<&crate::neighbor::Mac>) -> std::io::Result<()> {
    let h = host.to_ascii_lowercase();
    write_line(p, &format!("mac {h}"), mac.map(|m| format!("mac {h} = {}", crate::neighbor::format(m))))
}

/// What an answer at an address came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// Nothing to say: the unit recorded, it was recorded now, or there is nothing it could inherit.
    Fine,
    /// Another unit than the one recorded: `was`. Nothing has been changed.
    Changed { was: Unit },
}

/// `now` answered at `key`, which has something to inherit when `matters`: recorded if nothing was, compared if something was.
pub fn seen_in(p: &Path, key: &str, now: &Unit, matters: bool) -> std::io::Result<Seen> {
    let key = key.to_ascii_lowercase();
    match load_from(p).into_iter().find(|(k, _)| *k == key) {
        Some((_, was)) if was.same(now) => Ok(Seen::Fine),
        Some((_, was)) => Ok(Seen::Changed { was }),
        None if matters => record_in(p, &key, Some(now)).map(|_| Seen::Fine),
        None => Ok(Seen::Fine),
    }
}

/// An alias that points at one of an instrument's resource names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub alias: String,
    pub resource: String,
    /// The group's (a computer in a group keeps a network instrument's aliases at the registry).
    pub group: bool,
    /// Bound to one unit: the DLL looks for that unit when it is opened, so it is never moved here.
    pub bound: bool,
    /// The unit it is bound to, when it is.
    pub unit: Option<Unit>,
}

/// What an address keeps of the instrument that had it: the settings of its resource names, the aliases that point at them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    pub settings: Vec<(String, Settings)>,
    pub aliases: Vec<Alias>,
}

impl Kept {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.aliases.is_empty()
    }
}

/// What `key` keeps, out of every resource's settings and every alias.
pub fn kept(key: &str, settings: &[(String, Settings)], aliases: &[Alias]) -> Kept {
    let mine = |r: &str| key_of(r).is_some_and(|k| k.eq_ignore_ascii_case(key));
    Kept {
        settings: settings.iter().filter(|(r, s)| mine(r) && *s != Settings::default()).cloned().collect(),
        aliases: aliases.iter().filter(|a| mine(&a.resource)).cloned().collect(),
    }
}

impl Kept {
    /// What is left of another unit's, with `now` answering: an alias bound to `now` is the newcomer's own, not something left.
    pub fn left_by_another(mut self, now: &Unit) -> Kept {
        self.aliases.retain(|a| !a.unit.as_ref().is_some_and(|u| u.same(now)));
        self
    }
}

fn unit_of_identity(i: crate::identity::Identity) -> Unit {
    Unit { vendor: i.vendor, model: i.model, serial: i.serial }
}

// ---- the real files ------------------------------------------------------------------------------

/// Every alias this computer has, with whether it is bound: `aliases.conf`'s, and on a computer in a group the group's network ones.
pub fn all_aliases() -> Vec<Alias> {
    let bound = crate::identity::load();
    let mut out: Vec<Alias> = crate::alias::load()
        .into_iter()
        .map(|(alias, resource)| {
            let unit = bound.iter().find(|(a, _)| a.eq_ignore_ascii_case(&alias)).map(|(_, i)| unit_of_identity(i.clone()));
            Alias { alias, resource, group: false, bound: unit.is_some(), unit }
        })
        .collect();
    for g in crate::share::names::group_lan() {
        out.push(Alias { alias: g.alias, resource: g.resource, group: true, bound: g.identity.is_some(), unit: g.identity.map(unit_of_identity) });
    }
    out
}

/// What `key` keeps now.
pub fn kept_now(key: &str) -> Kept {
    kept(key, &crate::instruments::load(), &all_aliases())
}

fn file() -> std::io::Result<PathBuf> {
    let p = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no place for units.conf (the configuration directory is not known)"))?;
    // An earlier name of the file: taken over once.
    if !p.exists() {
        if let Some(old) = p.parent().map(|d| d.join("lan-units.conf")).filter(|o| o.exists()) {
            let _ = std::fs::rename(&old, &p);
        }
    }
    Ok(p)
}

/// `idn` answered at `resource`: what that came to (a resource that is not a LAN instrument, or an answer that names no unit, is `Fine`).
pub fn seen(resource: &str, idn: &str) -> std::io::Result<Seen> {
    let (Some(key), Some(now)) = (key_of(resource), Unit::from_idn(idn)) else { return Ok(Seen::Fine) };
    let matters = !kept_now(&key).is_empty();
    seen_in(&file()?, &key, &now, matters)
}

fn bad(m: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, m)
}

fn args(resource: &str, idn: &str) -> std::io::Result<(String, Option<Unit>)> {
    let key = key_of(resource).ok_or_else(|| bad(format!("{resource:?} is not a resource name that has a unit to record")))?;
    let now = match idn.trim() {
        "" => None,
        t => Some(Unit::from_idn(t).ok_or_else(|| bad(format!("{t:?} is not an *IDN? answer")))?),
    };
    Ok((key, now))
}

/// The instrument at `resource` now is the one its address's settings are for: its unit (`now`, if an answer named it) and, for a
/// LAN host, the MAC it has now.
fn record_now(p: &Path, resource: &str, key: &str, now: Option<&Unit>) -> std::io::Result<()> {
    if let Some(u) = now {
        record_in(p, key, Some(u))?;
    }
    if let Some(h) = host_of(resource) {
        if let Some(m) = crate::neighbor::ipv4_of(h.trim_start_matches('[').trim_end_matches(']')).and_then(|a| crate::neighbor::macs(&[a]).remove(&a)) {
            record_mac_in(p, &h, Some(&m))?;
        }
    }
    Ok(())
}

/// What the address keeps stays: the instrument there now (`idn`'s unit, if there is an answer, and its MAC) is the one it is for.
pub fn keep(resource: &str, idn: &str) -> std::io::Result<()> {
    let (key, now) = args(resource, idn)?;
    record_now(&file()?, resource, &key, now.as_ref())
}

/// What a settlement writes: each resource's settings (the defaults take its line out), and each alias's new resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub settings: Vec<(String, Settings)>,
    pub aliases: Vec<(String, String)>,
}

/// Clearing what `kept` holds: its settings back to the defaults; the aliases as they are.
pub fn plan_clear(kept: &Kept) -> Plan {
    Plan { settings: kept.settings.iter().map(|(r, _)| (r.clone(), Settings::default())).collect(), aliases: Vec::new() }
}

/// Moving what `kept` holds to `host`: each setting to the same resource name there (replacing what that has) and its own back to the
/// defaults, each alias that is not bound to the same resource name there. None when a name cannot take that host.
pub fn plan_move(kept: &Kept, host: &str) -> Option<Plan> {
    let mut plan = Plan::default();
    for (r, s) in &kept.settings {
        plan.settings.push((moved_name(r, host)?, s.clone()));
    }
    for (r, _) in &kept.settings {
        // Last, so that a name that is its own target (it cannot be: another host) would keep what was moved.
        plan.settings.push((r.clone(), Settings::default()));
    }
    for a in kept.aliases.iter().filter(|a| !a.bound) {
        plan.aliases.push((a.alias.clone(), moved_name(&a.resource, host)?));
    }
    Some(plan)
}

fn apply(plan: &Plan) -> std::io::Result<()> {
    for (r, s) in &plan.settings {
        crate::instruments::set(r, s)?;
    }
    for (alias, to) in &plan.aliases {
        // Taken out first: a name that is already an alias is refused, not moved (share::names).
        crate::share::names::set(alias, None).map_err(bad)?;
        crate::share::names::set(alias, Some(to)).map_err(bad)?;
    }
    Ok(())
}

/// The settings the address keeps go (back to the defaults), and `idn`'s unit is the one there. The aliases are left as they are.
pub fn clear(resource: &str, idn: &str) -> std::io::Result<()> {
    let (key, now) = args(resource, idn)?;
    apply(&plan_clear(&kept_now(&key)))?;
    record_now(&file()?, resource, &key, now.as_ref())
}

/// What the address keeps goes to `host`, where the unit it was recorded for is now: the settings (they replace the ones there)
/// and the aliases that are not bound (a bound one finds its unit itself). Then the address is `idn`'s unit's, with nothing kept,
/// and `host` is the old unit's.
pub fn move_to(resource: &str, idn: &str, host: &str) -> std::io::Result<()> {
    let (key, now) = args(resource, idn)?;
    let p = file()?;
    let was = load_from(&p).into_iter().find(|(k, _)| *k == key).map(|(_, u)| u).ok_or_else(|| bad(format!("nothing is recorded for {key}")))?;
    let new_key = moved_name(resource, host).and_then(|r| key_of(&r)).ok_or_else(|| bad(format!("{host:?} is not a host")))?;
    if new_key == key {
        return Err(bad(format!("{host} is this instrument's own address")));
    }
    let plan = plan_move(&kept_now(&key), host).ok_or_else(|| bad(format!("{host:?} is not a host for every name this address keeps")))?;
    apply(&plan)?;
    record_in(&p, &new_key, Some(&was))?;
    // The old unit's MAC goes with it to its new host.
    if let (Some(from), Some(to)) = (host_of(resource), moved_name(resource, host).and_then(|r| host_of(&r))) {
        if let Some((_, m)) = macs_from(&p).into_iter().find(|(h, _)| *h == from) {
            record_mac_in(&p, &to, Some(&m))?;
        }
    }
    record_now(&p, resource, &key, now.as_ref())
}

// ---- LAN settings follow the MAC ---------------------------------------------------------------------------------------

use crate::neighbor::Mac;
use std::collections::BTreeSet;

/// What the MACs of the hosts found came to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// Hosts found whose settings count: the recorded MAC, or one recorded now, or nothing kept to count.
    pub confirmed: BTreeSet<String>,
    /// Hosts found that keep something and had no MAC recorded: recorded now (the first one seen is believed).
    pub first: Vec<(String, Mac)>,
    /// Hosts found whose MAC is not the recorded one (None: none could be had): what they keep does not count.
    pub changed: Vec<(String, Mac, Option<Mac>)>,
    /// Hosts found that keep something, with no MAC recorded and none to be had: what they keep does not count.
    pub unknown: Vec<String>,
    /// What a host kept goes where its recorded MAC is found now: (from, to, mac).
    pub follow: Vec<(String, String, Mac)>,
}

/// `found`: each host found and its MAC now, if one could be had; `recorded`: the MACs on record; `kept`: the hosts that keep settings
/// or bound aliases. Pure: what to record and move is for `reconcile` to do.
pub fn decide(found: &[(String, Option<Mac>)], recorded: &[(String, Mac)], kept: &BTreeSet<String>) -> Decision {
    let mut d = Decision::default();
    let rec = |h: &str| recorded.iter().find(|(x, _)| x == h).map(|(_, m)| *m);
    for (h, now) in found {
        match (rec(h), now) {
            (Some(r), Some(n)) if r == *n => {
                d.confirmed.insert(h.clone());
            }
            (Some(r), n) => d.changed.push((h.clone(), r, *n)),
            (None, Some(n)) => {
                if kept.contains(h) {
                    d.first.push((h.clone(), *n));
                }
                d.confirmed.insert(h.clone());
            }
            (None, None) if kept.contains(h) => d.unknown.push(h.clone()),
            (None, None) => {
                d.confirmed.insert(h.clone());
            }
        }
    }
    // A host that keeps something and is not confirmed (gone, or another instrument there): its MAC found at another host takes it
    // along -- unless more than one host has that MAC, or the MAC is already that other host's own.
    for (h1, m) in recorded {
        if d.confirmed.contains(h1) || !kept.contains(h1) {
            continue;
        }
        let at: Vec<&String> = found.iter().filter(|(h2, n)| h2 != h1 && *n == Some(*m)).map(|(h2, _)| h2).collect();
        if at.len() != 1 || rec(at[0]) == Some(*m) && kept.contains(at[0]) {
            continue;
        }
        let h2 = at[0].clone();
        d.changed.retain(|(x, ..)| *x != h2);
        d.unknown.retain(|x| *x != h2);
        d.confirmed.insert(h2.clone());
        d.follow.push((h1.clone(), h2, *m));
    }
    d
}

/// What `reconcile` found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciled {
    /// Hosts found whose settings do not count now (another instrument there, or none could be told).
    pub unconfirmed: BTreeSet<String>,
    /// Hosts found with another MAC than recorded: (host, recorded, now).
    pub changed: Vec<(String, Mac, Option<Mac>)>,
    /// Hosts whose settings went with their MAC to another: (from, to).
    pub moved: Vec<(String, String)>,
    /// What could not be written (the result above holds anyway).
    pub errors: Vec<String>,
}

fn bare(h: &str) -> &str {
    h.trim_start_matches('[').trim_end_matches(']')
}

/// For the LAN resources `found` (the ones that answered just now): which hosts' settings count, by their MAC
/// (`decide`), with the first MACs recorded and settings moved where their MAC went. Asked at every `viFindRsrc` and
/// window refresh; only hosts that answered are looked up.
pub fn reconcile(found: &[String]) -> Reconciled {
    let mut out = Reconciled::default();
    let hosts: BTreeSet<String> = found.iter().filter_map(|r| host_of(r)).collect();
    if hosts.is_empty() {
        return out;
    }
    let Ok(p) = file() else {
        return out;
    };
    let settings = crate::instruments::load();
    let bound: Vec<(String, String)> = {
        let ids = crate::identity::load();
        crate::alias::load().into_iter().filter(|(a, _)| ids.iter().any(|(x, _)| x.eq_ignore_ascii_case(a))).collect()
    };
    let kept: BTreeSet<String> = settings
        .iter()
        .filter(|(_, s)| *s != Settings::default())
        .filter_map(|(r, _)| host_of(r))
        .chain(bound.iter().filter_map(|(_, r)| host_of(r)))
        .collect();
    let recorded = macs_from(&p);
    let looked_up = |hs: &[&String]| -> Vec<(String, Option<Mac>)> {
        let addrs: Vec<(String, Option<std::net::Ipv4Addr>)> = hs.iter().map(|h| ((*h).clone(), crate::neighbor::ipv4_of(bare(h)))).collect();
        let m = crate::neighbor::macs(&addrs.iter().filter_map(|(_, a)| *a).collect::<Vec<_>>());
        // No MAC to be had (across a router, an IPv6 host): the identification page's, if it has one -- all at once.
        let pages: Vec<_> = addrs
            .into_iter()
            .map(|(h, a)| match a.and_then(|a| m.get(&a).copied()) {
                Some(mac) => (h, Ok(Some(mac))),
                None => {
                    let host = h.clone();
                    (h, Err(std::thread::spawn(move || crate::lan::lxi::identification(&host, std::time::Duration::from_millis(1500)).and_then(|i| i.mac))))
                }
            })
            .collect();
        pages.into_iter().map(|(h, r)| (h, r.unwrap_or_else(|t| t.join().ok().flatten()))).collect()
    };
    // The hosts that matter first; the rest only when a recorded MAC may have gone to one of them.
    let matters = |h: &String| kept.contains(h) || recorded.iter().any(|(x, _)| x == h);
    let mut seen = looked_up(&hosts.iter().filter(|h| matters(h)).collect::<Vec<_>>());
    if recorded.iter().any(|(h, _)| kept.contains(h) && !seen.iter().any(|(x, m)| x == h && recorded.iter().any(|(y, r)| y == h && Some(*r) == *m))) {
        let rest: Vec<&String> = hosts.iter().filter(|h| !matters(h)).collect();
        seen.extend(looked_up(&rest));
    }
    let d = decide(&seen, &recorded, &kept);
    for (h, m) in &d.first {
        if let Err(e) = record_mac_in(&p, h, Some(m)) {
            out.errors.push(e.to_string());
        }
    }
    let mut confirmed = d.confirmed.clone();
    for (from, to, m) in &d.follow {
        match move_host(&p, &settings, &bound, from, to, m) {
            Ok(()) => out.moved.push((from.clone(), to.clone())),
            Err(e) => {
                confirmed.remove(to);
                out.errors.push(format!("{from} -> {to}: {e}"));
            }
        }
    }
    out.unconfirmed = hosts.iter().filter(|h| kept.contains(*h) && !confirmed.contains(*h)).cloned().collect();
    out.changed = d.changed;
    out
}

/// Everything `from` keeps -- its settings, the aliases bound to its unit, its units on record -- to the same names at `to`, which is
/// recorded with `mac`. The settings there are replaced by the ones moved.
fn move_host(p: &Path, settings: &[(String, Settings)], bound: &[(String, String)], from: &str, to: &str, mac: &Mac) -> std::io::Result<()> {
    let mine = |r: &str| host_of(r).as_deref() == Some(from);
    let k = Kept { settings: settings.iter().filter(|(r, s)| mine(r) && *s != Settings::default()).cloned().collect(), aliases: Vec::new() };
    let plan = plan_move(&k, to).ok_or_else(|| bad(format!("{to:?} is not a host for every name {from} keeps")))?;
    apply(&plan)?;
    for (alias, r) in bound.iter().filter(|(_, r)| mine(r)) {
        let n = moved_name(r, to).ok_or_else(|| bad(format!("{to:?} is not a host for {r}")))?;
        crate::share::names::follow(alias, &n).map_err(bad)?;
    }
    let prefix = format!("{from}#");
    for (key, u) in load_from(p).into_iter().filter(|(k, _)| k.starts_with(&prefix)) {
        record_in(p, &format!("{to}#{}", &key[prefix.len()..]), Some(&u))?;
    }
    record_mac_in(p, to, Some(mac))
}

/// `alias` went from `from` to `to` (its unit was found there): what the old place kept of it goes too. A LAN host takes everything
/// of that host (`move_host`, its MAC with it); any other resource its own line. The settings at `to` are replaced.
pub fn settings_follow(from: &str, to: &str) -> std::io::Result<()> {
    let p = file()?;
    if let (Some(fh), Some(th)) = (host_of(from), host_of(to)) {
        if fh == th {
            return Ok(());
        }
        let settings = crate::instruments::load();
        let mac = macs_from(&p).into_iter().find(|(h, _)| *h == fh).map(|(_, m)| m);
        let mine = |r: &str| host_of(r).as_deref() == Some(fh.as_str());
        let k = Kept { settings: settings.iter().filter(|(r, s)| mine(r) && *s != Settings::default()).cloned().collect(), aliases: Vec::new() };
        apply(&plan_move(&k, &th).ok_or_else(|| bad(format!("{th:?} is not a host for every name {fh} keeps")))?)?;
        let prefix = format!("{fh}#");
        for (key, u) in load_from(&p).into_iter().filter(|(k, _)| k.starts_with(&prefix)) {
            record_in(&p, &format!("{th}#{}", &key[prefix.len()..]), Some(&u))?;
        }
        return match mac {
            Some(m) => record_mac_in(&p, &th, Some(&m)),
            None => Ok(()),
        };
    }
    let (Some(f), Some(t)) = (crate::resource::canonical(from), crate::resource::canonical(to)) else { return Ok(()) };
    if f.eq_ignore_ascii_case(&t) {
        return Ok(());
    }
    let s = crate::instruments::get(&f);
    if s != Settings::default() {
        crate::instruments::set(&t, &s)?;
        crate::instruments::set(&f, &Settings::default())?;
    }
    if let (Some(fk), Some(tk)) = (key_of(&f), key_of(&t)) {
        if let Some((_, u)) = load_from(&p).into_iter().find(|(k, _)| *k == fk) {
            record_in(&p, &tk, Some(&u))?;
            record_in(&p, &fk, None)?;
        }
    }
    Ok(())
}

/// May `resource`'s settings be written now? Not while its LAN host has another instrument than they are for (another MAC answers
/// there): they are the last one's, shown as the defaults, and a save would write the defaults over them. The user settles the
/// address first (its notice). An instrument that is off, or one that cannot be told, may be written.
pub fn writable(resource: &str) -> Result<(), String> {
    let Some(h) = host_of(resource) else { return Ok(()) };
    let Ok(p) = file() else { return Ok(()) };
    let Some((_, rec)) = macs_from(&p).into_iter().find(|(x, _)| *x == h) else { return Ok(()) };
    let Some(a) = crate::neighbor::ipv4_of(bare(&h)) else { return Ok(()) };
    match crate::neighbor::macs(&[a]).remove(&a) {
        Some(now) if now != rec => Err(format!(
            "{h} has another instrument (MAC {}) than its settings are for ({}): settle that on its page first",
            crate::neighbor::format(&now),
            crate::neighbor::format(&rec)
        )),
        _ => Ok(()),
    }
}

/// Point `alias` at `to`, where its unit is now, keeping its binding (`alias::follow`), with what its old place kept (`settings_follow`).
/// The settings are best effort: the alias moved whatever became of them.
pub fn follow_alias(alias: &str, to: &str) -> std::io::Result<()> {
    let from = crate::alias::resolve(alias).map(|(_, r)| r);
    crate::alias::follow(alias, to)?;
    if let Some(f) = from {
        let _ = settings_follow(&f, to);
    }
    Ok(())
}

/// The instrument of `resources` is taken out (the window's Remove): their settings and their units on record go, and a LAN host's
/// MAC once nothing of that host is left.
pub fn forget(resources: &[String]) -> std::io::Result<()> {
    let p = file()?;
    let mut hosts = BTreeSet::new();
    for r in resources {
        crate::instruments::set(r, &Settings::default())?;
        if let Some(k) = key_of(r) {
            record_in(&p, &k, None)?;
        }
        if let Some(h) = host_of(r) {
            hosts.insert(h);
        }
    }
    let settings = crate::instruments::load();
    let aliases = crate::alias::load();
    for h in hosts {
        let left = settings.iter().any(|(r, s)| host_of(r).as_deref() == Some(h.as_str()) && *s != Settings::default())
            || aliases.iter().any(|(_, r)| host_of(r).as_deref() == Some(h.as_str()));
        if !left {
            record_mac_in(&p, &h, None)?;
        }
    }
    Ok(())
}

// ---- bound LAN aliases following their units ---------------------------------------------------------------------------

/// One instrument of the window's tree: its resource names, whether it answers, and its `*IDN?` answer if it has one.
#[derive(Debug, Clone)]
pub struct Place {
    pub resources: Vec<String>,
    pub online: bool,
    pub idn: Option<String>,
}

/// Where a bound alias should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Follow {
    /// Where it is, or nothing better known.
    Stay,
    /// Its unit answers there, and nowhere else.
    Move(String),
    /// Its unit answers at more than one address: none is chosen.
    Ambiguous(Vec<String>),
}

fn plain_lan(resource: &str) -> Option<crate::lan::name::Target> {
    crate::lan::name::parse(resource).filter(|t| crate::lan::name::gpib_of(&t.device).is_none())
}

/// Where `resource`, the resource of an alias bound to `id`, should be, by what the window's tree knows: moved when its address is not
/// that unit any more (gone from the tree, not answering, or answering as another) and exactly one other LAN address answers as it.
pub fn follow_target(resource: &str, id: &crate::identity::Identity, places: &[Place]) -> Follow {
    let (Some(_), Some(key)) = (plain_lan(resource), key_of(resource)) else { return Follow::Stay };
    let key_of_place = |p: &Place| p.resources.iter().filter_map(|r| key_of(r)).next();
    let here = places.iter().find(|p| p.resources.iter().any(|r| key_of(r).as_deref() == Some(key.as_str())));
    if let Some(p) = here.filter(|p| p.online) {
        match &p.idn {
            // No conclusion yet (being asked, or *IDN? is off for it): wait.
            None => return Follow::Stay,
            Some(t) if id.matches_idn(t) => return Follow::Stay,
            Some(_) => {}
        }
    }
    let mut to: Vec<(String, String)> = Vec::new();
    for p in places.iter().filter(|p| p.online && p.idn.as_deref().is_some_and(|t| id.matches_idn(t))) {
        let Some(k) = key_of_place(p).filter(|k| *k != key) else { continue };
        let Some(t) = p.resources.iter().find_map(|r| plain_lan(r)) else { continue };
        if to.iter().any(|(x, _)| *x == k) {
            continue;
        }
        if let Some(n) = moved_name(resource, &t.host) {
            to.push((k, n));
        }
    }
    match to.len() {
        0 => Follow::Stay,
        1 => Follow::Move(to.remove(0).1),
        _ => Follow::Ambiguous(to.into_iter().map(|(_, n)| n).collect()),
    }
}

/// What following came to for one alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followed {
    pub alias: String,
    /// Where it went (one), or where its unit answers (several: it stayed).
    pub to: Vec<String>,
    /// Moved; else ambiguous, or the write failed (`error`).
    pub moved: bool,
    pub error: Option<String>,
    /// The resource names at the new address that took the old address's settings, and of those the ones whose own settings they replaced.
    pub settings: Vec<String>,
    pub replaced: Vec<String>,
}

/// Every bound LAN alias, this computer's and the group's, followed to where `places` say its unit is.
pub fn follow_bound(places: &[Place]) -> Vec<Followed> {
    let mut bound: Vec<(String, String, crate::identity::Identity)> = Vec::new();
    let ids = crate::identity::load();
    for (alias, resource) in crate::alias::load() {
        if let Some((_, id)) = ids.iter().find(|(a, _)| a.eq_ignore_ascii_case(&alias)) {
            bound.push((alias, resource, id.clone()));
        }
    }
    for g in crate::share::names::group_lan() {
        if let Some(id) = g.identity {
            bound.push((g.alias, g.resource, id));
        }
    }
    let mut out = Vec::new();
    for (alias, resource, id) in bound {
        match follow_target(&resource, &id, places) {
            Follow::Stay => {}
            Follow::Ambiguous(to) => out.push(Followed { alias, to, moved: false, error: None, settings: Vec::new(), replaced: Vec::new() }),
            Follow::Move(to) => {
                let f = match crate::share::names::follow(&alias, &to) {
                    Err(e) => Followed { alias, to: vec![to], moved: false, error: Some(e), settings: Vec::new(), replaced: Vec::new() },
                    Ok(()) => match take_settings(&resource, &to, &id) {
                        Ok((settings, replaced)) => Followed { alias, to: vec![to], moved: true, error: None, settings, replaced },
                        Err(e) => Followed { alias, to: vec![to], moved: true, error: Some(format!("its settings did not move: {e}")), settings: Vec::new(), replaced: Vec::new() },
                    },
                };
                out.push(f);
            }
        }
    }
    out
}

/// What the address of `from` keeps in `instruments.conf` goes to the same names at the address of `to`, replacing what those have (the
/// aliases are not taken: an unbound one is not known to be this unit). `to`'s address is recorded as `id`'s unit; `from`'s record stays.
/// The names that took settings, and those whose own were replaced.
fn take_settings(from: &str, to: &str, id: &crate::identity::Identity) -> std::io::Result<(Vec<String>, Vec<String>)> {
    let key = key_of(from).ok_or_else(|| bad(format!("{from:?} is not a LAN resource name")))?;
    let host = crate::lan::name::parse(to).map(|t| t.host).ok_or_else(|| bad(format!("{to:?} is not a LAN resource name")))?;
    let all = crate::instruments::load();
    let k = Kept { settings: kept(&key, &all, &[]).settings, aliases: Vec::new() };
    let plan = plan_move(&k, &host).ok_or_else(|| bad(format!("{host:?} is not a host for every name {key} keeps")))?;
    let theirs = |r: &str| all.iter().any(|(x, s)| x == r && *s != Settings::default());
    let n = k.settings.len();
    let taken: Vec<String> = plan.settings[..n].iter().map(|(r, _)| r.clone()).collect();
    let replaced = taken.iter().filter(|r| theirs(r)).cloned().collect();
    apply(&plan)?;
    if let Some(new_key) = key_of(to) {
        record_in(&file()?, &new_key, Some(&Unit { vendor: id.vendor.clone(), model: id.model.clone(), serial: id.serial.clone() }))?;
    }
    Ok((taken, replaced))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::Expose;

    fn u(v: &str, m: &str, s: &str) -> Unit {
        Unit { vendor: v.into(), model: m.into(), serial: s.into() }
    }

    #[test]
    fn one_key_for_an_instruments_protocols_and_one_for_each_gateway_address() {
        let k = |r| key_of(r).unwrap();
        assert_eq!(k("TCPIP0::198.18.68.57::inst0::INSTR"), "198.18.68.57#0");
        assert_eq!(k("TCPIP::198.18.68.57::hislip0::INSTR"), "198.18.68.57#0");
        assert_eq!(k("TCPIP0::198.18.68.57::5025::SOCKET"), "198.18.68.57#0");
        assert_eq!(k("TCPIP0::198.18.68.57,1024::inst0::INSTR"), "198.18.68.57#0", "the port is not the host");
        assert_eq!(k("TCPIP0::Scope.lab::inst1::INSTR"), "scope.lab#1");
        assert_eq!(k("TCPIP0::198.18.68.65::gpib0,20::INSTR"), "198.18.68.65#gpib0,20");
        assert_eq!(k("TCPIP0::[2001:db8::5]::hislip0::INSTR"), "[2001:db8::5]#0");
        assert_eq!(key_of("GPIB0::6::INSTR").as_deref(), Some("gpib0::6::instr"), "a GPIB resource is a key of its own");
        assert_eq!(moved_name("TCPIP0::198.18.68.57::5025::SOCKET", "198.18.68.66").unwrap(), "TCPIP0::198.18.68.66::5025::SOCKET");
        assert_eq!(moved_name("TCPIP0::198.18.68.57,1024::inst0::INSTR", "10.0.0.2").unwrap(), "TCPIP0::10.0.0.2,1024::inst0::INSTR");
    }

    #[test]
    fn a_unit_is_the_same_by_its_serial_or_by_its_model_when_it_has_none() {
        let a = Unit::from_idn("Keysight Technologies,34465A,MY20000001,A.03.03").unwrap();
        assert!(a.same(&u("KEYSIGHT  technologies", "34465a", "my20000001")));
        assert!(!a.same(&u("Keysight Technologies", "34465A", "MY00000001")));
        assert!(!a.same(&Unit::from_idn("Siglent Technologies,SDG1062X Plus,SDG1EXAMPLE001,1").unwrap()));
        let old = Unit::from_idn("HEWLETT-PACKARD,34970A,0,11-1-2").unwrap();
        assert!(old.same(&u("Hewlett-Packard", "34970A", "0")) && old.same(&u("Hewlett-Packard", "34970A", "")), "no serial: by model");
        assert!(!old.same(&u("Hewlett-Packard", "34401A", "0")));
        assert_eq!(Unit::from_idn("garbage"), None);
        assert_eq!(Unit::from_idn("A|B,C\n,D").unwrap(), u("A B", "C", "D"), "a field the file cannot hold is cleaned");
    }

    #[test]
    fn an_answer_is_recorded_when_it_matters_and_compared_after() {
        let dir = std::env::temp_dir().join(format!("sym-lanunits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("lan-units.conf");
        let ks = u("Keysight Technologies", "34465A", "MY20000001");
        let sdg = u("Siglent Technologies", "SDG1062X Plus", "SDG1EXAMPLE001");
        assert_eq!(seen_in(&p, "198.18.68.57#0", &ks, false).unwrap(), Seen::Fine);
        assert!(load_from(&p).is_empty(), "nothing to inherit: not recorded");
        assert_eq!(seen_in(&p, "198.18.68.57#0", &ks, true).unwrap(), Seen::Fine);
        assert_eq!(seen_in(&p, "198.18.68.57#0", &ks, true).unwrap(), Seen::Fine);
        assert_eq!(seen_in(&p, "198.18.68.57#0", &sdg, true).unwrap(), Seen::Changed { was: ks.clone() });
        assert_eq!(seen_in(&p, "198.18.68.57#0", &sdg, false).unwrap(), Seen::Changed { was: ks.clone() }, "said until the user settles it");
        assert_eq!(load_from(&p), [("198.18.68.57#0".to_string(), ks.clone())], "and nothing changed meanwhile");
        record_in(&p, "198.18.68.57#0", Some(&sdg)).unwrap();
        record_in(&p, "198.18.68.66#0", Some(&ks)).unwrap();
        assert_eq!(seen_in(&p, "198.18.68.57#0", &sdg, true).unwrap(), Seen::Fine);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with('#') && text.contains("198.18.68.66#0 = Keysight Technologies | 34465A | MY20000001\n"), "{text}");
        assert_eq!(parse(&text).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_address_keeps_its_instruments_settings_and_aliases_only() {
        let none = Settings { expose: Some(Expose::None), ..Settings::default() };
        let settings = vec![
            ("TCPIP0::198.18.68.57::inst0::INSTR".to_string(), none.clone()),
            ("TCPIP0::198.18.68.57::5025::SOCKET".to_string(), none.clone()),
            ("TCPIP0::198.18.68.57::inst1::INSTR".to_string(), none.clone()),
            ("TCPIP0::198.18.68.66::inst0::INSTR".to_string(), none.clone()),
            ("GPIB0::6::INSTR".to_string(), none.clone()),
        ];
        let a = |alias: &str, resource: &str, bound| Alias { alias: alias.into(), resource: resource.into(), group: false, bound, unit: None };
        let aliases = vec![a("OLD", "TCPIP0::198.18.68.57::hislip0::INSTR", false), a("B", "TCPIP0::198.18.68.57::inst0::INSTR", true), a("X", "TCPIP0::198.18.68.66::inst0::INSTR", false)];
        let k = kept("198.18.68.57#0", &settings, &aliases);
        assert_eq!(k.settings.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(), ["TCPIP0::198.18.68.57::inst0::INSTR", "TCPIP0::198.18.68.57::5025::SOCKET"]);
        assert_eq!(k.aliases.iter().map(|a| a.alias.as_str()).collect::<Vec<_>>(), ["OLD", "B"]);
        assert!(kept("198.18.68.99#0", &settings, &aliases).is_empty());
        // An alias bound to the unit that answers now is that unit's own; one bound to another is left.
        let sdg = u("Siglent Technologies", "SDG1062X Plus", "SDG1EXAMPLE001");
        let mine = Alias { alias: "SDG".into(), resource: "TCPIP0::198.18.68.57::inst0::INSTR".into(), group: false, bound: true, unit: Some(sdg.clone()) };
        let theirs = Alias { alias: "KS".into(), resource: "TCPIP0::198.18.68.57::hislip0::INSTR".into(), group: false, bound: true, unit: Some(u("Keysight Technologies", "34465A", "MY20000001")) };
        let k = kept("198.18.68.57#0", &[], &[mine, theirs.clone()]).left_by_another(&sdg);
        assert_eq!(k.aliases, [theirs]);
    }

    #[test]
    fn a_hosts_settings_count_only_with_its_mac_and_go_where_the_mac_goes() {
        let (ks, sdg, ag) = ([0x02, 0x00, 0x00, 0x00, 0x00, 0x66], [0x02, 0x00, 0x00, 0x00, 0x00, 0x62], [0x02, 0x00, 0x00, 0x00, 0x00, 0x21]);
        let h = |s: &str| s.to_string();
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<String>>();
        // The recorded MAC: counts. No record and something kept: the first MAC seen is recorded and counts. Nothing kept: nothing to count.
        let d = decide(&[(h("198.18.68.66"), Some(ks)), (h("198.18.0.62"), Some(sdg)), (h("198.18.0.21"), Some(ag))], &[(h("198.18.68.66"), ks)], &set(&["198.18.68.66", "198.18.0.62"]));
        assert_eq!(d.confirmed, set(&["198.18.68.66", "198.18.0.62", "198.18.0.21"]));
        assert_eq!(d.first, [(h("198.18.0.62"), sdg)]);
        assert!(d.changed.is_empty() && d.follow.is_empty() && d.unknown.is_empty());
        // Another instrument at .57: its settings do not count; and none to be had, nor without a record.
        let d = decide(&[(h("198.18.68.57"), Some(sdg)), (h("198.18.68.65"), None), (h("10.0.0.9"), None)], &[(h("198.18.68.57"), ks), (h("198.18.68.65"), ag)], &set(&["198.18.68.57", "198.18.68.65", "10.0.0.9"]));
        assert!(d.confirmed.is_empty());
        assert_eq!(d.changed, [(h("198.18.68.57"), ks, Some(sdg)), (h("198.18.68.65"), ag, None)]);
        assert_eq!(d.unknown, [h("10.0.0.9")]);
        // The 34465A turns up at .66: .57's settings go with it, and .66 counts -- even if .66 had another MAC recorded.
        let d = decide(&[(h("198.18.68.57"), Some(sdg)), (h("198.18.68.66"), Some(ks))], &[(h("198.18.68.57"), ks), (h("198.18.68.66"), sdg)], &set(&["198.18.68.57", "198.18.68.66"]));
        assert_eq!(d.follow, [(h("198.18.68.57"), h("198.18.68.66"), ks)]);
        assert!(d.confirmed.contains("198.18.68.66") && !d.confirmed.contains("198.18.68.57"));
        assert_eq!(d.changed, [(h("198.18.68.57"), ks, Some(sdg))], ".66 is no longer said to have changed");
        // Gone from the tree, found elsewhere: follows. Found at two hosts: none is chosen. Nothing kept at the old host: nothing to take.
        let d = decide(&[(h("198.18.0.21"), Some(ag))], &[(h("198.18.0.210"), ag)], &set(&["198.18.0.210"]));
        assert_eq!(d.follow, [(h("198.18.0.210"), h("198.18.0.21"), ag)]);
        let d = decide(&[(h("198.18.0.21"), Some(ag)), (h("198.18.0.22"), Some(ag))], &[(h("198.18.0.210"), ag)], &set(&["198.18.0.210"]));
        assert!(d.follow.is_empty());
        let d = decide(&[(h("198.18.0.21"), Some(ag))], &[(h("198.18.0.210"), ag)], &set(&[]));
        assert!(d.follow.is_empty());
    }

    #[test]
    fn gpib_and_serial_resources_are_keys_of_their_own_and_usb_ones_are_not() {
        assert_eq!(key_of("GPIB0::15::INSTR").as_deref(), Some("gpib0::15::instr"));
        assert_eq!(key_of("ASRL3::INSTR").as_deref(), Some("asrl3::instr"));
        assert_eq!(key_of("USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR"), None);
        assert_eq!(host_of("TCPIP0::198.18.68.65::gpib0,20::INSTR").as_deref(), Some("198.18.68.65"));
        assert_eq!(host_of("GPIB0::15::INSTR"), None);
        let t = "# h\n198.18.0.62#0 = Siglent Technologies | SDG1062X Plus | SDG1EXAMPLE001\nmac 198.18.0.62 = 02:00:00:00:00:62\nmac bad = zz\n";
        assert_eq!(parse(t).len(), 1, "a MAC line is no unit");
        assert_eq!(parse_macs(t), [("198.18.0.62".to_string(), [0x02, 0x00, 0x00, 0x00, 0x00, 0x62])]);
    }

    #[test]
    fn a_bound_alias_moves_only_where_its_unit_alone_answers_and_its_address_is_not_it() {
        let id = crate::identity::Identity::from_idn("Agilent Technologies,33210A,MY20000002,1.04").unwrap();
        let me = "Agilent Technologies,33210A,MY20000002,1.04-1.02-22-2";
        let other = "Siglent Technologies,SDG1062X Plus,SDG1EXAMPLE001,1";
        let place = |rs: &[&str], online, idn: Option<&str>| Place { resources: rs.iter().map(|r| r.to_string()).collect(), online, idn: idn.map(str::to_string) };
        let old = "TCPIP0::198.18.0.210::inst0::INSTR";
        let new = place(&["TCPIP0::198.18.0.21::inst0::INSTR", "TCPIP0::198.18.0.21::5025::SOCKET"], true, Some(me));
        let gpib = place(&["GPIB0::10::INSTR"], true, Some(me));
        let to = Follow::Move("TCPIP0::198.18.0.21::inst0::INSTR".into());
        // The old address gone from the tree, or there and not answering: moved. GPIB is not a LAN address.
        assert_eq!(follow_target(old, &id, &[new.clone(), gpib.clone()]), to);
        assert_eq!(follow_target(old, &id, &[place(&[old], false, None), new.clone()]), to);
        // Answering as another unit: moved too. Answering as it, or not concluded yet: stays.
        assert_eq!(follow_target(old, &id, &[place(&[old], true, Some(other)), new.clone()]), to);
        assert_eq!(follow_target(old, &id, &[place(&[old], true, Some(me)), new.clone()]), Follow::Stay);
        assert_eq!(follow_target(old, &id, &[place(&[old], true, None), new.clone()]), Follow::Stay);
        // Nowhere else, or only on GPIB: stays. Two addresses: none chosen.
        assert_eq!(follow_target(old, &id, std::slice::from_ref(&gpib)), Follow::Stay);
        let twin = place(&["TCPIP0::198.18.0.22::hislip0::INSTR"], true, Some(me));
        assert_eq!(follow_target(old, &id, &[new.clone(), twin]), Follow::Ambiguous(vec!["TCPIP0::198.18.0.21::inst0::INSTR".into(), "TCPIP0::198.18.0.22::inst0::INSTR".into()]));
        // The name keeps its protocol and port; one behind a gateway does not follow.
        assert_eq!(follow_target("TCPIP0::198.18.0.210::hislip0::INSTR", &id, std::slice::from_ref(&new)), Follow::Move("TCPIP0::198.18.0.21::hislip0::INSTR".into()));
        assert_eq!(follow_target("TCPIP0::198.18.0.210::5025::SOCKET", &id, std::slice::from_ref(&new)), Follow::Move("TCPIP0::198.18.0.21::5025::SOCKET".into()));
        assert_eq!(follow_target("TCPIP0::198.18.68.65::gpib0,10::INSTR", &id, std::slice::from_ref(&new)), Follow::Stay);
        assert_eq!(follow_target("GPIB0::10::INSTR", &id, &[new]), Follow::Stay);
    }

    #[test]
    fn clearing_takes_the_settings_out_and_moving_takes_them_and_the_unbound_aliases_along() {
        let none = Settings { expose: Some(Expose::None), ..Settings::default() };
        let named = Settings { name: Some("bench".into()), ..Settings::default() };
        let k = Kept {
            settings: vec![("TCPIP0::198.18.68.57::inst0::INSTR".into(), none.clone()), ("TCPIP0::198.18.68.57::5025::SOCKET".into(), named.clone())],
            aliases: vec![
                Alias { alias: "OLD".into(), resource: "TCPIP0::198.18.68.57::hislip0::INSTR".into(), group: false, bound: false, unit: None },
                Alias { alias: "B".into(), resource: "TCPIP0::198.18.68.57::inst0::INSTR".into(), group: true, bound: true, unit: None },
            ],
        };
        let c = plan_clear(&k);
        assert_eq!(c.settings, [("TCPIP0::198.18.68.57::inst0::INSTR".to_string(), Settings::default()), ("TCPIP0::198.18.68.57::5025::SOCKET".to_string(), Settings::default())]);
        assert!(c.aliases.is_empty(), "clearing leaves the aliases");
        let m = plan_move(&k, "198.18.68.66").unwrap();
        assert_eq!(
            m.settings,
            [
                ("TCPIP0::198.18.68.66::inst0::INSTR".to_string(), none),
                ("TCPIP0::198.18.68.66::5025::SOCKET".to_string(), named),
                ("TCPIP0::198.18.68.57::inst0::INSTR".to_string(), Settings::default()),
                ("TCPIP0::198.18.68.57::5025::SOCKET".to_string(), Settings::default()),
            ]
        );
        assert_eq!(m.aliases, [("OLD".to_string(), "TCPIP0::198.18.68.66::hislip0::INSTR".to_string())], "a bound alias finds its unit itself");
        assert_eq!(plan_move(&k, "bad host").map(|_| ()), None);
    }
}
