//! Finding LAN instruments without being told where they are. Off unless `lan.conf` says `discover`; `symbiotic-ctl lan
//! scan` runs it on demand.
//!
//! Two ways, run together for `WAIT`:
//!
//! - **VXI-11**: a portmapper GETPORT for the VXI-11 core program, broadcast over UDP on every
//!   interface's subnet (pyvisa-py does the same), and to 255.255.255.255 from every interface: a
//!   device whose subnet mask is not the network's (a static address typed in with the factory mask)
//!   does not take the subnet's broadcast as one, but every stack takes 255.255.255.255. A host that
//!   answers with a port, from an address on one of the interfaces' subnets, has a core channel:
//!   `TCPIP0::<ip>::inst0::INSTR`.
//! - **mDNS**: PTR questions for `_vxi-11._tcp`, `_hislip._tcp` and `_scpi-raw._tcp` sent to
//!   224.0.0.251:5353 from an ephemeral port, on every interface. A query whose source port is not
//!   5353 is answered by unicast to that port (RFC 6762 §6.7, "legacy unicast"), so nothing binds
//!   5353 -- Windows' own mDNS has it -- and nothing joins a group.
//!
//! Neither listens: both only take the answers to what they sent. Names carry the address, not a `.local` host name, so
//! they work whether or not the system resolves `.local`.

use super::name::{self, Target};
use super::rpc;
use super::xdr::{Decoder, Encoder};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// How long discovery waits for answers.
pub const WAIT: Duration = Duration::from_secs(1);

const MDNS: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 251), 5353);
const SERVICES: [&str; 3] = ["_vxi-11._tcp.local", "_hislip._tcp.local", "_scpi-raw._tcp.local"];

const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_SRV: u16 = 33;

/// An interface to discover on: its address, its subnet mask and its subnet's broadcast address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iface {
    pub addr: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub broadcast: Ipv4Addr,
}

impl Iface {
    fn new(addr: Ipv4Addr, mask: u32) -> Iface {
        Iface { addr, mask: Ipv4Addr::from(mask), broadcast: Ipv4Addr::from(u32::from(addr) | !mask) }
    }

    /// Is `ip` on this interface's subnet: reachable from here without a router.
    pub fn holds(&self, ip: Ipv4Addr) -> bool {
        let m = u32::from(self.mask);
        u32::from(ip) & m == u32::from(self.addr) & m
    }
}

/// A LAN/GPIB gateway: the host, and the bus address it answered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gateway {
    pub ip: Ipv4Addr,
    pub address: u8,
}

/// What discovery found: resources, and the gateways that were told apart from plain instruments
/// (a gateway has no `inst0` of its own to list).
#[derive(Debug, Default)]
pub struct Found {
    pub targets: Vec<Target>,
    pub gateways: Vec<Gateway>,
}

/// What each host was judged, kept for the process (the window, a program using the DLL).
fn verdicts() -> &'static std::sync::Mutex<BTreeMap<Ipv4Addr, super::vxi11::Verdict>> {
    static V: std::sync::OnceLock<std::sync::Mutex<BTreeMap<Ipv4Addr, super::vxi11::Verdict>>> = std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

