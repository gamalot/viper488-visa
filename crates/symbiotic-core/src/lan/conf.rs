//! Configured LAN instruments: the `TCPIP` resources `viFindRsrc` lists.
//!
//! VISA lists the LAN resources that were configured; discovery by broadcast is an option, off unless switched on here
//! (discover.rs). One resource name per line, `#` comments, in `lan.conf` in the configuration directory, beside
//! `speed.conf`; `SYMBIOTIC_LAN_CONF` names another file. A line that is not a `TCPIP` name is ignored, except
//! `discover` on a line of its own, which turns on discovery (discover.rs).
//!
//! An entry is listed only while something answers a TCP connection at its port -- the port the name gives, HiSLIP's,
//! or the portmapper's for VXI-11 -- within `REACH`: hosts at once, one host's entries one after another (see
//! `check_all`). That is a handshake and nothing more: no message goes to the instrument.

use super::name::{self, Proto, Target};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an entry has to answer a connection to be listed: room for a slow host, and what an absent host costs.
pub const REACH: Duration = Duration::from_millis(2500);

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_LAN_CONF") {
        return Some(PathBuf::from(p));
    }
    // The configuration directory: shared by all users after an all-users install (paths.rs).
    crate::paths::config_file("lan.conf")
}

/// The resources in `text`, in their full form, once each, in file order.
pub fn parse(text: &str) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if let Some(t) = name::parse(l) {
            if !out.iter().any(|o| o.name() == t.name()) {
                out.push(t);
            }
        }
    }
    out
}

pub fn load_from(p: &Path) -> Vec<Target> {
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

pub fn load() -> Vec<Target> {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

/// Does `text` turn discovery on?
pub fn discover_in(text: &str) -> bool {
    text.lines().any(|l| l.trim().eq_ignore_ascii_case("discover"))
}

/// Is discovery on in the configuration?
pub fn discover_enabled() -> bool {
    path().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|t| discover_in(&t))
}

/// Turn discovery on or off in the file at `p`; false if it already was that way.
pub fn set_discover(p: &Path, on: bool) -> std::io::Result<bool> {
    crate::conffile::update(p, |text| {
        if discover_in(text) == on {
            return None;
        }
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().eq_ignore_ascii_case("discover")).collect();
        if on {
            lines.push("discover");
        }
        Some(lines.join("\n") + "\n")
    })
}

/// Add `name` (in its full form) unless it is there; false if it was.
pub fn add(p: &Path, t: &Target) -> std::io::Result<bool> {
    crate::conffile::update(p, |text| {
        if parse(text).iter().any(|o| o.name() == t.name()) {
            return None;
        }
        let mut text = if text.is_empty() {
            "# LAN instruments viFindRsrc lists (symbiotic-ctl lan add/remove, or edit by hand): one VISA name per line.\n".to_string()
        } else {
            text.to_string()
        };
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&t.name());
        text.push('\n');
        Some(text)
    })
}

/// Remove every line that names the same resource as `t`; false if none did. Comments and the
/// other lines stay as they were.
pub fn remove(p: &Path, t: &Target) -> std::io::Result<bool> {
    let full = t.name();
    crate::conffile::update(p, |text| {
        let kept: Vec<&str> = text.lines().filter(|l| name::parse(l.trim()).is_none_or(|o| o.name() != full)).collect();
        (kept.len() != text.lines().count()).then(|| kept.join("\n") + "\n")
    })
}

/// The port a reachability check connects to.
fn probe_port(t: &Target) -> u16 {
    match &t.proto {
        Proto::Vxi11 { port: None, .. } => super::rpc::PMAP_PORT,
        _ => t.port().unwrap_or(super::rpc::PMAP_PORT),
    }
}

/// How one entry's check went: where it answered, or why not, and how long it took either way.
#[derive(Debug, Clone)]
pub struct Reach {
    pub target: Target,
    pub took: Duration,
    pub result: Result<std::net::SocketAddr, String>,
}

impl std::fmt::Display for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.result {
            Ok(a) => write!(f, "{}: answered at {a} in {} ms", self.target.name(), self.took.as_millis()),
            Err(e) => write!(f, "{}: no answer after {} ms ({e})", self.target.name(), self.took.as_millis()),
        }
    }
}

/// Does something accept a TCP connection where `t` would be reached, within `within`?
pub fn check(t: Target, within: Duration) -> Reach {
    let start = std::time::Instant::now();
    let host = t.host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = probe_port(&t);
    let result = match (host.as_str(), port).to_socket_addrs() {
        Err(e) => Err(format!("resolving {host}: {e}")),
        Ok(addrs) => {
            let mut last = format!("{host}: no address");
            let mut hit = None;
            for a in addrs {
                match std::net::TcpStream::connect_timeout(&a, within) {
                    Ok(_) => {
                        hit = Some(a);
                        break;
                    }
                    Err(e) => last = format!("{a}: {:?}: {e}", e.kind()),
                }
            }
            hit.ok_or(last)
        }
    };
    Reach { target: t, took: start.elapsed(), result }
}

