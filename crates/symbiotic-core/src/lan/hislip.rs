//! HiSLIP (IVI-6.1), the client, in synchronized or overlapped mode.
//!
//! Two TCP connections to the same port: the synchronous channel carries the messages (Data, DataEnd, Trigger) and the
//! answers; the asynchronous one carries requests that must not wait in line behind them (device clear, status query,
//! lock, remote/local). Every message is a 16-byte header -- "HS", type, control code, a 32-bit parameter, a 64-bit
//! payload length -- and a payload. pyvisa-py's `protocols/hislip.py` was a reference.
//!
//! What has to be right:
//!
//! - **The stream must never lose its place.** A timeout can land halfway through a header or a
//!   payload. What arrived stays in the channel's buffer, and the current frame's remaining length
//!   is kept, so the next read picks up exactly where this one stopped.
//! - **Two modes (IVI-6.1 section 3), chosen by the instrument at Initialize and renegotiated by
//!   every device clear.** Each message sent takes the next id (from 0xFFFF_FF00, by 2) in both.
//!   Synchronized: an answer carries the MessageID of the message it answers, so a Data or DataEnd
//!   that does not carry the last one sent is the late answer to an earlier query and is dropped,
//!   and sending throws away what is left of an answer being read. Overlapped: answers queue up
//!   in the order the queries went, numbered by the instrument's own count; nothing is dropped
//!   but by a device clear, and a status query quotes the id of the last answer delivered.
//! - **RMT-delivered.** Once a whole answer (through its DataEnd) has been read, the next message
//!   sent says so, which is how the instrument knows the answer was taken.
//! - **HiSLIP has no termination character.** VI_ATTR_TERMCHAR_EN is done here, in the data; the
//!   rest of the message stays for the next read.
//!
//! An asynchronous request that times out ends that channel (its late answer would be taken for the answer to the next
//! request); the synchronous channel is unaffected.
//!
//! Service requests arrive unasked on the asynchronous channel (AsyncServiceRequest), so the channel is behind a lock:
//! a request holds it until its answer is in, counting the service requests it passes; `SrqWatch`, while the session
//! has events enabled, holds it for at most `WATCH_SLICE` at a time to read what has come.

use super::{ms, Deadline, Error, LockState, Ren};
use crate::adapter::{ReadEnd, ReadResult};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INITIALIZE: u8 = 0;
const INITIALIZE_RESPONSE: u8 = 1;
const FATAL_ERROR: u8 = 2;
const ERROR: u8 = 3;
const ASYNC_LOCK: u8 = 4;
const ASYNC_LOCK_RESPONSE: u8 = 5;
const DATA: u8 = 6;
const DATA_END: u8 = 7;
const DEVICE_CLEAR_COMPLETE: u8 = 8;
const DEVICE_CLEAR_ACKNOWLEDGE: u8 = 9;
const ASYNC_REMOTE_LOCAL_CONTROL: u8 = 10;
const ASYNC_REMOTE_LOCAL_RESPONSE: u8 = 11;
const TRIGGER: u8 = 12;
const ASYNC_MAXIMUM_MESSAGE_SIZE: u8 = 15;
const ASYNC_MAXIMUM_MESSAGE_SIZE_RESPONSE: u8 = 16;
const ASYNC_INITIALIZE: u8 = 17;
const ASYNC_SERVICE_REQUEST: u8 = 20;
const ASYNC_INITIALIZE_RESPONSE: u8 = 18;
const ASYNC_DEVICE_CLEAR: u8 = 19;
const ASYNC_STATUS_QUERY: u8 = 21;
const ASYNC_STATUS_RESPONSE: u8 = 22;
const ASYNC_DEVICE_CLEAR_ACKNOWLEDGE: u8 = 23;
const ASYNC_LOCK_INFO: u8 = 24;
const ASYNC_LOCK_INFO_RESPONSE: u8 = 25;

const HEADER: usize = 16;
const FIRST_ID: u32 = 0xFFFF_FF00;
/// HiSLIP 1.0 asked for; the instrument answers with what it speaks.
const VERSION: u32 = 0x0100;
/// The vendor id a client sends: two letters. "SY" for Symbiotic.
const VENDOR: u32 = 0x5359;
/// The largest message we take, as told to the instrument: we stream, so this is generous.
const OUR_MAX: u64 = 16 << 20;
/// What we send in one message until the instrument says otherwise (VISA's default, 1 MiB).
const DEFAULT_MAX: u64 = 1 << 20;
/// The most a control message's payload may be: they carry a few bytes, or an error's text.
const MAX_CONTROL: u64 = 64 * 1024;
/// How much longer an asynchronous request waits than the timeout it carries.
const MARGIN: Duration = Duration::from_secs(5);
/// The longest `SrqWatch` holds the asynchronous channel at a time: what a request may wait for it.
const WATCH_SLICE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy)]
struct Header {
    ty: u8,
    control: u8,
    param: u32,
    len: u64,
}

fn encode(ty: u8, control: u8, param: u32, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(HEADER + payload.len());
    m.extend_from_slice(b"HS");
    m.push(ty);
    m.push(control);
    m.extend_from_slice(&param.to_be_bytes());
    m.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    m.extend_from_slice(payload);
    m
}

/// One channel: the socket and what has been received from it and not yet used.
struct Chan {
    s: TcpStream,
    buf: Vec<u8>,
    at: usize,
    /// Payload bytes still to come that belong to nobody: thrown away before the next header.
    skip: u64,
    dead: Option<String>,
}

