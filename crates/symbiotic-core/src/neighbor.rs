//! The MAC address an IPv4 address has on the local network, from the system's neighbor (ARP) table: how a LAN
//! instrument is told from another without a command to it. Nothing is sent to the instrument; on Windows one ARP
//! request may be (`SendARP`), elsewhere the table is only read. On Linux and macOS a cached entry may lag, so an
//! address just given to another instrument may still show the last one's MAC for a while.
//!
//! Only an address that has just answered is asked about (`viFindRsrc` lists what it could connect to): an address
//! nobody has costs seconds before the system gives up.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};

pub type Mac = [u8; 6];

pub fn format(m: &Mac) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// `aa:bb:cc:dd:ee:ff` or `AA-BB-...`; None for anything else, and for the all-zero address (an entry being resolved).
pub fn parse(s: &str) -> Option<Mac> {
    let p: Vec<&str> = s.trim().split([':', '-']).collect();
    if p.len() != 6 {
        return None;
    }
    let mut m = [0u8; 6];
    for (i, x) in p.iter().enumerate() {
        if x.is_empty() || x.len() > 2 {
            return None;
        }
        m[i] = u8::from_str_radix(x, 16).ok()?;
    }
    (m != [0; 6]).then_some(m)
}

/// The IPv4 address a host name or address stands for (brackets of an IPv6 address are no IPv4 address: None).
pub fn ipv4_of(host: &str) -> Option<Ipv4Addr> {
    if let Ok(IpAddr::V4(a)) = host.parse::<IpAddr>() {
        return Some(a);
    }
    if host.starts_with('[') {
        return None;
    }
    (host, 0).to_socket_addrs().ok()?.find_map(|a| match a.ip() {
        IpAddr::V4(v) => Some(v),
        IpAddr::V6(_) => None,
    })
}

/// The MAC of each address, looked up at once (one thread each on Windows, where each lookup may ask the network).
pub fn macs(addrs: &[Ipv4Addr]) -> HashMap<Ipv4Addr, Mac> {
    let mut out = HashMap::new();
    if addrs.is_empty() {
        return out;
    }
    #[cfg(windows)]
    {
        // An address the system has just found nobody at (an invalid entry) is not asked again: that is seconds each.
        let gone = invalid_entries();
        let addrs: Vec<Ipv4Addr> = addrs.iter().filter(|a| !gone.contains(a)).copied().collect();
        let hs: Vec<_> = addrs.iter().map(|a| (*a, std::thread::spawn({
            let a = *a;
            move || send_arp(a)
        }))).collect();
        for (a, h) in hs {
            if let Ok(Some(m)) = h.join() {
                out.insert(a, m);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let t = table();
        for a in addrs {
            if let Some(m) = t.get(a) {
                out.insert(*a, *m);
            }
        }
    }
    out
}

/// The addresses of the system's ARP table whose entry says nobody answered (`MIB_IPNET_TYPE_INVALID`).
#[cfg(windows)]
fn invalid_entries() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetIpNetTable, MIB_IPNETROW_LH, MIB_IPNETTABLE};
    const MIB_IPNET_TYPE_INVALID: u32 = 2;
    let mut size: u32 = 0;
    unsafe {
        GetIpNetTable(std::ptr::null_mut(), &mut size, 0);
        if size == 0 {
            return Vec::new();
        }
        // u32-aligned, as the table is.
        let mut buf = vec![0u32; (size as usize).div_ceil(4) + 1];
        let t = buf.as_mut_ptr().cast::<MIB_IPNETTABLE>();
        if GetIpNetTable(t, &mut size, 0) != 0 {
            return Vec::new();
        }
        let n = (*t).dwNumEntries as usize;
        let rows = std::slice::from_raw_parts((*t).table.as_ptr().cast::<MIB_IPNETROW_LH>(), n);
        rows.iter().filter(|r| r.Anonymous.dwType == MIB_IPNET_TYPE_INVALID).map(|r| Ipv4Addr::from(r.dwAddr.to_ne_bytes())).collect()
    }
}

#[cfg(windows)]
fn send_arp(a: Ipv4Addr) -> Option<Mac> {
    use windows_sys::Win32::NetworkManagement::IpHelper::SendARP;
    let mut buf = [0u8; 8];
    let mut len: u32 = 8;
    // The address in network order, as an IPAddr (a u32 in memory order).
    let dst = u32::from_ne_bytes(a.octets());
    let r = unsafe { SendARP(dst, 0, buf.as_mut_ptr().cast(), &mut len) };
    if r != 0 || len != 6 {
        return None;
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(&buf[..6]);
    (m != [0; 6]).then_some(m)
}

/// Linux: `/proc/net/arp`, the complete entries (flags 0x2).
#[cfg(target_os = "linux")]
fn table() -> HashMap<Ipv4Addr, Mac> {
    std::fs::read_to_string("/proc/net/arp").map(|t| parse_proc_arp(&t)).unwrap_or_default()
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_arp(text: &str) -> HashMap<Ipv4Addr, Mac> {
    let mut out = HashMap::new();
    for l in text.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        let (Some(ip), Some(flags), Some(mac)) = (f.first().and_then(|x| x.parse::<Ipv4Addr>().ok()), f.get(2), f.get(3).and_then(|m| parse(m))) else { continue };
        if u32::from_str_radix(flags.trim_start_matches("0x"), 16).is_ok_and(|v| v & 0x2 != 0) {
            out.insert(ip, mac);
        }
    }
    out
}

/// macOS: the routing table's link-layer entries (`sysctl` `NET_RT_FLAGS`, `RTF_LLINFO`), what `arp -an` reads.
#[cfg(target_os = "macos")]
fn table() -> HashMap<Ipv4Addr, Mac> {
    const CTL_NET: libc::c_int = 4;
    const PF_ROUTE: libc::c_int = 17;
    const NET_RT_FLAGS: libc::c_int = 2;
    const RTF_LLINFO: libc::c_int = 0x400;
    let mut mib = [CTL_NET, PF_ROUTE, 0, libc::AF_INET, NET_RT_FLAGS, RTF_LLINFO];
    let mut n: libc::size_t = 0;
    let mut out = HashMap::new();
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 6, std::ptr::null_mut(), &mut n, std::ptr::null_mut(), 0) != 0 {
            return out;
        }
        let mut buf = vec![0u8; n + 4096];
        n = buf.len();
        if libc::sysctl(mib.as_mut_ptr(), 6, buf.as_mut_ptr().cast(), &mut n, std::ptr::null_mut(), 0) != 0 {
            return out;
        }
        buf.truncate(n);
        out = parse_route_dump(&buf);
    }
    out
}

