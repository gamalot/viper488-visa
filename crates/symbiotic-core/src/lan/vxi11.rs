//! VXI-11 (TCP/IP Instrument Protocol), the client: one link on the device's core channel.
//!
//! The procedures and argument layouts are VXI-11's (rev 1.0, B.6); pyvisa-py's `protocols/vxi11.py` was a reference
//! for how clients use them. Two rules:
//!
//! - The socket waits a little longer than the timeout the device was given (`MARGIN`), so the
//!   device's own io_timeout (error 15), with the bytes it had, is the ordinary timeout. The socket
//!   running out is a timeout too, and the connection stays: replies are matched to calls by xid,
//!   so the late one is dropped when it comes (`rpc::Rpc::call`).
//! - A link is not a probe: through a LAN/GPIB gateway, create_link succeeds for an address with
//!   nothing on it. Only VXI-11's own errors say anything is missing.
//!
//! Service requests come the other way: the device calls `device_intr_srq` on an RPC server this end runs (the
//! interrupt channel). `watch_srq` opens it -- a listening port, the only one in the DLL, bound to the address the core
//! channel goes out from and open only while it is being watched -- and `unwatch_srq` closes it.

use super::rpc::{self, Rpc};
use super::xdr::{Decoder, Encoder};
use super::{ms, Deadline, Error, Ren};
use crate::adapter::{ReadEnd, ReadResult};
use std::io::Read;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const CORE_PROG: u32 = 0x0607AF;
const CORE_VERS: u32 = 1;

const CREATE_LINK: u32 = 10;
const DEVICE_WRITE: u32 = 11;
const DEVICE_READ: u32 = 12;
const DEVICE_READSTB: u32 = 13;
const DEVICE_TRIGGER: u32 = 14;
const DEVICE_CLEAR: u32 = 15;
const DEVICE_REMOTE: u32 = 16;
const DEVICE_LOCAL: u32 = 17;
const DEVICE_LOCK: u32 = 18;
const DEVICE_UNLOCK: u32 = 19;
const DEVICE_ENABLE_SRQ: u32 = 20;
const DESTROY_LINK: u32 = 23;
const DEVICE_DOCMD: u32 = 22;
const CREATE_INTR_CHAN: u32 = 25;
const DESTROY_INTR_CHAN: u32 = 26;

const INTR_PROG: u32 = 0x0607B1;
const INTR_VERS: u32 = 1;
const INTR_SRQ: u32 = 30;
/// create_intr_chan's address family: TCP.
const DEVICE_TCP: u32 = 0;
// device_docmd commands of a LAN/GPIB gateway's interface link (VXI-11.2; checked against
// python-vxi11). Values go in network order: REN and bus status take two bytes.
const CMD_SEND_COMMAND: u32 = 0x020000;
const CMD_BUS_STATUS: u32 = 0x020001;
const CMD_ATN_CTRL: u32 = 0x020002;
const CMD_REN_CTRL: u32 = 0x020003;
const CMD_IFC_CTRL: u32 = 0x020010;
const BUS_STATUS_REN: u16 = 1;
const BUS_STATUS_NDAC: u16 = 3;
const BUS_STATUS_ADDRESS: u16 = 8;
/// GPIB universal command: local lockout.
const GPIB_LLO: u8 = 0x11;
/// GPIB universal command: unlisten; and the listen address group's base (LAD n = 0x20 + n).
const GPIB_UNL: u8 = 0x3F;
const GPIB_LAD: u8 = 0x20;

/// What device_enable_srq registers; the device sends it back with each request (40 bytes at most).
const SRQ_HANDLE: &[u8] = b"symbiotic";

const FLAG_WAITLOCK: u32 = 1;
const FLAG_END: u32 = 8;
const FLAG_TERMCHRSET: u32 = 128;

const REASON_REQCNT: u32 = 1;
const REASON_CHR: u32 = 2;
const REASON_END: u32 = 4;

/// How much longer the socket waits than the device was told to: time for the device's own
/// timeout reply to arrive.
const MARGIN: Duration = Duration::from_secs(1);

/// How long the socket waits for a call that carries no timeout of its own.
const QUICK: Duration = Duration::from_secs(5);

/// The most one device_read asks for.
pub const READ_CHUNK: usize = 1 << 20;

/// VXI-11 does not define an io_timeout of 0, so the least given is this (pyvisa-py does the same).
const MIN_IO_TIMEOUT: Duration = Duration::from_millis(10);

/// A Device_ErrorCode as an error (VXI-11 B.5.2 table; the mapping follows pyvisa-py's where it
/// is sure of it).
fn device_error(code: u32, what: &str) -> Error {
    match code {
        3 => Error::NotFound(format!("{what}: the device is not accessible")),
        5 => Error::Param,
        8 => Error::Unsupported,
        11 => Error::Locked,
        12 => Error::NotLocked,
        15 => Error::Timeout,
        23 => Error::Abort,
        // 4 (invalid link), 6 and 29 (interrupt channel), 9 (out of resources), 17 (I/O error),
        // 21 (invalid address) and whatever else: worth seeing with the number.
        n => Error::Io(format!("{what}: the device returned VXI-11 error {n}")),
    }
}

fn truncated() -> Error {
    Error::Lost("a truncated VXI-11 reply".into())
}

/// The io_timeout to give the device, and how long the socket waits for its answer.
fn io_times(dl: &Deadline) -> (u32, Option<Duration>) {
    match dl.left() {
        None => (u32::MAX, None),
        Some(l) => {
            let l = l.max(MIN_IO_TIMEOUT);
            (ms(Some(l)), Some(l + MARGIN))
        }
    }
}

/// What asking a host whether it is a LAN/GPIB gateway came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A gateway, with its own bus address.
    Gateway(u8),
    /// Answered, and is not one: worth remembering, asking again would only cost it a connection.
    Instrument,
    /// Did not answer (unreachable, a timeout, a dropped connection): no conclusion.
    Unknown,
}

/// Is `host` a LAN/GPIB gateway? Two answers make one: the interface link `iface` answers bus status (an interface
/// link's docmd), and the host refuses a link to `inst0` -- a gateway has only `gpib0,N` devices, an instrument has
/// `inst0`. Neither alone will do: some instruments answer bus status on `gpib0` and are not gateways; they have
/// `inst0`. Reads state; nothing happens on the bus.
pub fn judge_host(host: &str, iface: &str, within: Duration) -> Verdict {
    judge_at(host, None, iface, within)
}