/// Every entry of `targets` checked, in the order given. Hosts are checked at once, but one host's entries one after
/// another: with a stale neighbour (ARP) entry, Windows sends only the first of several packets to a host and the rest
/// wait for a retransmission seconds later; the first connection refreshes the entry. A host that does not answer the
/// first within `within` is not waited for again: its other entries are reported with the same cause at once. One that
/// refuses (it is there; that port is not) has its others checked.
pub fn check_all(targets: Vec<Target>, within: Duration) -> Vec<Reach> {
    let mut hosts: Vec<(String, Vec<(usize, Target)>)> = Vec::new();
    for (i, t) in targets.into_iter().enumerate() {
        let h = t.host.to_ascii_lowercase();
        match hosts.iter_mut().find(|(k, _)| *k == h) {
            Some((_, v)) => v.push((i, t)),
            None => hosts.push((h, vec![(i, t)])),
        }
    }
    let checks: Vec<_> = hosts
        .into_iter()
        .map(|(_, entries)| {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let mut silent: Option<String> = None;
                for (i, t) in entries {
                    let r = match &silent {
                        Some(why) => Reach { target: t, took: Duration::ZERO, result: Err(format!("the host did not answer ({why})")) },
                        None => check(t, within),
                    };
                    if let Err(e) = &r.result {
                        if r.took >= within && silent.is_none() {
                            silent = Some(e.clone());
                        }
                    }
                    out.push((i, r));
                }
                out
            })
        })
        .collect();
    let mut all: Vec<(usize, Reach)> = checks.into_iter().filter_map(|h| h.join().ok()).flatten().collect();
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, r)| r).collect()
}