impl Chan {
    fn new(s: TcpStream) -> Chan {
        Chan { s, buf: Vec::new(), at: 0, skip: 0, dead: None }
    }

    fn check(&self) -> Result<(), Error> {
        match &self.dead {
            Some(why) => Err(Error::Lost(why.clone())),
            None => Ok(()),
        }
    }

    fn kill(&mut self, why: String) -> Error {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
        self.dead = Some(why.clone());
        Error::Lost(why)
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.at..]
    }

    fn consume(&mut self, n: usize) {
        self.at += n;
        if self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        }
    }

    /// Receive more, waiting until the deadline (at least a millisecond, so that an immediate
    /// timeout still takes what is already there). A timeout loses nothing.
    fn fill(&mut self, dl: &Deadline) -> Result<(), Error> {
        self.check()?;
        let wait = dl.left().map(|l| l.max(Duration::from_millis(1)));
        if let Err(e) = self.s.set_read_timeout(wait) {
            return Err(self.kill(e.to_string()));
        }
        if self.at > 0 && self.at * 2 >= self.buf.len() {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        let mut tmp = [0u8; 64 * 1024];
        match self.s.read(&mut tmp) {
            Ok(0) => Err(self.kill("the instrument closed the connection".into())),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(())
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Err(Error::Timeout),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
            Err(e) => Err(self.kill(e.to_string())),
        }
    }

    /// The next header, once all of it is here (and anything owed to `skip` is gone).
    fn header(&mut self, dl: &Deadline) -> Result<Header, Error> {
        while self.skip > 0 {
            if self.avail().is_empty() {
                self.fill(dl)?;
            }
            let n = (self.avail().len() as u64).min(self.skip) as usize;
            self.consume(n);
            self.skip -= n as u64;
        }
        while self.avail().len() < HEADER {
            self.fill(dl)?;
        }
        let h = self.avail();
        if &h[..2] != b"HS" {
            return Err(self.kill("the HiSLIP stream lost its framing".into()));
        }
        let hdr = Header {
            ty: h[2],
            control: h[3],
            param: u32::from_be_bytes(h[4..8].try_into().unwrap()),
            len: u64::from_be_bytes(h[8..16].try_into().unwrap()),
        };
        self.consume(HEADER);
        Ok(hdr)
    }

    /// The whole payload of a control message. It is small; a timeout halfway through it ends the
    /// channel rather than being resumed.
    fn payload(&mut self, h: &Header, dl: &Deadline) -> Result<Vec<u8>, Error> {
        if h.len > MAX_CONTROL {
            return Err(self.kill(format!("a {}-byte payload on message type {}", h.len, h.ty)));
        }
        while (self.avail().len() as u64) < h.len {
            match self.fill(dl) {
                Ok(()) => {}
                Err(Error::Timeout) => return Err(self.kill("timed out in the middle of a message".into())),
                Err(e) => return Err(e),
            }
        }
        let p = self.avail()[..h.len as usize].to_vec();
        self.consume(h.len as usize);
        Ok(p)
    }

    fn send(&mut self, ty: u8, control: u8, param: u32, payload: &[u8], dl: &Deadline) -> Result<(), Error> {
        self.check()?;
        let wait = dl.left().map(|l| l.max(Duration::from_millis(1)));
        let r = self.s.set_write_timeout(wait).and_then(|_| self.s.write_all(&encode(ty, control, param, payload)));
        match r {
            Ok(()) => Ok(()),
            // Part of a message may have gone out: the stream cannot be trusted after that.
            Err(e) => Err(self.kill(format!("sending: {e}"))),
        }
    }
}

/// The frame being read: how much of its payload is still to come, whether it ends a message,
/// and its MessageID.
struct Cur {
    end: bool,
    left: u64,
    id: u32,
}

/// The asynchronous channel, and the service requests that went past while a request was waiting
/// for its answer.
struct Async {
    chan: Chan,
    srq: u32,
}

/// The channel's lock, and how many requests are waiting for it. A mutex is not fair, so the watcher stands aside while
/// any request is waiting.
struct AsyncSide {
    m: Mutex<Async>,
    waiting: AtomicU32,
}

impl AsyncSide {
    /// The lock, for a request: it goes ahead of the watcher.
    fn take(&self) -> std::sync::MutexGuard<'_, Async> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let g = self.m.lock().unwrap();
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        g
    }
}

pub struct Conn {
    sync: Chan,
    asy: Arc<AsyncSide>,
    /// The most payload one message sent may carry.
    max_payload: usize,
    next_id: u32,
    last_id: Option<u32>,
    rmt: bool,
    cur: Option<Cur>,
    /// The protocol version the instrument chose (major << 8 | minor).
    pub version: u16,
    /// Overlapped mode (else synchronized): what the instrument said at Initialize, then what the
    /// last device clear settled on.
    pub overlap: bool,
    /// Overlapped mode: the MessageID of the last of the instrument's messages read to its end,
    /// which a status query quotes (IVI-6.1 3.2.2); FIRST_ID - 2 until there is one.
    delivered: u32,
    /// The largest message this client said it takes (AsyncMaximumMessageSize), in bytes.
    pub our_max: u64,
}

fn text(p: &[u8]) -> String {
    String::from_utf8_lossy(p).trim().to_string()
}