/// The gateways among `hosts`: from `known` where a host has a verdict, else asked (in parallel),
/// and remembered unless it did not answer. Hosts not in `hosts` are dropped from `known`.
fn judge(hosts: &[Ipv4Addr], known: &mut BTreeMap<Ipv4Addr, super::vxi11::Verdict>, ask: impl Fn(Ipv4Addr) -> super::vxi11::Verdict + Sync) -> Vec<Gateway> {
    use super::vxi11::Verdict;
    known.retain(|ip, _| hosts.contains(ip));
    let fresh: Vec<Ipv4Addr> = hosts.iter().copied().filter(|ip| !known.contains_key(ip)).collect();
    let asked: Vec<(Ipv4Addr, Verdict)> = std::thread::scope(|sc| {
        let ask = &ask;
        let hs: Vec<_> = fresh.iter().map(|ip| sc.spawn(move || (*ip, ask(*ip)))).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (ip, v) in asked {
        if v != Verdict::Unknown {
            known.insert(ip, v);
        }
    }
    hosts.iter().filter_map(|ip| match known.get(ip) { Some(Verdict::Gateway(a)) => Some(Gateway { ip: *ip, address: *a }), _ => None }).collect()
}

/// How long to ask a VXI-11 host whether it is a gateway.
const GATEWAY_WAIT: Duration = Duration::from_millis(500);

/// The interface name a gateway answers to unless it was renamed.
const GATEWAY_IFACE: &str = "gpib0";

/// Everything found on the network within `wait`: VXI-11 hosts first, then what mDNS named, each
/// resource once, sorted within each kind by address. Gateways are left out (`discover_full`).
pub fn discover(wait: Duration) -> Vec<Target> {
    discover_full(wait).targets
}

/// `discover`, with the gateways named instead of listed as `inst0`.
pub fn discover_full(wait: Duration) -> Found {
    let ifaces = interfaces();
    let i2 = ifaces.clone();
    let v = std::thread::spawn(move || vxi11(&i2, wait));
    let m = mdns(&ifaces, wait);
    let mut names: Vec<String> = v.join().unwrap_or_default().iter().map(|ip| format!("TCPIP0::{ip}::inst0::INSTR")).collect();
    names.extend(m);
    let mut targets: Vec<Target> = Vec::new();
    for n in names {
        if let Some(t) = name::parse(&n) {
            if !targets.iter().any(|o| o.name() == t.name()) {
                targets.push(t);
            }
        }
    }
    // Which `inst0` hosts are gateways. Each host is judged once while it keeps answering, so a periodic scan does not
    // open a connection to every instrument every time. A host that is not in a scan is forgotten, and judged again
    // when it is back.
    let hosts: Vec<Ipv4Addr> = targets
        .iter()
        .filter_map(|t| {
            let n = t.name();
            n.strip_prefix("TCPIP0::")?.strip_suffix("::inst0::INSTR")?.parse().ok()
        })
        .collect();
    let gateways = judge(&hosts, &mut verdicts().lock().unwrap_or_else(|e| e.into_inner()), |ip| {
        super::vxi11::judge_host(&ip.to_string(), GATEWAY_IFACE, GATEWAY_WAIT)
    });
    targets.retain(|t| !gateways.iter().any(|g| t.name() == format!("TCPIP0::{}::inst0::INSTR", g.ip)));
    Found { targets, gateways }
}

/// Portmapper GETPORT for the VXI-11 core channel, broadcast; the hosts that have one.
///
/// Each interface has its own socket, bound to its address, which sends to the subnet's broadcast address and to
/// 255.255.255.255. The second reaches a device that does not take the first as a broadcast: one whose mask is wider
/// than the network's, which sees the subnet's broadcast address as some other host's. An answer counts only from an
/// address on one of the interfaces' subnets: 255.255.255.255 reaches a device whatever its address, and one that is on
/// none of them cannot be connected to from here.
fn vxi11(ifaces: &[Iface], wait: Duration) -> Vec<Ipv4Addr> {
    let xid = std::process::id().wrapping_mul(0x2545_F491) | 1;
    let mut a = Encoder::new();
    a.u32(0x0607AF).u32(1).u32(rpc::IPPROTO_TCP).u32(0);
    let call = rpc::build_call(xid, rpc::PMAP_PROG, rpc::PMAP_VERS, rpc::PMAPPROC_GETPORT, a.as_slice());
    // Without interfaces (none listed), one socket on any address and the one broadcast.
    let sends: Vec<(Ipv4Addr, Vec<Ipv4Addr>)> = if ifaces.is_empty() {
        vec![(Ipv4Addr::UNSPECIFIED, vec![Ipv4Addr::BROADCAST])]
    } else {
        ifaces.iter().map(|i| (i.addr, vec![i.broadcast, Ipv4Addr::BROADCAST])).collect()
    };
    let answered: Vec<Ipv4Addr> = std::thread::scope(|sc| {
        let call = &call;
        let threads: Vec<_> = sends
            .into_iter()
            .map(|(from, to)| {
                sc.spawn(move || {
                    let mut found = Vec::new();
                    let Ok(s) = UdpSocket::bind((from, 0)) else { return found };
                    if s.set_broadcast(true).is_err() {
                        return found;
                    }
                    for b in to {
                        let _ = s.send_to(call, (b, rpc::PMAP_PORT));
                    }
                    collect(&s, wait, |msg, from| {
                        if let (Ok(r), SocketAddr::V4(f)) = (rpc::parse_reply(msg, xid), from) {
                            if Decoder::new(r).u32().is_some_and(|p| p > 0) {
                                found.push(*f.ip());
                            }
                        }
                    });
                    found
                })
            })
            .collect();
        threads.into_iter().flat_map(|t| t.join().unwrap_or_default()).collect()
    });
    let mut found: Vec<Ipv4Addr> = answered.into_iter().filter(|ip| ifaces.is_empty() || ifaces.iter().any(|i| i.holds(*ip))).collect();
    found.sort();
    found.dedup();
    found
}

/// Receive datagrams until `wait` is up, handing each to `f` with its sender.
fn collect(s: &UdpSocket, wait: Duration, mut f: impl FnMut(&[u8], SocketAddr)) {
    let end = Instant::now() + wait;
    let mut buf = [0u8; 9000];
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left.max(Duration::from_millis(1)))).is_err() {
            return;
        }
        match s.recv_from(&mut buf) {
            Ok((n, from)) => f(&buf[..n], from),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return,
            // Windows reports an ICMP "port unreachable" for an earlier send as an error here: the answers
            // after it still count. An error that stays (a socket gone bad) is not spun on to the end.
            Err(_) => std::thread::sleep(Duration::from_millis(5).min(left)),
        }
    }
}