/// Of `reached` (entries whose host answered), the ones that are there: an instrument behind a LAN/GPIB
/// gateway (`gpib0,6`) only if its address has a listener, which `find` says of each gateway's
/// interface once -- `find(host, core port, interface, within)`, in practice `vxi11::find_at`. The
/// others pass as they are. A gateway `find` cannot tell about (another client holds the interface,
/// no ATN control, no answer) keeps all its entries: not finding out is not finding it absent.
/// Gateways are asked at once, in the order of `reached` otherwise.
pub fn present_behind_gateways(
    reached: Vec<Target>,
    within: Duration,
    find: &(dyn Fn(&str, Option<u16>, &str, Duration) -> Result<Vec<u8>, super::Error> + Sync),
) -> Vec<Target> {
    // One question per gateway interface.
    let key = |t: &Target| match &t.proto {
        Proto::Vxi11 { port, .. } => t.gpib().map(|g| (t.host.to_ascii_lowercase(), *port, g.iface.to_ascii_lowercase())),
        _ => None,
    };
    let mut asks: Vec<(String, Option<u16>, String)> = Vec::new();
    for k in reached.iter().filter_map(key) {
        if !asks.contains(&k) {
            asks.push(k);
        }
    }
    let answers: Vec<Option<Vec<u8>>> = std::thread::scope(|s| {
        let hs: Vec<_> = asks
            .iter()
            .map(|(host, port, iface)| s.spawn(move || find(host.trim_start_matches('[').trim_end_matches(']'), *port, iface, within).ok()))
            .collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    reached
        .into_iter()
        .filter(|t| match key(t) {
            None => true,
            Some(k) => {
                let found = asks.iter().position(|a| *a == k).and_then(|i| answers[i].as_ref());
                // `key` is Some only for a gateway name, so `gpib()` is too.
                found.is_none_or(|f| t.gpib().is_some_and(|g| f.contains(&g.pad)))
            }
        })
        .collect()
}

/// The entries of `targets` that answer, checked all at once; in the order given.
pub fn reachable_all(targets: Vec<Target>, within: Duration) -> Vec<Target> {
    check_all(targets, within).into_iter().filter(|r| r.result.is_ok()).map(|r| r.target).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_of_names() {
        let t = parse(
            "# comment\n\nTCPIP::10.0.0.5::inst0\n  tcpip0::10.0.0.5::INST0::INSTR\nGPIB0::20::INSTR\nnonsense\n\
             TCPIP0::10.0.0.6::hislip0::INSTR\nTCPIP0::10.0.0.6::5025::SOCKET\n",
        );
        let names: Vec<String> = t.iter().map(|t| t.name()).collect();
        // The device name is compared as written (VXI-11 device names are the device's to interpret).
        assert_eq!(
            names,
            [
                "TCPIP0::10.0.0.5::inst0::INSTR",
                "TCPIP0::10.0.0.5::INST0::INSTR",
                "TCPIP0::10.0.0.6::hislip0::INSTR",
                "TCPIP0::10.0.0.6::5025::SOCKET"
            ]
        );
        assert_eq!(parse("TCPIP::h\nTCPIP0::h::inst0::INSTR\n").len(), 1, "one resource, two spellings");
    }

    #[test]
    fn add_and_remove_keep_the_rest() {
        let dir = std::env::temp_dir().join(format!("symbiotic-lanconf-{}", std::process::id()));
        let p = dir.join("lan.conf");
        let _ = std::fs::remove_dir_all(&dir);
        let a = name::parse("TCPIP::10.0.0.5").unwrap();
        let b = name::parse("TCPIP::10.0.0.6::5025::SOCKET").unwrap();
        assert!(add(&p, &a).unwrap());
        assert!(!add(&p, &name::parse("TCPIP0::10.0.0.5::inst0::INSTR").unwrap()).unwrap(), "already there");
        assert!(add(&p, &b).unwrap());
        assert_eq!(load_from(&p), vec![a.clone(), b.clone()]);
        assert!(remove(&p, &a).unwrap());
        assert!(!remove(&p, &a).unwrap());
        assert_eq!(load_from(&p), vec![b]);
        assert!(std::fs::read_to_string(&p).unwrap().starts_with('#'), "the comment stays");
        assert!(!discover_in(&std::fs::read_to_string(&p).unwrap()));
        assert!(set_discover(&p, true).unwrap());
        assert!(!set_discover(&p, true).unwrap(), "already on");
        assert!(discover_in(&std::fs::read_to_string(&p).unwrap()));
        assert_eq!(load_from(&p), vec![name::parse("TCPIP::10.0.0.6::5025::SOCKET").unwrap()], "the line is not an entry");
        assert!(set_discover(&p, false).unwrap());
        assert!(!discover_in(&std::fs::read_to_string(&p).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_hosts_entries_are_checked_in_turn_and_a_silent_host_is_waited_for_once() {
        // 127.0.0.1: one port answers, one refuses; both checked. 192.0.2.1 (TEST-NET, nothing
        // there): the first entry waits out the limit, the second is not waited for again.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let up = l.local_addr().unwrap().port();
        let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t = |h: &str, p: u16| name::parse(&format!("TCPIP::{h}::{p}::SOCKET")).unwrap();
        let within = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let r = check_all(vec![t("192.0.2.1", 5025), t("127.0.0.1", down), t("192.0.2.1", 5026), t("127.0.0.1", up)], within);
        assert!(start.elapsed() < within * 2, "the silent host is waited for once, alongside the other: {:?}", start.elapsed());
        let names: Vec<String> = r.iter().map(|r| r.target.name()).collect();
        assert_eq!(names[0], "TCPIP0::192.0.2.1::5025::SOCKET", "results in the order given");
        assert!(r[0].result.is_err() && r[1].result.is_err() && r[2].result.is_err());
        if r[0].took >= within {
            // (Where the network says "unreachable" at once instead, there is nothing to wait out.)
            assert!(r[2].result.as_ref().unwrap_err().starts_with("the host did not answer"));
        }
        assert!(r[3].result.is_ok(), "a refusal does not stop the host's other entries: {:?}", r[3].result);
    }

    #[test]
    fn only_what_answers_is_reachable() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let up = l.local_addr().unwrap().port();
        let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t = |p: u16| name::parse(&format!("TCPIP::127.0.0.1::{p}::SOCKET")).unwrap();
        let got = reachable_all(vec![t(down), t(up)], REACH);
        assert_eq!(got, vec![t(up)]);
    }

    #[test]
    fn a_gateways_entries_are_listed_when_their_address_has_a_listener() {
        let reached: Vec<Target> = [
            "TCPIP0::gw::gpib0,6::INSTR",
            "TCPIP0::gw::gpib0,29::INSTR",
            "TCPIP0::gw::gpib0,10,96::INSTR",
            "TCPIP0::busy::gpib0,5::INSTR",
            "TCPIP0::dmm::inst0::INSTR",
            "TCPIP0::gw::5025::SOCKET",
        ]
        .iter()
        .map(|n| name::parse(n).unwrap())
        .collect();
        let asked = std::sync::Mutex::new(Vec::new());
        let find = |host: &str, _port: Option<u16>, iface: &str, _w: Duration| {
            asked.lock().unwrap().push(format!("{host}/{iface}"));
            match host {
                "gw" => Ok(vec![6, 10]),
                _ => Err(super::super::Error::Unsupported),
            }
        };
        let kept: Vec<String> = present_behind_gateways(reached, Duration::from_secs(1), &find).iter().map(|t| t.name()).collect();
        // 29 has no listener; the busy gateway could not say, so its entry stays; non-gateway entries are not asked about.
        assert_eq!(
            kept,
            ["TCPIP0::gw::gpib0,6::INSTR", "TCPIP0::gw::gpib0,10,96::INSTR", "TCPIP0::busy::gpib0,5::INSTR", "TCPIP0::dmm::inst0::INSTR", "TCPIP0::gw::5025::SOCKET"]
        );
        let mut asked = asked.into_inner().unwrap();
        asked.sort();
        assert_eq!(asked, ["busy/gpib0", "gw/gpib0"], "one question per gateway interface");
    }
}
