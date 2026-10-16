//! Keysight 82357B, spoken to directly over WinUSB. Written from the protocol as the Linux kernel's driver implements
//! it, used as a specification; no code is copied from it.
//!
//! THE ADAPTER ENUMERATES TWICE. It comes up blank (0x0957:0x0518), takes a firmware image over USB, and re-enumerates
//! as 0x0718. The image is Keysight's and is not distributed with this project (`firmware/README.md`); `wake()` reads
//! it from a file. The first upload always fails -- the device disconnects and comes back still blank -- and the second
//! works, so `wake()` uploads until the running product id appears.
//!
//! A termination character is the adapter's own (ARF_END_ON_EOS_CHAR: it stops the read on the character, EOI or not,
//! as VISA means it to; the rest of the message stays in the instrument). Only the 82357B is supported (the 82357A has
//! other endpoints and image). SPEED sets the fast-talker T1 register (see `t1_units`) for as long as this daemon holds
//! the adapter. LINES and SRQ come from the TMS9914's bus status register.
//!
//! A WRITE TO AN ADDRESS NOBODY LISTENS AT IS ACCEPTED: the adapter reports the bytes sent and they reach nobody. So
//! before writing, the instrument is serial-polled, unless it answered within the last two seconds (`probe` asks
//! differently: whether a listener holds NDAC, `listens`). A serial poll reads (and so clears) a pending
//! request-for-service bit, so a byte with RQS is kept and handed to the daemon (`take_polled`), which delivers it as a
//! service request and gives it to the next viReadSTB.

use crate::adapter::{claim_error, open_error, Error, ReadEnd, ReadResult};
use crate::backend::{Backend, BusOps, Entry, Kind, RAW};
use crate::owed::Owed;
use crate::proto::{SPEED_HIGH, SPEED_LOW, SPEED_NORMAL, SPEED_QUERY, ST_BAD_REQUEST};
use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError};
use nusb::MaybeFuture;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const VID: u16 = 0x0957;
pub const PID_RUN: u16 = 0x0718;
pub const PID_BLANK: u16 = 0x0518;

const REQ_FIRMWARE_LOAD: u8 = 0xa0;
const CPUCS: u16 = 0xe600;
const FIRMWARE_NAME: &str = "measat_releaseX1.8.hex";

const EP_BULK_IN: u8 = 0x82;
const EP_BULK_OUT: u8 = 0x06;
const EP_INT_IN: u8 = 0x88;
const MAX_PACKET: usize = 512;
const READ_CHUNK: usize = 16 * 1024;
/// The most one READ operation asks for. A read ends cleanly on its count, so a long read is a series of these, and a
/// timeout is taken between them rather than by aborting one mid-flow.
const READ_OP_MAX: usize = 16 * 1024;
/// How long an operation whose data is still arriving may run past the read's deadline to end on
/// its count: a 16 KiB operation at 64 KiB/s.
const READ_GRACE: Duration = Duration::from_millis(250);

const CMD_WRITE: u8 = 0x01;
const CMD_READ: u8 = 0x03;
const CMD_WR_REGS: u8 = 0x04;
const CMD_RD_REGS: u8 = 0x05;

const AWF_SEND_EOI: u8 = 0x01;
const AWF_NO_FAST_TALKER_FIRST_BYTE: u8 = 0x02;
const AWF_NO_FAST_TALKER: u8 = 0x04;
const AWF_NO_ADDRESS: u8 = 0x08;
const AWF_ATN: u8 = 0x10;
const ARF_END_ON_EOI: u8 = 0x01;
const ARF_NO_ADDRESS: u8 = 0x02;
const ARF_END_ON_EOS_CHAR: u8 = 0x04;
const ATRF_EOI: u8 = 0x01;
const ATRF_EOS: u8 = 0x08;
const ATRF_UNADDRESSED: u8 = 0x80;

const REQ_CONTROL: u8 = 0x04;
const XFER_ABORT: u16 = 0xa0;
const XFER_STATUS: u16 = 0xb0;
const XA_FLUSH: u16 = 0x01;

const REG_IMR0_W: u8 = 0;
const REG_IMR1_W: u8 = 1;
const REG_AUXCR_W: u8 = 3;
const REG_ADR_W: u8 = 4;
const REG_SPMR_W: u8 = 5;
const REG_PPR_W: u8 = 6;
/// TMS9914 bus status (read side): which lines are asserted.
const REG_BSR_R: u8 = 3;
const BSR_ATN: u8 = 0x80;
const BSR_SRQ: u8 = 0x04;
const BSR_NDAC: u8 = 0x20;
const BSR_NRFD: u8 = 0x10;
const REG_HW_CONTROL: u8 = 0x0a;
const REG_LED_CONTROL: u8 = 0x0b;
const REG_RESET_TO_POWERUP: u8 = 0x0c;
const REG_PROTOCOL_CONTROL: u8 = 0x0d;
const REG_FAST_TALKER_T1: u8 = 0x0e;

const AUX_CS: u8 = 0x80;
const AUX_CHIP_RESET: u8 = 0x00;
const AUX_INVAL: u8 = 0x01;
const AUX_HLDE: u8 = 0x04;
const AUX_NBAF: u8 = 0x05;
const AUX_LON: u8 = 0x09;
const AUX_TON: u8 = 0x0a;
const AUX_GTS: u8 = 0x0b;
const AUX_TCA: u8 = 0x0c;
const AUX_RPP: u8 = 0x0e;
const AUX_SIC: u8 = 0x0f;
const AUX_SRE: u8 = 0x10;
const AUX_RQC: u8 = 0x11;
const AUX_STDL: u8 = 0x15;
const AUX_VSTDL: u8 = 0x17;
const AUX_RSV2: u8 = 0x18;

const HW_NOT_TI_RESET: u8 = 0x01;
const HW_SYSTEM_CONTROLLER: u8 = 0x02;
const HW_NOT_PARALLEL_POLL: u8 = 0x04;
const LED_FIRMWARE_CONTROL: u8 = 0x01;
const LED_FAIL_ON: u8 = 0x20;
const RESET_SPACEBALL: u8 = 0x01;
const WRITE_COMPLETE_INTERRUPT_EN: u8 = 0x01;
const IMR0_BOIE_BIIE: u8 = 0x30;
const IMR1_SRQIE: u8 = 0x02;
const MAX_INTERRUPT_DRAIN: usize = 64;
const AIF_WRITE_DONE: u8 = 0x02;
const AIF_READ_DONE: u8 = 0x04;
const T1_UNITS: u8 = 24;

