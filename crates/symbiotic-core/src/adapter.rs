//! One Viper488 adapter over WinUSB: request/response framing, and the typed operations
//! the VISA layer builds on.
//!
//! The protocol is strictly one request, one response, and the adapter has one message
//! open at a time, so the whole exchange is serialised by one mutex: a caller that needs
//! several requests to be one operation (a large write, a read that spans READ frames)
//! holds the guard from `lock()` across them.

use crate::proto::*;
use nusb::transfer::{Bulk, In, Out, TransferError};
use nusb::MaybeFuture;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The least per-byte stall a READ frame gets, however little of the read's timeout is left (`read_message_op`).
const READ_GRACE: Duration = Duration::from_millis(250);

pub const VID: u16 = 0x1209;
/// Viper488 Classic, Dualic, Kinetic (WinUSB personality).
pub const PIDS: [u16; 3] = [0x0405, 0x0406, 0x0407];
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const IN_BUF: usize = 65536;
/// One WRITE frame. The firmware streams a frame's payload to the bus, so this only bounds
/// how long the bus is held per request.
pub const WRITE_CHUNK: usize = 16384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A GPIB handshake timed out.
    Timeout,
    /// Nothing listens at that address.
    NoDevice,
    /// The adapter is open in another process (Windows gives WinUSB to one at a time).
    Busy,
    /// The adapter went away (or, seen from a client, the connection to the daemon did).
    Gone,
    /// No adapter has that board number.
    NoBoard,
    /// The adapter answered with a status this operation cannot use.
    Status(u8),
    /// USB or framing failure.
    Io(String),
    /// Another session holds a lock on the resource (viLock).
    Locked,
    /// viUnlock by a session that holds no lock on the resource.
    NotLocked,
    /// A nested shared lock asked for with another access key than the session holds.
    LockKey,
    /// Seen only by a client: the connection to the daemon was gone before the request went out,
    /// so nothing of it ran and it may be sent again on a new connection.
    NotSent,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Timeout => write!(f, "GPIB timeout"),
            Error::NoDevice => write!(f, "no device at that address"),
            Error::Busy => write!(f, "adapter is in use by another process"),
            Error::Gone => write!(f, "adapter disconnected"),
            Error::NoBoard => write!(f, "no adapter with that board number"),
            Error::Status(s) => write!(f, "adapter status {}", status_name(*s)),
            Error::Io(m) => write!(f, "{m}"),
            Error::Locked => write!(f, "another session has the resource locked"),
            Error::NotLocked => write!(f, "this session holds no lock on the resource"),
            Error::LockKey => write!(f, "the access key is not the one this session's shared lock has"),
            Error::NotSent => write!(f, "the connection to the daemon was gone before the request was sent"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub serial: String,
    pub pid: u16,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub proto_version: u16,
    pub serial: u32,
    pub caps: u32,
    pub max_read_chunk: u32,
    pub name: String,
}

/// What a READ returned. `timed_out` still carries whatever arrived before the timeout.
#[derive(Debug, Default)]
pub struct ReadOut {
    pub data: Vec<u8>,
    pub reason: u8,
    pub timed_out: bool,
}

/// Why a whole-message read stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadEnd {
    /// EOI: the instrument's message is over.
    End = 0,
    /// The termination character arrived (the message may go on).
    TermChar = 1,
    /// The requested count was reached.
    MaxCount = 2,
    /// The GPIB timeout expired; `data` is what arrived before it.
    Timeout = 3,
}

impl ReadEnd {
    pub fn from_u32(v: u32) -> Option<ReadEnd> {
        [ReadEnd::End, ReadEnd::TermChar, ReadEnd::MaxCount, ReadEnd::Timeout].into_iter().find(|e| *e as u32 == v)
    }
}

#[derive(Debug)]
pub struct ReadResult {
    pub data: Vec<u8>,
    pub end: ReadEnd,
}

struct Pipe {
    ep_out: nusb::Endpoint<Bulk, Out>,
    ep_in: nusb::Endpoint<Bulk, In>,
    tag: u16,
    gone: bool,
}

pub struct Adapter {
    pipe: Mutex<Pipe>,
    pub info: Info,
}

/// A held bus: all requests through one guard are back to back.
pub struct Bus<'a> {
    pipe: MutexGuard<'a, Pipe>,
    max_read_chunk: u32,
    caps: u32,
}

