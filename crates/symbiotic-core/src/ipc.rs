//! The wire between a client (the VISA DLL, symbiotic-ctl) and the daemon, over the local channel in local.rs (a named
//! pipe / Unix socket, not the network).
//!
//! One request, one response, at the granularity of a whole VISA operation: a write of any size, a read to its end, are
//! each one exchange, and the daemon holds the bus for all of it. Little endian. Request header 19 bytes: op u8, board
//! u16, pad u8, flags u8, term u8, timeout_ms u32 (backend::TMO_INFINITE: VISA's infinite), count u32, data_len u32,
//! sad u8, then the data. (The relay to another machine can also speak the 17-byte layout with a u16 timeout,
//! `HEADER_U16`.) `sad` is a GPIB instrument's secondary address, 0..=30, or backend::NO_SAD; the instrument operations
//! (PROBE, WRITE, READ, CLEAR, TRIGGER, SPOLL, LOCAL), the locks and SRQ take it. Response header 9 bytes: code u8, ext
//! u32, data_len u32, then the data. `ext` is per operation (see server.rs).
//!
//! `F_USB` in `flags` says the request is for a USBTMC instrument: `board` is then its number (usbtmc.rs) and `pad` is
//! unused. The instrument operations (WRITE, READ, CLEAR, TRIGGER, SPOLL, LOCAL, LLO, REN) and the lock and SRQ ones
//! take it; the bus ones do not.
//!
//! `F_ASRL` says it is for a serial port (serial.rs): `board` is its COM number. PROBE opens the port for this
//! connection (it stays open while a connection that used it is there); WRITE with F_EOI ends with a break of `count`
//! ms; CLEAR sends `*CLS\n` with F_ON and otherwise a break of `count` ms; TRIGGER and SPOLL are `*TRG\n` and `*STB?\n`
//! (the client decides whether the instrument takes those); ASRL_SET, ASRL_GET and FLUSH are the port's own.

use crate::adapter::Error;
use std::io::{self, Read, Write};

/// The version of this protocol, raised with every change to a request or an answer.
pub const VERSION: u32 = 13;

/// The request header's layouts, for the relay (share/peer.rs), which agrees on one with the other
/// machine: the timeout u16 (IPC v10 and before), and u32.
pub const HEADER_U16: u8 = 1;
pub const HEADER_U32: u8 = 2;

/// This build's version, `major.minor.commits` (build.rs): a PING's answer carries it after the IPC version.
pub const BUILD: &str = env!("SYMBIOTIC_BUILD_VERSION");

/// A build version as numbers, for comparing; None for what is not one.
pub fn build_number(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}
const MAX_DATA: usize = 256 << 20;