impl Conn {
    pub fn open(host: &str, port: u16, sub: &str, within: Duration) -> Result<Conn, Error> {
        let dl = Deadline::after(Some(within));
        let lost_to_nf = |e: Error| match e {
            Error::Lost(s) => Error::NotFound(s),
            Error::Timeout => Error::NotFound(format!("{host}:{port} did not answer the HiSLIP handshake")),
            e => e,
        };
        let mut sync = Chan::new(super::connect(host, port, within)?);
        sync.send(INITIALIZE, 0, VERSION << 16 | VENDOR, sub.as_bytes(), &dl).map_err(lost_to_nf)?;
        let h = sync.header(&dl).map_err(lost_to_nf)?;
        let p = sync.payload(&h, &dl).map_err(lost_to_nf)?;
        if h.ty != INITIALIZE_RESPONSE {
            return Err(Error::NotFound(format!("{host}:{port} refused {sub:?}: {}", text(&p))));
        }
        let (version, session) = ((h.param >> 16) as u16, h.param & 0xFFFF);
        let mut asy = Chan::new(super::connect(host, port, within)?);
        asy.send(ASYNC_INITIALIZE, 0, session, &[], &dl).map_err(lost_to_nf)?;
        let h2 = asy.header(&dl).map_err(lost_to_nf)?;
        let p2 = asy.payload(&h2, &dl).map_err(lost_to_nf)?;
        if h2.ty != ASYNC_INITIALIZE_RESPONSE {
            return Err(Error::NotFound(format!("{host}:{port}: the asynchronous channel was refused: {}", text(&p2))));
        }
        let mut c = Conn {
            sync,
            asy: Arc::new(AsyncSide { m: Mutex::new(Async { chan: asy, srq: 0 }), waiting: AtomicU32::new(0) }),
            max_payload: (DEFAULT_MAX as usize) - HEADER,
            next_id: FIRST_ID,
            last_id: None,
            rmt: false,
            cur: None,
            version,
            overlap: h.control & 1 != 0,
            delivered: FIRST_ID.wrapping_sub(2),
            our_max: OUR_MAX,
        };
        match c.ask(ASYNC_MAXIMUM_MESSAGE_SIZE, 0, 0, &OUR_MAX.to_be_bytes(), ASYNC_MAXIMUM_MESSAGE_SIZE_RESPONSE, dl) {
            Ok((_, p)) if p.len() == 8 => {
                let theirs = u64::from_be_bytes(p[..8].try_into().unwrap());
                // The size counts the header (pyvisa-py reads it so); never below something usable.
                c.max_payload = theirs.saturating_sub(HEADER as u64).clamp(256, 1 << 30) as usize;
            }
            // An instrument that does not answer this keeps the default.
            Ok(_) | Err(Error::Io(_)) => {}
            Err(e) => return Err(lost_to_nf(e)),
        }
        Ok(c)
    }

    /// VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB set: say the new largest message this client takes, and
    /// take the instrument's in return, as at the handshake.
    pub fn set_max_message(&mut self, bytes: u64, dl: Deadline) -> Result<(), Error> {
        let (_, p) = self.ask(ASYNC_MAXIMUM_MESSAGE_SIZE, 0, 0, &bytes.to_be_bytes(), ASYNC_MAXIMUM_MESSAGE_SIZE_RESPONSE, dl)?;
        if p.len() == 8 {
            let theirs = u64::from_be_bytes(p[..8].try_into().unwrap());
            self.max_payload = theirs.saturating_sub(HEADER as u64).clamp(256, 1 << 30) as usize;
        }
        self.our_max = bytes;
        Ok(())
    }

    /// An asynchronous request and its answer. Unrequested messages that arrive first (a service
    /// request) are passed over. An Error answer is `Io`; a timeout ends the channel.
    fn ask(&mut self, ty: u8, control: u8, param: u32, payload: &[u8], want: u8, dl: Deadline) -> Result<(Header, Vec<u8>), Error> {
        let mut a = self.asy.take();
        a.chan.send(ty, control, param, payload, &dl)?;
        loop {
            let h = match a.chan.header(&dl) {
                Ok(h) => h,
                Err(Error::Timeout) => return Err(a.chan.kill("an asynchronous request timed out".into())),
                Err(e) => return Err(e),
            };
            let p = a.chan.payload(&h, &dl)?;
            match h.ty {
                t if t == want => return Ok((h, p)),
                ERROR => return Err(Error::Io(format!("the instrument answered error {}: {}", h.control, text(&p)))),
                FATAL_ERROR => return Err(a.chan.kill(format!("fatal error {}: {}", h.control, text(&p)))),
                ASYNC_SERVICE_REQUEST => a.srq = a.srq.saturating_add(1),
                _ => {}
            }
        }
    }

    /// The synchronous channel, for its TCP options.
    pub(super) fn stream(&self) -> &TcpStream {
        &self.sync.s
    }

    /// `f` on both channels' connections, the synchronous one first.
    pub(super) fn each_stream(&self, mut f: impl FnMut(&TcpStream) -> std::io::Result<()>) -> std::io::Result<()> {
        f(&self.sync.s)?;
        f(&self.asy.m.lock().unwrap().chan.s)
    }

    /// Start watching for service requests: the ones that went past before this are forgotten.
    pub fn watch_srq(&mut self) -> SrqWatch {
        self.asy.take().srq = 0;
        SrqWatch(self.asy.clone())
    }

    /// Send one message on the synchronous channel. In synchronized mode what is left of an
    /// answer being read belongs to nobody after this (IVI-6.1 3.1.2, rule 3); in overlapped mode
    /// it is still the caller's.
    fn send(&mut self, ty: u8, payload: &[u8], dl: &Deadline) -> Result<(), Error> {
        if !self.overlap {
            if let Some(c) = self.cur.take() {
                self.sync.skip += c.left;
            }
        }
        self.sync.send(ty, self.rmt as u8, self.next_id, payload, dl)?;
        self.rmt = false;
        self.last_id = Some(self.next_id);
        self.next_id = self.next_id.wrapping_add(2);
        Ok(())
    }

