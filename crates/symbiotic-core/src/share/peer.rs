//! One shared instrument over an encrypted channel. The first message names the alias; if the owner serves it the
//! connection is bound to that instrument for its life, and each later message is a request in the daemon's own wire
//! (`ipc.rs`) whose instrument fields the owner replaces with the bound one. A connection is one client of the owner's
//! daemon: its locks and subscriptions go when it does.
//!
//! The request header's layout is agreed on at the open, so machines of different builds still reach each other: the
//! user's open says the newest it reads (`ipc::HEADER_U32`), and the owner's answer which one the connection uses. A
//! member of an earlier build says neither, ignores what it does not read, and gets the u16 layout (a longer timeout is
//! cut to 65.5 s for it).

use super::chan::Channel;
use super::codec::{R, W};
use crate::ipc::{HEADER_U16, HEADER_U32, Req, Resp, op};
use crate::locks::Target;
use std::io::{self, Read, Write};

/// What the daemon does for a peer connection (the daemon implements it; tests have a fake).
pub trait PeerHost: Send + Sync {
    /// A peer connection began, from the machine `who` describes; its identity in the daemon.
    fn begin(&self, who: &str) -> u64;
    fn call(&self, me: u64, q: Req) -> Resp;
    /// It ended: drop its locks, subscriptions and ports.
    fn end(&self, me: u64);
}

/// What the owner tells the user about the instrument, for the attributes (the interface and
/// addresses are the owner's, so every machine reads the same).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub target: Target,
    /// The owner's canonical resource name for it.
    pub resource: String,
    /// A USBTMC instrument's descriptors, which only the owner can read.
    pub usb: Option<UsbInfo>,
    /// The request header's layout on this connection (`ipc::HEADER_U16`, `HEADER_U32`).
    pub header: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbInfo {
    pub iface: u8,
    pub usb488: bool,
    pub manufacturer: String,
    pub product: String,
}

/// The operations that belong to an instrument session on `target`. The interface-level ones (`CMD`, `RAW_*`,
/// `IFC`, `LINES`), the daemon's own (`SPEED`, `SHUTDOWN`, `CLIENTS`, the lists) are not offered; nor are a GPIB
/// instrument's `LLO` and `REN`, which reach every instrument on its board, not just the one shared.
pub fn allowed(o: u8, target: Target) -> bool {
    if matches!(o, op::LLO | op::REN) && matches!(target, Target::Gpib(..)) {
        return false;
    }
    crate::ipc::props(o).is_some_and(|p| p.relayed)
}

fn encode_open(a: &str) -> Vec<u8> {
    let mut w = W::default();
    w.u8(1).str(a).u8(HEADER_U32);
    w.0
}

pub fn encode_opened(o: &Opened) -> Vec<u8> {
    let mut w = W::default();
    w.u8(0);
    match o.target {
        Target::Gpib(b, p, s) => w.u8(0).u32(b as u32).u32(p as u32).u32(s as u32),
        Target::Usb(n) => w.u8(1).u32(n as u32).u32(0).u32(0),
        Target::Asrl(n) => w.u8(2).u32(n as u32).u32(0).u32(0),
    };
    w.str(&o.resource);
    match &o.usb {
        Some(u) => w.u8(1).u8(u.iface).u8(u.usb488 as u8).str(&u.manufacturer).str(&u.product),
        None => w.u8(0),
    };
    w.u8(o.header);
    w.0
}

pub fn decode_opened(b: &[u8]) -> Result<Opened, String> {
    let mut r = R::new(b);
    match r.u8().map_err(String::from)? {
        0 => {
            let k = r.u8().map_err(String::from)?;
            let (a, b2, c) = (r.u32().map_err(String::from)?, r.u32().map_err(String::from)?, r.u32().map_err(String::from)?);
            let target = match k {
                0 => Target::Gpib(a as u16, b2 as u8, c as u8),
                1 => Target::Usb(a as u16),
                2 => Target::Asrl(a as u16),
                _ => return Err("unknown instrument kind".into()),
            };
            let resource = r.str().map_err(String::from)?;
            // What follows the name is optional: an owner of an earlier build sends nothing.
            let usb = match r.u8() {
                Ok(1) => {
                    let (iface, usb488) = (r.u8().map_err(String::from)?, r.u8().map_err(String::from)? != 0);
                    Some(UsbInfo { iface, usb488, manufacturer: r.str().map_err(String::from)?, product: r.str().map_err(String::from)? })
                }
                _ => None,
            };
            // So is the header layout: an owner that does not say it reads the u16 one.
            let header = match r.u8() {
                Ok(HEADER_U32) => HEADER_U32,
                _ => HEADER_U16,
            };
            Ok(Opened { target, resource, usb, header })
        }
        _ => Err(r.str().map_err(String::from)?),
    }
}