// ---------------------------------------------------------------------------------------
// mDNS
// ---------------------------------------------------------------------------------------

/// A DNS query with one question per name, each asking for a unicast answer (the QU bit).
pub fn query(names: &[(&str, u16)]) -> Vec<u8> {
    let mut m = vec![0, 0, 0, 0, 0, names.len() as u8, 0, 0, 0, 0, 0, 0];
    m[4] = (names.len() >> 8) as u8;
    for (n, ty) in names {
        for label in n.split('.').filter(|l| !l.is_empty()) {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&ty.to_be_bytes());
        m.extend_from_slice(&0x8001u16.to_be_bytes()); // QU, class IN
    }
    m
}

/// One resource record, as far as discovery cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rr {
    Ptr { owner: String, target: String },
    Srv { owner: String, port: u16, target: String },
    A { owner: String, addr: Ipv4Addr },
}

/// A name at `pos`, following compression pointers; lower-cased, no trailing dot. Returns the name
/// and where the name ends in the message (after the first pointer, if any).
fn read_name(m: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *m.get(pos)? as usize;
        match len {
            0 => {
                return Some((labels.join("."), end.unwrap_or(pos + 1)));
            }
            l if l & 0xC0 == 0xC0 => {
                let p = ((l & 0x3F) << 8) | *m.get(pos + 1)? as usize;
                end.get_or_insert(pos + 2);
                pos = p;
            }
            l if l < 64 => {
                let b = m.get(pos + 1..pos + 1 + l)?;
                labels.push(String::from_utf8_lossy(b).to_ascii_lowercase());
                pos += 1 + l;
            }
            _ => return None,
        }
    }
    None // a pointer loop
}

/// The PTR, SRV and A records of a DNS message, from every section.
pub fn records(m: &[u8]) -> Vec<Rr> {
    let mut out = Vec::new();
    let Some(h) = m.get(..12) else { return out };
    let count = |i: usize| u16::from_be_bytes([h[i], h[i + 1]]) as usize;
    let (qd, rrs) = (count(4), count(6) + count(8) + count(10));
    let mut pos = 12;
    for _ in 0..qd {
        let Some((_, p)) = read_name(m, pos) else { return out };
        pos = p + 4;
    }
    for _ in 0..rrs {
        let Some((owner, p)) = read_name(m, pos) else { return out };
        let Some(f) = m.get(p..p + 10) else { return out };
        let ty = u16::from_be_bytes([f[0], f[1]]);
        let len = u16::from_be_bytes([f[8], f[9]]) as usize;
        let data = p + 10;
        if m.len() < data + len {
            return out;
        }
        match ty {
            T_PTR => {
                if let Some((target, _)) = read_name(m, data) {
                    out.push(Rr::Ptr { owner, target });
                }
            }
            T_SRV if len >= 7 => {
                let port = u16::from_be_bytes([m[data + 4], m[data + 5]]);
                if let Some((target, _)) = read_name(m, data + 6) {
                    out.push(Rr::Srv { owner, port, target });
                }
            }
            T_A if len == 4 => {
                out.push(Rr::A { owner, addr: Ipv4Addr::new(m[data], m[data + 1], m[data + 2], m[data + 3]) });
            }
            _ => {}
        }
        pos = data + len;
    }
    out
}