fn judge_at(host: &str, port: Option<u16>, iface: &str, within: Duration) -> Verdict {
    // A refused create_link is the host's answer; anything else (connection, timeout) is none.
    let answered = |e: &Error| matches!(e, Error::NotFound(s) if s.contains("creating a link to"));
    let mut l = match Link::open(host, port, iface, within, None) {
        Ok(l) => l,
        Err(e) if answered(&e) => return Verdict::Instrument,
        Err(_) => return Verdict::Unknown,
    };
    let a = match l.bus_status(BUS_STATUS_ADDRESS, Deadline::after(Some(within))) {
        Ok(a) => a,
        Err(Error::Unsupported | Error::Param) => return Verdict::Instrument,
        Err(_) => return Verdict::Unknown,
    };
    let Some(a) = u8::try_from(a).ok().filter(|a| *a <= 30) else { return Verdict::Instrument };
    if let Ok(lid) = l.create_link("inst0") {
        let mut d = Encoder::new();
        d.u32(lid);
        let _ = l.call(DESTROY_LINK, d.as_slice(), Some(Duration::from_secs(1)));
        return Verdict::Instrument;
    }
    Verdict::Gateway(a)
}

/// Which primary addresses on a gateway's bus have a listener: for each, address it as a listener,
/// release ATN, and see whether NDAC is held (the way the adapters' own probe works; no serial
/// poll, which would clear the instrument's RQS). REN is left alone. The interface is locked for the
/// duration, without waiting: another client's lock is `Locked`, a gateway without ATN control is
/// `Unsupported`. A bus with every device off answers no listener. `progress` is told each address as it is tried.
pub fn find_listeners(host: &str, iface: &str, within: Duration, progress: impl FnMut(u8)) -> Result<Vec<u8>, Error> {
    find_at(host, None, iface, within, progress)
}