/// Every Viper488 WinUSB adapter on the machine, ordered by serial, so the order (and with
/// it the GPIB board number) does not depend on which was plugged in first.
pub fn list() -> Result<Vec<DeviceEntry>, Error> {
    let mut v: Vec<DeviceEntry> = nusb::list_devices()
        .wait()
        .map_err(|e| Error::Io(format!("cannot enumerate USB devices: {e}")))?
        .filter(|d| d.vendor_id() == VID && PIDS.contains(&d.product_id()))
        .map(|d| DeviceEntry { serial: d.serial_number().unwrap_or("").to_string(), pid: d.product_id() })
        .collect();
    v.sort_by(|a, b| a.serial.cmp(&b.serial));
    Ok(v)
}

/// Why claiming an interface failed. On Linux a kernel driver holding the interface (usbtmc, linux-gpib) is named; it
/// is never detached.
pub(crate) fn claim_error(info: &nusb::DeviceInfo, iface: u8, what: &str, e: impl std::fmt::Display) -> Error {
    if let Some(d) = kernel_driver(info, iface) {
        return Error::Io(format!(
            "{what}: interface {iface} belongs to the kernel driver {d}, which this runtime leaves it to"
        ));
    }
    open_error(what, e)
}

/// Put a device that nothing has configured into its first configuration, so its interfaces can be claimed. macOS
/// leaves vendor-class devices (such as the 82357B) unconfigured; on Linux and Windows this does nothing.
pub(crate) fn configure(device: &nusb::Device) {
    #[cfg(not(windows))]
    if device.active_configuration().is_err() {
        if let Some(v) = device.configurations().next().map(|c| c.configuration_value()) {
            let _ = device.set_configuration(v).wait();
        }
    }
    #[cfg(windows)]
    let _ = device;
}

/// The kernel driver bound to one interface of a device, if any other than usbfs (which is a
/// process's claim, not a driver's). Linux only; elsewhere None.
#[cfg(target_os = "linux")]
pub fn kernel_driver(info: &nusb::DeviceInfo, iface: u8) -> Option<String> {
    let dev = info.sysfs_path();
    let name = dev.file_name()?.to_str()?;
    let config = std::fs::read_to_string(dev.join("bConfigurationValue")).ok()?;
    let link = std::fs::read_link(dev.join(format!("{name}:{}.{iface}", config.trim())).join("driver")).ok()?;
    let driver = link.file_name()?.to_str()?.to_string();
    (driver != "usbfs").then_some(driver)
}

#[cfg(not(target_os = "linux"))]
pub fn kernel_driver(_: &nusb::DeviceInfo, _: u8) -> Option<String> {
    None
}

pub(crate) fn open_error(what: &str, e: impl std::fmt::Display) -> Error {
    let m = e.to_string();
    let l = m.to_lowercase();
    // Windows reports a WinUSB device held by another process as "access denied" (error 5).
    let denied = l.contains("denied") || l.contains("error 5)") || l.contains("os error 5");
    // On Linux "denied" means the device node's permissions.
    if cfg!(not(windows)) && denied {
        Error::Io(format!("{what}: {m} (no permission on the USB device node: a udev rule must give this user access)"))
    } else if denied || l.contains("busy") {
        Error::Busy
    } else {
        Error::Io(format!("{what}: {m}"))
    }
}

impl Adapter {
    pub fn open(serial: &str) -> Result<Adapter, Error> {
        let dev = nusb::list_devices()
            .wait()
            .map_err(|e| Error::Io(format!("cannot enumerate USB devices: {e}")))?
            .find(|d| {
                d.vendor_id() == VID && PIDS.contains(&d.product_id()) && d.serial_number().unwrap_or("") == serial
            })
            .ok_or(Error::Gone)?;
        let device = dev.open().wait().map_err(|e| open_error("open", e))?;
        crate::adapter::configure(&device);
        let iface = device.claim_interface(0).wait().map_err(|e| claim_error(&dev, 0, "claim interface 0", e))?;
        let ep_out = iface.endpoint::<Bulk, Out>(EP_OUT).map_err(|e| Error::Io(format!("bulk OUT: {e}")))?;
        let ep_in = iface.endpoint::<Bulk, In>(EP_IN).map_err(|e| Error::Io(format!("bulk IN: {e}")))?;
        let mut pipe = Pipe { ep_out, ep_in, tag: 0, gone: false };

        // Drain bytes a previous host left in the device, which would otherwise answer the wrong request.
        drain(&mut pipe);

        let mut a = Adapter {
            pipe: Mutex::new(pipe),
            info: Info { proto_version: 0, serial: 0, caps: 0, max_read_chunk: 0, name: String::new() },
        };
        let r = a.lock().exchange(OP_IDENTIFY, 0, ADDR_NONE, 0, &[])?;
        let p = &r.payload;
        if r.status != ST_OK || p.len() < 17 {
            return Err(Error::Io(format!("IDENTIFY: status {} with {} bytes", status_name(r.status), p.len())));
        }
        let nlen = p[16] as usize;
        a.info = Info {
            proto_version: u16::from_le_bytes([p[0], p[1]]),
            serial: u32::from_le_bytes([p[4], p[5], p[6], p[7]]),
            caps: u32::from_le_bytes([p[8], p[9], p[10], p[11]]),
            max_read_chunk: u32::from_le_bytes([p[12], p[13], p[14], p[15]]),
            name: String::from_utf8_lossy(&p[17..(17 + nlen).min(p.len())]).into_owned(),
        };
        if a.info.proto_version != PROTO_VERSION {
            return Err(Error::Io(format!(
                "adapter speaks protocol v{} but this runtime speaks v{PROTO_VERSION}",
                a.info.proto_version
            )));
        }
        Ok(a)
    }