/// What mDNS answered, put together: for each service instance, its service, port and address. An instance whose host
/// has no A record takes the address the answer came from (the instrument answers for itself). A loopback address from
/// another machine is that machine's own and cannot be reached from here: the instance is left out.
#[derive(Default)]
struct Seen {
    ptr: Vec<(String, String)>,
    srv: BTreeMap<String, (u16, String)>,
    a: BTreeMap<String, Ipv4Addr>,
    from: BTreeMap<String, Ipv4Addr>,
}

impl Seen {
    fn take(&mut self, m: &[u8], from: Ipv4Addr) {
        for r in records(m) {
            match r {
                Rr::Ptr { owner, target } => {
                    if SERVICES.contains(&owner.as_str()) && !self.ptr.contains(&(owner.clone(), target.clone())) {
                        self.from.entry(target.clone()).or_insert(from);
                        self.ptr.push((owner, target));
                    }
                }
                Rr::Srv { owner, port, target } => {
                    self.from.entry(owner.clone()).or_insert(from);
                    self.srv.insert(owner, (port, target));
                }
                Rr::A { owner, addr } => {
                    self.a.insert(owner, addr);
                }
            }
        }
    }

    /// Instances named but not yet located.
    fn missing(&self) -> Vec<String> {
        self.ptr.iter().filter(|(_, i)| !self.srv.contains_key(i)).map(|(_, i)| i.clone()).collect()
    }

    fn names(&self) -> Vec<String> {
        let mut out: Vec<(Ipv4Addr, String)> = Vec::new();
        for (service, instance) in &self.ptr {
            let srv = self.srv.get(instance);
            let addr = srv.and_then(|(_, host)| self.a.get(host)).or_else(|| self.from.get(instance)).copied();
            let Some(ip) = addr else { continue };
            let sender = self.from.get(instance);
            if (ip.is_loopback() || ip.is_unspecified()) && sender.is_some_and(|f| !f.is_loopback()) {
                continue;
            }
            let n = match (service.as_str(), srv) {
                ("_vxi-11._tcp.local", _) => format!("TCPIP0::{ip}::inst0::INSTR"),
                ("_hislip._tcp.local", Some((p, _))) if *p != name::HISLIP_PORT => format!("TCPIP0::{ip}::hislip0,{p}::INSTR"),
                ("_hislip._tcp.local", _) => format!("TCPIP0::{ip}::hislip0::INSTR"),
                ("_scpi-raw._tcp.local", Some((p, _))) => format!("TCPIP0::{ip}::{p}::SOCKET"),
                _ => continue, // a raw socket without its port is no name
            };
            if !out.iter().any(|(_, o)| *o == n) {
                out.push((ip, n));
            }
        }
        out.sort();
        out.into_iter().map(|(_, n)| n).collect()
    }
}

fn mdns(ifaces: &[Iface], wait: Duration) -> Vec<String> {
    let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return Vec::new() };
    let send = |q: &[u8]| {
        if ifaces.is_empty() {
            let _ = s.send_to(q, MDNS);
        }
        for i in ifaces {
            if multicast_if(&s, i.addr) {
                let _ = s.send_to(q, MDNS);
            }
        }
    };
    let questions: Vec<(&str, u16)> = SERVICES.iter().map(|n| (*n, T_PTR)).collect();
    send(&query(&questions));
    let mut seen = Seen::default();
    let take = |seen: &mut Seen, m: &[u8], from: SocketAddr| {
        if let SocketAddr::V4(f) = from {
            seen.take(m, *f.ip());
        }
    };
    // Half the time for the answers; then ask for the SRV records that did not come with them.
    collect(&s, wait / 2, |m, from| take(&mut seen, m, from));
    let missing = seen.missing();
    if !missing.is_empty() {
        let q: Vec<(&str, u16)> = missing.iter().map(|n| (n.as_str(), T_SRV)).collect();
        send(&query(&q));
    }
    collect(&s, wait / 2, |m, from| take(&mut seen, m, from));
    seen.names()
}