/// `find_listeners` for a core channel on a given port (`None`: ask the portmapper).
pub fn find_at(host: &str, port: Option<u16>, iface: &str, within: Duration, mut progress: impl FnMut(u8)) -> Result<Vec<u8>, Error> {
    let mut l = Link::open(host, port, iface, within, Some(Some(Duration::ZERO)))?;
    let dl = || Deadline::after(Some(Duration::from_secs(3)));
    let own = l.bus_status(BUS_STATUS_ADDRESS, dl())?;
    let mut found = Vec::new();
    let mut result = Ok(());
    for addr in 1u8..=30 {
        if u16::from(addr) == own {
            continue;
        }
        progress(addr);
        let one = (|| -> Result<bool, Error> {
            l.command_on_interface(&[GPIB_UNL, GPIB_LAD + addr], dl())?;
            l.atn(false, dl())?;
            let held = l.bus_status(BUS_STATUS_NDAC, dl());
            // Back to command phase and unaddress, whatever NDAC said.
            let back = l.atn(true, dl()).and_then(|_| l.command_on_interface(&[GPIB_UNL], dl()));
            let held = held?;
            back?;
            Ok(held != 0)
        })();
        match one {
            Ok(true) => found.push(addr),
            Ok(false) => {}
            // Command bytes nobody accepts: with ATN asserted every powered device holds NDAC, so a bus where none
            // does has nothing on (every instrument switched off), and no listener rather than no answer.
            Err(Error::Timeout) if l.atn(true, dl()).and_then(|_| l.bus_status(BUS_STATUS_NDAC, dl())) == Ok(0) => break,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    // Leave the bus idle even after a failure (best effort).
    let _ = l.atn(true, dl()).and_then(|_| l.command_on_interface(&[GPIB_UNL], dl()));
    result.map(|_| found)
}

pub struct Link {
    rpc: Rpc,
    lid: u32,
    /// The largest device_write the device takes (create_link's maxRecvSize).
    max_send: usize,
    /// The core channel's port, as connected.
    pub port: u16,
    /// The interrupt channel, while service requests are watched.
    intr: Option<Arc<Inbox>>,
    /// Behind a LAN/GPIB gateway (`gpib0,5`): the address, and the interface link (`gpib0`) that
    /// REN, LLO and IFC go through, made the first time one is wanted.
    gateway: Option<super::name::Gpib>,
    iface_lid: Option<u32>,
}

impl Link {
    /// Open the core channel -- at `port`, or where the portmapper says -- and create a link to
    /// `device`. `lock`: take the exclusive lock as the link is made, waiting at most that long.
    pub fn open(host: &str, port: Option<u16>, device: &str, within: Duration, lock: Option<Option<Duration>>) -> Result<Link, Error> {
        let port = match port {
            Some(p) => p,
            None => match rpc::getport(host, CORE_PROG, CORE_VERS, within)? {
                0 => return Err(Error::NotFound(format!("{host} has no VXI-11 core channel"))),
                p => p,
            },
        };
        let mut rpc = Rpc::new(super::connect(host, port, within)?)?;
        let port = rpc.peer_port().unwrap_or(port);
        let (lock_device, lock_ms, wait) = match lock {
            None => (false, 0, Some(within + QUICK)),
            Some(w) => (true, ms(w), w.map(|w| w + within + QUICK)),
        };
        let mut a = Encoder::new();
        // The client id only shows up in the device's own logs; the process id is unique enough.
        a.u32(std::process::id()).bool(lock_device).u32(lock_ms).opaque(device.as_bytes());
        let r = rpc.call(CORE_PROG, CORE_VERS, CREATE_LINK, a.as_slice(), wait).map_err(|e| match e {
            Error::Lost(s) => Error::NotFound(s),
            Error::Timeout => Error::NotFound(format!("{host}:{port} did not answer create_link")),
            e => e,
        })?;
        let mut d = Decoder::new(&r);
        let (err, lid, _abort_port, max_recv) = (d.u32(), d.u32(), d.u32(), d.u32());
        let (Some(err), Some(lid), Some(_), Some(max_recv)) = (err, lid, _abort_port, max_recv) else {
            return Err(Error::NotFound("a truncated create_link reply".into()));
        };
        if err != 0 {
            return Err(match device_error(err, &format!("creating a link to {device}")) {
                // Locked stays locked: VISA reports a lock not had in time as that.
                e @ (Error::Locked | Error::Timeout) => e,
                Error::NotFound(s) | Error::Io(s) => Error::NotFound(s),
                e => Error::NotFound(e.to_string()),
            });
        }
        // VXI-11 says at least 1024; a device that says less is believed, but never below 64.
        let max_send = (max_recv as usize).clamp(64, 1 << 30);
        Ok(Link { rpc, lid, max_send, port, intr: None, gateway: super::name::gpib_of(device), iface_lid: None })
    }

    fn create_link(&mut self, device: &str) -> Result<u32, Error> {
        let mut a = Encoder::new();
        a.u32(std::process::id()).bool(false).u32(0).opaque(device.as_bytes());
        let r = self.call(CREATE_LINK, a.as_slice(), Some(QUICK))?;
        let mut d = Decoder::new(&r);
        match (d.u32(), d.u32()) {
            (Some(0), Some(lid)) => Ok(lid),
            (Some(e), _) => Err(device_error(e, &format!("creating a link to {device}"))),
            _ => Err(truncated()),
        }
    }

    /// The gateway's interface link, made if this is the first time; Unsupported if this is not
    /// an instrument behind a gateway.
    fn iface(&mut self) -> Result<u32, Error> {
        if let Some(l) = self.iface_lid {
            return Ok(l);
        }
        let name = self.gateway.as_ref().ok_or(Error::Unsupported)?.iface.clone();
        let l = self.create_link(&name)?;
        self.iface_lid = Some(l);
        Ok(l)
    }

    /// A device_docmd on the interface link; what it answers.
    fn docmd(&mut self, cmd: u32, datasize: u32, data: &[u8], dl: Deadline, what: &str) -> Result<Vec<u8>, Error> {
        let lid = self.iface()?;
        self.docmd_on(lid, cmd, datasize, data, dl, what)
    }

    fn docmd_on(&mut self, lid: u32, cmd: u32, datasize: u32, data: &[u8], dl: Deadline, what: &str) -> Result<Vec<u8>, Error> {
        let (io, wait) = io_times(&dl);
        let mut a = Encoder::new();
        a.u32(lid).u32(0).u32(io).u32(0).u32(cmd).bool(true).u32(datasize).opaque(data);
        let r = self.call(DEVICE_DOCMD, a.as_slice(), wait)?;
        let mut d = Decoder::new(&r);
        match d.u32() {
            Some(0) => Ok(d.opaque(4096).unwrap_or_default().to_vec()),
            Some(e) => Err(device_error(e, what)),
            None => Err(truncated()),
        }
    }

    fn set_ren(&mut self, on: bool, dl: Deadline) -> Result<(), Error> {
        self.docmd(CMD_REN_CTRL, 2, &(on as u16).to_be_bytes(), dl, "REN control").map(|_| ())
    }

    fn send_command(&mut self, cmds: &[u8], dl: Deadline) -> Result<(), Error> {
        self.docmd(CMD_SEND_COMMAND, 1, cmds, dl, "send command").map(|_| ())
    }

    /// The instrument's address behind a gateway.
    pub fn gpib(&self) -> Option<&super::name::Gpib> {
        self.gateway.as_ref()
    }

    /// The core channel, for its TCP options.
    pub(super) fn stream(&self) -> &std::net::TcpStream {
        self.rpc.stream()
    }

    /// Interface clear on the gateway's bus.
    pub fn ifc(&mut self, dl: Deadline) -> Result<(), Error> {
        self.docmd(CMD_IFC_CTRL, 1, &[], dl, "IFC").map(|_| ())
    }

    /// Whether REN is asserted on the gateway's bus.
    pub fn ren_state(&mut self, dl: Deadline) -> Result<bool, Error> {
        let r = self.docmd(CMD_BUS_STATUS, 2, &BUS_STATUS_REN.to_be_bytes(), dl, "bus status")?;
        match r.get(..2) {
            Some(b) => Ok(u16::from_be_bytes([b[0], b[1]]) != 0),
            None => Err(truncated()),
        }
    }

    /// One bus-status value of a gateway, asked on the link `open` made to its interface.
    fn bus_status(&mut self, what: u16, dl: Deadline) -> Result<u16, Error> {
        let lid = self.lid;
        let r = self.docmd_on(lid, CMD_BUS_STATUS, 2, &what.to_be_bytes(), dl, "bus status")?;
        match r.get(..2) {
            Some(b) => Ok(u16::from_be_bytes([b[0], b[1]])),
            None => Err(truncated()),
        }
    }

    /// ATN control on the interface link `open` made: assert or release.
    fn atn(&mut self, on: bool, dl: Deadline) -> Result<(), Error> {
        let lid = self.lid;
        self.docmd_on(lid, CMD_ATN_CTRL, 2, &(on as u16).to_be_bytes(), dl, "ATN control").map(|_| ())
    }

    fn command_on_interface(&mut self, cmds: &[u8], dl: Deadline) -> Result<(), Error> {
        let lid = self.lid;
        self.docmd_on(lid, CMD_SEND_COMMAND, 1, cmds, dl, "send command").map(|_| ())
    }

    fn call(&mut self, proc_num: u32, args: &[u8], wait: Option<Duration>) -> Result<Vec<u8>, Error> {
        self.rpc.call(CORE_PROG, CORE_VERS, proc_num, args, wait)
    }

    /// The procedures that take (link, flags, lock_timeout, io_timeout) and answer an error only.
    fn generic(&mut self, proc_num: u32, dl: Deadline, what: &str) -> Result<Vec<u8>, Error> {
        let (io, wait) = io_times(&dl);
        let mut a = Encoder::new();
        a.u32(self.lid).u32(0).u32(0).u32(io);
        let r = self.call(proc_num, a.as_slice(), wait)?;
        match Decoder::new(&r).u32() {
            Some(0) => Ok(r),
            Some(e) => Err(device_error(e, what)),
            None => Err(truncated()),
        }
    }

    pub fn write(&mut self, data: &[u8], end: bool, dl: Deadline) -> Result<usize, (usize, Error)> {
        let mut sent = 0usize;
        // At least one call, so an empty write with END still ends a message.
        loop {
            let part = &data[sent..data.len().min(sent + self.max_send)];
            let last = sent + part.len() == data.len();
            let (io, wait) = io_times(&dl);
            let mut a = Encoder::new();
            a.u32(self.lid).u32(io).u32(0).u32(if last && end { FLAG_END } else { 0 }).opaque(part);
            let r = self.call(DEVICE_WRITE, a.as_slice(), wait).map_err(|e| (sent, e))?;
            let mut d = Decoder::new(&r);
            let (err, n) = match (d.u32(), d.u32()) {
                (Some(e), Some(n)) => (e, (n as usize).min(part.len())),
                _ => return Err((sent, truncated())),
            };
            sent += n;
            if err != 0 {
                return Err((sent, device_error(err, "device_write")));
            }
            if sent == data.len() {
                return Ok(sent);
            }
            // Fewer bytes taken than given, and no error: send the rest. Taking none is not progress.
            if n == 0 {
                return Err((sent, Error::Io("device_write took no bytes".into())));
            }
        }
    }

    pub fn read(&mut self, max: usize, term: Option<u8>, suppress_end: bool, dl: Deadline) -> Result<ReadResult, Error> {
        let mut out = Vec::new();
        let stop = REASON_CHR | if suppress_end { 0 } else { REASON_END };
        let (flags, tc) = match term {
            Some(c) => (FLAG_TERMCHRSET, c as u32),
            None => (0, 0),
        };
        loop {
            let want = max - out.len();
            if want == 0 {
                return Ok(ReadResult { data: out, end: ReadEnd::MaxCount });
            }
            if dl.expired() && !out.is_empty() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            let (io, wait) = io_times(&dl);
            let mut a = Encoder::new();
            a.u32(self.lid).u32(want.min(READ_CHUNK) as u32).u32(io).u32(0).u32(flags).u32(tc);
            let r = match self.call(DEVICE_READ, a.as_slice(), wait) {
                Err(Error::Timeout) if !out.is_empty() => return Ok(ReadResult { data: out, end: ReadEnd::Timeout }),
                r => r?,
            };
            let mut d = Decoder::new(&r);
            let (err, reason) = (d.u32().ok_or_else(truncated)?, d.u32().ok_or_else(truncated)?);
            let data = d.opaque(READ_CHUNK).ok_or_else(truncated)?;
            out.extend_from_slice(&data[..data.len().min(want)]);
            match err {
                0 => {}
                // What arrived before the device gave up is returned with the timeout.
                15 => return Ok(ReadResult { data: out, end: ReadEnd::Timeout }),
                e => return Err(device_error(e, "device_read")),
            }
            // END first: VPP-4.3 RULE 6.1.1 reports it whether or not the termination character
            // came with it.
            if reason & stop & REASON_END != 0 {
                return Ok(ReadResult { data: out, end: ReadEnd::End });
            }
            if reason & stop & REASON_CHR != 0 && term.is_some() {
                return Ok(ReadResult { data: out, end: ReadEnd::TermChar });
            }
            if reason & REASON_REQCNT != 0 && out.len() >= max {
                return Ok(ReadResult { data: out, end: ReadEnd::MaxCount });
            }
            // Nothing that ends the read (a partial chunk, or END suppressed): ask again.
            if dl.expired() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
        }
    }

    pub fn read_stb(&mut self, dl: Deadline) -> Result<u8, Error> {
        let r = self.generic(DEVICE_READSTB, dl, "device_readstb")?;
        let mut d = Decoder::new(&r);
        d.u32();
        d.u32().map(|s| s as u8).ok_or_else(truncated)
    }

    pub fn trigger(&mut self, dl: Deadline) -> Result<(), Error> {
        self.generic(DEVICE_TRIGGER, dl, "device_trigger").map(|_| ())
    }

    pub fn clear(&mut self, dl: Deadline) -> Result<(), Error> {
        self.generic(DEVICE_CLEAR, dl, "device_clear").map(|_| ())
    }

    /// VXI-11 has remote and local and nothing about REN or local lockout, so only the modes that
    /// are one of those two are offered. VPP-4.3 RULE 6.5.6 asks for ASSERT_ADDRESS_LLO too, and
    /// OBSERVATION 6.5.1 says it is ASSERT_ADDRESS here: remote, without the lockout.
    pub fn ren(&mut self, mode: Ren, dl: Deadline) -> Result<(), Error> {
        if self.gateway.is_some() {
            return self.gateway_ren(mode, dl);
        }
        match mode {
            Ren::AssertAddress | Ren::AssertAddressLlo => self.generic(DEVICE_REMOTE, dl, "device_remote").map(|_| ()),
            Ren::AddressGtl | Ren::DeassertGtl => self.generic(DEVICE_LOCAL, dl, "device_local").map(|_| ()),
            _ => Err(Error::Unsupported),
        }
    }

    /// All of VISA's REN modes, behind a gateway: REN and LLO through the interface link, remote
    /// and local (which address this instrument) through its own.
    fn gateway_ren(&mut self, mode: Ren, dl: Deadline) -> Result<(), Error> {
        let remote = |l: &mut Self| l.generic(DEVICE_REMOTE, dl, "device_remote").map(|_| ());
        let local = |l: &mut Self| l.generic(DEVICE_LOCAL, dl, "device_local").map(|_| ());
        match mode {
            Ren::Deassert => self.set_ren(false, dl),
            Ren::Assert => self.set_ren(true, dl),
            Ren::DeassertGtl => {
                local(self)?;
                self.set_ren(false, dl)
            }
            Ren::AssertAddress => {
                self.set_ren(true, dl)?;
                remote(self)
            }
            Ren::AssertLlo => self.send_command(&[GPIB_LLO], dl),
            Ren::AssertAddressLlo => {
                self.set_ren(true, dl)?;
                remote(self)?;
                self.send_command(&[GPIB_LLO], dl)
            }
            Ren::AddressGtl => local(self),
        }
    }

    pub fn lock(&mut self, wait: Option<Duration>) -> Result<(), Error> {
        let (flags, lock_ms) = match wait {
            Some(w) if w.is_zero() => (0, 0),
            w => (FLAG_WAITLOCK, ms(w)),
        };
        let mut a = Encoder::new();
        a.u32(self.lid).u32(flags).u32(lock_ms);
        let r = self.call(DEVICE_LOCK, a.as_slice(), wait.map(|w| w + MARGIN))?;
        match Decoder::new(&r).u32() {
            Some(0) => Ok(()),
            // Not free in time: a device may say "timeout" instead of "locked" (pyvisa-py has seen
            // it); either way the lock was not had.
            Some(11 | 15) => Err(Error::Locked),
            Some(e) => Err(device_error(e, "device_lock")),
            None => Err(truncated()),
        }
    }

    pub fn unlock(&mut self) -> Result<(), Error> {
        let mut a = Encoder::new();
        a.u32(self.lid);
        let r = self.call(DEVICE_UNLOCK, a.as_slice(), Some(QUICK))?;
        match Decoder::new(&r).u32() {
            Some(0) => Ok(()),
            Some(e) => Err(device_error(e, "device_unlock")),
            None => Err(truncated()),
        }
    }
}

impl Link {
    /// Open the interrupt channel and ask the device for service requests. Watching again only
    /// forgets what came before.
    pub fn watch_srq(&mut self) -> Result<SrqWatch, Error> {
        if let Some(i) = &self.intr {
            *i.raised.lock().unwrap() = 0;
            return Ok(SrqWatch(i.clone()));
        }
        // The address the core channel leaves from is the one the device can reach us at; binding
        // only that opens nothing on the other interfaces.
        let local = self.rpc.local_addr().ok_or_else(|| Error::Io("no local address".into()))?;
        let IpAddr::V4(v4) = local.ip() else {
            // create_intr_chan carries a 32-bit address.
            return Err(Error::Unsupported);
        };
        let listener = TcpListener::bind((local.ip(), 0)).map_err(|e| Error::Io(format!("the interrupt channel: {e}")))?;
        let port = listener.local_addr().map_err(|e| Error::Io(e.to_string()))?.port();
        let device = self.rpc.peer_addr().ok_or_else(|| Error::Io("no device address".into()))?.ip();
        let inbox = Arc::new(Inbox::default());
        serve_interrupts(listener, device, inbox.clone()).map_err(|e| Error::Io(format!("the interrupt channel: {e}")))?;
        let mut a = Encoder::new();
        a.u32(u32::from(v4)).u32(port as u32).u32(INTR_PROG).u32(INTR_VERS).u32(DEVICE_TCP);
        let r = self.call(CREATE_INTR_CHAN, a.as_slice(), Some(QUICK)).and_then(|r| match Decoder::new(&r).u32() {
            Some(0) => Ok(()),
            Some(e) => Err(device_error(e, "create_intr_chan")),
            None => Err(truncated()),
        });
        if let Err(e) = r {
            inbox.stop();
            return Err(e);
        }
        if let Err(e) = self.enable_srq(true) {
            let _ = self.call(DESTROY_INTR_CHAN, &[], Some(QUICK));
            inbox.stop();
            return Err(e);
        }
        self.intr = Some(inbox.clone());
        Ok(SrqWatch(inbox))
    }

    /// Stop the device's service requests and close the interrupt channel.
    pub fn unwatch_srq(&mut self) {
        if let Some(i) = self.intr.take() {
            let _ = self.enable_srq(false);
            let _ = self.call(DESTROY_INTR_CHAN, &[], Some(Duration::from_secs(1)));
            i.stop();
        }
    }

    fn enable_srq(&mut self, on: bool) -> Result<(), Error> {
        let mut a = Encoder::new();
        a.u32(self.lid).bool(on).opaque(SRQ_HANDLE);
        let r = self.call(DEVICE_ENABLE_SRQ, a.as_slice(), Some(QUICK))?;
        match Decoder::new(&r).u32() {
            Some(0) => Ok(()),
            Some(e) => Err(device_error(e, "device_enable_srq")),
            None => Err(truncated()),
        }
    }
}

/// Where the interrupt channel's calls land.
#[derive(Default)]
struct Inbox {
    raised: Mutex<u32>,
    cv: Condvar,
    stopped: AtomicBool,
}

impl Inbox {
    fn raise(&self) {
        *self.raised.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }
}

/// The service requests of one VXI-11 link, for the thread that delivers its events.
#[derive(Clone)]
pub struct SrqWatch(Arc<Inbox>);

impl SrqWatch {
    /// Wait up to `wait` for a service request; Ok(true) is one. Once the channel is closed this
    /// is `Lost`, so that whoever waits stops.
    pub fn wait(&self, wait: Duration) -> Result<bool, Error> {
        let mut n = self.0.raised.lock().unwrap();
        if *n == 0 && !self.0.stopped.load(Ordering::SeqCst) {
            n = self.0.cv.wait_timeout(n, wait).unwrap().0;
        }
        if *n > 0 {
            *n -= 1;
            return Ok(true);
        }
        if self.0.stopped.load(Ordering::SeqCst) {
            return Err(Error::Lost("the interrupt channel is closed".into()));
        }
        Ok(false)
    }
}

/// The RPC server of the interrupt channel: accept the device's connection and count its
/// device_intr_srq calls. They are not answered: the device expects no reply (pyvisa-py's reading
/// of VXI-11), and a reply nobody reads only fills its buffer. Stops when the inbox is stopped.
/// Only `device` -- the address the core channel goes to -- may connect, and only `MAX_INTR_CONNS`
/// connections at once (one, and room for a reconnect while the old one closes): anyone else on the
/// network could otherwise make up service requests.
fn serve_interrupts(listener: TcpListener, device: IpAddr, inbox: Arc<Inbox>) -> std::io::Result<()> {
    const MAX_INTR_CONNS: usize = 2;
    listener.set_nonblocking(true)?;
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        while !inbox.stopped.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, from)) => {
                    if from.ip() != device || open.load(Ordering::SeqCst) >= MAX_INTR_CONNS {
                        continue; // dropped: closed without a word
                    }
                    open.fetch_add(1, Ordering::SeqCst);
                    let (inbox, open) = (inbox.clone(), open.clone());
                    std::thread::spawn(move || {
                        interrupt_calls(s, &inbox);
                        open.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return,
            }
        }
    });
    Ok(())
}