const UNL: u8 = 0x3f;
const UNT: u8 = 0x5f;
const SPE: u8 = 0x18;
const SPD: u8 = 0x19;
const SDC: u8 = 0x04;
const GET: u8 = 0x08;
const GTL: u8 = 0x01;
const LLO: u8 = 0x11;

fn lag(pad: u8) -> u8 {
    0x20 + pad
}
fn tad(pad: u8) -> u8 {
    0x40 + pad
}

const USB_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_WAIT: Duration = Duration::from_millis(15);
const DRAIN_WAIT: Duration = Duration::from_millis(15);
/// How long the rest of a timed-out read has to arrive once it is aborted (the kernel's figure).
const SALVAGE_WAIT: Duration = Duration::from_millis(100);
/// How long the zero-length packet after a reply that filled its transfers is waited for.
const ZLP_WAIT: Duration = Duration::from_millis(100);
/// How long to wait for a register read that only asks whether the adapter still answers.
const ALIVE_WAIT: Duration = Duration::from_millis(500);
/// ATN bytes. Devices take them in hardware, in microseconds; on a bus with nobody to take them the
/// write never finishes, and the longer the wait the longer a probe of an empty bus takes.
const COMMAND_WAIT: Duration = Duration::from_secs(1);
const SEEN_FOR: Duration = Duration::from_secs(2);

fn trace(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SYMBIOTIC_KS_TRACE").is_some()) {
        crate::server::log(&format!("[ks] {}", msg()));
    }
}

fn wedged() -> Error {
    io("the adapter stopped answering, even after an abort; unplug it and plug it back")
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(e.to_string())
}

fn device_error(code: u8) -> Error {
    match code {
        1 => io("the adapter did not recognise the command"),
        2 => io("a parameter was out of range"),
        3 => io("that register does not exist"),
        4 => io("the GPIB read failed"),
        5 => io("the GPIB write failed"),
        6 | 7 => io("the adapter is flushing an operation that will not finish"),
        8 => io("the adapter does not support that"),
        e => io(format!("adapter error code {e}")),
    }
}

/// The caller's GPIB timeout; 0 means "no preference" and gets 10 s. Every wait here is the host's
/// own (the adapter is never given a timeout), so VI_TMO_INFINITE is infinite as it is.
fn timeout_of(ms: u32) -> Duration {
    crate::backend::wait_of(ms, Duration::from_secs(10))
}

// ---------------------------------------------------------------------------------------
// Enumeration and the two-step firmware load
// ---------------------------------------------------------------------------------------

pub fn list() -> Result<Vec<Entry>, Error> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
        .filter(|d| d.vendor_id() == VID && (d.product_id() == PID_RUN || d.product_id() == PID_BLANK))
        .map(|d| Entry { kind: Kind::Keysight, serial: d.serial_number().unwrap_or("").to_string(), pid: d.product_id() })
        .collect())
}

fn find(pid: u16) -> Option<nusb::DeviceInfo> {
    nusb::list_devices().wait().ok()?.find(|d| d.vendor_id() == VID && d.product_id() == pid)
}

/// Where the image is, and whether it was named by SYMBIOTIC_KS_FIRMWARE (then it is not checked against `IMAGE_SHA256`).
fn firmware_path() -> Option<(std::path::PathBuf, bool)> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_KS_FIRMWARE").map(std::path::PathBuf::from).filter(|p| p.is_file()) {
        return Some((p, true));
    }
    let mut tries: Vec<std::path::PathBuf> = Vec::new();
    // The configuration directory's firmware\ first: under an all-users install the daemon's own
    // directory is Program Files, where only an administrator can put a file (paths.rs).
    if let Some(dir) = crate::paths::config_dir() {
        tries.push(dir.join("firmware").join(FIRMWARE_NAME));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        tries.push(dir.join("firmware").join(FIRMWARE_NAME));
        tries.push(dir.join(FIRMWARE_NAME));
        // Run from a source tree (target/[<triple>/]release or debug): its firmware/. The daemon runs
        // in its own directory (client.rs), so the caller's working directory is no longer a place.
        if matches!(dir.file_name().and_then(|n| n.to_str()), Some("release" | "debug")) {
            if let Some(root) = dir.ancestors().skip(1).find(|a| a.file_name().and_then(|n| n.to_str()) == Some("target")).and_then(|t| t.parent()) {
                tries.push(root.join("firmware").join(FIRMWARE_NAME));
            }
        }
    }
    tries.into_iter().find(|p| p.is_file()).map(|p| (p, false))
}

/// The image this runtime loads, by its records' digest (`image_digest`): Keysight's `measat_releaseX1.8.hex`, 6908
/// bytes in 526 records. A file found in a directory must be it: every user may write to the shared configuration
/// directory, and a program loaded into the adapter's 8051 could make it any USB device. An image named by
/// SYMBIOTIC_KS_FIRMWARE is the user's own choice for the daemon's process and is taken as it is.
const IMAGE_SHA256: &str = "992a6eb00583fc4301026f7e23be6d95a4bc62b102ebf36a3369a0f8857d6a57";