pub mod op {
    pub const PING: u8 = 0;
    /// The board numbers, one line each (`serial pid kind [absent [forgotten]] [model=<name>]`, tab-separated);
    /// `model=` is what the adapter says it is (a Viper488's IDENTIFY board name).
    pub const LIST: u8 = 1;
    pub const PROBE: u8 = 2;
    pub const WRITE: u8 = 3;
    pub const READ: u8 = 4;
    pub const CLEAR: u8 = 5;
    pub const TRIGGER: u8 = 6;
    pub const SPOLL: u8 = 7;
    pub const LOCAL: u8 = 8;
    pub const LLO: u8 = 9;
    pub const REN: u8 = 10;
    pub const IFC: u8 = 11;
    pub const SPEED: u8 = 12;
    /// The bus lines: `ext` low 16 bits proto::BUS_* (set = asserted), high 16 bits the lines the
    /// adapter cannot tell (a Prologix or AR488 knows SRQ, and an AR488 REN, only).
    pub const LINES: u8 = 13;
    pub const SHUTDOWN: u8 = 14;
    /// Watch SRQ for one instrument. The answer is a subscription id.
    pub const SRQ_SUB: u8 = 15;
    pub const SRQ_UNSUB: u8 = 16;
    /// Wait up to `tmo` ms for a service request on a subscription; `ext` 1 = one arrived (its status byte is the data).
    pub const SRQ_WAIT: u8 = 17;
    /// viLock: `flags` 1 exclusive, 2 shared; `count` the timeout in ms (u32::MAX: forever);
    /// `data` the requested shared key. Answer: `ext` 1 if nested; `data` the access key.
    pub const LOCK: u8 = 18;
    /// viUnlock. Answer: `ext` what the session still holds there (0 none, 1 exclusive, 2 shared).
    pub const UNLOCK: u8 = 19;
    /// VI_ATTR_RSRC_LOCK_STATE. Answer: `ext` 0 none, 1 exclusive, 2 shared.
    pub const LOCK_STATE: u8 = 20;
    /// The interface level (a GPIB INTFC session); `pad` is backend::RAW. CMD: `data` is the
    /// command bytes. RAW_WRITE and RAW_READ are WRITE and READ that address nobody.
    pub const CMD: u8 = 21;
    pub const RAW_WRITE: u8 = 22;
    pub const RAW_READ: u8 = 23;
    /// The USBTMC instruments, one line each in number order (`vid pid serial product [absent]`,
    /// tab-separated; vid and pid in hex); `ext` how many.
    pub const USB_LIST: u8 = 24;
    /// A serial port's setting: `count` is the field (serial::field), `data` the value, u32.
    pub const ASRL_SET: u8 = 25;
    /// A serial port's settings and state: `data` is a serial::Status.
    pub const ASRL_GET: u8 = 26;
    /// A serial port's buffers: `count` is serial::flush's bits, `tmo` the wait for OUT_DRAIN.
    pub const FLUSH: u8 = 27;
    /// A USB control transfer to a USBTMC instrument (viUsbControlIn/Out): `count` is
    /// bmRequestType | bRequest << 8 | wValue << 16, `data` wIndex and wLength (u16 each) and, going
    /// out, the payload. Answer: `data` what came in.
    pub const USB_CTRL: u8 = 28;
    /// The other connections' processes, one line each: a process id, or `?` where the system does not say. `ext` how
    /// many.
    pub const CLIENTS: u8 = 29;
    /// An administrator command for the registry embedded in this daemon (share/admin.rs): `data`
    /// is the command line, the answer's `data` its text; an error answer carries the message.
    pub const SHARE_ADMIN: u8 = 30;
    /// Reach a shared alias another machine owns: `data` is the alias. The daemon connects to the
    /// owner (share/member.rs) and answers with what the owner says the instrument is
    /// (share/peer.rs, `encode_opened`), or C_NOBOARD if there is no such shared alias, its owner
    /// cannot be reached or this machine is not in a group. From then on THIS CONNECTION is the
    /// tunnel: each request is the instrument session's, and is carried to the owner, who binds it
    /// to the instrument; each answer comes back. The connection is the session's, and the
    /// owner's locks and subscriptions end with it.
    pub const PEER_CONNECT: u8 = 31;
    /// Fetch the group's directory from the registry if the cached one is more than 10 seconds
    /// old (at most one try in 10 seconds, whether it works or not), so that the library, which
    /// reads the cache and never talks to the registry, sees what the group shares. Nothing to
    /// fetch from (no group) is not an error.
    pub const SHARE_REFRESH: u8 = 32;
    /// Which addresses (1 to 30) of a GPIB board have a listener, by the daemon's own scan. `board`; `count` the oldest
    /// a kept result may be, seconds; `data` the asker's process id (u32, so its own connections do not count as other
    /// programs using the service). Answer `ext` 0 scanned now, 1 kept (from the daemon's periodic scan or an earlier
    /// ask); `data` the result's age in seconds (u32) then the addresses that answered, one byte each. C_BUSY: another
    /// program is connected (or holds the adapter), so nothing is probed. With `F_UNITS` in `flags`, `ext` has 2 added
    /// and each address is followed by what the daemon knows it to be: u16 length (0xFFFF: not known) and its `*IDN?`
    /// answer, as the sharing scan learned it (server.rs, `learn_units`).
    pub const BUS_SCAN: u8 = 33;
    /// Forget `board`, an adapter not attached (boards.rs): its LIST line says `forgotten` after `absent`, and the next new adapter
    /// takes its number. C_GONE: the adapter is attached; C_NOBOARD: no such number.
    pub const FORGET: u8 = 34;
}