/// Send multicast out of the interface with address `addr`.
#[cfg(windows)]
fn multicast_if(s: &UdpSocket, addr: Ipv4Addr) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_MULTICAST_IF};
    let v = addr.octets(); // IP_MULTICAST_IF takes the address, in network order
    // SAFETY: a valid socket, and a 4-byte value whose length is passed.
    unsafe { setsockopt(s.as_raw_socket() as usize, IPPROTO_IP, IP_MULTICAST_IF, v.as_ptr(), 4) == 0 }
}

#[cfg(unix)]
fn multicast_if(s: &UdpSocket, addr: Ipv4Addr) -> bool {
    use std::os::fd::AsRawFd;
    let v = libc::in_addr { s_addr: u32::from_ne_bytes(addr.octets()) }; // network order
    // SAFETY: a valid socket, and an in_addr whose length is passed.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            &v as *const libc::in_addr as *const libc::c_void,
            std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
        ) == 0
    }
}

/// The interfaces that are up, IPv4, and not loopback.
#[cfg(windows)]
pub fn interfaces() -> Vec<Iface> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    const AF_INET: u32 = 2;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u64> = Vec::new(); // u64: the structures want 8-byte alignment
    for _ in 0..4 {
        buf.resize((size as usize).div_ceil(8), 0);
        // SAFETY: the buffer is `size` bytes long, as passed.
        let r = unsafe { GetAdaptersAddresses(AF_INET, flags, std::ptr::null(), buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH, &mut size) };
        match r {
            0 => break,
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: a list GetAdaptersAddresses built in `buf`, walked by its own `Next` pointers.
    unsafe {
        while !a.is_null() {
            let ad = &*a;
            if ad.OperStatus == IfOperStatusUp && ad.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let sa = (*u).Address.lpSockaddr as *const u8;
                    // SOCKADDR_IN: family (2 bytes), port (2), address (4).
                    if !sa.is_null() && u16::from_ne_bytes([*sa, *sa.add(1)]) == AF_INET as u16 {
                        let ip = Ipv4Addr::new(*sa.add(4), *sa.add(5), *sa.add(6), *sa.add(7));
                        let prefix = (*u).OnLinkPrefixLength.min(32) as u32;
                        let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
                        if !ip.is_loopback() && !ip.is_link_local() {
                            out.push(Iface::new(ip, mask));
                        }
                    }
                    u = (*u).Next;
                }
            }
            a = ad.Next;
        }
    }
    out
}