/// SHA-256 over each record's address and length (big endian, two bytes each) and data: the image,
/// whatever line ends or record lengths the file was written with.
fn image_digest(records: &[(u16, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (addr, data) in records {
        h.update(addr.to_be_bytes());
        h.update((data.len() as u16).to_be_bytes());
        h.update(data);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Intel HEX to (address, bytes) records; checks every line's length byte and checksum.
pub(crate) fn parse_hex(text: &str) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let body = line.strip_prefix(':').ok_or_else(|| format!("line {}: no leading colon", n + 1))?;
        // Two hex digits a byte: anything else (a character of more than one byte too) is not a record.
        if !body.is_ascii() || body.len() % 2 != 0 {
            return Err(format!("line {}: not pairs of hex digits", n + 1));
        }
        let bytes: Vec<u8> = (0..body.len() / 2)
            .map(|i| u8::from_str_radix(&body[i * 2..i * 2 + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("line {}: {e}", n + 1))?;
        if bytes.len() < 5 || bytes[0] as usize + 5 != bytes.len() {
            return Err(format!("line {}: the length byte does not match", n + 1));
        }
        if bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
            return Err(format!("line {}: bad checksum", n + 1));
        }
        match bytes[3] {
            0x00 => {
                let (addr, data) = (u16::from_be_bytes([bytes[1], bytes[2]]), bytes[4..bytes.len() - 1].to_vec());
                // The 8051's 64 KiB: a record does not run past its end.
                if addr as usize + data.len() > 0x1_0000 {
                    return Err(format!("line {}: past the end of the address space", n + 1));
                }
                out.push((addr, data))
            }
            0x01 => break,
            t => return Err(format!("line {}: record type {t:#04x}", n + 1)),
        }
    }
    Ok(out)
}

/// The plain EZ-USB download: hold the 8051 in reset (CPUCS = 1), write the image, release it.
fn download(info: &nusb::DeviceInfo, records: &[(u16, Vec<u8>)]) -> Result<(), Error> {
    let device = info.open().wait().map_err(|e| open_error("open the blank adapter", e))?;
    crate::adapter::configure(&device);
    let iface = device.claim_interface(0).wait().map_err(|e| claim_error(info, 0, "claim the blank adapter", e))?;
    let write = |addr: u16, data: &[u8]| -> Result<(), Error> {
        iface
            .control_out(
                ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request: REQ_FIRMWARE_LOAD, value: addr, index: 0, data },
                USB_TIMEOUT,
            )
            .wait()
            .map(|_| ())
            .map_err(|e| io(format!("writing {} byte(s) to {addr:#06x}: {e}", data.len())))
    };
    write(CPUCS, &[1])?;
    for (addr, data) in records {
        for (i, chunk) in data.chunks(64).enumerate() {
            write(addr.wrapping_add((i * 64) as u16), chunk)?; // within 64 KiB: parse_hex saw to it
        }
    }
    write(CPUCS, &[0])
}

/// Get a blank adapter running: upload, and wait for the running product id; if it comes back blank, upload again.
/// Returns the serial it comes back with: the one running 82357B that was not running before, never just any running
/// one.
pub fn wake() -> Result<String, Error> {
    let (path, named) = firmware_path().ok_or_else(|| {
        io(format!(
            "a Keysight 82357B is attached with no firmware in it ({VID:04x}:{PID_BLANK:04x}) and {FIRMWARE_NAME} \
             was not found (SYMBIOTIC_KS_FIRMWARE, firmware/ in the configuration directory, firmware/ beside \
             symbioticd; see firmware/README.md)"
        ))
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| io(format!("{}: {e}", path.display())))?;
    let records = parse_hex(&text).map_err(|e| io(format!("{}: {e}", path.display())))?;
    if !named && image_digest(&records) != IMAGE_SHA256 {
        return Err(io(format!(
            "{}: not the {FIRMWARE_NAME} this runtime loads (its records' SHA-256 is not {IMAGE_SHA256}); \
             nothing was sent to the adapter. SYMBIOTIC_KS_FIRMWARE names an image to load as it is",
            path.display()
        )));
    }
    let total: usize = records.iter().map(|(_, d)| d.len()).sum();
    crate::server::log(&format!("82357B has no firmware; loading {total} bytes from {}", path.display()));

    let before = running()?;
    let new = || running().ok()?.into_iter().find(|s| !before.contains(s));
    for attempt in 1..=3 {
        let Some(blank) = find(PID_BLANK) else { break };
        download(&blank, &records)?;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(serial) = new() {
                crate::server::log(&format!("82357B: firmware accepted on upload {attempt}, serial {serial}"));
                return Ok(serial);
            }
        }
    }
    new().ok_or_else(|| io("the 82357B never came back with firmware in it"))
}

/// The serials of the 82357Bs running now.
fn running() -> Result<Vec<String>, Error> {
    Ok(list()?.into_iter().filter(|d| d.pid == PID_RUN).map(|d| d.serial).collect())
}

// ---------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------

struct Pipe {
    iface: nusb::Interface,
    ep_out: nusb::Endpoint<Bulk, Out>,
    ep_in: nusb::Endpoint<Bulk, In>,
    ep_int: nusb::Endpoint<Interrupt, In>,
    gone: bool,
    /// The adapter stopped answering (see `write_timed_out`): nothing more is sent to it.
    wedged: bool,
    suspect: bool,
    /// Instruments that answered a serial poll recently, so a write need not poll again.
    seen: HashMap<u8, Instant>,
    /// Status bytes with RQS that `present` read, not yet handed over (`take_polled`).
    polled: Vec<(u8, u8)>,
    /// Bytes read past where a read had to stop, owed to that instrument's next read.
    owed: Owed,
    /// The T1 level in force (SPEED_*): normal after bring-up.
    speed: u8,
}

/// FAST_TALKER_T1 for a level, in the adapter's 21 ns units (0x11..0x72): the T1 of every data byte after the first
/// (writes go with AWF_NO_FAST_TALKER_FIRST_BYTE, so the first keeps the chip's own). Low 2 us, normal 504 ns (what
/// bring-up sets), high 357 ns (the floor).
fn t1_units(level: u8) -> u8 {
    match level {
        SPEED_LOW => 95,
        SPEED_HIGH => 0x11,
        _ => T1_UNITS,
    }
}

impl Pipe {
    fn note(&mut self, e: &TransferError) -> Error {
        if *e == TransferError::Disconnected {
            self.gone = true;
            Error::Gone
        } else {
            io(e)
        }
    }

    fn send(&mut self, msg: &[u8]) -> Result<(), Error> {
        if self.gone {
            return Err(Error::Gone);
        }
        if self.wedged {
            return Err(wedged());
        }
        if self.suspect {
            self.suspect = false;
            self.drain();
        }
        let mut b = self.ep_out.allocate(msg.len());
        b.extend_from_slice(msg);
        match self.ep_out.transfer_blocking(b, USB_TIMEOUT).status {
            Ok(()) => Ok(()),
            Err(e) => {
                self.suspect = true;
                Err(self.note(&e))
            }
        }
    }

    fn reply_into(&mut self, size: usize, wait: Duration) -> Result<Vec<u8>, Error> {
        let b = self.ep_in.allocate(size);
        let c = self.ep_in.transfer_blocking(b, wait);
        match c.status {
            Ok(()) => Ok(c.buffer[..c.actual_len].to_vec()),
            Err(e) => {
                self.suspect = true;
                Err(self.note(&e))
            }
        }
    }

    fn exchange(&mut self, msg: &[u8], wait: Duration) -> Result<Vec<u8>, Error> {
        let t0 = Instant::now();
        let r = self.send(msg).and_then(|_| self.reply_into(MAX_PACKET, wait));
        if let Err(e) = &r {
            trace(|| format!("command {:#04x} ({} byte(s)): no reply after {} ms: {e}", msg[0], msg.len(), t0.elapsed().as_millis()));
        }
        r
    }

    /// Abort an outstanding operation and flush: a read left outstanding keeps the adapter waiting on the bus and every
    /// later reply queues behind it.
    ///
    /// Twice: with the flush flag, then without. A write held in the source handshake (no listener) answers the flush
    /// with status 7, "already flushing", and then nothing on its bulk pipe until a plain abort ends it.
    fn abort(&mut self) {
        self.abort_request(true);
        self.drain();
        self.abort_request(false);
        self.drain();
    }

    /// XFER_ABORT, with or without its flush flag. The reply is the complemented request and a status: 0 is done, and
    /// with the flush flag so is 6 (flushing); anything else is logged, since it should not happen. Returns whether it
    /// was accepted.
    fn abort_request(&mut self, flush: bool) -> bool {
        let r = self
            .iface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: REQ_CONTROL,
                    value: XFER_ABORT,
                    index: if flush { XA_FLUSH } else { 0 },
                    length: 8,
                },
                USB_TIMEOUT,
            )
            .wait();
        trace(|| format!("abort (flush {flush}): {r:02x?}"));
        let ok = r.as_deref().is_ok_and(|reply| abort_accepted(reply, flush));
        if !ok {
            crate::server::log(&format!("82357B: an abort (flush {flush}) was not accepted: {r:02x?}"));
        }
        ok
    }

    fn drain(&mut self) {
        let mut total = 0;
        for _ in 0..8 {
            let b = self.ep_in.allocate(MAX_PACKET);
            let c = self.ep_in.transfer_blocking(b, DRAIN_WAIT);
            match c.status {
                Ok(()) if c.actual_len > 0 => total += c.actual_len,
                _ => break,
            }
        }
        if total > 0 {
            trace(|| format!("drained {total} stale byte(s)"));
        }
    }

    fn arm(&mut self) {
        if self.ep_int.pending() == 0 {
            let b = self.ep_int.allocate(8);
            self.ep_int.submit(b);
        }
    }

    fn flush_interrupts(&mut self) {
        for _ in 0..MAX_INTERRUPT_DRAIN {
            if self.ep_int.pending() == 0 {
                break;
            }
            let Some(c) = self.ep_int.wait_next_complete(Duration::ZERO) else { break };
            if let Err(e) = c.status {
                let _ = self.note(&e);
                return;
            }
            let b = self.ep_int.allocate(8);
            self.ep_int.submit(b);
        }
        self.arm();
    }

    /// Wait for the operation's completion interrupt, then ask how many bytes it moved.
    fn finish(&mut self, wait: Duration, atn: bool) -> Result<u32, Error> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.ep_int.wait_next_complete(left) {
                Some(c) => {
                    // A failed interrupt read is not resubmitted at once: on an adapter that was unplugged every
                    // resubmission fails as fast, and this loop would spin until the deadline.
                    if let Err(e) = c.status {
                        let err = self.note(&e);
                        if self.gone {
                            return Err(err);
                        }
                        trace(|| format!("interrupt read failed: {e}"));
                        std::thread::sleep(Duration::from_millis(10).min(left));
                        let b = self.ep_int.allocate(8);
                        self.ep_int.submit(b);
                        if left.is_zero() {
                            return Err(self.write_timed_out(atn));
                        }
                        continue;
                    }
                    let flags = c.buffer.first().copied().unwrap_or(0);
                    trace(|| format!("interrupt {:02x?}", &c.buffer[..c.actual_len]));
                    let b = self.ep_int.allocate(8);
                    self.ep_int.submit(b);
                    if flags & (AIF_WRITE_DONE | AIF_READ_DONE) != 0 {
                        break;
                    }
                    if left.is_zero() {
                        return Err(self.write_timed_out(atn));
                    }
                }
                None => {
                    trace(|| format!("no completion interrupt within {} ms", wait.as_millis()));
                    return Err(self.write_timed_out(atn));
                }
            }
        }
        self.arm();
        let st = self
            .iface
            .control_in(
                ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request: REQ_CONTROL, value: XFER_STATUS, index: 0, length: 8 },
                USB_TIMEOUT,
            )
            .wait()
            .map_err(|e| io(format!("asking for the transfer status: {e}")))?;
        if st.len() < 6 {
            return Err(io("the transfer status is short"));
        }
        Ok(u32::from_le_bytes([st[2], st[3], st[4], st[5]]))
    }

    /// A write that did not finish: abort it, make sure the adapter answers, and for ATN bytes tell
    /// a bus with nobody on it from a slow one.
    ///
    /// A write that finds no listener -- ATN bytes on a bus with nothing on it, or with everything
    /// on it switched off -- is held in the source handshake and never finishes; while it is, the
    /// adapter serves nothing on its bulk pipe. `abort` ends it. If it still does not answer, it is
    /// given up on (said once, then every request fails at once) rather than waited on each time.
    fn write_timed_out(&mut self, atn: bool) -> Error {
        self.abort();
        self.suspect = false;
        if !self.answers() {
            self.wedged = true;
            crate::server::log(&format!("82357B: {}", wedged()));
            return wedged();
        }
        if atn {
            // ATN still asserted (the chip is still the active controller) with NDAC and NRFD both released: nothing on
            // the bus takes part in the handshake. Read only for ATN bytes.
            match self.regs_read(&[REG_BSR_R]) {
                Ok(b) => {
                    let bsr = b[0];
                    trace(|| format!("after an ATN write timed out, BSR {bsr:#04x}"));
                    if bsr & BSR_ATN != 0 && bsr & (BSR_NDAC | BSR_NRFD) == 0 {
                        return Error::NoDevice;
                    }
                }
                Err(e) => return e,
            }
        }
        Error::Timeout
    }

    /// A register read, with a short wait: does the adapter answer at all?
    fn answers(&mut self) -> bool {
        let t0 = Instant::now();
        let ok = self
            .send(&[CMD_RD_REGS, 1, REG_HW_CONTROL])
            .and_then(|_| self.reply_into(MAX_PACKET, ALIVE_WAIT))
            .is_ok_and(|r| check(&r, CMD_RD_REGS).is_ok());
        trace(|| format!("answers: {ok} after {} ms", t0.elapsed().as_millis()));
        ok
    }

    fn regs_write(&mut self, regs: &[(u8, u8)]) -> Result<(), Error> {
        let mut msg = vec![CMD_WR_REGS, regs.len() as u8];
        for (a, v) in regs {
            msg.extend_from_slice(&[*a, *v]);
        }
        let r = self.exchange(&msg, USB_TIMEOUT)?;
        check(&r, CMD_WR_REGS)
    }

    fn regs_read(&mut self, addrs: &[u8]) -> Result<Vec<u8>, Error> {
        let mut msg = vec![CMD_RD_REGS, addrs.len() as u8];
        msg.extend_from_slice(addrs);
        let r = self.exchange(&msg, USB_TIMEOUT)?;
        check(&r, CMD_RD_REGS)?;
        if r.len() < 2 + addrs.len() {
            return Err(io(format!("a read of {} register(s) came back with {}", addrs.len(), r.len().saturating_sub(2))));
        }
        Ok(r[2..2 + addrs.len()].to_vec())
    }

    /// Interface messages, with ATN asserted and no addressing: the addressing around this
    /// backend's own operations, which a listening device takes at once.
    fn commands(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.commands_within(bytes, COMMAND_WAIT)
    }

    /// The same, waiting up to `wait` for the bus to take them.
    fn commands_within(&mut self, bytes: &[u8], wait: Duration) -> Result<(), Error> {
        self.flush_interrupts();
        let mut msg = vec![CMD_WRITE, 0x00, 0x00, AWF_ATN | AWF_NO_ADDRESS | AWF_NO_FAST_TALKER | AWF_NO_FAST_TALKER_FIRST_BYTE];
        msg.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        msg.extend_from_slice(bytes);
        self.send(&msg)?;
        self.finish(wait, true)?;
        Ok(())
    }

    /// Serial poll. The bus is put back afterwards on the failing path too.
    fn poll(&mut self, pad: u8, wait: Duration) -> Result<u8, Error> {
        self.commands(&[UNL, SPE, tad(pad)])?;
        self.regs_write(&[(REG_AUXCR_W, AUX_LON | AUX_CS), (REG_AUXCR_W, AUX_GTS)])?;
        let mut msg = vec![CMD_READ, 0x00, 0x00, ARF_NO_ADDRESS];
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.push(0x00);
        let got = self.exchange(&msg, wait);
        if got.is_err() {
            self.abort();
        }
        // Back to controller active, then SPD and untalk: the polled instrument is still addressed to talk. Both are
        // tried whatever the other did, or an instrument is left answering every read with its status byte.
        let back = self.regs_write(&[(REG_AUXCR_W, AUX_TCA), (REG_AUXCR_W, AUX_LON)]);
        let undone = self.commands(&[SPD, UNT]);
        back?;
        undone?;
        let got = got?;
        match got.split_last() {
            Some((_, [stb])) => Ok(*stb),
            _ => Err(Error::Timeout),
        }
    }

    /// Is anything at `pad`? A poll that gets no answer is "no", unless the adapter itself went.
    fn present(&mut self, pad: u8) -> Result<bool, Error> {
        match self.poll(pad, POLL_WAIT) {
            Ok(stb) => {
                self.seen.insert(pad, Instant::now());
                // The poll cleared RQS in the instrument and released SRQ: keep the byte, or the
                // request is gone without a trace. One per address, the latest.
                if stb & 0x40 != 0 {
                    self.polled.retain(|(p, _)| *p != pad);
                    self.polled.push((pad, stb));
                }
                Ok(true)
            }
            Err(Error::Gone) => Err(Error::Gone),
            // Not "nobody at this address": nobody on the bus at all, or an adapter that stopped
            // answering. Either way a search should move on to the next board.
            Err(Error::NoDevice) => Err(Error::NoDevice),
            Err(e) if self.wedged => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// Is anything at `pad`: address it to listen, go to standby and see whether it holds NDAC. Much faster than a
    /// serial poll, which waits out a timeout at every empty address. Under ATN every device holds NDAC, addressed or
    /// not, hence standby. The chip goes to standby as talker, so the bus status register shows the listener's lines
    /// rather than the chip's own.
    fn listens(&mut self, pad: u8) -> Result<bool, Error> {
        self.commands(&[UNL, lag(pad)])?;
        self.regs_write(&[(REG_AUXCR_W, AUX_TON | AUX_CS), (REG_AUXCR_W, AUX_GTS)])?;
        let bsr = self.regs_read(&[REG_BSR_R]);
        // Back to controller active, and no longer talker, on the failing path too.
        let back = self.regs_write(&[(REG_AUXCR_W, AUX_TCA), (REG_AUXCR_W, AUX_TON)]);
        let bsr = bsr?[0];
        back?;
        self.commands(&[UNL])?;
        trace(|| format!("listens({pad}): BSR {bsr:#04x}"));
        Ok(bsr & BSR_NDAC != 0)
    }

    /// The adapter accepts a write to an empty address and reports it sent: check first.
    fn verify(&mut self, pad: u8) -> Result<(), Error> {
        if self.seen.get(&pad).is_some_and(|t| t.elapsed() < SEEN_FOR) {
            return Ok(());
        }
        if self.present(pad)? { Ok(()) } else { Err(Error::NoDevice) }
    }

    fn bring_up(&mut self) -> Result<(), Error> {
        self.regs_write(&[(REG_LED_CONTROL, LED_FAIL_ON), (REG_RESET_TO_POWERUP, RESET_SPACEBALL)])?;
        std::thread::sleep(Duration::from_millis(2));
        // A chip still held in reset through HW_CONTROL takes none of what follows: let it out first.
        let hw = *self.regs_read(&[REG_HW_CONTROL])?.first().ok_or_else(|| io("the hardware control reply is short"))?;
        if hw & HW_NOT_TI_RESET == 0 {
            trace(|| format!("HW_CONTROL {hw:#04x}: the chip is held in reset; releasing it"));
            self.regs_write(&[(REG_HW_CONTROL, hw | HW_NOT_TI_RESET)])?;
        }
        self.regs_write(&[
            (REG_AUXCR_W, AUX_NBAF), (REG_AUXCR_W, AUX_HLDE), (REG_AUXCR_W, AUX_TON), (REG_AUXCR_W, AUX_LON),
            (REG_AUXCR_W, AUX_RSV2), (REG_AUXCR_W, AUX_INVAL), (REG_AUXCR_W, AUX_RPP), (REG_AUXCR_W, AUX_STDL),
            (REG_AUXCR_W, AUX_VSTDL), (REG_FAST_TALKER_T1, T1_UNITS), (REG_ADR_W, 0), (REG_PPR_W, 0),
            (REG_SPMR_W, 0), (REG_PROTOCOL_CONTROL, WRITE_COMPLETE_INTERRUPT_EN), (REG_IMR0_W, IMR0_BOIE_BIIE),
            (REG_IMR1_W, IMR1_SRQIE),
            (REG_AUXCR_W, AUX_CHIP_RESET), // this device's PON: the write that ends the reset state
            (REG_LED_CONTROL, LED_FIRMWARE_CONTROL),
        ])?;
        // The adapter powers up without the system-controller bit; nothing addresses anyone until it is set. The low
        // three bits are the chip's reset line, system control and parallel poll: out of reset, not polling,
        // controller.
        let hw = *self.regs_read(&[REG_HW_CONTROL])?.first().ok_or_else(|| io("the hardware control reply is short"))?;
        let hw = (hw & !0x07) | HW_NOT_TI_RESET | HW_NOT_PARALLEL_POLL | HW_SYSTEM_CONTROLLER;
        self.regs_write(&[(REG_AUXCR_W, AUX_RQC), (REG_HW_CONTROL, hw)])?;
        self.pulse_ifc()
    }

    fn pulse_ifc(&mut self) -> Result<(), Error> {
        self.regs_write(&[(REG_AUXCR_W, AUX_SIC | AUX_CS)])?;
        std::thread::sleep(Duration::from_millis(1));
        self.regs_write(&[(REG_AUXCR_W, AUX_SIC)])
    }
}

/// Whether an XFER_ABORT reply says it was done: the complemented request, then status 0 -- or,
/// with the flush flag, 6 (flushing). 7 (already flushing) is not.
pub(crate) fn abort_accepted(reply: &[u8], flush: bool) -> bool {
    matches!(reply, [b, st, ..] if *b == !(XFER_ABORT as u8) && (*st == 0 || (flush && *st == 6)))
}

/// Every reply carries the request's opcode complemented: the one thing that catches a pipe
/// that has slipped by one.
pub(crate) fn check(r: &[u8], cmd: u8) -> Result<(), Error> {
    if r.len() < 2 {
        return Err(io(format!("a reply of {} byte(s)", r.len())));
    }
    if r[0] != !cmd {
        return Err(io(format!("the reply says {:#04x}, not {:#04x} -- the pipe is out of step", r[0], !cmd)));
    }
    if r[1] != 0 {
        return Err(device_error(r[1]));
    }
    Ok(())
}

pub struct KsAdapter {
    pipe: Mutex<Pipe>,
}

pub struct KsBus<'a> {
    pipe: MutexGuard<'a, Pipe>,
}