/// What one session through a shared alias did, as its owner saw it (share/usage.rs keeps these).
#[derive(Clone, Debug, PartialEq)]
pub struct Used {
    pub alias: String,
    /// When it was opened, Unix seconds.
    pub began: u64,
    pub secs: f64,
    pub requests: u64,
    /// Requests for operations not offered over an alias.
    pub refused: u64,
}

/// Owner side. `open` says whether `alias` is served and what it is; a refusal's text is sent as
/// it is, and says nothing about whether the alias exists elsewhere. `used` hears of a session that
/// was opened when it ends, however it ends.
pub fn serve<S: Read + Write>(
    ch: &mut Channel<S>,
    host: &dyn PeerHost,
    who: &str,
    open: &dyn Fn(&str) -> Result<Opened, String>,
    used: &dyn Fn(&Used),
) -> io::Result<()> {
    let m = ch.recv()?;
    let mut r = R::new(&m);
    let alias = match (r.u8(), r.str()) {
        (Ok(1), Ok(a)) => a,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "expected an open request")),
    };
    // A user of an earlier build says no layout, and writes the u16 one.
    let header = if matches!(r.u8(), Ok(HEADER_U32)) { HEADER_U32 } else { HEADER_U16 };
    let opened = match open(&alias) {
        Ok(o) => Opened { header, ..o },
        Err(why) => {
            let mut w = W::default();
            w.u8(1).str(&why);
            ch.send(&w.0)?;
            return Ok(());
        }
    };
    ch.send(&encode_opened(&opened))?;
    let began = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t0 = std::time::Instant::now();
    let (mut requests, mut refused) = (0u64, 0u64);
    let who = format!("{who}, alias {alias}");
    let r = relay(ch, host, &who, &opened, &mut requests, &mut refused);
    used(&Used { alias, began, secs: t0.elapsed().as_secs_f64(), requests, refused });
    r
}

/// The requests of one opened session, until it ends.
fn relay<S: Read + Write>(
    ch: &mut Channel<S>,
    host: &dyn PeerHost,
    who: &str,
    opened: &Opened,
    requests: &mut u64,
    refused: &mut u64,
) -> io::Result<()> {
    let me = host.begin(who);
    struct End<'a>(&'a dyn PeerHost, u64);
    impl Drop for End<'_> {
        fn drop(&mut self) {
            self.0.end(self.1);
        }
    }
    let _end = End(host, me);
    loop {
        let m = match ch.recv() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut q = Req::read_as(&mut &m[..], opened.header)?;
        *requests += 1;
        let resp = if allowed(q.op, opened.target) {
            q.set_target(opened.target);
            host.call(me, q)
        } else {
            *refused += 1;
            Resp { code: crate::ipc::C_IO, ext: 0, data: format!("operation {} is not available over a shared alias", q.op).into_bytes() }
        };
        let mut b = Vec::new();
        resp.write_to(&mut b)?;
        ch.send(&b)?;
    }
}

/// User side: ask the owner for `alias`.
pub fn open<S: Read + Write>(ch: &mut Channel<S>, alias: &str) -> io::Result<Result<Opened, io::Error>> {
    ch.send(&encode_open(alias))?;
    Ok(decode_opened(&ch.recv()?).map_err(|m| io::Error::new(io::ErrorKind::NotFound, m)))
}