/// Read up to `buf.len()` bytes, giving up on the rest after a quiet `patience` once some came:
/// some devices leave off the last four bytes of an interrupt record (pyvisa-py found it). None:
/// the connection is over, or the inbox was stopped.
fn read_some(s: &mut TcpStream, buf: &mut [u8], inbox: &Inbox, patience: Duration) -> Option<usize> {
    let mut got = 0;
    let mut quiet = Duration::ZERO;
    let tick = Duration::from_millis(100);
    let _ = s.set_read_timeout(Some(tick));
    while got < buf.len() {
        if inbox.stopped.load(Ordering::SeqCst) {
            return None;
        }
        match s.read(&mut buf[got..]) {
            Ok(0) => return None,
            Ok(n) => {
                got += n;
                quiet = Duration::ZERO;
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                quiet += tick;
                if got > 0 && quiet >= patience {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(got)
}

fn interrupt_calls(mut s: TcpStream, inbox: &Inbox) {
    let _ = s.set_nonblocking(false);
    loop {
        let mut marker = [0u8; 4];
        match read_some(&mut s, &mut marker, inbox, Duration::from_secs(1)) {
            Some(4) => {}
            _ => return,
        }
        let m = u32::from_be_bytes(marker);
        let mut body = vec![0u8; ((m & 0x7FFF_FFFF) as usize).min(4096)];
        let Some(n) = read_some(&mut s, &mut body, inbox, Duration::from_millis(300)) else { return };
        // The call header is xid, CALL, RPC version, program, version, procedure: enough to know.
        let mut d = Decoder::new(&body[..n]);
        let (_xid, _call, _rpcvers, prog, _vers, proc_num) = (d.u32(), d.u32(), d.u32(), d.u32(), d.u32(), d.u32());
        if prog == Some(INTR_PROG) && proc_num == Some(INTR_SRQ) {
            inbox.raise();
        }
    }
}

impl Drop for Link {
    /// The device keeps a link until told, or until the connection closes; telling it is polite
    /// and quick. A dead connection is not waited on.
    fn drop(&mut self) {
        self.unwatch_srq();
        for lid in [Some(self.lid), self.iface_lid.take()].into_iter().flatten() {
            let mut a = Encoder::new();
            a.u32(lid);
            let _ = self.call(DESTROY_LINK, a.as_slice(), Some(Duration::from_secs(1)));
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A VXI-11 device in a thread, enough to hold the client to the protocol: a message buffer
    //! that answers `*IDN?`, END on its last byte, and a hook to answer device_read with a timeout.
    use super::*;
    use crate::lan::rpc::server::{parse_call, reply};
    use crate::lan::rpc::{read_record, write_record};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Dev {
        pub written: Vec<u8>,
        pub out: Vec<u8>,
        pub max_recv: u32,
        /// device_read answers at most this many bytes per call (0: no limit).
        pub chunk: usize,
        /// device_read answers "io timeout" with no data.
        pub silent: bool,
        pub locked: bool,
        pub calls: Vec<u32>,
        /// Where create_intr_chan said to call back, and whether service requests are enabled.
        pub intr: Option<(u32, u16)>,
        pub srq_on: bool,
        /// Links by id, as named; what the bus saw (REN, the command bytes, IFCs); remote/local.
        pub links: Vec<String>,
        pub ren: bool,
        pub commands: Vec<u8>,
        pub ifcs: u32,
        pub remote: Vec<bool>,
        /// A gateway's bus: the addresses with a listener, who is addressed, ATN, its own address;
        /// whether `inst0` is refused, ATN control is "not supported", the bus status is not answered.
        pub listeners: Vec<u8>,
        pub addressed: Option<u8>,
        pub atn: bool,
        pub bus_address: u16,
        pub refuse_inst0: bool,
        pub no_atn_control: bool,
        pub no_bus_status: bool,
    }

    pub fn serve(dev: Arc<Mutex<Dev>>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let dev = dev.clone();
                std::thread::spawn(move || {
                    let mut r = s.try_clone().unwrap();
                    while let Ok(msg) = read_record(&mut r, 1 << 24) {
                        let c = parse_call(&msg).unwrap();
                        let res = answer(&mut dev.lock().unwrap(), c.proc_num, &c.args);
                        if write_record(&mut s, &reply(c.xid, &res)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    fn answer(d: &mut Dev, proc_num: u32, args: &[u8]) -> Vec<u8> {
        d.calls.push(proc_num);
        let mut a = Decoder::new(args);
        let mut e = Encoder::new();
        match proc_num {
            CREATE_LINK => {
                let (_id, lock, _lt) = (a.u32(), a.u32().unwrap(), a.u32());
                let name = String::from_utf8_lossy(a.opaque(256).unwrap()).into_owned();
                if d.refuse_inst0 && name == "inst0" {
                    e.u32(3).u32(0).u32(0).u32(0);
                    return e.into_vec();
                }
                if lock != 0 && d.locked {
                    e.u32(11).u32(0).u32(0).u32(0);
                    return e.into_vec();
                }
                if lock != 0 {
                    d.locked = true;
                }
                d.links.push(name);
                e.u32(0).u32(d.links.len() as u32).u32(0).u32(if d.max_recv == 0 { 1024 } else { d.max_recv });
            }
            DEVICE_DOCMD => {
                let lid = a.u32().unwrap() as usize;
                let (_f, _io, _lt) = (a.u32(), a.u32(), a.u32());
                let (cmd, _order, _size) = (a.u32().unwrap(), a.u32(), a.u32());
                let data = a.opaque(256).unwrap().to_vec();
                // VXI-11.2: docmd on a device link is "operation not supported".
                if d.links.get(lid - 1).is_none_or(|n| n.contains(',')) {
                    e.u32(8).opaque(&[]);
                    return e.into_vec();
                }
                match cmd {
                    CMD_REN_CTRL => d.ren = data != [0, 0],
                    CMD_ATN_CTRL if d.no_atn_control => {
                        e.u32(8).opaque(&[]);
                        return e.into_vec();
                    }
                    CMD_ATN_CTRL => d.atn = data != [0, 0],
                    CMD_SEND_COMMAND => {
                        d.commands.extend_from_slice(&data);
                        for b in &data {
                            match *b {
                                GPIB_UNL => d.addressed = None,
                                l @ 0x20..=0x3E => d.addressed = d.listeners.contains(&(l - GPIB_LAD)).then_some(l - GPIB_LAD),
                                _ => {}
                            }
                        }
                    }
                    CMD_IFC_CTRL => d.ifcs += 1,
                    CMD_BUS_STATUS if d.no_bus_status => {
                        e.u32(8).opaque(&[]);
                        return e.into_vec();
                    }
                    CMD_BUS_STATUS => {
                        let v: u16 = match u16::from_be_bytes([data[0], data[1]]) {
                            1 => d.ren as u16,
                            // NDAC is held by an addressed listener once ATN is released.
                            3 => (d.addressed.is_some() && !d.atn) as u16,
                            8 => d.bus_address,
                            _ => 0,
                        };
                        e.u32(0).opaque(&v.to_be_bytes());
                        return e.into_vec();
                    }
                    _ => {}
                }
                e.u32(0).opaque(&data);
            }
            DEVICE_REMOTE | DEVICE_LOCAL => {
                d.remote.push(proc_num == DEVICE_REMOTE);
                e.u32(0);
            }
            DEVICE_WRITE => {
                let (_lid, _io, _lt, flags) = (a.u32(), a.u32(), a.u32(), a.u32().unwrap());
                let data = a.opaque(1 << 24).unwrap();
                d.written.extend_from_slice(data);
                if flags & FLAG_END != 0 {
                    if d.written.starts_with(b"*IDN?") {
                        d.out = b"FAKE,VXI11,0,1.0\n".to_vec();
                    }
                    d.written.clear();
                }
                e.u32(0).u32(data.len() as u32);
            }
            DEVICE_READ => {
                let (_lid, size) = (a.u32(), a.u32().unwrap() as usize);
                let (_io, _lt, flags, tc) = (a.u32(), a.u32(), a.u32().unwrap(), a.u32().unwrap() as u8);
                if d.silent || d.out.is_empty() {
                    e.u32(15).u32(0).opaque(&[]);
                    return e.into_vec();
                }
                let mut n = size.min(d.out.len());
                if d.chunk > 0 {
                    n = n.min(d.chunk);
                }
                let mut reason = 0;
                if flags & FLAG_TERMCHRSET != 0 {
                    if let Some(i) = d.out[..n].iter().position(|b| *b == tc) {
                        n = i + 1;
                        reason |= REASON_CHR;
                    }
                }
                let data: Vec<u8> = d.out.drain(..n).collect();
                if d.out.is_empty() {
                    reason |= REASON_END;
                }
                if n == size {
                    reason |= REASON_REQCNT;
                }
                e.u32(0).u32(reason).opaque(&data);
            }
            DEVICE_READSTB => {
                e.u32(0).u32(if d.out.is_empty() { 0 } else { 0x10 });
            }
            DEVICE_LOCK => {
                e.u32(if d.locked { 11 } else { 0 });
                d.locked = true;
            }
            DEVICE_UNLOCK => {
                e.u32(if d.locked { 0 } else { 12 });
                d.locked = false;
            }
            CREATE_INTR_CHAN => {
                let (ip, port) = (a.u32().unwrap(), a.u32().unwrap());
                d.intr = Some((ip, port as u16));
                e.u32(0);
            }
            DEVICE_ENABLE_SRQ => {
                let (_lid, on) = (a.u32(), a.u32().unwrap());
                d.srq_on = on != 0;
                e.u32(0);
            }
            DESTROY_INTR_CHAN => {
                d.intr = None;
                e.u32(0);
            }
            _ => {
                e.u32(0);
            }
        }
        e.into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{serve, Dev};
    use super::*;
    use std::sync::{Arc, Mutex};

    fn open(dev: &Arc<Mutex<Dev>>) -> Link {
        let port = serve(dev.clone());
        Link::open("127.0.0.1", Some(port), "inst0", Duration::from_secs(2), None).unwrap()
    }

    fn dl() -> Deadline {
        Deadline::after(Some(Duration::from_secs(2)))
    }

    #[test]
    fn a_query_and_its_answer() {
        let dev = Arc::new(Mutex::new(Dev::default()));
        let mut l = open(&dev);
        assert_eq!(l.write(b"*IDN?\n", true, dl()), Ok(6));
        let r = l.read(1000, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"FAKE,VXI11,0,1.0\n"[..], ReadEnd::End));
    }

    #[test]
    fn a_long_write_is_split_with_end_only_on_the_last_part() {
        let dev = Arc::new(Mutex::new(Dev { max_recv: 100, ..Default::default() }));
        let mut l = open(&dev);
        let mut msg = b"*IDN?".to_vec();
        msg.resize(250, b' ');
        assert_eq!(l.write(&msg, true, dl()), Ok(250));
        let d = dev.lock().unwrap();
        assert_eq!(d.calls.iter().filter(|c| **c == DEVICE_WRITE).count(), 3);
        assert!(d.written.is_empty(), "END came with the last part, so the message was taken");
        assert!(!d.out.is_empty());
    }

    #[test]
    fn reads_go_on_until_end_and_stop_at_count_and_term_char() {
        let dev = Arc::new(Mutex::new(Dev { chunk: 3, ..Default::default() }));
        let mut l = open(&dev);
        dev.lock().unwrap().out = b"1,2,3\n".to_vec();
        let r = l.read(100, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"1,2,3\n"[..], ReadEnd::End), "partial answers are asked again");
        dev.lock().unwrap().out = b"abcdef\n".to_vec();
        let r = l.read(4, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"abcd"[..], ReadEnd::MaxCount));
        dev.lock().unwrap().out = b"ab,cd\n".to_vec();
        let r = l.read(100, Some(b','), false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"ab,"[..], ReadEnd::TermChar));
    }

    #[test]
    fn the_devices_timeout_returns_what_arrived() {
        let dev = Arc::new(Mutex::new(Dev::default()));
        let mut l = open(&dev);
        let r = l.read(100, None, false, dl()).unwrap();
        assert_eq!((r.data.len(), r.end), (0, ReadEnd::Timeout));
        // The connection is still good.
        assert_eq!(l.write(b"*IDN?\n", true, dl()), Ok(6));
    }

    #[test]
    fn locks_are_the_devices() {
        let dev = Arc::new(Mutex::new(Dev::default()));
        let mut l = open(&dev);
        assert_eq!(l.lock(Some(Duration::ZERO)), Ok(()));
        assert_eq!(l.lock(Some(Duration::ZERO)), Err(Error::Locked));
        assert_eq!(l.unlock(), Ok(()));
        assert_eq!(l.unlock(), Err(Error::NotLocked));
    }

    #[test]
    fn service_requests_come_back_over_the_interrupt_channel() {
        use crate::lan::rpc::{build_call, write_record};
        use std::io::Write;
        let dev = Arc::new(Mutex::new(Dev::default()));
        let mut l = open(&dev);
        let w = l.watch_srq().unwrap();
        let (ip, port) = dev.lock().unwrap().intr.unwrap();
        assert_eq!(std::net::Ipv4Addr::from(ip), std::net::Ipv4Addr::LOCALHOST, "the core channel's own address");
        assert!(dev.lock().unwrap().srq_on);
        assert_eq!(w.wait(Duration::from_millis(50)), Ok(false));
        // The device calls back: once properly, once leaving off the record's last four bytes.
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut h = Encoder::new();
        h.opaque(SRQ_HANDLE);
        let call = build_call(1, INTR_PROG, INTR_VERS, INTR_SRQ, h.as_slice());
        write_record(&mut s, &call).unwrap();
        assert_eq!(w.wait(Duration::from_secs(2)), Ok(true));
        let mut short = ((call.len() as u32) | 0x8000_0000).to_be_bytes().to_vec();
        short.extend_from_slice(&call[..call.len() - 4]);
        s.write_all(&short).unwrap();
        assert_eq!(w.wait(Duration::from_secs(2)), Ok(true));
        // The link still works, and closing the watch closes the channel.
        assert_eq!(l.write(b"*IDN?\n", true, dl()), Ok(6));
        l.unwatch_srq();
        assert!(!dev.lock().unwrap().srq_on && dev.lock().unwrap().intr.is_none());
        assert!(matches!(w.wait(Duration::from_millis(50)), Err(Error::Lost(_))));
        std::thread::sleep(Duration::from_millis(300));
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "nothing listens any more");
    }

    #[test]
    fn a_gateway_instrument_gets_ren_llo_and_ifc_through_the_interface_link() {
        let dev = Arc::new(Mutex::new(Dev::default()));
        let port = serve(dev.clone());
        let mut l = Link::open("127.0.0.1", Some(port), "gpib0,5", Duration::from_secs(2), None).unwrap();
        assert_eq!(l.gpib().map(|g| g.pad), Some(5));
        assert_eq!(dev.lock().unwrap().links, ["gpib0,5"], "no interface link until one is wanted");
        l.ren(Ren::AssertAddressLlo, dl()).unwrap();
        {
            let d = dev.lock().unwrap();
            assert_eq!(d.links, ["gpib0,5", "gpib0"]);
            assert!(d.ren);
            assert_eq!(d.remote, [true], "device_remote on the instrument's own link");
            assert_eq!(d.commands, [GPIB_LLO]);
        }
        assert_eq!(l.ren_state(dl()), Ok(true));
        l.ren(Ren::DeassertGtl, dl()).unwrap();
        assert_eq!(l.ren_state(dl()), Ok(false));
        l.ren(Ren::AssertLlo, dl()).unwrap();
        l.ren(Ren::AddressGtl, dl()).unwrap();
        l.ifc(dl()).unwrap();
        {
            let d = dev.lock().unwrap();
            assert_eq!(d.links.len(), 2, "one interface link, reused");
            assert_eq!(d.remote, [true, false, false]);
            assert_eq!(d.commands, [GPIB_LLO, GPIB_LLO]);
            assert_eq!(d.ifcs, 1);
        }
        // Not behind a gateway: VXI-11 alone has no REN, and no interface link is made.
        let mut plain = Link::open("127.0.0.1", Some(port), "inst0", Duration::from_secs(2), None).unwrap();
        assert_eq!(plain.ren(Ren::Assert, dl()), Err(Error::Unsupported));
        assert_eq!(plain.ifc(dl()), Err(Error::Unsupported));
        assert_eq!(dev.lock().unwrap().links.len(), 3);
    }

    #[test]
    fn nobody_there_is_not_found() {
        // A port nothing listens on (bound, then released).
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        match Link::open("127.0.0.1", Some(port), "inst0", Duration::from_secs(1), None) {
            Err(Error::NotFound(_)) => {}
            other => panic!("{:?}", other.err()),
        }
    }

    fn gateway_dev(listeners: &[u8]) -> (Arc<Mutex<fake::Dev>>, u16) {
        let dev = Arc::new(Mutex::new(fake::Dev { listeners: listeners.to_vec(), bus_address: 21, refuse_inst0: true, ..Default::default() }));
        let port = fake::serve(dev.clone());
        (dev, port)
    }

    const WITHIN: Duration = Duration::from_secs(2);

    #[test]
    fn a_host_is_a_gateway_only_if_it_answers_bus_status_and_has_no_inst0() {
        let (_g, port) = gateway_dev(&[]);
        assert_eq!(judge_at("127.0.0.1", Some(port), "gpib0", WITHIN), Verdict::Gateway(21));
        // An instrument that answers bus status on gpib0 (address 21) and has an inst0.
        let (d, port) = gateway_dev(&[]);
        d.lock().unwrap().refuse_inst0 = false;
        assert_eq!(judge_at("127.0.0.1", Some(port), "gpib0", WITHIN), Verdict::Instrument);
        // An instrument whose interface link says "not supported".
        let (d, port) = gateway_dev(&[]);
        d.lock().unwrap().no_bus_status = true;
        assert_eq!(judge_at("127.0.0.1", Some(port), "gpib0", WITHIN), Verdict::Instrument);
        // Nothing listening: no answer, so no conclusion.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(judge_at("127.0.0.1", Some(closed), "gpib0", WITHIN), Verdict::Unknown);
    }

    #[test]
    fn the_inst0_link_made_to_judge_is_given_back() {
        let (d, port) = gateway_dev(&[]);
        d.lock().unwrap().refuse_inst0 = false;
        judge_at("127.0.0.1", Some(port), "gpib0", WITHIN);
        let d = d.lock().unwrap();
        let destroyed = d.calls.iter().filter(|c| **c == DESTROY_LINK).count();
        assert_eq!(destroyed, 2, "the interface link and the inst0 link, both told");
    }

    #[test]
    fn listeners_are_found_by_ndac_with_atn_released_and_everything_put_back() {
        let (d, port) = gateway_dev(&[6, 10, 20, 21, 29]);
        let mut seen = Vec::new();
        let found = find_at("127.0.0.1", Some(port), "gpib0", WITHIN, |a| seen.push(a)).unwrap();
        assert_eq!(found, [6, 10, 20, 29], "the gateway's own address (21) is not asked");
        assert_eq!(seen.len(), 29);
        assert!(!seen.contains(&21));
        let d = d.lock().unwrap();
        assert!(d.atn && d.addressed.is_none(), "ATN asserted again and nobody left addressed");
        assert!(!d.ren && d.ifcs == 0, "REN and IFC untouched");
        assert!(d.calls.contains(&DESTROY_LINK), "the link, and with it the lock, is given back");
    }

    #[test]
    fn a_gateway_without_atn_control_cannot_look_and_a_locked_one_is_not_disturbed() {
        let (d, port) = gateway_dev(&[6]);
        d.lock().unwrap().no_atn_control = true;
        assert_eq!(find_at("127.0.0.1", Some(port), "gpib0", WITHIN, |_| {}), Err(Error::Unsupported));
        let (d, port) = gateway_dev(&[6]);
        d.lock().unwrap().locked = true; // another client holds the lock
        assert_eq!(find_at("127.0.0.1", Some(port), "gpib0", WITHIN, |_| {}), Err(Error::Locked));
        assert!(d.lock().unwrap().commands.is_empty(), "nothing sent to the bus");
    }
}