    pub fn write(&mut self, data: &[u8], end: bool, dl: Deadline) -> Result<usize, (usize, Error)> {
        let mut sent = 0;
        loop {
            let part = &data[sent..data.len().min(sent + self.max_payload)];
            let last = sent + part.len() == data.len();
            if last && !end && part.is_empty() {
                return Ok(sent);
            }
            let ty = if last && end { DATA_END } else { DATA };
            self.send(ty, part, &dl).map_err(|e| (sent, e))?;
            sent += part.len();
            if last {
                return Ok(sent);
            }
        }
    }

    pub fn read(&mut self, max: usize, term: Option<u8>, suppress_end: bool, dl: Deadline) -> Result<ReadResult, Error> {
        let mut out = Vec::new();
        let done = |data, end| Ok(ReadResult { data, end });
        loop {
            if self.cur.is_none() {
                if out.len() >= max {
                    return done(out, ReadEnd::MaxCount);
                }
                let h = match self.sync.header(&dl) {
                    Ok(h) => h,
                    Err(Error::Timeout) => return done(out, ReadEnd::Timeout),
                    Err(e) => return Err(e),
                };
                match h.ty {
                    DATA | DATA_END if self.overlap || Some(h.param) == self.last_id || h.param == u32::MAX => {
                        self.cur = Some(Cur { end: h.ty == DATA_END, left: h.len, id: h.param });
                    }
                    // A late answer to an earlier message, or an Interrupted: not this read's.
                    DATA | DATA_END => self.sync.skip = h.len,
                    ERROR => {
                        let p = self.sync.payload(&h, &dl)?;
                        return Err(Error::Io(format!("the instrument answered error {}: {}", h.control, text(&p))));
                    }
                    FATAL_ERROR => {
                        let p = self.sync.payload(&h, &dl).unwrap_or_default();
                        return Err(self.sync.kill(format!("fatal error {}: {}", h.control, text(&p))));
                    }
                    _ => self.sync.skip = h.len,
                }
                continue;
            }
            let c = self.cur.as_mut().unwrap();
            let mut hit = false;
            while c.left > 0 && out.len() < max {
                if self.sync.avail().is_empty() {
                    match self.sync.fill(&dl) {
                        Ok(()) => {}
                        Err(Error::Timeout) => return done(out, ReadEnd::Timeout),
                        Err(e) => return Err(e),
                    }
                }
                let avail = self.sync.avail();
                let mut n = avail.len().min(max - out.len()).min(c.left.min(usize::MAX as u64) as usize);
                if let Some(i) = term.and_then(|t| avail[..n].iter().position(|b| *b == t)) {
                    n = i + 1;
                    hit = true;
                }
                out.extend_from_slice(&avail[..n]);
                self.sync.consume(n);
                c.left -= n as u64;
                if hit {
                    break;
                }
            }
            if c.left == 0 {
                let end = c.end;
                self.delivered = c.id;
                self.cur = None;
                if end {
                    self.rmt = true;
                    // END first (VPP-4.3 RULE 6.1.1), even when the last byte was the term char.
                    if !suppress_end {
                        return done(out, ReadEnd::End);
                    }
                }
            }
            if hit {
                return done(out, ReadEnd::TermChar);
            }
            if out.len() >= max {
                return done(out, ReadEnd::MaxCount);
            }
        }
    }

    /// AsyncDeviceClear, then DeviceClearComplete on the synchronous channel and everything up to
    /// its acknowledgement thrown away; message ids start again. The session stays in its mode.
    pub fn clear(&mut self, dl: Deadline) -> Result<(), Error> {
        self.clear_asking(self.overlap, dl)
    }

    /// Ask for overlapped mode, or synchronized: a device clear that requests it (IVI-6.1 6.12.1).
    /// The instrument may say no; `overlap` is what it settled on.
    pub fn set_overlap(&mut self, overlapped: bool, dl: Deadline) -> Result<(), Error> {
        self.clear_asking(overlapped, dl)
    }

    /// A device clear whose DeviceClearComplete requests `overlapped` -- the session's own mode on
    /// a plain clear, never the instrument's proposal in AsyncDeviceClearAcknowledge, or the first
    /// viClear would change modes -- and the mode its DeviceClearAcknowledge settles on.
    fn clear_asking(&mut self, overlapped: bool, dl: Deadline) -> Result<(), Error> {
        self.ask(ASYNC_DEVICE_CLEAR, 0, 0, &[], ASYNC_DEVICE_CLEAR_ACKNOWLEDGE, dl)?;
        if let Some(c) = self.cur.take() {
            self.sync.skip += c.left;
        }
        self.sync.send(DEVICE_CLEAR_COMPLETE, overlapped as u8, 0, &[], &dl)?;
        let agreed = loop {
            let h = self.sync.header(&dl)?;
            self.sync.skip = h.len;
            if h.ty == DEVICE_CLEAR_ACKNOWLEDGE {
                break h.control;
            }
        };
        self.overlap = agreed & 1 != 0;
        self.next_id = FIRST_ID;
        self.last_id = None;
        self.delivered = FIRST_ID.wrapping_sub(2);
        self.rmt = false;
        Ok(())
    }

