//! `TCPIP` resource names (VPP-4.3), case-insensitive, board optional (default 0):
//!
//! ```text
//!     TCPIP[board]::host[::lan device name][::INSTR]    VXI-11 (inst0 when the name is left out),
//!                                                       or HiSLIP when the device name is hislip...
//!     TCPIP[board]::host::port::SOCKET                  a raw socket
//! ```
//!
//! Two forms beyond VPP-4.3: `host,port` gives a VXI-11 core channel's port so the portmapper is not asked (pyvisa-py
//! writes it that way, for where port 111 is blocked or taken), and `hislip0,port` gives HiSLIP's. A host may be an IPv6
//! address in brackets, `[fe80::1]`, so its colons are not read as separators.

/// An instrument behind a LAN/GPIB gateway, from a VXI-11 device name `gpib<n>,<pad>[,<sad>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpib {
    /// The gateway's interface: the device name of its interface link, `gpib<n>` as written.
    pub iface: String,
    pub pad: u8,
    pub sad: Option<u8>,
}

/// `gpib<n>,<pad>[,<sad>]`, case-insensitive, addresses 0..=30; anything else is not a gateway name.
pub fn gpib_of(device: &str) -> Option<Gpib> {
    let mut parts = device.split(',');
    let iface = parts.next()?;
    if !crate::resource::strip_prefix_ci(iface, "gpib")?.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let addr = |s: &str| s.parse::<u8>().ok().filter(|a| *a <= 30);
    let pad = addr(parts.next()?)?;
    let sad = match parts.next() {
        Some(s) => Some(addr(s)?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Gpib { iface: iface.to_string(), pad, sad })
}

/// The HiSLIP port (IANA).
pub const HISLIP_PORT: u16 = 4880;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proto {
    /// `port`: the core channel's, given in the name; `None` asks the portmapper.
    Vxi11 { port: Option<u16>, device: String },
    Hislip { port: u16, sub: String },
    Socket { port: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub board: u16,
    /// As written, brackets and all for IPv6.
    pub host: String,
    pub proto: Proto,
    /// The LAN device name as written (`inst0`, `gpib0,5`, `hislip0,4880`); empty for a socket.
    pub device: String,
}

impl Target {
    /// The resource class: INSTR or SOCKET.
    pub fn class(&self) -> &'static str {
        match self.proto {
            Proto::Socket { .. } => "SOCKET",
            _ => "INSTR",
        }
    }

    /// The name in its full form, as VI_ATTR_RSRC_NAME and viParseRsrcEx give it.
    pub fn name(&self) -> String {
        match &self.proto {
            Proto::Socket { port } => format!("TCPIP{}::{}::{}::SOCKET", self.board, self.host, port),
            Proto::Vxi11 { port: Some(p), .. } => {
                format!("TCPIP{}::{},{}::{}::INSTR", self.board, self.host, p, self.device)
            }
            _ => format!("TCPIP{}::{}::{}::INSTR", self.board, self.host, self.device),
        }
    }

    /// The port the name gives, or the protocol's own; `None` for VXI-11 through the portmapper.
    pub fn port(&self) -> Option<u16> {
        match &self.proto {
            Proto::Vxi11 { port, .. } => *port,
            Proto::Hislip { port, .. } => Some(*port),
            Proto::Socket { port } => Some(*port),
        }
    }

    /// The GPIB address, if this is an instrument behind a LAN/GPIB gateway.
    pub fn gpib(&self) -> Option<Gpib> {
        match &self.proto {
            Proto::Vxi11 { device, .. } => gpib_of(device),
            _ => None,
        }
    }

    pub fn is_hislip(&self) -> bool {
        matches!(self.proto, Proto::Hislip { .. })
    }
}

fn port_of(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|p| *p != 0)
}

/// `TCPIP` and an optional board number.
fn board_of(head: &str) -> Option<u16> {
    match crate::resource::strip_prefix_ci(head, "TCPIP")? {
        "" => Some(0),
        d if d.bytes().all(|c| c.is_ascii_digit()) => d.parse().ok(),
        _ => None,
    }
}