    pub fn lock(&self) -> Bus<'_> {
        Bus { pipe: self.pipe.lock().unwrap_or_else(|e| e.into_inner()), max_read_chunk: self.info.max_read_chunk, caps: self.info.caps }
    }
}

fn drain(pipe: &mut Pipe) -> usize {
    let mut total = 0;
    for _ in 0..256 {
        let c = pipe.ep_in.transfer_blocking(pipe.ep_in.allocate(IN_BUF), Duration::from_millis(60));
        match c.status {
            Ok(()) if c.actual_len > 0 => total += c.actual_len,
            _ => break,
        }
    }
    total
}

fn usb_err(pipe: &mut Pipe, what: &str, e: TransferError) -> Error {
    if e == TransferError::Disconnected {
        pipe.gone = true;
        Error::Gone
    } else {
        Error::Io(format!("{what}: {e}"))
    }
}

/// The response to request `op`/`tag` once `acc`, what the IN pipe has brought so far, holds all of
/// it; None while more is to come. A whole response to another request is an error: the pipe is
/// out of step.
pub(crate) fn complete(acc: &[u8], op: u8, tag: u16) -> Option<Result<Response, Error>> {
    let (mut r, len) = Response::parse_header(acc)?;
    if acc.len() < HDR_LEN.checked_add(len)? {
        return None;
    }
    r.payload = acc[HDR_LEN..HDR_LEN + len].to_vec();
    if r.tag != tag || r.op != op {
        return Some(Err(Error::Io(format!("expected op {op:#04x} tag {tag}, got op {:#04x} tag {}", r.op, r.tag))));
    }
    Some(Ok(r))
}