    pub fn trigger(&mut self, dl: Deadline) -> Result<(), Error> {
        self.send(TRIGGER, &[], &dl)
    }

    pub fn read_stb(&mut self, dl: Deadline) -> Result<u8, Error> {
        // Synchronized: the last id sent, FIRST_ID - 2 before any (IVI-6.1 6.14); an instrument
        // that finds it is not the last message it received reports MAV clear (6.14.3).
        // Overlapped: the last of the instrument's messages read (3.2.2), from which it works MAV out.
        let id = if self.overlap { self.delivered } else { self.next_id.wrapping_sub(2) };
        let (h, _) = self.ask(ASYNC_STATUS_QUERY, self.rmt as u8, id, &[], ASYNC_STATUS_RESPONSE, dl)?;
        self.rmt = false;
        Ok(h.control)
    }

    /// AsyncRemoteLocalControl's codes are VISA's REN modes, one for one.
    pub fn ren(&mut self, mode: Ren, dl: Deadline) -> Result<(), Error> {
        let id = self.last_id.unwrap_or(0);
        self.ask(ASYNC_REMOTE_LOCAL_CONTROL, mode as u8, id, &[], ASYNC_REMOTE_LOCAL_RESPONSE, dl).map(|_| ())
    }

    /// Exclusive with no key, shared under `key`; the instrument waits at most `wait` for it.
    pub fn lock(&mut self, key: Option<&str>, wait: Option<Duration>) -> Result<(), Error> {
        let dl = Deadline::after(wait.map(|w| w + MARGIN));
        let (h, _) = self.ask(ASYNC_LOCK, 1, ms(wait), key.unwrap_or("").as_bytes(), ASYNC_LOCK_RESPONSE, dl)?;
        match h.control {
            1 | 2 => Ok(()),
            0 => Err(Error::Locked),
            n => Err(Error::Io(format!("the lock request failed ({n})"))),
        }
    }

    pub fn unlock(&mut self) -> Result<(), Error> {
        let id = self.last_id.unwrap_or(0);
        let (h, _) = self.ask(ASYNC_LOCK, 0, id, &[], ASYNC_LOCK_RESPONSE, Deadline::after(Some(MARGIN)))?;
        match h.control {
            1 | 2 => Ok(()),
            _ => Err(Error::NotLocked),
        }
    }

    /// HiSLIP 1.1's AsyncLockInfo: an exclusive lock granted to anybody, and how many clients hold
    /// a lock. A 1.0 instrument answers Error (`Io`).
    pub fn lock_info(&mut self, dl: Deadline) -> Result<LockState, Error> {
        let (h, _) = self.ask(ASYNC_LOCK_INFO, 0, 0, &[], ASYNC_LOCK_INFO_RESPONSE, dl)?;
        Ok(match (h.control, h.param) {
            (1, _) => LockState::Exclusive,
            (_, 0) => LockState::None,
            _ => LockState::Shared,
        })
    }
}

/// The service requests of one HiSLIP session, for the thread that delivers its events.
#[derive(Clone)]
pub struct SrqWatch(Arc<AsyncSide>);