/// A dump of `rt_msghdr`s, each followed by its socket addresses: the destination (`sockaddr_in`) and the gateway, which for a
/// link-layer entry is a `sockaddr_dl` holding the MAC.
#[cfg(any(target_os = "macos", test))]
fn parse_route_dump(b: &[u8]) -> HashMap<Ipv4Addr, Mac> {
    // sizeof(struct rt_msghdr) on macOS (64-bit and 32-bit alike: it has no pointers).
    const RT_MSGHDR: usize = 92;
    let mut out = HashMap::new();
    let mut i = 0;
    while i + 2 <= b.len() {
        let len = u16::from_le_bytes([b[i], b[i + 1]]) as usize;
        if len == 0 || i + len > b.len() {
            break;
        }
        let m = &b[i..i + len];
        i += len;
        let Some(sin) = m.get(RT_MSGHDR..) else { continue };
        // sockaddr_in: sin_len, sin_family, port (2), address (4).
        if sin.len() < 8 || sin[1] != 2 {
            continue;
        }
        let ip = Ipv4Addr::new(sin[4], sin[5], sin[6], sin[7]);
        let step = (sin[0] as usize).max(4).div_ceil(4) * 4;
        // sockaddr_dl: sdl_len, sdl_family (18), index (2), type, nlen, alen, slen, then the name and the address.
        let Some(dl) = sin.get(step..) else { continue };
        if dl.len() < 8 || dl[1] != 18 {
            continue;
        }
        let (nlen, alen) = (dl[5] as usize, dl[6] as usize);
        if alen == 6 {
            if let Some(a) = dl.get(8 + nlen..8 + nlen + 6) {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(a);
                if mac != [0; 6] {
                    out.insert(ip, mac);
                }
            }
        }
    }
    out
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn table() -> HashMap<Ipv4Addr, Mac> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_read_and_written_in_either_spelling() {
        assert_eq!(parse("02:00:00:00:00:62"), Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x62]));
        assert_eq!(parse("02-00-00-00-00-66"), Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]));
        assert_eq!(parse("2:0:0:0:0:66"), Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]), "macOS leaves out leading zeros");
        assert_eq!(parse("00:00:00:00:00:00"), None);
        assert_eq!(parse("02:00:00:00:00"), None);
        assert_eq!(format(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x66]), "02:00:00:00:00:66");
        assert_eq!(ipv4_of("198.18.0.62"), Some(Ipv4Addr::new(198, 18, 0, 62)));
        assert_eq!(ipv4_of("[fe80::1]"), None);
    }

    #[test]
    fn linuxs_table_gives_the_complete_entries_only() {
        let t = "IP address       HW type     Flags       HW address            Mask     Device\n\
                 198.18.68.66    0x1         0x2         02:00:00:00:00:66     *        br0\n\
                 198.18.68.250   0x1         0x0         00:00:00:00:00:00     *        br0\n\
                 198.18.68.1     0x1         0x6         02:00:00:00:00:01     *        br0\n";
        let m = parse_proc_arp(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&Ipv4Addr::new(198, 18, 68, 66)], [0x02, 0x00, 0x00, 0x00, 0x00, 0x66]);
    }

    #[test]
    fn macoss_route_dump_gives_each_entrys_mac() {
        let entry = |ip: [u8; 4], mac: Option<[u8; 6]>| {
            let mut m = vec![0u8; 92];
            m.extend_from_slice(&[16, 2, 0, 0]);
            m.extend_from_slice(&ip);
            m.extend_from_slice(&[0u8; 8]);
            let alen = if mac.is_some() { 6 } else { 0 };
            let mut dl = vec![20u8, 18, 7, 0, 6, 3, alen, 0];
            dl.extend_from_slice(b"en1");
            dl.extend_from_slice(&mac.unwrap_or_default()[..alen as usize]);
            dl.resize(20, 0);
            m.extend_from_slice(&dl);
            let len = m.len() as u16;
            m[..2].copy_from_slice(&len.to_le_bytes());
            m
        };
        let mut b = entry([198, 18, 68, 66], Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]));
        b.extend(entry([198, 18, 68, 252], None));
        b.extend(entry([198, 18, 68, 1], Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x01])));
        let m = parse_route_dump(&b);
        assert_eq!(m.len(), 2, "an incomplete entry has no MAC");
        assert_eq!(m[&Ipv4Addr::new(198, 18, 68, 1)], [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    }
}