impl Bus<'_> {
    fn next_tag(&mut self) -> u16 {
        self.pipe.tag = if self.pipe.tag == 0xFFFF { 1 } else { self.pipe.tag + 1 };
        self.pipe.tag
    }

    /// One request, one response. `gpib_timeout_ms` is what the firmware applies to the bus
    /// work (0 = its default; the firmware's u16, `backend::u16_ms`); the USB wait is that plus
    /// slack, and for VI_TMO_INFINITE (no deadline in the firmware) as long as it takes.
    pub fn exchange(&mut self, op: u8, flags: u8, pad: u8, gpib_timeout_ms: u32, payload: &[u8]) -> Result<Response, Error> {
        if self.pipe.gone {
            return Err(Error::Gone);
        }
        let tag = self.next_tag();
        let gpib_timeout_ms = crate::backend::u16_ms(gpib_timeout_ms);
        let wait = match gpib_timeout_ms {
            u16::MAX => crate::backend::wait_of(crate::backend::TMO_INFINITE, Duration::ZERO),
            ms => Duration::from_millis(3000u64.max(ms as u64 + 2000)),
        };
        let mut buf = self.pipe.ep_out.allocate(HDR_LEN + payload.len());
        buf.extend_from_slice(&request_header(op, flags, pad, tag, gpib_timeout_ms, payload.len() as u32));
        buf.extend_from_slice(payload);
        let c = self.pipe.ep_out.transfer_blocking(buf, wait);
        if let Err(e) = c.status {
            return Err(usb_err(&mut self.pipe, "OUT transfer", e));
        }

        let mut acc: Vec<u8> = Vec::new();
        loop {
            let buf = self.pipe.ep_in.allocate(IN_BUF);
            let c = self.pipe.ep_in.transfer_blocking(buf, wait);
            if let Err(e) = c.status {
                return Err(usb_err(&mut self.pipe, "IN transfer", e));
            }
            if c.actual_len == 0 && acc.len() < HDR_LEN {
                return Err(Error::Io("IN transfer returned nothing".into()));
            }
            acc.extend_from_slice(&c.buffer[..c.actual_len]);
            if let Some(r) = complete(&acc, op, tag) {
                return r;
            }
        }
    }

    fn check(r: &Response) -> Result<(), Error> {
        match r.status {
            ST_OK => Ok(()),
            ST_TIMEOUT => Err(Error::Timeout),
            ST_NO_DEVICE => Err(Error::NoDevice),
            s => Err(Error::Status(s)),
        }
    }

    /// Is anything listening at `pad`? Touches only that address (no IFC), and returns the
    /// instrument to Local afterwards so probing does not lock its front panel.
    pub fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        let r = self.exchange(OP_PROBE, PROBE_RESTORE_LOCAL, pad, 0, &[])?;
        match r.status {
            ST_OK => Ok(true),
            ST_NO_DEVICE => Ok(false),
            s => Err(Error::Status(s)),
        }
    }

    /// Send `data` as one GPIB message, with EOI on the last byte if `eoi`. Returns the byte
    /// count the adapter took, also on error (in `Err`'s second field).
    pub fn write(&mut self, pad: u8, data: &[u8], eoi: bool, timeout_ms: u32) -> Result<usize, (usize, Error)> {
        self.write_op(OP_WRITE, pad, data, eoi, timeout_ms)
    }

    /// WRITE, or RAW_WRITE with `pad` ADDR_NONE: the same frames either way.
    fn write_op(&mut self, op: u8, pad: u8, data: &[u8], eoi: bool, timeout_ms: u32) -> Result<usize, (usize, Error)> {
        let mut sent = 0;
        loop {
            let part = &data[sent..(sent + WRITE_CHUNK).min(data.len())];
            let last = sent + part.len() >= data.len();
            let flags = (if last && eoi && !part.is_empty() { W_EOI_LAST } else { 0 }) | (if last { 0 } else { W_MORE });
            let r = self.exchange(op, flags, pad, timeout_ms, part).map_err(|e| (sent, e))?;
            if let Err(e) = Self::check(&r) {
                return Err((sent, e));
            }
            sent += part.len();
            if last {
                return Ok(sent);
            }
        }
    }

    /// One READ frame: up to `max_len` bytes (bounded by the adapter's chunk), stopping at
    /// `term` if given, at EOI, or at the count. A timeout still returns what arrived.
    pub fn read(&mut self, pad: u8, max_len: usize, term: Option<u8>, timeout_ms: u32) -> Result<ReadOut, Error> {
        self.read_op(OP_READ, pad, max_len, term, timeout_ms)
    }

    /// READ, or RAW_READ with `pad` ADDR_NONE: the same request and answer either way.
    fn read_op(&mut self, op: u8, pad: u8, max_len: usize, term: Option<u8>, timeout_ms: u32) -> Result<ReadOut, Error> {
        let want = max_len.min(self.max_read_chunk as usize) as u32;
        let mut req = [0u8; 8];
        req[0..4].copy_from_slice(&want.to_le_bytes());
        req[4] = term.unwrap_or(0);
        req[5] = if term.is_some() { R_END_ON_CHAR } else { 0 };
        let r = self.exchange(op, 0, pad, timeout_ms, &req)?;
        match r.status {
            ST_OK | ST_TIMEOUT => {
                Ok(ReadOut { data: r.payload, reason: r.reason, timed_out: r.status == ST_TIMEOUT })
            }
            s => Err(if s == ST_NO_DEVICE { Error::NoDevice } else { Error::Status(s) }),
        }
    }

    /// The VISA read: READ frames back to back until EOI, the termination character, `count`
    /// bytes, or a timeout. Held on the bus throughout, so no other request can end the message.
    pub fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, timeout_ms: u32) -> Result<ReadResult, Error> {
        self.read_message_op(OP_READ, pad, count, term, timeout_ms)
    }

    fn read_message_op(&mut self, op: u8, pad: u8, count: usize, term: Option<u8>, timeout_ms: u32) -> Result<ReadResult, Error> {
        let mut data = Vec::new();
        // The timeout covers the whole read and is checked between frames. The firmware's timeout is a per-byte stall
        // limit; each frame gets at least READ_GRACE of it, so a frame ends only when the talker has stopped. 0 is the
        // firmware's default (1500 ms). A frame's stall is at most 65 534 ms (the firmware's u16), so a longer or
        // infinite timeout reads on after a frame that timed out (`backend::read_on`).
        let full = crate::backend::wait_of(timeout_ms, Duration::from_millis(1500));
        let deadline = Instant::now() + full;
        let mut first = true;
        loop {
            if data.len() >= count {
                return Ok(ReadResult { data, end: ReadEnd::MaxCount });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !first && left.is_zero() {
                return Ok(ReadResult { data, end: ReadEnd::Timeout });
            }
            // Rounded up and at least 1 ms: 0 would ask for the firmware's default.
            let stall = left.max(READ_GRACE.min(full));
            let frame_ms = stall.as_micros().div_ceil(1000).clamp(1, u16::MAX as u128 - 1) as u32;
            first = false;
            let r = self.read_op(op, pad, count - data.len(), term, frame_ms)?;
            data.extend_from_slice(&r.data);
            if r.timed_out && crate::backend::read_on(deadline) {
                continue;
            }
            if r.timed_out {
                return Ok(ReadResult { data, end: ReadEnd::Timeout });
            }
            if r.reason & REASON_END != 0 {
                return Ok(ReadResult { data, end: ReadEnd::End });
            }
            if r.reason & REASON_CHR != 0 {
                return Ok(ReadResult { data, end: ReadEnd::TermChar });
            }
        }
    }

    pub fn clear(&mut self, pad: u8) -> Result<(), Error> {
        let r = self.exchange(OP_CLEAR, 0, pad, 0, &[])?;
        Self::check(&r)
    }

    pub fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        let r = self.exchange(OP_TRIGGER, 0, pad, 0, &[])?;
        Self::check(&r)
    }

    pub fn spoll(&mut self, pad: u8, timeout_ms: u32) -> Result<u8, Error> {
        let r = self.exchange(OP_SPOLL, 0, pad, timeout_ms, &[])?;
        Self::check(&r)?;
        r.payload.first().copied().ok_or_else(|| Error::Io("SPOLL returned no status byte".into()))
    }

    pub fn local(&mut self, pad: u8) -> Result<(), Error> {
        let r = self.exchange(OP_LOCAL, 0, pad, 0, &[])?;
        Self::check(&r)
    }

    pub fn llo(&mut self) -> Result<(), Error> {
        let r = self.exchange(OP_LLO, 0, ADDR_NONE, 0, &[])?;
        Self::check(&r)
    }

    pub fn ren(&mut self, enable: bool) -> Result<(), Error> {
        let r = self.exchange(OP_REN, if enable { REN_ENABLE } else { 0 }, ADDR_NONE, 0, &[])?;
        Self::check(&r)
    }

    pub fn ifc(&mut self) -> Result<(), Error> {
        let r = self.exchange(OP_IFC, 0, ADDR_NONE, 0, &[])?;
        Self::check(&r)
    }

    /// Select the T1 level (SPEED_LOW/NORMAL/HIGH) or, with SPEED_QUERY, only ask; returns the
    /// level in force. Adapter-wide, not per instrument. A board without levels answers
    /// `Error::Status(ST_UNSUPPORTED)`.
    pub fn speed(&mut self, level: u8) -> Result<u8, Error> {
        let r = self.exchange(OP_SPEED, level, ADDR_NONE, 0, &[])?;
        Self::check(&r)?;
        r.payload.first().copied().ok_or_else(|| Error::Io("SPEED returned no level".into()))
    }

    /// The interface level needs firmware with CAP_RAW; this is checked before any request is sent.
    fn need_raw(&self) -> Result<(), Error> {
        if self.caps & CAP_RAW == 0 {
            return Err(Error::Status(ST_UNSUPPORTED));
        }
        Ok(())
    }

    /// Interface messages, sent with ATN; ATN stays asserted afterwards.
    pub fn command(&mut self, cmds: &[u8], timeout_ms: u32) -> Result<(), Error> {
        self.need_raw()?;
        let r = self.exchange(OP_CMD, 0, ADDR_NONE, timeout_ms, cmds)?;
        Self::check(&r)
    }

    /// Data out as whatever the host's commands made the controller; nobody addressed.
    pub fn raw_write(&mut self, data: &[u8], eoi: bool, timeout_ms: u32) -> Result<usize, (usize, Error)> {
        self.need_raw().map_err(|e| (0, e))?;
        self.write_op(OP_RAW_WRITE, ADDR_NONE, data, eoi, timeout_ms)
    }

    /// Data in, the same way.
    pub fn raw_read(&mut self, count: usize, term: Option<u8>, timeout_ms: u32) -> Result<ReadResult, Error> {
        self.need_raw()?;
        self.read_message_op(OP_RAW_READ, ADDR_NONE, count, term, timeout_ms)
    }

    /// Bus lines now (WINUSB_BUS_*): every response carries them.
    pub fn lines(&mut self) -> Result<u16, Error> {
        Ok(self.exchange(OP_LINES, 0, ADDR_NONE, 0, &[])?.bus)
    }
}

