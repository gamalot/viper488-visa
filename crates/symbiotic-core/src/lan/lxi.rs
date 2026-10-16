//! An LXI instrument's identification page, `GET /lxi/identification` on port 80 (LXI Device Specification): who it is,
//! by an HTTP request its SCPI parser never sees. Where the neighbor table cannot tell a LAN instrument by its MAC
//! (across a router, an IPv6 address), this is how it is told without a command to it. Instruments older than the page
//! answer 404.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// What the page says, the parts that tell one instrument from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identification {
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub mac: Option<crate::neighbor::Mac>,
}

/// The first `<Tag>value</Tag>` (with or without a namespace prefix) in `xml`.
fn field(xml: &str, tag: &str) -> Option<String> {
    let mut at = 0;
    while let Some(i) = xml[at..].find('<') {
        let start = at + i + 1;
        let end = start + xml[start..].find('>')?;
        let name = &xml[start..end];
        let bare = name.rsplit(':').next().unwrap_or(name);
        if bare == tag && !name.starts_with('/') && !name.ends_with('/') {
            let rest = &xml[end + 1..];
            let close = rest.find('<')?;
            return Some(rest[..close].trim().to_string());
        }
        at = end + 1;
    }
    None
}

pub fn parse(xml: &str) -> Option<Identification> {
    let manufacturer = field(xml, "Manufacturer").filter(|s| !s.is_empty())?;
    let model = field(xml, "Model").filter(|s| !s.is_empty())?;
    Some(Identification { manufacturer, model, serial: field(xml, "SerialNumber").unwrap_or_default(), mac: field(xml, "MACAddress").and_then(|m| crate::neighbor::parse(&m)) })
}

/// The identification page of `host`, within about `within`; None when there is none (no web server, not LXI, 404).
pub fn identification(host: &str, within: Duration) -> Option<Identification> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addr = (bare, 80).to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&addr, within).ok()?;
    s.set_read_timeout(Some(within)).ok()?;
    s.set_write_timeout(Some(within)).ok()?;
    s.write_all(format!("GET /lxi/identification HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut body = Vec::new();
    let _ = s.take(256 * 1024).read_to_end(&mut body);
    let text = String::from_utf8_lossy(&body);
    let (head, xml) = text.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/") || head.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    parse(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parts_that_tell_an_instrument_are_read_with_or_without_a_namespace() {
        let ks = r#"<?xml version="1.0"?><LXIDevice xmlns="http://www.lxistandard.org/InstrumentIdentification/1.0"><Manufacturer>Keysight</Manufacturer>
            <Model>34465A</Model><SerialNumber>MY20000001</SerialNumber><Interface><MACAddress>02-00-00-00-00-66</MACAddress></Interface></LXIDevice>"#;
        let i = parse(ks).unwrap();
        assert_eq!((i.manufacturer.as_str(), i.model.as_str(), i.serial.as_str()), ("Keysight", "34465A", "MY20000001"));
        assert_eq!(i.mac, Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]));
        let sdg = "<lxi:LXIDevice><lxi:Manufacturer>Siglent</lxi:Manufacturer><lxi:Model>SDG1062X Plus</lxi:Model>\
                   <lxi:SerialNumber>SDG1EXAMPLE001</lxi:SerialNumber><lxi:MACAddress>02:00:00:00:00:62</lxi:MACAddress></lxi:LXIDevice>";
        assert_eq!(parse(sdg).unwrap().mac, Some([0x02, 0x00, 0x00, 0x00, 0x00, 0x62]));
        assert_eq!(parse("<html>404</html>"), None);
    }
}