/// What the daemon and the sharing relay need to know about an operation, kept in one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Props {
    /// Acts on a session's instrument (or its interface): refused while another session holds the lock.
    pub locked: bool,
    /// Belongs to an instrument session, so a shared alias may carry it (`share::peer::allowed` adds
    /// what depends on the instrument).
    pub relayed: bool,
    /// Watched by the watchdog while it runs; one that waits for as long as it is asked to is not.
    pub watched: bool,
}

/// The properties of operation `o`; None for a number that is no operation.
pub fn props(o: u8) -> Option<Props> {
    use op::*;
    let p = |locked, relayed| Some(Props { locked, relayed, watched: true });
    match o {
        WRITE | READ | CLEAR | TRIGGER | SPOLL | LOCAL | LLO | REN | ASRL_SET | FLUSH | USB_CTRL => p(true, true),
        IFC | CMD | RAW_WRITE | RAW_READ => p(true, false),
        PING | PROBE | SRQ_SUB | SRQ_UNSUB | UNLOCK | LOCK_STATE | ASRL_GET => p(false, true),
        SRQ_WAIT | LOCK => Some(Props { locked: false, relayed: true, watched: false }),
        LIST | SPEED | LINES | SHUTDOWN | USB_LIST | CLIENTS | SHARE_ADMIN | PEER_CONNECT | SHARE_REFRESH | BUS_SCAN | FORGET => p(false, false),
        _ => None,
    }
}

/// Whether operation `o` is refused while another session holds the lock on its target.
pub fn locked(o: u8) -> bool {
    props(o).is_some_and(|p| p.locked)
}

pub const F_EOI: u8 = 1;
pub const F_TERM: u8 = 2;
pub const F_ON: u8 = 4;
/// `board` is a USBTMC instrument's number, not a GPIB board.
pub const F_USB: u8 = 0x80;
/// `board` is a serial port's COM number.
pub const F_ASRL: u8 = 0x40;
/// WRITE: only if no other program's connection used this instrument in the last `QUIET_FOR`, else C_BUSY. Used for our
/// own `*IDN?`, which could make an instrument drop an answer another program has not read yet (identity.rs).
pub const F_IF_QUIET: u8 = 0x20;
/// BUS_SCAN: with what is known of each address.
pub const F_UNITS: u8 = 0x08;
/// LIST: bind this connection to the listed board numbers: a later request for a number that has gone to another
/// adapter since is `Gone` (boards.rs).
pub const F_BIND: u8 = 0x10;
/// How long after another connection used an instrument a WRITE with `F_IF_QUIET` is refused.
pub const QUIET_FOR: std::time::Duration = std::time::Duration::from_secs(30);

pub const C_OK: u8 = 0;
pub const C_TIMEOUT: u8 = 1;
pub const C_NODEVICE: u8 = 2;
pub const C_BUSY: u8 = 3;
pub const C_GONE: u8 = 4;
pub const C_STATUS: u8 = 5;
pub const C_IO: u8 = 6;
pub const C_NOBOARD: u8 = 7;
pub const C_LOCKED: u8 = 8;
pub const C_NOT_LOCKED: u8 = 9;
pub const C_LOCK_KEY: u8 = 10;

#[derive(Debug, Clone)]
pub struct Req {
    pub op: u8,
    pub board: u16,
    pub pad: u8,
    pub sad: u8,
    pub flags: u8,
    pub term: u8,
    pub tmo: u32,
    pub count: u32,
    pub data: Vec<u8>,
}