impl KsAdapter {
    /// Open the running adapter (`serial` as the USB string reports it). Wake a blank one first
    /// with `wake()`.
    pub fn open(serial: &str) -> Result<KsAdapter, Error> {
        let info = nusb::list_devices()
            .wait()
            .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID_RUN && d.serial_number().unwrap_or("") == serial)
            .ok_or(Error::Gone)?;
        let device = info.open().wait().map_err(|e| open_error("open", e))?;
        crate::adapter::configure(&device);
        let iface = device.claim_interface(0).wait().map_err(|e| claim_error(&info, 0, "claim interface 0", e))?;
        let mut pipe = Pipe {
            ep_out: iface.endpoint::<Bulk, Out>(EP_BULK_OUT).map_err(|e| io(format!("bulk out: {e}")))?,
            ep_in: iface.endpoint::<Bulk, In>(EP_BULK_IN).map_err(|e| io(format!("bulk in: {e}")))?,
            ep_int: iface.endpoint::<Interrupt, In>(EP_INT_IN).map_err(|e| io(format!("interrupt in: {e}")))?,
            iface,
            gone: false,
            wedged: false,
            suspect: false,
            seen: HashMap::new(),
            polled: Vec::new(),
            owed: Owed::default(),
            speed: SPEED_NORMAL,
        };
        // A halt left on an endpoint by an earlier owner would fail every transfer on it.
        let _ = pipe.ep_in.clear_halt().wait();
        let _ = pipe.ep_out.clear_halt().wait();
        pipe.abort();
        pipe.arm();
        // One left stuck by an earlier owner answers nothing (see `write_timed_out`): say so at once
        // rather than after a bring-up's worth of timeouts.
        if !pipe.answers() {
            return Err(wedged());
        }
        // Just after its firmware is loaded the adapter may drop the reply to the first command: one more try.
        if let Err(e) = pipe.bring_up() {
            trace(|| format!("bring-up failed ({e}); once more"));
            pipe.drain();
            pipe.bring_up().map_err(|e| io(format!("bring-up: {e}")))?;
        }
        Ok(KsAdapter { pipe: Mutex::new(pipe) })
    }
}