impl crate::backend::Backend for Adapter {
    fn lock(&self) -> Box<dyn crate::backend::BusOps + '_> {
        Box::new(Adapter::lock(self))
    }
    fn try_lock(&self) -> Option<Box<dyn crate::backend::BusOps + '_>> {
        let pipe = self.pipe.try_lock().ok()?;
        Some(Box::new(Bus { pipe, max_read_chunk: self.info.max_read_chunk, caps: self.info.caps }))
    }
    fn model(&self) -> Option<String> {
        Some(self.info.name.trim_end_matches('\0').trim().to_string()).filter(|n| !n.is_empty())
    }
}

impl crate::backend::BusOps for Bus<'_> {
    fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        Bus::probe(self, pad)
    }
    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        Bus::write(self, pad, data, eoi, tmo_ms)
    }
    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        Bus::read_message(self, pad, count, term, tmo_ms)
    }
    fn clear(&mut self, pad: u8) -> Result<(), Error> {
        Bus::clear(self, pad)
    }
    fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        Bus::trigger(self, pad)
    }
    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error> {
        Bus::spoll(self, pad, tmo_ms)
    }
    fn local(&mut self, pad: u8) -> Result<(), Error> {
        Bus::local(self, pad)
    }
    fn llo(&mut self) -> Result<(), Error> {
        Bus::llo(self)
    }
    fn ren(&mut self, enable: bool) -> Result<(), Error> {
        Bus::ren(self, enable)
    }
    fn ifc(&mut self) -> Result<(), Error> {
        Bus::ifc(self)
    }
    fn speed(&mut self, level: u8) -> Result<u8, Error> {
        Bus::speed(self, level)
    }
    fn lines(&mut self) -> Result<u16, Error> {
        Bus::lines(self)
    }
    fn command(&mut self, cmds: &[u8], tmo_ms: u32) -> Result<(), Error> {
        Bus::command(self, cmds, tmo_ms)
    }
    fn raw_write(&mut self, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        Bus::raw_write(self, data, eoi, tmo_ms)
    }
    fn raw_read(&mut self, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        Bus::raw_read(self, count, term, tmo_ms)
    }
    fn interface_level(&self) -> bool {
        self.caps & CAP_RAW != 0
    }
    fn srq_asserted(&mut self) -> Result<bool, Error> {
        // Every response carries the bus lines and LINES touches no session, so this is free.
        Ok(Bus::lines(self)? & BUS_SRQ != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response as the firmware frames it.
    fn frame(op: u8, status: u8, tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![op, status, 0, 0];
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn a_response_is_taken_once_all_of_it_has_come_in_however_it_was_split() {
        let f = frame(OP_READ, ST_OK, 7, b"KEITHLEY,2015\n");
        for split in 0..f.len() {
            assert!(complete(&f[..split], OP_READ, 7).is_none(), "{split} of {} bytes is not all of it", f.len());
        }
        let r = complete(&f, OP_READ, 7).unwrap().unwrap();
        assert_eq!((r.status, r.payload.as_slice()), (ST_OK, &b"KEITHLEY,2015\n"[..]));
    }

    #[test]
    fn a_response_to_another_request_is_an_error_not_an_answer() {
        // A late answer to the request before (a tag behind), or to another operation.
        let late = frame(OP_READ, ST_OK, 6, b"old");
        assert!(matches!(complete(&late, OP_READ, 7), Some(Err(Error::Io(_)))));
        let other = frame(OP_WRITE, ST_OK, 7, b"");
        assert!(matches!(complete(&other, OP_READ, 7), Some(Err(Error::Io(_)))));
        // A length past anything addressable waits for more rather than overflowing.
        let mut huge = frame(OP_READ, ST_OK, 7, b"");
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(complete(&huge, OP_READ, 7).is_none());
    }
}