impl SrqWatch {
    /// Wait up to `wait` for a service request; Ok(true) is one. The channel is held for at most
    /// `WATCH_SLICE` at a time, so a request made meanwhile waits no longer than that.
    pub fn wait(&self, wait: Duration) -> Result<bool, Error> {
        let end = Deadline::after(Some(wait));
        loop {
            // A request waiting for the channel goes first.
            while self.0.waiting.load(Ordering::SeqCst) > 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            {
                let mut a = self.0.m.lock().unwrap();
                if a.srq > 0 {
                    a.srq -= 1;
                    return Ok(true);
                }
                let slice = Deadline::after(Some(end.left().unwrap_or_default().min(WATCH_SLICE)));
                match a.chan.header(&slice) {
                    Ok(h) => {
                        // The header is in; what follows it is not held to the slice.
                        a.chan.payload(&h, &Deadline::after(Some(Duration::from_secs(2))))?;
                        match h.ty {
                            ASYNC_SERVICE_REQUEST => return Ok(true),
                            FATAL_ERROR => return Err(a.chan.kill(format!("fatal error {}", h.control))),
                            // Nothing else comes unasked; an answer nobody waits for is dropped.
                            _ => {}
                        }
                    }
                    Err(Error::Timeout) => {}
                    Err(e) => return Err(e),
                }
            }
            if end.expired() {
                return Ok(false);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A HiSLIP instrument in threads: answers `*IDN?` (in `chunk`-sized Data frames, the last a
    //! DataEnd) and `*OPC?`, answers other queries with nothing, and handles the asynchronous
    //! requests. It starts in overlapped mode if `overlapped`, and a device clear puts it in the mode
    //! asked for unless `sync_only`; in overlapped mode its answers carry its own count.
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Dev {
        pub answer: Vec<u8>,
        pub chunk: usize,
        /// Messages received on the synchronous channel: (type, control, id, payload).
        pub got: Vec<(u8, u8, u32, Vec<u8>)>,
        /// The next answer goes out with this id instead of the query's.
        pub stale_id: Option<u32>,
        pub locked: bool,
        /// The asynchronous channel, for pushing service requests: a message `SRQ` asks for one.
        pub asy: Option<TcpStream>,
        pub overlapped: bool,
        pub sync_only: bool,
        /// Overlapped mode: the id the next Data or DataEnd goes out with.
        pub reply_id: u32,
        /// The MessageIDs status queries quoted.
        pub stb_ids: Vec<u32>,
    }

    fn read_msg(s: &mut TcpStream) -> Option<(Header, Vec<u8>)> {
        let mut h = [0u8; HEADER];
        s.read_exact(&mut h).ok()?;
        let hdr = Header {
            ty: h[2],
            control: h[3],
            param: u32::from_be_bytes(h[4..8].try_into().unwrap()),
            len: u64::from_be_bytes(h[8..16].try_into().unwrap()),
        };
        let mut p = vec![0u8; hdr.len as usize];
        s.read_exact(&mut p).ok()?;
        Some((hdr, p))
    }

    pub fn serve(dev: Arc<Mutex<Dev>>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let dev = dev.clone();
                std::thread::spawn(move || {
                    let Some((h, _)) = read_msg(&mut s) else { return };
                    if h.ty == INITIALIZE {
                        let mode = {
                            let mut d = dev.lock().unwrap();
                            d.reply_id = FIRST_ID;
                            d.overlapped as u8
                        };
                        s.write_all(&encode(INITIALIZE_RESPONSE, mode, 0x0100_0000 | 7, &[])).unwrap();
                        sync_loop(s, dev);
                    } else if h.ty == ASYNC_INITIALIZE {
                        s.write_all(&encode(ASYNC_INITIALIZE_RESPONSE, 0, 0, &[])).unwrap();
                        dev.lock().unwrap().asy = Some(s.try_clone().unwrap());
                        async_loop(s, dev);
                    }
                });
            }
        });
        port
    }

    fn sync_loop(mut s: TcpStream, dev: Arc<Mutex<Dev>>) {
        let mut msg = Vec::new();
        while let Some((h, p)) = read_msg(&mut s) {
            let mut d = dev.lock().unwrap();
            d.got.push((h.ty, h.control, h.param, p.clone()));
            match h.ty {
                DATA => msg.extend_from_slice(&p),
                DATA_END => {
                    msg.extend_from_slice(&p);
                    if msg.starts_with(b"SRQ") {
                        let asy = d.asy.as_mut().unwrap();
                        asy.write_all(&encode(ASYNC_SERVICE_REQUEST, 0x50, 0, &[])).unwrap();
                    }
                    let answer = if msg.starts_with(b"*IDN?") {
                        Some(d.answer.clone())
                    } else if msg.starts_with(b"*OPC?") {
                        Some(b"1\n".to_vec())
                    } else {
                        None
                    };
                    if let Some(a) = answer {
                        let chunk = d.chunk.max(1);
                        let parts: Vec<&[u8]> = a.chunks(chunk).collect();
                        for (i, part) in parts.iter().enumerate() {
                            let ty = if i + 1 == parts.len() { DATA_END } else { DATA };
                            let id = if d.overlapped {
                                d.reply_id = d.reply_id.wrapping_add(2);
                                d.reply_id.wrapping_sub(2)
                            } else {
                                d.stale_id.take().unwrap_or(h.param)
                            };
                            s.write_all(&encode(ty, 0, id, part)).unwrap();
                        }
                    }
                    msg.clear();
                }
                DEVICE_CLEAR_COMPLETE => {
                    d.overlapped = h.control & 1 != 0 && !d.sync_only;
                    d.reply_id = FIRST_ID;
                    s.write_all(&encode(DEVICE_CLEAR_ACKNOWLEDGE, d.overlapped as u8, 0, &[])).unwrap();
                }
                _ => {}
            }
        }
    }

    fn async_loop(mut s: TcpStream, dev: Arc<Mutex<Dev>>) {
        while let Some((h, _p)) = read_msg(&mut s) {
            let mut d = dev.lock().unwrap();
            let m = match h.ty {
                ASYNC_MAXIMUM_MESSAGE_SIZE => encode(ASYNC_MAXIMUM_MESSAGE_SIZE_RESPONSE, 0, 0, &(HEADER as u64 + 300).to_be_bytes()),
                ASYNC_STATUS_QUERY => {
                    d.stb_ids.push(h.param);
                    // An unrequested service request first: the client must pass over it.
                    s.write_all(&encode(20, 0x40, 0, &[])).unwrap();
                    encode(ASYNC_STATUS_RESPONSE, 0x10, 0, &[])
                }
                // It proposes the other mode: a client asks for the session's own, not the proposal.
                ASYNC_DEVICE_CLEAR => encode(ASYNC_DEVICE_CLEAR_ACKNOWLEDGE, !d.overlapped as u8, 0, &[]),
                ASYNC_LOCK if h.control == 1 => {
                    let ok = !d.locked;
                    d.locked = true;
                    encode(ASYNC_LOCK_RESPONSE, ok as u8, 0, &[])
                }
                ASYNC_LOCK => {
                    let ok = d.locked;
                    d.locked = false;
                    encode(ASYNC_LOCK_RESPONSE, if ok { 1 } else { 3 }, 0, &[])
                }
                ASYNC_LOCK_INFO => encode(ASYNC_LOCK_INFO_RESPONSE, d.locked as u8, d.locked as u32, &[]),
                ASYNC_REMOTE_LOCAL_CONTROL => encode(ASYNC_REMOTE_LOCAL_RESPONSE, 0, 0, &[]),
                _ => encode(ERROR, 1, 0, b"unrecognized"),
            };
            s.write_all(&m).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{serve, Dev};
    use super::*;
    use std::sync::{Arc, Mutex};

    fn dl() -> Deadline {
        Deadline::after(Some(Duration::from_secs(2)))
    }

    fn open(dev: &Arc<Mutex<Dev>>) -> Conn {
        let port = serve(dev.clone());
        Conn::open("127.0.0.1", port, "hislip0", Duration::from_secs(2)).unwrap()
    }

    /// What the instrument has received once it has `n` messages (it runs in its own thread).
    fn got(dev: &Arc<Mutex<Dev>>, n: usize) -> Vec<(u8, u8, u32, Vec<u8>)> {
        for _ in 0..200 {
            let g = dev.lock().unwrap().got.clone();
            if g.len() >= n {
                return g;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the instrument did not get {n} messages");
    }

    fn idn(answer: &[u8], chunk: usize) -> Arc<Mutex<Dev>> {
        Arc::new(Mutex::new(Dev { answer: answer.to_vec(), chunk, ..Default::default() }))
    }

    #[test]
    fn a_query_its_answer_and_the_message_size() {
        let dev = idn(b"FAKE,HISLIP,0,1.0\n", 5);
        let mut c = open(&dev);
        assert_eq!(c.max_payload, 300, "the instrument's limit, less the header");
        assert_eq!(c.write(b"*IDN?\n", true, dl()), Ok(6));
        let r = c.read(1000, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"FAKE,HISLIP,0,1.0\n"[..], ReadEnd::End));
        // The next message says the answer was taken.
        c.write(b"X\n", true, dl()).unwrap();
        let got = got(&dev, 2);
        assert_eq!((got[0].1, got[0].2), (0, FIRST_ID));
        assert_eq!((got[1].1, got[1].2), (1, FIRST_ID + 2));
    }

    #[test]
    fn tcp_options_reach_both_channels() {
        use super::super::{Session, TcpOpt};
        let dev = idn(b"x\n", 100);
        let mut s = Session::Hislip(open(&dev));
        s.set_tcp_opt(TcpOpt::KeepAlive, true).unwrap();
        assert!(s.tcp_opt(TcpOpt::KeepAlive).unwrap());
        let Session::Hislip(c) = &s else { unreachable!() };
        let mut on = vec![];
        c.each_stream(|t| socket2::SockRef::from(t).keepalive().map(|k| on.push(k))).unwrap();
        assert_eq!(on, [true, true]);
    }

    fn query(c: &mut Conn, q: &[u8]) -> Vec<u8> {
        c.write(q, true, dl()).unwrap();
        c.read(1000, None, false, dl()).unwrap().data
    }

    #[test]
    fn overlapped_from_the_start_queues_answers_and_keeps_a_part_read_one() {
        let dev = idn(b"FAKE,HISLIP,0,1.0\n", 5);
        dev.lock().unwrap().overlapped = true;
        let mut c = open(&dev);
        assert!(c.overlap, "the mode the instrument said at Initialize");
        // A command with no answer first: from here on the instrument's ids are not ours.
        c.write(b"*CLS\n", true, dl()).unwrap();
        for q in [&b"*IDN?\n"[..], b"*OPC?\n", b"*IDN?\n"] {
            c.write(q, true, dl()).unwrap();
        }
        let head = c.read(4, None, false, dl()).unwrap();
        assert_eq!((head.data.as_slice(), head.end), (&b"FAKE"[..], ReadEnd::MaxCount));
        c.write(b"*OPC?\n", true, dl()).unwrap(); // sending keeps what is left of it
        assert_eq!(c.read(1000, None, false, dl()).unwrap().data, b",HISLIP,0,1.0\n");
        assert_eq!(c.read(1000, None, false, dl()).unwrap().data, b"1\n");
        assert_eq!(c.read(1000, None, false, dl()).unwrap().data, b"FAKE,HISLIP,0,1.0\n");
        assert_eq!(c.read(1000, None, false, dl()).unwrap().data, b"1\n");
    }

    #[test]
    fn a_status_query_quotes_by_the_mode() {
        // Synchronized: the last id sent, FIRST_ID - 2 before any.
        let dev = idn(b"x\n", 100);
        let mut c = open(&dev);
        c.read_stb(dl()).unwrap();
        c.write(b"*CLS\n", true, dl()).unwrap();
        c.read_stb(dl()).unwrap();
        assert_eq!(dev.lock().unwrap().stb_ids, [FIRST_ID.wrapping_sub(2), FIRST_ID]);
        // Overlapped: the last answer read, FIRST_ID - 2 before any and after a clear.
        let dev = idn(b"x\n", 100);
        dev.lock().unwrap().overlapped = true;
        let mut c = open(&dev);
        c.read_stb(dl()).unwrap();
        c.write(b"*CLS\n", true, dl()).unwrap();
        assert_eq!(query(&mut c, b"*IDN?\n"), b"x\n");
        c.read_stb(dl()).unwrap();
        c.clear(dl()).unwrap();
        c.read_stb(dl()).unwrap();
        assert_eq!(dev.lock().unwrap().stb_ids, [FIRST_ID.wrapping_sub(2), FIRST_ID, FIRST_ID.wrapping_sub(2)]);
    }

    #[test]
    fn the_mode_is_asked_for_by_a_device_clear_and_a_clear_keeps_it() {
        let dev = idn(b"x\n", 100);
        let mut c = open(&dev);
        c.set_overlap(true, dl()).unwrap();
        assert!(c.overlap && dev.lock().unwrap().overlapped);
        // The instrument now proposes synchronized; a plain clear still asks for overlapped.
        c.clear(dl()).unwrap();
        assert!(c.overlap);
        let asked: Vec<u8> = dev.lock().unwrap().got.iter().filter(|m| m.0 == DEVICE_CLEAR_COMPLETE).map(|m| m.1).collect();
        assert_eq!(asked, [1, 1]);
        c.set_overlap(false, dl()).unwrap();
        assert!(!c.overlap);
        assert_eq!(query(&mut c, b"*IDN?\n"), b"x\n");
        // One that has no overlapped mode says no, and the session stays synchronized.
        let dev = idn(b"x\n", 100);
        dev.lock().unwrap().sync_only = true;
        let mut c = open(&dev);
        c.set_overlap(true, dl()).unwrap();
        assert!(!c.overlap);
        assert_eq!(query(&mut c, b"*IDN?\n"), b"x\n");
    }

    #[test]
    fn a_long_write_is_several_messages_and_one_data_end() {
        let dev = idn(b"x\n", 100);
        let mut c = open(&dev);
        let mut msg = b"*IDN?".to_vec();
        msg.resize(700, b' ');
        assert_eq!(c.write(&msg, true, dl()), Ok(700));
        let r = c.read(10, None, false, dl()).unwrap();
        assert_eq!(r.data, b"x\n");
        let types: Vec<u8> = got(&dev, 3).iter().map(|g| g.0).collect();
        assert_eq!(types, vec![DATA, DATA, DATA_END]);
    }

    #[test]
    fn a_read_stops_at_count_and_term_char_and_the_rest_waits() {
        let dev = idn(b"1,2,3\n", 4);
        let mut c = open(&dev);
        c.write(b"*IDN?\n", true, dl()).unwrap();
        let r = c.read(2, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"1,"[..], ReadEnd::MaxCount));
        let r = c.read(100, Some(b','), false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"2,"[..], ReadEnd::TermChar));
        // The term char that ends the message is END first.
        let r = c.read(100, Some(b'\n'), false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"3\n"[..], ReadEnd::End));
    }

    #[test]
    fn a_timeout_loses_nothing_and_a_late_answer_is_dropped() {
        let dev = idn(b"late\n", 100);
        let mut c = open(&dev);
        // Nothing asked: nothing comes.
        let r = c.read(100, None, false, Deadline::after(Some(Duration::from_millis(50)))).unwrap();
        assert_eq!((r.data.len(), r.end), (0, ReadEnd::Timeout));
        // An answer that carries an old id is the late answer to something else.
        dev.lock().unwrap().stale_id = Some(0x1234);
        c.write(b"*IDN?\n", true, dl()).unwrap();
        c.write(b"*IDN?\n", true, dl()).unwrap();
        let r = c.read(100, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"late\n"[..], ReadEnd::End));
        let r = c.read(100, None, false, Deadline::after(Some(Duration::from_millis(50)))).unwrap();
        assert_eq!(r.end, ReadEnd::Timeout, "the stale one was dropped, not delivered");
    }