impl Drop for KsAdapter {
    fn drop(&mut self) {
        if let Ok(mut p) = self.pipe.lock() {
            if !p.gone && !p.wedged {
                // Most of the kernel driver's go_idle: the chip into its reset state, interrupts off, LEDs off. Not its
                // HW_CONTROL write, which would hold the chip in reset and leave the next open finding no instrument on
                // the bus.
                p.abort();
                let _ = p.regs_write(&[
                    (REG_AUXCR_W, AUX_LON), (REG_AUXCR_W, AUX_TON), (REG_AUXCR_W, AUX_CHIP_RESET | AUX_CS),
                    (REG_PROTOCOL_CONTROL, 0), (REG_IMR0_W, 0), (REG_IMR1_W, 0), (REG_LED_CONTROL, 0),
                ]);
            }
        }
    }
}

impl Backend for KsAdapter {
    fn lock(&self) -> Box<dyn BusOps + '_> {
        Box::new(KsBus { pipe: self.pipe.lock().unwrap_or_else(|e| e.into_inner()) })
    }
    fn try_lock(&self) -> Option<Box<dyn BusOps + '_>> {
        Some(Box::new(KsBus { pipe: self.pipe.try_lock().ok()? }))
    }
}

impl KsBus<'_> {
    /// One READ operation: up to `want` bytes, until EOI, the termination character (the adapter stops on it itself:
    /// ARF_END_ON_EOS_CHAR, the character in the last byte) or the count. Returns the bytes, the adapter's trailing
    /// flags (ATRF_EOI, ATRF_EOS, ...), and whether the timeout ended it (with what arrived before it).
    ///
    /// `deadline` is the whole read's. If it passes while this operation's data is still arriving, the operation gets
    /// `READ_GRACE` more to end on its count: aborting a transfer in full flow can lose or duplicate the byte in hand.
    /// Only a talker that stops mid-operation is aborted.
    fn read_op(&mut self, pad: u8, want: usize, deadline: Instant, term: Option<u8>) -> Result<(Vec<u8>, u8, bool), Error> {
        let p = &mut self.pipe;
        let eos = if term.is_some() { ARF_END_ON_EOS_CHAR } else { 0 };
        let mut msg = if pad == RAW {
            vec![CMD_READ, 0, 0x00, ARF_END_ON_EOI | ARF_NO_ADDRESS | eos]
        } else {
            vec![CMD_READ, pad, 0x00, ARF_END_ON_EOI | eos]
        };
        msg.extend_from_slice(&(want as u32).to_le_bytes());
        msg.push(term.unwrap_or(0));
        p.send(&msg)?;
        let mut deadline = deadline;
        let mut graced = false;
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            // Not `reply_into`: a transfer cut off by the timeout still carries what it received
            // before it was cancelled, and that is data (the kernel keeps it too).
            let b = p.ep_in.allocate(READ_CHUNK);
            let c = p.ep_in.transfer_blocking(b, left);
            let got = c.buffer[..c.actual_len].to_vec();
            let part = match c.status {
                Ok(()) => got,
                Err(TransferError::Disconnected) => {
                    p.gone = true;
                    return Err(Error::Gone);
                }
                Err(e) => {
                    acc.extend_from_slice(&got);
                    if !graced && !acc.is_empty() {
                        // Data is flowing: let the operation end on its count.
                        graced = true;
                        deadline = Instant::now() + READ_GRACE;
                        trace(|| format!("deadline passed with {} byte(s) of this operation in; {} ms more", acc.len(), READ_GRACE.as_millis()));
                        continue;
                    }
                    // No (more) reply in time. What the adapter has collected is inside the
                    // transfer it is still running: abort it with the flush flag and it ends the
                    // reply -- the rest of the data and the trailing flags -- which is read here
                    // rather than drained away (the kernel driver does the same). Only if that
                    // does not come is it finished with a plain abort.
                    trace(|| format!("read timed out ({e}) with {} byte(s); salvaging", acc.len()));
                    p.abort_request(true);
                    let mut ended = false;
                    loop {
                        let b = p.ep_in.allocate(READ_CHUNK);
                        let c = p.ep_in.transfer_blocking(b, SALVAGE_WAIT);
                        if c.status.is_err() {
                            break;
                        }
                        acc.extend_from_slice(&c.buffer[..c.actual_len]);
                        if c.actual_len < READ_CHUNK {
                            ended = true;
                            break;
                        }
                    }
                    p.abort_request(false);
                    p.drain();
                    p.suspect = false;
                    if !ended {
                        trace(|| format!("nothing salvaged; {} byte(s) kept", acc.len()));
                        return Ok((acc, 0, true));
                    }
                    let Some((trailing, data)) = acc.split_last() else { return Ok((Vec::new(), 0, true)) };
                    trace(|| format!("salvaged: {} byte(s), trailing flags {trailing:#04x}", data.len()));
                    // It may have ended just as the timeout did.
                    return Ok((data.to_vec(), *trailing, trailing & (ATRF_EOI | ATRF_EOS) == 0));
                }
            };
            let short = part.len() < READ_CHUNK;
            acc.extend_from_slice(&part);
            if short || acc.len() > want {
                if !short {
                    // The reply, data and status, filled its last transfer exactly (a whole number of 16 KiB): the
                    // adapter ends it with a zero-length packet, which is taken here so it is not the next exchange's
                    // first.
                    let b = p.ep_in.allocate(READ_CHUNK);
                    let c = p.ep_in.transfer_blocking(b, ZLP_WAIT);
                    if c.actual_len > 0 {
                        trace(|| format!("{} byte(s) after a reply that was whole; let go", c.actual_len));
                    }
                }
                break;
            }
        }
        p.flush_interrupts();
        // The last byte of a whole reply is the adapter's status flags, not data.
        let Some((trailing, data)) = acc.split_last() else { return Err(io("the read reply is empty")) };
        Ok((data.to_vec(), *trailing, false))
    }
}