/// The interfaces that are up, IPv4, and not loopback (getifaddrs: Linux and macOS alike).
#[cfg(unix)]
pub fn interfaces() -> Vec<Iface> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut a = list;
    while !a.is_null() {
        // SAFETY: a node of the list getifaddrs built, walked by its own `ifa_next` pointers; an
        // AF_INET address and netmask are sockaddr_in.
        unsafe {
            let i = &*a;
            let up = i.ifa_flags & libc::IFF_UP as libc::c_uint != 0 && i.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint == 0;
            if up && !i.ifa_addr.is_null() && !i.ifa_netmask.is_null() && (*i.ifa_addr).sa_family as i32 == libc::AF_INET {
                let ip = Ipv4Addr::from((*(i.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr.to_ne_bytes());
                let mask = u32::from_be((*(i.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr);
                if !ip.is_loopback() && !ip.is_link_local() && !out.iter().any(|f: &Iface| f.addr == ip) {
                    out.push(Iface::new(ip, mask));
                }
            }
            a = i.ifa_next;
        }
    }
    // SAFETY: the list getifaddrs gave, once.
    unsafe { libc::freeifaddrs(list) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DNS response as an instrument sends it: PTR in the answers, SRV and A as additional records,
    /// names compressed.
    fn response(service: &str, instance: &str, host: &str, port: u16, ip: [u8; 4]) -> Vec<u8> {
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 2];
        let name = |m: &mut Vec<u8>, n: &str| {
            for l in n.split('.') {
                m.push(l.len() as u8);
                m.extend_from_slice(l.as_bytes());
            }
            m.push(0);
        };
        let rr = |m: &mut Vec<u8>, ty: u16, data: &[u8]| {
            m.extend_from_slice(&ty.to_be_bytes());
            m.extend_from_slice(&[0x80, 1, 0, 0, 0x11, 0x94]);
            m.extend_from_slice(&(data.len() as u16).to_be_bytes());
            m.extend_from_slice(data);
        };
        let svc_at = m.len();
        name(&mut m, service);
        // PTR -> "<instance>" + pointer to the service name.
        let mut d = vec![instance.len() as u8];
        d.extend_from_slice(instance.as_bytes());
        d.extend_from_slice(&[0xC0, svc_at as u8]);
        rr(&mut m, T_PTR, &d);
        let inst_at = m.len() - d.len();
        // SRV, owner = pointer to the instance name.
        m.extend_from_slice(&[0xC0, inst_at as u8]);
        let mut s = vec![0, 0, 0, 0];
        s.extend_from_slice(&port.to_be_bytes());
        let host_at = m.len() + 10 + s.len();
        for l in host.split('.') {
            s.push(l.len() as u8);
            s.extend_from_slice(l.as_bytes());
        }
        s.push(0);
        rr(&mut m, T_SRV, &s);
        m.extend_from_slice(&[0xC0, host_at as u8]);
        rr(&mut m, T_A, &ip);
        m
    }

    #[test]
    #[cfg(unix)]
    fn this_machines_interfaces_are_found_with_their_broadcast_addresses() {
        let ifs = interfaces();
        eprintln!("{ifs:?}");
        for i in &ifs {
            assert!(!i.addr.is_loopback() && !i.addr.is_unspecified());
            // The broadcast address is the interface's own with the host bits set.
            assert_eq!(u32::from(i.broadcast) & u32::from(i.addr), u32::from(i.addr));
        }
    }

    #[test]
    fn an_answer_counts_from_an_interfaces_subnet() {
        // A host on a /22 (198.18.68.53, broadcast 198.18.71.255).
        let i = Iface::new(Ipv4Addr::new(198, 18, 68, 53), u32::MAX << 10);
        assert_eq!(i.broadcast, Ipv4Addr::new(198, 18, 71, 255));
        // An Etheric at a static .222 with the factory /16 is on it, though it does not take .71.255 as a broadcast.
        assert!(i.holds(Ipv4Addr::new(198, 18, 68, 222)));
        assert!(i.holds(Ipv4Addr::new(198, 18, 71, 254)));
        assert!(!i.holds(Ipv4Addr::new(198, 18, 72, 1)));
        assert!(!i.holds(Ipv4Addr::new(10, 0, 0, 5)));
    }

    #[test]
    fn a_host_is_judged_once_while_it_keeps_answering() {
        use super::super::vxi11::Verdict;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (gw, plain, mute) = (Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2), Ipv4Addr::new(10, 0, 0, 3));
        let asks = AtomicUsize::new(0);
        let ask = |ip: Ipv4Addr| {
            asks.fetch_add(1, Ordering::SeqCst);
            if ip == gw { Verdict::Gateway(21) } else if ip == plain { Verdict::Instrument } else { Verdict::Unknown }
        };
        let mut known = BTreeMap::new();
        let g = judge(&[gw, plain, mute], &mut known, ask);
        assert_eq!(g, [Gateway { ip: gw, address: 21 }]);
        assert_eq!(asks.load(Ordering::SeqCst), 3);
        // Next scan: only the one that gave no answer is asked again.
        assert_eq!(judge(&[gw, plain, mute], &mut known, ask), g);
        assert_eq!(asks.load(Ordering::SeqCst), 4);
        // Gone from a scan: forgotten, so judged afresh when it is back.
        judge(&[plain], &mut known, ask);
        assert_eq!(judge(&[gw, plain], &mut known, ask), g);
        assert_eq!(asks.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn a_query_asks_each_name() {
        let q = query(&[("_hislip._tcp.local", T_PTR), ("_scpi-raw._tcp.local", T_PTR)]);
        assert_eq!(&q[4..6], &[0, 2]);
        assert_eq!(&q[12..20], b"\x07_hislip");
        assert_eq!(read_name(&q, 12).unwrap().0, "_hislip._tcp.local");
    }

    #[test]
    fn records_are_read_through_compression() {
        let m = response("_hislip._tcp.local", "K-34465A-02417", "A-34465A-02417.local", 4880, [198, 18, 68, 63]);
        let r = records(&m);
        assert_eq!(r[0], Rr::Ptr { owner: "_hislip._tcp.local".into(), target: "k-34465a-02417._hislip._tcp.local".into() });
        assert_eq!(r[1], Rr::Srv { owner: "k-34465a-02417._hislip._tcp.local".into(), port: 4880, target: "a-34465a-02417.local".into() });
        assert_eq!(r[2], Rr::A { owner: "a-34465a-02417.local".into(), addr: Ipv4Addr::new(198, 18, 68, 63) });
        assert!(records(&m[..m.len() - 3]).len() < 3, "a truncated record is left out");
        // A pointer to itself is not followed for ever.
        assert_eq!(read_name(&[0xC0, 0], 0), None);
    }

    #[test]
    fn answers_become_resource_names() {
        let from = Ipv4Addr::new(198, 18, 68, 99);
        let mut seen = Seen::default();
        seen.take(&response("_hislip._tcp.local", "dmm", "dmm.local", 4880, [198, 18, 68, 63]), from);
        seen.take(&response("_scpi-raw._tcp.local", "dmm", "dmm.local", 5025, [198, 18, 68, 63]), from);
        seen.take(&response("_vxi-11._tcp.local", "dmm", "dmm.local", 111, [198, 18, 68, 63]), from);
        seen.take(&response("_hislip._tcp.local", "other", "o.local", 4881, [10, 0, 0, 2]), from);
        seen.take(&response("_http._tcp.local", "web", "w.local", 80, [10, 0, 0, 3]), from);
        assert_eq!(
            seen.names(),
            [
                "TCPIP0::10.0.0.2::hislip0,4881::INSTR",
                "TCPIP0::198.18.68.63::5025::SOCKET",
                "TCPIP0::198.18.68.63::hislip0::INSTR",
                "TCPIP0::198.18.68.63::inst0::INSTR"
            ]
        );
        // Another machine's loopback address is no use here.
        let mut s3 = Seen::default();
        s3.take(&response("_hislip._tcp.local", "ViperGPIB 6-29", "v.local", 4880, [127, 1, 6, 29]), from);
        assert!(s3.names().is_empty());
        let mut s4 = Seen::default();
        s4.take(&response("_hislip._tcp.local", "local", "v.local", 4880, [127, 1, 6, 29]), Ipv4Addr::LOCALHOST);
        assert_eq!(s4.names(), ["TCPIP0::127.1.6.29::hislip0::INSTR"], "but this machine's own is");
        // Named but not located: asked for again; without an A record, the sender's address.
        let mut ptr_only = response("_hislip._tcp.local", "bare", "b.local", 4880, [1, 2, 3, 4]);
        ptr_only[11] = 0; // no additional records
        let mut s2 = Seen::default();
        s2.take(&ptr_only, from);
        assert_eq!(s2.missing(), ["bare._hislip._tcp.local"]);
        assert_eq!(s2.names(), ["TCPIP0::198.18.68.99::hislip0::INSTR"]);
    }

    #[test]
    fn a_portmapper_answer_is_a_vxi11_host() {
        // A fake portmapper on loopback answers GETPORT; the broadcast is sent to it directly.
        let pm = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = pm.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            let (n, from) = pm.recv_from(&mut b).unwrap();
            let c = rpc::server::parse_call(&b[..n]).unwrap();
            let mut r = Encoder::new();
            r.u32(1024);
            pm.send_to(&rpc::server::reply(c.xid, r.as_slice()), from).unwrap();
        });
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let xid = 77;
        let mut a = Encoder::new();
        a.u32(0x0607AF).u32(1).u32(rpc::IPPROTO_TCP).u32(0);
        s.send_to(&rpc::build_call(xid, rpc::PMAP_PROG, rpc::PMAP_VERS, rpc::PMAPPROC_GETPORT, a.as_slice()), ("127.0.0.1", port)).unwrap();
        let mut got = None;
        collect(&s, Duration::from_millis(500), |m, _| got = rpc::parse_reply(m, xid).ok().and_then(|r| Decoder::new(r).u32()));
        assert_eq!(got, Some(1024));
    }
}