    #[test]
    fn a_half_read_answer_is_skipped_by_the_next_message() {
        let dev = idn(b"abcdefgh\n", 100);
        let mut c = open(&dev);
        c.write(b"*IDN?\n", true, dl()).unwrap();
        assert_eq!(c.read(3, None, false, dl()).unwrap().data, b"abc");
        c.write(b"*IDN?\n", true, dl()).unwrap();
        let r = c.read(100, None, false, dl()).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"abcdefgh\n"[..], ReadEnd::End));
    }

    #[test]
    fn service_requests_are_watched_and_requests_still_answered() {
        let dev = idn(b"x\n", 100);
        let mut c = open(&dev);
        // One that goes past a request before anyone watches is forgotten when watching starts.
        assert_eq!(c.read_stb(dl()), Ok(0x10));
        let w = c.watch_srq();
        assert_eq!(w.wait(Duration::from_millis(60)), Ok(false));
        c.write(b"SRQ\n", true, dl()).unwrap();
        assert_eq!(w.wait(Duration::from_secs(2)), Ok(true));
        // A watcher in another thread does not keep requests from their answers.
        let w2 = w.clone();
        let t = std::thread::spawn(move || w2.wait(Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(20));
        let t0 = std::time::Instant::now();
        assert_eq!(c.read_stb(dl()), Ok(0x10));
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
        // The status query's own service request, counted on the way, is the watcher's.
        assert_eq!(t.join().unwrap(), Ok(true));
    }

    #[test]
    fn asynchronous_requests() {
        let dev = idn(b"x\n", 100);
        let mut c = open(&dev);
        assert_eq!(c.read_stb(dl()), Ok(0x10), "the service request in front is passed over");
        assert_eq!(c.lock(None, Some(Duration::ZERO)), Ok(()));
        assert_eq!(c.lock_info(dl()), Ok(LockState::Exclusive));
        assert_eq!(c.lock(None, Some(Duration::ZERO)), Err(Error::Locked));
        assert_eq!(c.unlock(), Ok(()));
        assert_eq!(c.unlock(), Err(Error::NotLocked));
        assert_eq!(c.ren(Ren::AssertAddress, dl()), Ok(()));
        c.write(b"*IDN?\n", true, dl()).unwrap();
        assert_eq!(c.clear(dl()), Ok(()));
        // The answer that was waiting went with the clear; ids start again.
        let r = c.read(100, None, false, Deadline::after(Some(Duration::from_millis(50)))).unwrap();
        assert_eq!(r.end, ReadEnd::Timeout);
        c.write(b"*IDN?\n", true, dl()).unwrap();
        assert_eq!(got(&dev, 3)[2].2, FIRST_ID);
        assert_eq!(c.read(100, None, false, dl()).unwrap().data, b"x\n");
    }
}