pub fn parse(name: &str) -> Option<Target> {
    let (head, rest) = name.split_once("::")?;
    let board = board_of(head)?;
    // The host: up to the next separator, or a bracketed IPv6 address and what follows it.
    let (host, tail) = if rest.starts_with('[') {
        let close = rest.find(']')?;
        let after = &rest[close + 1..];
        let (extra, tail) = match after.find("::") {
            Some(i) => (&after[..i], Some(&after[i + 2..])),
            None => (after, None),
        };
        if !(extra.is_empty() || extra.starts_with(',')) {
            return None;
        }
        (&rest[..close + 1 + extra.len()], tail)
    } else {
        match rest.split_once("::") {
            Some((h, t)) => (h, Some(t)),
            None => (rest, None),
        }
    };
    let (host, host_port) = match host.rsplit_once(',') {
        Some((h, p)) if !h.ends_with(':') => (h, Some(port_of(p)?)),
        _ => (host, None),
    };
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == ',') || host == "[]" {
        return None;
    }
    let parts: Vec<&str> = match tail {
        None => Vec::new(),
        Some(t) => t.split("::").collect(),
    };
    let (device, class) = match parts.as_slice() {
        [] => ("inst0", "INSTR"),
        [c] if c.eq_ignore_ascii_case("INSTR") => ("inst0", "INSTR"),
        [c] if c.eq_ignore_ascii_case("SOCKET") => return None,
        [d] => (*d, "INSTR"),
        [d, c] if c.eq_ignore_ascii_case("INSTR") || c.eq_ignore_ascii_case("SOCKET") => (*d, *c),
        _ => return None,
    };
    if device.is_empty() {
        return None;
    }
    let host = host.to_string();
    if class.eq_ignore_ascii_case("SOCKET") {
        // A socket's port is the whole of its "device"; a port on the host as well is one too many.
        let port = port_of(device)?;
        if host_port.is_some() {
            return None;
        }
        return Some(Target { board, host, proto: Proto::Socket { port }, device: String::new() });
    }
    if let Some(rest) = crate::resource::strip_prefix_ci(device, "hislip") {
        let (rest, port) = match rest.split_once(',') {
            Some((s, p)) => (s, port_of(p)?),
            None => (rest, HISLIP_PORT),
        };
        if host_port.is_some() {
            return None;
        }
        // VISA matches the name in any case (VPP-4.3 RULE 4.3.17); an instrument may know only
        // `hislip0` as sent. So the prefix goes lowercase.
        let proto = Proto::Hislip { port, sub: format!("hislip{rest}") };
        return Some(Target { board, host, proto, device: device.to_string() });
    }
    let proto = Proto::Vxi11 { port: host_port, device: lower_prefix(device) };
    Some(Target { board, host, proto, device: device.to_string() })
}