impl BusOps for KsBus<'_> {
    fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        if pad == 0 {
            return Ok(false); // the adapter's own address
        }
        let p = &mut self.pipe;
        match p.listens(pad) {
            Ok(true) => {
                p.seen.insert(pad, Instant::now());
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e @ (Error::Gone | Error::NoDevice)) => Err(e),
            Err(e) if p.wedged => Err(e),
            Err(_) => Ok(false),
        }
    }

    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        let p = &mut self.pipe;
        p.owed.forget(pad);
        // At the interface level (RAW) nobody is addressed, so there is no instrument to check for.
        let (target, no_addr) = if pad == RAW { (0, AWF_NO_ADDRESS) } else { (pad, 0) };
        if pad != RAW {
            p.verify(pad).map_err(|e| (0, e))?;
        }
        p.flush_interrupts();
        let mut msg = vec![CMD_WRITE, target, 0x00, AWF_NO_FAST_TALKER_FIRST_BYTE | no_addr | if eoi { AWF_SEND_EOI } else { 0 }];
        msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
        msg.extend_from_slice(data);
        p.send(&msg).map_err(|e| (0, e))?;
        let sent = p.finish(timeout_of(tmo_ms), false).map_err(|e| (0, e))? as usize;
        if pad != RAW {
            p.seen.insert(pad, Instant::now());
        }
        if sent < data.len() {
            // The adapter called the write done without sending it all; that is not a success.
            return Err((sent, io(format!("the adapter sent {sent} of {} bytes", data.len()))));
        }
        Ok(sent)
    }

    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let (mut out, _) = self.pipe.owed.take(pad); // never owes past EOI: reads stop at it
        let deadline = Instant::now() + timeout_of(tmo_ms);
        let mut first = true;
        loop {
            // A termination character or the count ends the read; what follows is owed.
            if let Some(end) = self.pipe.owed.cut(pad, &mut out, count, term, false) {
                return Ok(ReadResult { data: out, end });
            }
            // Between operations is where a timeout ends a read cleanly: the last one ended on its
            // count, with no byte half taken.
            if !first && Instant::now() >= deadline {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            first = false;
            let want = (count - out.len()).min(READ_OP_MAX);
            let (data, flags, timed_out) = self.read_op(pad, want, deadline, term)?;
            out.extend_from_slice(&data);
            if timed_out {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout }); // what did arrive stays
            }
            if flags & ATRF_EOI != 0 {
                return Ok(ReadResult { data: out, end: ReadEnd::End });
            }
            if flags & ATRF_UNADDRESSED != 0 && data.is_empty() {
                // The adapter refused the read at once: it does not consider itself a listener.
                return Err(io("the adapter refused the read: it is not addressed to listen"));
            }
            if flags & ATRF_EOS != 0 {
                // The adapter stopped on the termination character; the rest stays in the instrument.
                return Ok(ReadResult { data: out, end: ReadEnd::TermChar });
            }
            if data.is_empty() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
        }
    }

    fn clear(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.owed.forget(pad);
        self.pipe.commands(&[UNL, lag(pad), SDC])
    }

    fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.commands(&[UNL, lag(pad), GET])
    }

    fn take_polled(&mut self) -> Vec<(u8, u8)> {
        std::mem::take(&mut self.pipe.polled)
    }

    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error> {
        match self.pipe.poll(pad, timeout_of(tmo_ms)) {
            Err(Error::Gone) => Err(Error::Gone),
            Err(Error::Status(s)) => Err(Error::Status(s)),
            Err(_) => Err(Error::Timeout),
            Ok(b) => Ok(b),
        }
    }

    fn local(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.commands(&[UNL, lag(pad), GTL])
    }

    fn llo(&mut self) -> Result<(), Error> {
        self.pipe.commands(&[LLO])
    }

    fn ren(&mut self, enable: bool) -> Result<(), Error> {
        self.pipe.regs_write(&[(REG_AUXCR_W, if enable { AUX_SRE | AUX_CS } else { AUX_SRE })])
    }

    fn ifc(&mut self) -> Result<(), Error> {
        self.pipe.pulse_ifc()
    }

    fn command(&mut self, cmds: &[u8], tmo_ms: u32) -> Result<(), Error> {
        self.pipe.commands_within(cmds, timeout_of(tmo_ms))
    }

    fn raw_write(&mut self, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        self.write(RAW, data, eoi, tmo_ms)
    }

    fn raw_read(&mut self, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        self.read_message(RAW, count, term, tmo_ms)
    }

    fn interface_level(&self) -> bool {
        true
    }

    fn raw_for(&mut self, who: u16, keep: bool) {
        self.pipe.owed.raw_for(who, keep);
    }

    /// The T1 level: set it, or with SPEED_QUERY only report it. It lasts while this daemon holds
    /// the adapter; opening it again (a new daemon) starts at normal, as bring-up sets.
    fn speed(&mut self, level: u8) -> Result<u8, Error> {
        match level {
            SPEED_QUERY => {}
            SPEED_LOW | SPEED_NORMAL | SPEED_HIGH => {
                self.pipe.regs_write(&[(REG_FAST_TALKER_T1, t1_units(level))])?;
                self.pipe.speed = level;
            }
            _ => return Err(Error::Status(ST_BAD_REQUEST)),
        }
        Ok(self.pipe.speed)
    }

    fn lines(&mut self) -> Result<u16, Error> {
        Ok(crate::backend::lines_from_bsr(self.pipe.regs_read(&[REG_BSR_R])?[0]))
    }

    /// The SRQ line as it is now, from the bus status register, not the interrupt endpoint's AIF_SRQ, which reports an
    /// edge: SRQ is wired-OR, so a second instrument asking while the first still holds the line makes no edge.
    fn srq_asserted(&mut self) -> Result<bool, Error> {
        self.pipe.flush_interrupts();
        Ok(self.pipe.regs_read(&[REG_BSR_R])?[0] & BSR_SRQ != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intel_hex_records_are_parsed_and_checked() {
        let good = ":03000000020106F4\n:00000001FF\n";
        assert_eq!(parse_hex(good).unwrap(), vec![(0u16, vec![0x02, 0x01, 0x06])]);
        assert!(parse_hex(":03000000020106F5\n").unwrap_err().contains("checksum"));
        assert!(parse_hex("03000000020106F4\n").unwrap_err().contains("colon"));
        assert!(parse_hex(":04000000020106F4\n").unwrap_err().contains("length"));
    }

    #[test]
    fn an_abort_is_accepted_only_as_the_adapter_answers_a_good_one() {
        // As read from an 82357B: flush while a read ran, plain after it, and flush on a wedged one.
        assert!(abort_accepted(&[0x5f, 0x06], true));
        assert!(abort_accepted(&[0x5f, 0x00], false));
        assert!(!abort_accepted(&[0x5f, 0x07], true), "already flushing");
        assert!(!abort_accepted(&[0x5f, 0x06], false), "flushing is only right when a flush was asked for");
        assert!(!abort_accepted(&[0x5e, 0x00], false), "not the reply to an abort");
        assert!(!abort_accepted(&[0x5f], false));
    }

    #[test]
    fn a_reply_must_echo_the_complemented_opcode() {
        assert!(check(&[!CMD_WR_REGS, 0], CMD_WR_REGS).is_ok());
        assert!(check(&[!CMD_RD_REGS, 0], CMD_WR_REGS).unwrap_err().to_string().contains("out of step"));
        assert!(check(&[!CMD_WR_REGS, 4], CMD_WR_REGS).is_err());
    }

    #[test]
    fn the_shipped_firmware_image_parses_when_it_is_present() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware").join(FIRMWARE_NAME);
        if let Ok(t) = std::fs::read_to_string(p) {
            let r = parse_hex(&t).unwrap();
            assert_eq!(r.iter().map(|(_, d)| d.len()).sum::<usize>(), 6908);
            assert_eq!(image_digest(&r), IMAGE_SHA256, "the image this runtime is pinned to");
            // The same image with Windows line ends is the same image.
            assert_eq!(image_digest(&parse_hex(&t.replace('\n', "\r\n")).unwrap()), IMAGE_SHA256);
        }
    }

    #[test]
    fn a_file_that_is_not_hex_is_refused_not_a_crash() {
        // A multi-byte character where a digit should be is an error, not a panic.
        assert!(parse_hex(":0300\u{e9}00020106F4\n").unwrap_err().contains("hex digits"));
        assert!(parse_hex(":030000000201064\n").is_err(), "an odd number of digits");
        // A record that would run past 0xFFFF.
        assert!(parse_hex(":02FFFF00AABB9B\n").unwrap_err().contains("address space"));
        assert_ne!(image_digest(&[(0, vec![1, 2, 3])]), IMAGE_SHA256);
    }
}