/// User side: one request and its answer, in the header layout the open agreed on (`Opened::header`).
pub fn call<S: Read + Write>(ch: &mut Channel<S>, q: &Req, header: u8) -> io::Result<Resp> {
    let mut b = Vec::new();
    q.write_as(&mut b, header)?;
    ch.send(&b)?;
    Resp::read_from(&mut &ch.recv()?[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_reaches_the_whole_board_is_not_relayed() {
        let (gpib, usb) = (Target::Gpib(0, 6, 0), Target::Usb(0));
        for o in [op::LLO, op::REN] {
            assert!(!allowed(o, gpib), "op {o} on a GPIB instrument reaches its whole board");
            assert!(allowed(o, usb), "op {o} on a USBTMC instrument is that instrument's");
        }
        assert!(allowed(op::WRITE, gpib) && allowed(op::LOCAL, gpib));
        assert!(!allowed(op::IFC, gpib) && !allowed(op::CMD, usb));
    }

    #[test]
    fn what_the_owner_says_survives_the_wire() {
        for o in [
            Opened { target: Target::Gpib(1, 6, 255), resource: "GPIB1::6::INSTR".into(), usb: None, header: HEADER_U32 },
            Opened { target: Target::Gpib(1, 6, 255), resource: "GPIB1::6::INSTR".into(), usb: None, header: HEADER_U16 },
            Opened {
                target: Target::Usb(0),
                resource: "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR".into(),
                usb: Some(UsbInfo { iface: 0, usb488: true, manufacturer: "Siglent".into(), product: "SDG1022X Plus".into() }),
                header: HEADER_U32,
            },
        ] {
            assert_eq!(decode_opened(&encode_opened(&o)).unwrap(), o);
        }
        // Owners of earlier builds stop after the USB part, or after the resource name.
        let mut old = encode_opened(&Opened { target: Target::Asrl(6), resource: "ASRL6::INSTR".into(), usb: None, header: HEADER_U32 });
        old.pop();
        assert_eq!((decode_opened(&old).unwrap().usb, decode_opened(&old).unwrap().header), (None, HEADER_U16));
        old.pop();
        assert_eq!((decode_opened(&old).unwrap().usb, decode_opened(&old).unwrap().header), (None, HEADER_U16));
        assert!(decode_opened(&[1, 3, 0, 0, 0, b'n', b'o', b'!']).unwrap_err() == "no!");
    }

    /// What the owner's daemon was asked: the timeouts.
    #[derive(Default)]
    struct Seen(std::sync::Mutex<Vec<u32>>);
    impl PeerHost for Seen {
        fn begin(&self, _: &str) -> u64 {
            1
        }
        fn call(&self, _: u64, q: Req) -> Resp {
            self.0.lock().unwrap().push(q.tmo);
            Resp::ok(0, q.data)
        }
        fn end(&self, _: u64) {}
    }

    /// An owner serving one connection; the user's end of it, and what the owner's daemon saw.
    fn owner() -> (super::super::chan::Channel<impl Read + Write>, std::thread::JoinHandle<(Vec<u32>, Vec<Used>)>) {
        use super::super::chan;
        let (a, b) = chan::pipe_pair();
        let (ku, ko) = (chan::generate().unwrap(), chan::generate().unwrap());
        let ko2 = ko.clone();
        let h = std::thread::spawn(move || {
            let mut c = chan::accept(b, &ko2, |_| true).unwrap();
            let host = Seen::default();
            let open = |_: &str| Ok(Opened { target: Target::Gpib(0, 15, 255), resource: "GPIB0::15::INSTR".into(), usb: None, header: HEADER_U32 });
            let used = std::sync::Mutex::new(Vec::new());
            serve(&mut c, &host, "member test", &open, &|u| used.lock().unwrap().push(u.clone())).unwrap();
            (host.0.into_inner().unwrap(), used.into_inner().unwrap())
        });
        (chan::connect(a, &ku, &ko.public).unwrap(), h)
    }

    #[test]
    fn a_timeout_crosses_whole_and_a_user_of_an_earlier_build_still_gets_through() {
        use crate::backend::TMO_INFINITE;
        let (mut ch, h) = owner();
        let o = open(&mut ch, "DMM").unwrap().unwrap();
        assert_eq!(o.header, HEADER_U32);
        for tmo in [120_000, TMO_INFINITE] {
            call(&mut ch, &Req { op: op::READ, tmo, ..Default::default() }, o.header).unwrap();
        }
        drop(ch);
        let (seen, used) = h.join().unwrap();
        assert_eq!(seen, [120_000, TMO_INFINITE]);
        assert_eq!((used.len(), used[0].alias.as_str(), used[0].requests, used[0].refused), (1, "DMM", 2, 0), "one session, its requests");

        // An earlier build's open names the alias and nothing more, and its requests have the u16 header.
        let (mut ch, h) = owner();
        let mut w = W::default();
        w.u8(1).str("DMM");
        ch.send(&w.0).unwrap();
        let o = decode_opened(&ch.recv().unwrap()).unwrap();
        assert_eq!(o.header, HEADER_U16);
        for tmo in [5000, TMO_INFINITE, 120_000] {
            let r = call(&mut ch, &Req { op: op::READ, tmo, data: b"x".to_vec(), ..Default::default() }, HEADER_U16).unwrap();
            assert_eq!(r.data, b"x");
        }
        drop(ch);
        assert_eq!(h.join().unwrap().0, [5000, TMO_INFINITE, 65_534], "infinite stays infinite; a longer finite one is cut");
    }
}