/// The device name as the instrument is sent it: VISA matches names in any case (VPP-4.3 RULE
/// 4.3.17), an instrument matches what it is sent, and may know `inst0` but not `INST0`. Only
/// the leading letters go lowercase (`inst`, `gpib`, `usb`); what follows, a USB serial number
/// among it, is the instrument's to read as it likes.
fn lower_prefix(device: &str) -> String {
    let n = device.bytes().take_while(u8::is_ascii_alphabetic).count();
    device[..n].to_ascii_lowercase() + &device[n..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Target {
        parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn vxi11_names() {
        let t = p("TCPIP0::192.168.1.20::inst0::INSTR");
        assert_eq!((t.board, t.host.as_str(), t.class()), (0, "192.168.1.20", "INSTR"));
        assert_eq!(t.proto, Proto::Vxi11 { port: None, device: "inst0".into() });
        // The device name and the class are optional; the board is too.
        for s in ["TCPIP::192.168.1.20", "tcpip::192.168.1.20::instr", "TCPIP0::192.168.1.20::inst0"] {
            assert_eq!(p(s).name(), "TCPIP0::192.168.1.20::inst0::INSTR", "{s}");
        }
        let gw = p("TCPIP1::gw.lab::gpib0,5::INSTR");
        assert_eq!(gw.board, 1);
        assert_eq!(gw.proto, Proto::Vxi11 { port: None, device: "gpib0,5".into() });
        assert_eq!(gw.name(), "TCPIP1::gw.lab::gpib0,5::INSTR");
        // A core port on the host skips the portmapper (pyvisa-py's form).
        let lx = p("TCPIP::127.0.5.3,9010::inst0::INSTR");
        assert_eq!(lx.proto, Proto::Vxi11 { port: Some(9010), device: "inst0".into() });
        assert_eq!(lx.host, "127.0.5.3");
        assert_eq!(lx.name(), "TCPIP0::127.0.5.3,9010::inst0::INSTR");
    }

    #[test]
    fn a_device_name_goes_to_the_instrument_with_a_lowercase_prefix() {
        let v = p("TCPIP0::10.0.0.5::INST0::INSTR");
        assert_eq!(v.proto, Proto::Vxi11 { port: None, device: "inst0".into() });
        assert_eq!(v.name(), "TCPIP0::10.0.0.5::INST0::INSTR", "the name is as written");
        let g = p("TCPIP0::10.0.0.5::GPIB0,5::INSTR");
        assert_eq!(g.proto, Proto::Vxi11 { port: None, device: "gpib0,5".into() });
        let h = p("TCPIP0::10.0.0.5::HISLIP0::INSTR");
        assert_eq!(h.proto, Proto::Hislip { port: 4880, sub: "hislip0".into() });
    }

    #[test]
    fn hislip_and_socket_names() {
        let h = p("TCPIP0::10.0.0.5::hislip0::INSTR");
        assert_eq!(h.proto, Proto::Hislip { port: 4880, sub: "hislip0".into() });
        assert!(h.is_hislip());
        let h2 = p("TCPIP::10.0.0.5::HiSLIP1,4881");
        assert_eq!(h2.proto, Proto::Hislip { port: 4881, sub: "hislip1".into() });
        assert_eq!(h2.name(), "TCPIP0::10.0.0.5::HiSLIP1,4881::INSTR");
        let s = p("TCPIP0::10.0.0.5::5025::SOCKET");
        assert_eq!((s.proto.clone(), s.class()), (Proto::Socket { port: 5025 }, "SOCKET"));
        assert_eq!(p("tcpip3::host::5025::socket").name(), "TCPIP3::host::5025::SOCKET");
    }

    #[test]
    fn gateway_names() {
        assert_eq!(p("TCPIP::gw::gpib0,5::INSTR").gpib(), Some(Gpib { iface: "gpib0".into(), pad: 5, sad: None }));
        // The interface link is made by this name, so it goes as the instrument knows it.
        assert_eq!(p("TCPIP::gw::GPIB1,30,2").gpib(), Some(Gpib { iface: "gpib1".into(), pad: 30, sad: Some(2) }));
        for not in ["TCPIP::gw::gpib0", "TCPIP::gw::gpib0,31", "TCPIP::gw::gpib0,5,31", "TCPIP::gw::gpib0,5,2,1", "TCPIP::gw::gpibx,5",
                    "TCPIP::gw::inst0", "TCPIP::gw::hislip0", "TCPIP::gw::5025::SOCKET", "TCPIP::gw::usb0,5"] {
            assert_eq!(p(not).gpib(), None, "{not}");
        }
    }

    #[test]
    fn ipv6_in_brackets() {
        let t = p("TCPIP::[fe80::1]::inst0::INSTR");
        assert_eq!(t.host, "[fe80::1]");
        assert_eq!(t.name(), "TCPIP0::[fe80::1]::inst0::INSTR");
        assert_eq!(p("TCPIP::[::1]").host, "[::1]");
        assert_eq!(p("TCPIP::[::1]::5025::SOCKET").proto, Proto::Socket { port: 5025 });
        assert_eq!(p("TCPIP::[::1],9010::inst0::INSTR").proto, Proto::Vxi11 { port: Some(9010), device: "inst0".into() });
    }

    #[test]
    fn not_tcpip_names() {
        for bad in [
            "", "TCPIP", "TCPIP::", "TCPIP0::::inst0::INSTR", "TCPIPx::h::INSTR", "GPIB0::20::INSTR",
            "TCPIP::h::SOCKET", "TCPIP::h::0::SOCKET", "TCPIP::h::http::SOCKET", "TCPIP::h::inst0::INTFC",
            "TCPIP::h::a::b::INSTR", "TCPIP::h,1::5025::SOCKET", "TCPIP::h,1::hislip0::INSTR",
            "TCPIP::h,x::inst0::INSTR", "TCPIP::[::1::inst0", "TCPIP::[::1]x::inst0", "TCPIP::h::hislip0,0",
            "TCPIP::h::::INSTR", "TCPIP::a b::inst0",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