impl Default for Req {
    fn default() -> Req {
        Req { op: 0, board: 0, pad: 0, sad: crate::backend::NO_SAD, flags: 0, term: 0, tmo: 0, count: 0, data: Vec::new() }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Resp {
    pub code: u8,
    pub ext: u32,
    pub data: Vec<u8>,
}

impl Resp {
    pub fn ok(ext: u32, data: Vec<u8>) -> Resp {
        Resp { code: C_OK, ext, data }
    }

    pub fn err(e: &Error, ext: u32) -> Resp {
        let (code, data) = match e {
            Error::Timeout => (C_TIMEOUT, vec![]),
            Error::NoDevice => (C_NODEVICE, vec![]),
            Error::Busy => (C_BUSY, vec![]),
            Error::Gone => (C_GONE, vec![]),
            Error::NoBoard => (C_NOBOARD, vec![]),
            Error::Status(s) => (C_STATUS, vec![*s]),
            Error::Io(m) => (C_IO, m.clone().into_bytes()),
            Error::Locked => (C_LOCKED, vec![]),
            Error::NotLocked => (C_NOT_LOCKED, vec![]),
            Error::LockKey => (C_LOCK_KEY, vec![]),
            Error::NotSent => (C_GONE, vec![]), // a client's own; never on the wire
        };
        Resp { code, ext, data }
    }

    /// The error a non-OK response stands for.
    pub fn error(&self) -> Option<Error> {
        Some(match self.code {
            C_OK => return None,
            C_TIMEOUT => Error::Timeout,
            C_NODEVICE => Error::NoDevice,
            C_BUSY => Error::Busy,
            C_GONE => Error::Gone,
            C_NOBOARD => Error::NoBoard,
            C_STATUS => Error::Status(self.data.first().copied().unwrap_or(0xFF)),
            C_LOCKED => Error::Locked,
            C_NOT_LOCKED => Error::NotLocked,
            C_LOCK_KEY => Error::LockKey,
            _ => Error::Io(String::from_utf8_lossy(&self.data).into_owned()),
        })
    }
}

fn read_data(r: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    if len > MAX_DATA {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

impl Req {
    /// The instrument a request names.
    pub fn target(&self) -> crate::locks::Target {
        if self.flags & F_USB != 0 {
            crate::locks::Target::Usb(self.board)
        } else if self.flags & F_ASRL != 0 {
            crate::locks::Target::Asrl(self.board)
        } else {
            crate::locks::Target::Gpib(self.board, self.pad, self.sad)
        }
    }

    /// Make this request name `t` (board, addresses and the USB and serial flags).
    pub fn set_target(&mut self, t: crate::locks::Target) {
        use crate::locks::Target;
        self.flags &= !(F_USB | F_ASRL);
        match t {
            Target::Gpib(b, p, s) => (self.board, self.pad, self.sad) = (b, p, s),
            Target::Usb(n) => (self.board, self.flags) = (n, self.flags | F_USB),
            Target::Asrl(n) => (self.board, self.flags) = (n, self.flags | F_ASRL),
        }
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        self.write_as(w, HEADER_U32)
    }

    /// In the header layout `header` (HEADER_U16 cuts the timeout as `backend::u16_ms` does).
    pub fn write_as(&self, w: &mut impl Write, header: u8) -> io::Result<()> {
        let mut b = Vec::with_capacity(19 + self.data.len());
        b.push(self.op);
        b.extend_from_slice(&self.board.to_le_bytes());
        b.extend_from_slice(&[self.pad, self.flags, self.term]);
        if header == HEADER_U16 {
            b.extend_from_slice(&crate::backend::u16_ms(self.tmo).to_le_bytes());
        } else {
            b.extend_from_slice(&self.tmo.to_le_bytes());
        }
        b.extend_from_slice(&self.count.to_le_bytes());
        b.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        b.push(self.sad);
        b.extend_from_slice(&self.data);
        w.write_all(&b)
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<Req> {
        Req::read_as(r, HEADER_U32)
    }

    /// In the header layout `header`.
    pub fn read_as(r: &mut impl Read, header: u8) -> io::Result<Req> {
        // The u16 layout is read into the u32 one's places, the timeout's upper half zero.
        let mut h = [0u8; 19];
        if header == HEADER_U16 {
            r.read_exact(&mut h[..17])?;
            h.copy_within(8..17, 10);
            h[8..10].fill(0);
        } else {
            r.read_exact(&mut h)?;
        }
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let tmo = if header == HEADER_U16 { crate::backend::from_u16_ms(u32_at(6) as u16) } else { u32_at(6) };
        Ok(Req {
            op: h[0],
            board: u16::from_le_bytes([h[1], h[2]]),
            pad: h[3],
            sad: h[18],
            flags: h[4],
            term: h[5],
            tmo,
            count: u32_at(10),
            data: read_data(r, u32_at(14) as usize)?,
        })
    }
}

impl Resp {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut b = Vec::with_capacity(9 + self.data.len());
        b.push(self.code);
        b.extend_from_slice(&self.ext.to_le_bytes());
        b.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.data);
        w.write_all(&b)
    }

    pub fn read_from(r: &mut impl Read) -> io::Result<Resp> {
        let mut h = [0u8; 9];
        r.read_exact(&mut h)?;
        let len = u32::from_le_bytes([h[5], h[6], h[7], h[8]]) as usize;
        Ok(Resp { code: h[0], ext: u32::from_le_bytes([h[1], h[2], h[3], h[4]]), data: read_data(r, len)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_in_the_table() {
        for o in 0..=op::FORGET {
            assert!(props(o).is_some(), "operation {o} has no properties");
        }
        assert_eq!(props(op::FORGET + 1), None, "a new operation goes in the table (and this test moves up)");
        assert!(locked(op::WRITE) && locked(op::FLUSH) && locked(op::IFC) && !locked(op::PROBE) && !locked(op::LOCK));
        assert!(!props(op::LOCK).unwrap().watched && !props(op::SRQ_WAIT).unwrap().watched && props(op::READ).unwrap().watched);
    }

    #[test]
    fn request_and_response_round_trip() {
        let q = Req { op: op::WRITE, board: 3, pad: 20, sad: 4, flags: F_EOI, term: 10, tmo: 120_000, count: 7, data: b"*IDN?\n".to_vec() };
        let mut b = Vec::new();
        q.write_to(&mut b).unwrap();
        assert_eq!(b.len(), 19 + 6);
        let q2 = Req::read_from(&mut &b[..]).unwrap();
        assert_eq!((q2.op, q2.board, q2.pad, q2.sad, q2.flags, q2.term, q2.tmo, q2.count, q2.data), (op::WRITE, 3, 20, 4, F_EOI, 10, 120_000, 7, b"*IDN?\n".to_vec()));
        assert_eq!(Req::default().sad, crate::backend::NO_SAD);
        let r = Resp::err(&Error::Status(5), 9);
        let mut b = Vec::new();
        r.write_to(&mut b).unwrap();
        let r2 = Resp::read_from(&mut &b[..]).unwrap();
        assert_eq!((r2.ext, r2.error()), (9, Some(Error::Status(5))));
    }

    #[test]
    fn the_u16_header_is_v10s() {
        use crate::backend::TMO_INFINITE;
        let q = Req { op: op::READ, board: 2, pad: 9, sad: 1, flags: F_EOI, term: 13, tmo: 5000, count: 77, data: b"ab".to_vec() };
        let mut b = Vec::new();
        q.write_as(&mut b, HEADER_U16).unwrap();
        // As an IPC v10 client wrote it: op, board, pad, flags, term, timeout u16, count, length, sad.
        let mut v10 = vec![op::READ, 2, 0, 9, F_EOI, 13];
        v10.extend_from_slice(&5000u16.to_le_bytes());
        v10.extend_from_slice(&77u32.to_le_bytes());
        v10.extend_from_slice(&2u32.to_le_bytes());
        v10.push(1);
        v10.extend_from_slice(b"ab");
        assert_eq!(b, v10);
        let q2 = Req::read_as(&mut &b[..], HEADER_U16).unwrap();
        assert_eq!((q2.op, q2.board, q2.pad, q2.sad, q2.flags, q2.term, q2.tmo, q2.count, q2.data), (op::READ, 2, 9, 1, F_EOI, 13, 5000, 77, b"ab".to_vec()));
        for (tmo, cut) in [(TMO_INFINITE, TMO_INFINITE), (120_000, 65_534), (65_535, 65_534), (0, 0)] {
            let mut b = Vec::new();
            Req { tmo, ..Default::default() }.write_as(&mut b, HEADER_U16).unwrap();
            assert_eq!(Req::read_as(&mut &b[..], HEADER_U16).unwrap().tmo, cut, "{tmo}: infinite stays infinite, a finite one is never made it");
        }
    }

    #[test]
    fn every_error_survives_the_wire() {
        for e in [
            Error::Timeout, Error::NoDevice, Error::Busy, Error::Gone, Error::NoBoard, Error::Status(4), Error::Io("x".into()),
            Error::Locked, Error::NotLocked, Error::LockKey,
        ] {
            assert_eq!(Resp::err(&e, 0).error(), Some(e));
        }
        assert_eq!(Resp::ok(1, vec![]).error(), None);
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut h = [0u8; 19];
        h[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Req::read_from(&mut &h[..]).is_err());
    }
}
