//! NI GPIB-USB-HS, spoken to directly over WinUSB (no NI software in the path). Written from the protocol as the Linux
//! kernel's driver implements it, used as a specification; no code is copied from it.
//!
//! The adapter must be bound to WinUSB first (for instance with `windows/winusb-bind.ps1 -VidPid 3923:709B`), which
//! takes it away from NI's own software.
//!
//! Only the GPIB-USB-HS (0x709b) is listed. The adapter stops a read on the termination character itself, EOI or not,
//! as VISA means it to; the status word says END for either, so a read that ends in the character is reported as ended
//! by it. SPEED sets T1 as the Viper488's three levels do (see `t1_registers`), for as long as this daemon holds the
//! adapter; LINES is the TNT4882's bus status register.

use crate::adapter::{claim_error, open_error, Error, ReadEnd, ReadResult};
use crate::backend::{Backend, BusOps, Entry, Kind, RAW};
use crate::owed::Owed;
use crate::proto::{SPEED_HIGH, SPEED_LOW, SPEED_NORMAL, SPEED_QUERY, ST_BAD_REQUEST};
use nusb::transfer::{Bulk, ControlIn, ControlType, In, Interrupt, Out, Recipient, TransferError};
use nusb::MaybeFuture;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const VID: u16 = 0x3923;
const PID_HS: u16 = 0x709b;
const EP_OUT: u8 = 0x02;
const EP_IN: u8 = 0x84;
const EP_INT: u8 = 0x81;
const MAX_INTERRUPT_DRAIN: usize = 64;

const BLK_TAKE_CONTROL: u8 = 0x01;
const BLK_TERM: u8 = 0x04;
const BLK_STANDBY: u8 = 0x06;
const BLK_REG_READ: u8 = 0x08;
const BLK_REG_WRITE: u8 = 0x09;
const BLK_DATA_READ: u8 = 0x0a;
const BLK_COMMAND: u8 = 0x0c;
const BLK_DATA_WRITE: u8 = 0x0d;
const BLK_IFC: u8 = 0x0f;

const RSP_REG_START: u8 = 0x34;
const RSP_REG_END: u8 = 0x35;
/// How many replies to discard before giving up on finding the one that answers a request.
const RESYNC_TRIES: usize = 4;
const RSP_DATA_15: u8 = 0x36;
const RSP_DATA_30: u8 = 0x37;
const RSP_READ_STATUS: u8 = 0x38;

const REQ_STOP: u8 = 0x20;
const REQ_POLL_READY: u8 = 0x40;
const REQ_SERIAL: u8 = 0x41;

const IBSTA_END: u16 = 0x2000;
/// EOS mode bits in a data-read block (linux-gpib's REOS and BIN, shifted down a byte).
const EOS_REOS: u8 = 0x04;
const EOS_BIN: u8 = 0x10;
const IBSTA_CIC: u16 = 0x0020;

const SUBDEV_TNT: u8 = 1;
const REG_IMR1: u8 = 0x02;
const REG_IMR2: u8 = 0x04;
const REG_AUXCR: u8 = 0x06;
const REG_ADMR: u8 = 0x08;
const REG_AUXMR: u8 = 0x0a;
const REG_ADR: u8 = 0x0c;
const REG_HSSEL: u8 = 0x0d;
const REG_IMR3: u8 = 0x12;
const REG_KEYREG: u8 = 0x17;
const REG_CMDR: u8 = 0x1c;
const REG_IMR0: u8 = 0x1d;
const REG_BSR: u8 = 0x1f;
const BSR_SRQ: u8 = 0x04;
const BSR_NDAC: u8 = 0x20;

const AUX_PON: u8 = 0x00;
const AUX_CPPF: u8 = 0x01;
const AUX_CHIP_RESET: u8 = 0x02;
const AUX_CIFC: u8 = 0x16;
const AUX_CREN: u8 = 0x17;
const AUX_SREN: u8 = 0x1f;
const AUX_HLDI: u8 = 0x51;
const AUX_CLEAR_END: u8 = 0x55;

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

/// Bring-up: (subdevice, register, value). T1 starts at the normal level (see `t1_registers`):
/// AUXRI with USTD (0xe9), AUXRB | HR_TRI (0xa4), KEYREG 0.
const INIT: &[(u8, u8, u8)] = &[
    (3, 0x10, 0x00),
    (SUBDEV_TNT, REG_CMDR, 0x22),
    (SUBDEV_TNT, REG_AUXMR, 0x81),
    (SUBDEV_TNT, REG_AUXCR, 0x81),
    (SUBDEV_TNT, REG_HSSEL, 0x01),
    (SUBDEV_TNT, REG_AUXMR, AUX_CHIP_RESET),
    (SUBDEV_TNT, REG_IMR0, 0x80),
    (SUBDEV_TNT, REG_IMR1, 0x00),
    (SUBDEV_TNT, REG_IMR2, 0x00),
    (SUBDEV_TNT, REG_IMR3, 0x00),
    (SUBDEV_TNT, REG_AUXMR, AUX_HLDI),
    (SUBDEV_TNT, REG_AUXMR, 0xe9),
    (SUBDEV_TNT, REG_AUXMR, 0xa4),
    (SUBDEV_TNT, REG_KEYREG, 0x00),
    (SUBDEV_TNT, REG_AUXMR, 0x48),
    (SUBDEV_TNT, REG_CMDR, 0x03),
    (SUBDEV_TNT, REG_AUXMR, AUX_CIFC),
    (SUBDEV_TNT, REG_ADR, 0x00),
    (2, 0x00, 0x00),
    (SUBDEV_TNT, REG_ADR, 0xe0),
    (SUBDEV_TNT, REG_ADMR, 0x31),
    (2, 0x01, 0x00),
    (2, 0x02, 0xfd),
    (SUBDEV_TNT, 0x0f, 0x11),
    (SUBDEV_TNT, REG_AUXMR, AUX_PON),
    (SUBDEV_TNT, REG_AUXMR, AUX_CPPF),
];

const USB_TIMEOUT: Duration = Duration::from_secs(20);

fn trace(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SYMBIOTIC_NI_TRACE").is_some()) {
        crate::server::log(&format!("[ni] {}", msg()));
    }
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(e.to_string())
}

/// The adapter's timeout is a one-byte ladder, not a duration: (the rung's name in µs, its code, how long it really
/// waits in µs). The names are the GPIB ones (10 µs ... 1000 s); the adapter waits a power of two of microseconds near
/// them. The rungs below 100 ms are taken at their names; 300 s and 1000 s are taken at twice their names, and a read
/// does not use them.
const LADDER: &[(u64, u8, u64)] = &[
    (10, 0xf1, 10),
    (30, 0xf2, 30),
    (100, 0xf3, 100),
    (300, 0xf4, 300),
    (1_000, 0xf5, 1_000),
    (3_000, 0xf6, 3_000),
    (10_000, 0xf7, 10_000),
    (30_000, 0xf8, 30_000),
    (100_000, 0xf9, 131_072),
    (300_000, 0xfa, 262_144),
    (1_000_000, 0xfb, 1_048_576),
    (3_000_000, 0xfc, 4_194_304),
    (10_000_000, 0xfd, 16_777_216),
    (30_000_000, 0xfe, 33_554_432),
    (100_000_000, 0xff, 134_217_728),
    (300_000_000, 0x01, 600_000_000),
    (1_000_000_000, 0x02, 2_000_000_000),
];

/// The longest rung a read uses.
const READ_RUNGS: usize = 15;

/// The rung that waits at least `d` (the longest if none does): a write or a serial poll, which
/// cannot be taken up again where it stopped, may wait longer but does not give up early.
fn timeout_code(d: Duration) -> u8 {
    let us = d.as_micros();
    LADDER.iter().find(|r| us <= r.2 as u128).map_or(0x02, |r| r.1)
}

/// The longest rung that really waits no longer than `d` (the shortest if `d` is shorter than all), as the duration it
/// waits: a read waits that long, then the next for what is left, so its timeout ends near the caller's instead of up
/// to a rung later.
fn rung_within(d: Duration) -> Duration {
    let us = d.as_micros();
    let real = LADDER[..READ_RUNGS].iter().rev().find(|r| r.2 as u128 <= us).map_or(LADDER[0].2, |r| r.2);
    Duration::from_micros(real)
}

/// How long to wait on USB for a reply to a request sent with `code`: at least as long as the adapter itself waits, or
/// a USB read that gave up first leaves the adapter's own timeout status in the pipe to answer the next request. A
/// quarter more than the rung's real wait, and 2 s for the round trip.
fn usb_wait(code: u8) -> Duration {
    let real = Duration::from_micros(LADDER.iter().find(|r| r.1 == code).map_or(2_000_000_000, |r| r.2));
    real + real / 4 + Duration::from_secs(2)
}

/// The caller's GPIB timeout; 0 means "no preference" and gets 10 s. A longer one than the ladder's longest rung (1000
/// s), VI_TMO_INFINITE too, is that rung: a read goes on after it runs out (`read_message`); a write or a serial poll
/// ends there.
fn timeout_of(ms: u32) -> Duration {
    crate::backend::wait_of(ms, Duration::from_secs(10))
}

fn terminate(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v.extend_from_slice(&[BLK_TERM, 0, 0, 0]);
}

pub(crate) struct Status {
    ibsta: u16,
    error: u8,
    left: u16,
}

fn data_block(id: u8) -> (usize, usize) {
    if id == RSP_DATA_15 { (1, 15) } else { (2, 30) }
}

/// Walk the data blocks to the status block; `0x38` occurs inside data all the time, so
/// scanning for the byte is wrong.
pub(crate) fn find_read_status(r: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < r.len() {
        match r[i] {
            RSP_DATA_15 | RSP_DATA_30 => {
                let (h, n) = data_block(r[i]);
                i += h + n;
            }
            RSP_READ_STATUS if i + 8 <= r.len() => return Some(i),
            _ => return None,
        }
    }
    None
}

pub(crate) fn status(b: &[u8]) -> Result<Status, Error> {
    if b.len() < 8 {
        return Err(io(format!("status block is {} bytes", b.len())));
    }
    Ok(Status {
        ibsta: u16::from_be_bytes([b[1], b[2]]),
        error: b[3],
        left: (!u16::from_le_bytes([b[4], b[5]])).wrapping_add(1), // counts are sent complemented
    })
}

/// Can `reply` answer `request`? A reply echoes the request's id, except that a data read answers
/// with its first data block (or its status block, having read nothing) and a register read with
/// its first chunk of values.
pub(crate) fn answers(request: &[u8], reply: &[u8]) -> bool {
    let (Some(op), Some(id)) = (request.first().copied(), reply.first().copied()) else { return true };
    match op {
        BLK_DATA_READ => matches!(id, RSP_DATA_15 | RSP_DATA_30 | RSP_READ_STATUS),
        BLK_REG_READ => matches!(id, RSP_REG_START | RSP_REG_END),
        other => id == other,
    }
}

/// A register read's reply: chunks of `0x34` and up to three values, padded to four bytes, then
/// `0x35` and the count (checked modulo 3, as the kernel driver does).
pub(crate) fn parse_register_read(r: &[u8], n: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while out.len() < n {
        if r.get(i) != Some(&RSP_REG_START) {
            return Err(io("register read reply has no start marker"));
        }
        i += 1;
        for _ in 0..3 {
            if out.len() == n {
                break;
            }
            out.push(*r.get(i).ok_or_else(|| io("register read reply is short"))?);
            i += 1;
        }
    }
    while i % 4 != 0 {
        i += 1;
    }
    if r.get(i) != Some(&RSP_REG_END) || r.get(i + 1).map(|c| *c as usize % 3) != Some(n % 3) {
        return Err(io(format!("register read reply does not end as it should: {:02x?}", &r[..r.len().min(16)])));
    }
    Ok(out)
}

fn device_error(code: u8) -> Error {
    match code {
        1 => io("the operation was stopped"),
        2 => io("a board read was attempted while ATN was asserted"),
        3 => io("this board is not addressed, or not controller-in-charge"),
        5 | 8 => Error::NoDevice,
        10 => Error::Timeout,
        e => io(format!("adapter error code {e}")),
    }
}

struct Pipe {
    iface: nusb::Interface,
    ep_out: nusb::Endpoint<Bulk, Out>,
    ep_in: nusb::Endpoint<Bulk, In>,
    ep_int: nusb::Endpoint<Interrupt, In>,
    gone: bool,
    /// A request timed out or failed mid-exchange: a reply may be left in the pipe.
    suspect: bool,
    /// Who the adapter has addressed to talk/listen, so consecutive transfers to one
    /// instrument do not re-address it.
    addressed: Option<(u8, bool)>,
    /// Bytes read past where a read had to stop, owed to that instrument's next read.
    owed: Owed,
    /// The T1 level in force (SPEED_*): normal after bring-up.
    speed: u8,
}

/// The T1 registers for a level, as the kernel driver sets T1 for 2 us, 500 ns and 350 ns: AUXRI's USTD (1.1 us),
/// AUXRB's HR_TRI (500 ns), KEYREG's MSTD (350 ns). USTD shortens every byte, not only the first; HR_TRI overrides it.
fn t1_registers(level: u8) -> [(u8, u8, u8); 3] {
    let (auxri, auxrb, keyreg) = match level {
        SPEED_LOW => (0xe1, 0xa0, 0x00),
        SPEED_HIGH => (0xe9, 0xa4, 0x20),
        _ => (0xe9, 0xa4, 0x00),
    };
    [(SUBDEV_TNT, REG_AUXMR, auxri), (SUBDEV_TNT, REG_AUXMR, auxrb), (SUBDEV_TNT, REG_KEYREG, keyreg)]
}

impl Pipe {
    fn note(&mut self, e: &TransferError) -> Error {
        trace(|| format!("transfer failed: {e:?}"));
        if *e == TransferError::Disconnected {
            self.gone = true;
            Error::Gone
        } else {
            io(e)
        }
    }

    fn send(&mut self, out: &[u8]) -> Result<(), Error> {
        if self.gone {
            return Err(Error::Gone);
        }
        if self.suspect {
            self.suspect = false;
            self.drain_bulk();
        }
        let mut buf = self.ep_out.allocate(out.len());
        buf.extend_from_slice(out);
        match self.ep_out.transfer_blocking(buf, USB_TIMEOUT).status {
            Ok(()) => Ok(()),
            Err(e) => Err(self.note(&e)),
        }
    }

    fn exchange(&mut self, out: &[u8], wait: Duration) -> Result<Vec<u8>, Error> {
        self.send(out)?;
        let reply = self.reply_to(out, 4096, wait)?;
        self.drain_interrupt();
        Ok(reply)
    }

    /// The reply to `out`. One that answers another request (an earlier one given up on, which the adapter finished
    /// late) is discarded and the next one read, so later replies do not answer the request before them.
    fn reply_to(&mut self, out: &[u8], size: usize, wait: Duration) -> Result<Vec<u8>, Error> {
        for _ in 0..RESYNC_TRIES {
            let b = self.ep_in.allocate(size);
            let c = self.ep_in.transfer_blocking(b, wait);
            if let Err(e) = c.status {
                self.suspect = true;
                return Err(self.note(&e));
            }
            let reply = c.buffer[..c.actual_len].to_vec();
            if answers(out, &reply) {
                return Ok(reply);
            }
            trace(|| format!("discarded a reply {:02x?} that does not answer request {:#04x}", &reply[..reply.len().min(8)], out[0]));
        }
        self.suspect = true;
        Err(io("the adapter's replies do not answer its requests"))
    }

    /// A read's reply is as long as its data, across as many bulk transfers as that takes: keep
    /// reading until the status block turns up.
    fn exchange_until_status(&mut self, out: &[u8], wait: Duration) -> Result<Vec<u8>, Error> {
        self.send(out)?;
        let mut acc: Vec<u8> = self.reply_to(out, 16 * 1024, wait)?;
        loop {
            if find_read_status(&acc).is_some() {
                break;
            }
            let b = self.ep_in.allocate(16 * 1024);
            let c = self.ep_in.transfer_blocking(b, wait);
            if let Err(e) = c.status {
                self.suspect = true;
                return Err(self.note(&e));
            }
            if c.actual_len == 0 {
                break;
            }
            acc.extend_from_slice(&c.buffer[..c.actual_len]);
            if find_read_status(&acc).is_some() {
                break;
            }
        }
        self.drain_interrupt();
        Ok(acc)
    }

    fn drain_bulk(&mut self) -> usize {
        let mut total = 0;
        for _ in 0..16 {
            let b = self.ep_in.allocate(4096);
            let c = self.ep_in.transfer_blocking(b, Duration::from_millis(30));
            match c.status {
                Ok(()) if c.actual_len > 0 => total += c.actual_len,
                _ => break,
            }
        }
        if total > 0 {
            trace(|| format!("drained {total} stale byte(s)"));
        }
        total
    }

    /// Keep an interrupt read standing (bInterval is 10 ms, so a blocking read that finds
    /// nothing costs a whole interval per exchange).
    fn arm_interrupt(&mut self) {
        if self.ep_int.pending() == 0 {
            let b = self.ep_int.allocate(64);
            self.ep_int.submit(b);
        }
    }

    fn drain_interrupt(&mut self) {
        for _ in 0..MAX_INTERRUPT_DRAIN {
            if self.ep_int.pending() == 0 {
                break;
            }
            let Some(c) = self.ep_int.wait_next_complete(Duration::ZERO) else { break };
            if let Err(e) = c.status {
                let _ = self.note(&e);
                return;
            }
            let b = self.ep_int.allocate(64);
            self.ep_int.submit(b);
        }
        self.arm_interrupt();
    }

    fn op(&mut self, out: &[u8], wait: Duration) -> Result<Status, Error> {
        let r = self.exchange(out, wait)?;
        let s = status(&r)?;
        trace(|| format!("op {:#04x} ibsta {:#06x} error {} left {}", out.first().copied().unwrap_or(0), s.ibsta, s.error, s.left));
        if s.error != 0 {
            return Err(device_error(s.error));
        }
        Ok(s)
    }

    fn write_registers(&mut self, regs: &[(u8, u8, u8)]) -> Result<Status, Error> {
        let mut v = vec![BLK_REG_WRITE, regs.len() as u8, 0x00];
        for (dev, off, val) in regs {
            v.extend_from_slice(&[*dev, *off, *val]);
        }
        terminate(&mut v);
        self.op(&v, USB_TIMEOUT)
    }

    fn read_registers(&mut self, offsets: &[u8]) -> Result<Vec<u8>, Error> {
        let mut v = vec![BLK_REG_READ, offsets.len() as u8];
        for off in offsets {
            v.extend_from_slice(&[SUBDEV_TNT, *off]);
        }
        terminate(&mut v);
        let r = self.exchange(&v, USB_TIMEOUT)?;
        parse_register_read(&r, offsets.len())
    }

    fn simple(&mut self, id: u8, param: u8) -> Result<Status, Error> {
        let mut v = vec![id, param, 0x00, 0x00];
        terminate(&mut v);
        self.op(&v, USB_TIMEOUT)
    }

    /// Send interface messages with ATN asserted (at most 16 bytes per block): the addressing
    /// around this backend's own operations, which a listening device takes at once.
    fn commands(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.commands_within(bytes, Duration::from_secs(1))
    }

    /// The same, each block waiting up to `wait` (a rung of the ladder) for the bus to take it.
    fn commands_within(&mut self, bytes: &[u8], wait: Duration) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.simple(BLK_TAKE_CONTROL, 1)?;
        for part in bytes.chunks(16) {
            let code = timeout_code(wait);
            let mut v = vec![BLK_COMMAND, !(part.len() as u8 - 1), 0x00, code];
            v.extend_from_slice(part);
            terminate(&mut v);
            self.op(&v, usb_wait(code))?;
        }
        Ok(())
    }

    /// IFC, then become controller-in-charge.
    fn take_bus(&mut self) -> Result<(), Error> {
        self.simple(BLK_IFC, 0)?;
        let s = self.simple(BLK_TAKE_CONTROL, 1)?;
        if s.ibsta & IBSTA_CIC == 0 {
            return Err(io("the adapter did not become controller-in-charge"));
        }
        Ok(())
    }

    /// Ready the bus for data: address `pad`, or, at the interface level (`RAW`), only release
    /// ATN -- the caller's own commands said who talks and who listens.
    #[allow(clippy::wrong_self_convention)] // "to" the data phase, not a conversion
    fn to_data(&mut self, pad: u8, host_talks: bool) -> Result<(), Error> {
        if pad == RAW {
            self.addressed = None;
            return self.simple(BLK_STANDBY, 0).map(|_| ());
        }
        self.address(pad, host_talks)
    }

    fn address(&mut self, pad: u8, host_talks: bool) -> Result<(), Error> {
        if self.addressed == Some((pad, host_talks)) {
            return Ok(());
        }
        self.addressed = None;
        if host_talks {
            self.commands(&[UNL, lag(pad), tad(0)])?;
        } else {
            self.commands(&[UNL, lag(0), tad(pad)])?;
        }
        self.simple(BLK_STANDBY, 0)?;
        self.addressed = Some((pad, host_talks));
        Ok(())
    }

    /// One data-read block. Returns the bytes, the status word and the adapter's error code: a timeout (code 10) still
    /// carries what arrived before it, which the caller keeps.
    ///
    /// `term`: the adapter stops the read on this character itself. Byte 1 of the block is the EOS mode (REOS, and BIN
    /// for an 8-bit compare), byte 2 the character; with no character both must be zero, or reads fail.
    fn read_bytes(&mut self, max: u16, timeout: Duration, term: Option<u8>) -> Result<(Vec<u8>, u16, u8), Error> {
        let comp = !(max.saturating_sub(1));
        let code = timeout_code(timeout);
        let (mode, ch) = match term {
            Some(c) => (EOS_REOS | EOS_BIN, c),
            None => (0, 0),
        };
        let mut v = vec![
            BLK_DATA_READ, mode, ch, code, (comp & 0xff) as u8, (comp >> 8) as u8, 0x00, 0x00,
            // clear the END flag first, so END in the answer is this read's
            BLK_REG_WRITE, 0x01, 0x00, SUBDEV_TNT, REG_AUXMR, AUX_CLEAR_END,
        ];
        terminate(&mut v);
        let r = self.exchange_until_status(&v, usb_wait(code))?;
        let end = find_read_status(&r).ok_or_else(|| io("the read reply carries no status block"))?;
        let mut data = Vec::new();
        let mut i = 0;
        while i < end {
            let (h, n) = data_block(r[i]);
            let stop = (i + h + n).min(end);
            data.extend_from_slice(&r[i + h..stop]);
            i += h + n;
        }
        let s = status(&r[end..])?;
        trace(|| format!("read want {max} ibsta {:#06x} error {} left {} data {}", s.ibsta, s.error, s.left, data.len()));
        data.truncate(max.saturating_sub(s.left) as usize);
        Ok((data, s.ibsta, s.error))
    }

    /// Serial poll. SPE must be undone on the failing path too, or every instrument answers an
    /// ordinary read with its status byte.
    fn poll_one(&mut self, pad: u8, timeout: Duration) -> Result<u8, Error> {
        self.addressed = None;
        self.commands(&[UNL, SPE, lag(0), tad(pad)])?;
        self.simple(BLK_STANDBY, 0)?;
        let r = self.read_bytes(1, timeout, None);
        // SPD, then untalk: the polled instrument is still addressed to talk.
        let restored = self.commands(&[SPD, UNT]);
        let (data, _, err) = r?;
        restored?;
        if err != 0 {
            return Err(device_error(err));
        }
        data.first().copied().ok_or(Error::Timeout)
    }
}

pub struct NiAdapter {
    pipe: Mutex<Pipe>,
}

pub struct NiBus<'a> {
    pipe: MutexGuard<'a, Pipe>,
}

pub fn list() -> Result<Vec<Entry>, Error> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
        .filter(|d| d.vendor_id() == VID && d.product_id() == PID_HS)
        .map(|d| Entry { kind: Kind::Ni, serial: d.serial_number().unwrap_or("").to_string(), pid: d.product_id() })
        .collect())
}

fn ctl(req: u8, length: u16) -> ControlIn {
    ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request: req, value: 0, index: 0, length }
}

impl NiAdapter {
    pub fn open(serial: &str) -> Result<NiAdapter, Error> {
        let dev = nusb::list_devices()
            .wait()
            .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID_HS && d.serial_number().unwrap_or("") == serial)
            .ok_or(Error::Gone)?;
        let device = dev.open().wait().map_err(|e| open_error("open", e))?;
        crate::adapter::configure(&device);
        let iface = device.claim_interface(0).wait().map_err(|e| claim_error(&dev, 0, "claim interface 0", e))?;

        let r = iface.control_in(ctl(REQ_SERIAL, 0x10), USB_TIMEOUT).wait().map_err(|e| io(format!("serial number request: {e}")))?;
        if r.len() < 5 || r[0] != REQ_SERIAL {
            return Err(io("the serial number reply is not what was asked for"));
        }
        let mut ready = false;
        for _ in 0..50 {
            let r = iface.control_in(ctl(REQ_POLL_READY, 0x10), USB_TIMEOUT).wait().map_err(|e| io(format!("poll ready request: {e}")))?;
            if r.len() > 10 && (r[6] != 0 || r[7] != 0 || r[10] != 0) {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !ready {
            return Err(io("the adapter never reported itself ready"));
        }
        // Stop whatever a previous owner left running.
        let _ = iface.control_in(ctl(REQ_STOP, 8), USB_TIMEOUT).wait();

        let mut pipe = Pipe {
            iface: iface.clone(),
            ep_out: iface.endpoint::<Bulk, Out>(EP_OUT).map_err(|e| io(format!("bulk out: {e}")))?,
            ep_in: iface.endpoint::<Bulk, In>(EP_IN).map_err(|e| io(format!("bulk in: {e}")))?,
            ep_int: iface.endpoint::<Interrupt, In>(EP_INT).map_err(|e| io(format!("interrupt in: {e}")))?,
            gone: false,
            suspect: false,
            addressed: None,
            owed: Owed::default(),
            speed: SPEED_NORMAL,
        };
        let _ = pipe.ep_in.clear_halt().wait();
        let _ = pipe.ep_out.clear_halt().wait();
        pipe.drain_bulk(); // a killed process leaves a reply behind, and every later answer would belong to the request before it
        pipe.drain_interrupt();
        pipe.write_registers(INIT).map_err(|e| io(format!("bring-up: {e}")))?;
        pipe.take_bus().map_err(|e| io(format!("taking the bus: {e}")))?;
        Ok(NiAdapter { pipe: Mutex::new(pipe) })
    }
}

impl Drop for NiAdapter {
    fn drop(&mut self) {
        if let Ok(mut p) = self.pipe.lock() {
            if !p.gone {
                let _ = p.iface.control_in(ctl(REQ_STOP, 8), USB_TIMEOUT).wait();
                let _ = p.write_registers(&[(SUBDEV_TNT, REG_AUXMR, AUX_CHIP_RESET), (3, 0x10, 0x00)]);
            }
        }
    }
}

impl Backend for NiAdapter {
    fn lock(&self) -> Box<dyn BusOps + '_> {
        Box::new(NiBus { pipe: self.pipe.lock().unwrap_or_else(|e| e.into_inner()) })
    }
    fn try_lock(&self) -> Option<Box<dyn BusOps + '_>> {
        Some(Box::new(NiBus { pipe: self.pipe.try_lock().ok()? }))
    }
}

impl BusOps for NiBus<'_> {
    /// Address the device to listen, drop ATN, and read BSR: with ATN released only an addressed
    /// device keeps holding NDAC. Writes nothing to any instrument, needs no timeout. (A serial
    /// poll cannot do this: an empty address and an idle instrument both time out.)
    fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        if pad == 0 {
            return Ok(false); // the adapter's own address
        }
        let p = &mut self.pipe;
        p.addressed = None;
        p.commands(&[UNL, lag(pad)])?;
        p.simple(BLK_STANDBY, 0)?;
        let bsr = p.read_registers(&[REG_BSR])?;
        p.commands(&[UNL])?;
        Ok(bsr.first().copied().unwrap_or(0) & BSR_NDAC != 0)
    }

    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        let p = &mut self.pipe;
        p.owed.forget(pad);
        p.to_data(pad, true).map_err(|e| (0, e))?;
        let code = timeout_code(timeout_of(tmo_ms));
        let mut sent = 0usize;
        for chunk in data.chunks(0xffff) {
            let last = sent + chunk.len() >= data.len();
            let comp = !(chunk.len() as u16 - 1);
            let mut v = vec![
                BLK_DATA_WRITE, (comp & 0xff) as u8, (comp >> 8) as u8, code, 0x00, 0x00,
                if eoi && last { 0x08 } else { 0x00 }, 0x00,
            ];
            v.extend_from_slice(chunk);
            terminate(&mut v);
            match p.op(&v, usb_wait(code)) {
                Ok(s) if s.left == 0 => sent += chunk.len(),
                // The adapter ended the block with bytes unsent; that is not a success.
                Ok(s) => {
                    sent += chunk.len().saturating_sub(s.left as usize);
                    return Err((sent, io(format!("the adapter sent {sent} of {} bytes", data.len()))));
                }
                Err(e) => return Err((sent, e)),
            }
        }
        Ok(sent)
    }

    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let p = &mut self.pipe;
        let (mut out, _) = p.owed.take(pad); // never owes past EOI: reads stop at it
        // The timeout covers the whole read. Each operation gets the longest rung of the adapter's ladder that fits in
        // what is left, and when the adapter's wait runs out the next asks again for the rest, so the read ends near
        // the caller's timeout. Between operations is where the read is over. A write still rounds up: half a message
        // cannot be taken back, so it may wait longer but never gives up early.
        let deadline = std::time::Instant::now() + timeout_of(tmo_ms);
        let mut first = true;
        loop {
            // A termination character or the count ends the read; what follows is owed.
            if let Some(end) = p.owed.cut(pad, &mut out, count, term, false) {
                return Ok(ReadResult { data: out, end });
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if !first && left.is_zero() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            first = false;
            p.to_data(pad, false)?;
            let want = (count - out.len()).min(0xffff) as u16;
            let (data, ibsta, err) = p.read_bytes(want, rung_within(left), term)?;
            let n = data.len();
            out.extend_from_slice(&data);
            match err {
                0 => {}
                // The adapter's wait ran out: the loop asks again for what is left of the caller's, or
                // ends the read as timed out, keeping what came.
                10 => continue,
                e => return Err(device_error(e)),
            }
            if ibsta & IBSTA_END != 0 {
                // END is EOI or the termination character; the status word does not say which. A
                // read that stopped on the character ends in it.
                let end = if term.is_some() && data.last() == term.as_ref() { ReadEnd::TermChar } else { ReadEnd::End };
                return Ok(ReadResult { data: out, end });
            }
            if n == 0 {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
        }
    }

    fn clear(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.owed.forget(pad);
        self.pipe.addressed = None;
        self.pipe.commands(&[UNL, lag(pad), SDC])
    }

    fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.addressed = None;
        self.pipe.commands(&[UNL, lag(pad), GET])
    }

    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error> {
        self.pipe.poll_one(pad, timeout_of(tmo_ms))
    }

    fn local(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.addressed = None;
        self.pipe.commands(&[UNL, lag(pad), GTL])
    }

    fn llo(&mut self) -> Result<(), Error> {
        self.pipe.addressed = None;
        self.pipe.commands(&[LLO])
    }

    fn ren(&mut self, enable: bool) -> Result<(), Error> {
        let cmd = if enable { AUX_SREN } else { AUX_CREN };
        self.pipe.write_registers(&[(SUBDEV_TNT, REG_AUXMR, cmd)]).map(|_| ())
    }

    fn ifc(&mut self) -> Result<(), Error> {
        self.pipe.addressed = None;
        self.pipe.take_bus()
    }

    fn command(&mut self, cmds: &[u8], tmo_ms: u32) -> Result<(), Error> {
        self.pipe.addressed = None;
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
                self.pipe.write_registers(&t1_registers(level))?;
                self.pipe.speed = level;
            }
            _ => return Err(Error::Status(ST_BAD_REQUEST)),
        }
        Ok(self.pipe.speed)
    }

    fn lines(&mut self) -> Result<u16, Error> {
        Ok(crate::backend::lines_from_bsr(self.pipe.read_registers(&[REG_BSR])?.first().copied().unwrap_or(0)))
    }

    fn srq_asserted(&mut self) -> Result<bool, Error> {
        Ok(self.pipe.read_registers(&[REG_BSR])?.first().copied().unwrap_or(0) & BSR_SRQ != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_of_short_blocks_is_walked_to_its_status_block() {
        let mut r = vec![RSP_DATA_15];
        r.extend_from_slice(&[b'x'; 15]);
        r.extend_from_slice(&[RSP_DATA_30, 0x00]);
        r.extend_from_slice(&[b'y'; 30]);
        let start = r.len();
        r.extend_from_slice(&[RSP_READ_STATUS, 0x20, 0x24, 0x00, 0x6e, 0xff, 0x00, 0x00]);
        assert_eq!(find_read_status(&r), Some(start));
    }

    #[test]
    fn a_status_id_inside_the_data_is_not_mistaken_for_the_status_block() {
        let mut r = vec![RSP_DATA_15];
        r.extend_from_slice(&[RSP_READ_STATUS; 15]);
        assert_eq!(find_read_status(&r), None);
    }

    #[test]
    fn a_read_takes_the_rungs_that_fit_in_what_is_left() {
        let (ms, us) = (Duration::from_millis, Duration::from_micros);
        // By what the rungs really wait: the "3 s" one waits 4.19 s, the "10 s" 16.8 s.
        assert_eq!(rung_within(ms(5000)), us(4_194_304));
        assert_eq!(rung_within(ms(4000)), us(1_048_576));
        assert_eq!(rung_within(ms(75_000)), us(33_554_432));
        assert_eq!(rung_within(us(1_048_576)), us(1_048_576), "a rung that fits exactly");
        assert_eq!(rung_within(us(3)), us(10), "the shortest");
        assert_eq!(rung_within(Duration::from_secs(365 * 24 * 3600)), us(134_217_728), "the longest measured: 300 s and 1000 s are not");
        // 5 s taken rung by rung: the waits add up to 5 s, and none goes past what is left.
        let (mut left, mut waits) = (ms(5000), Vec::new());
        while !left.is_zero() {
            let r = rung_within(left).min(left);
            waits.push(r);
            left -= r;
        }
        assert_eq!(waits.iter().sum::<Duration>(), ms(5000));
        assert!(waits.len() < 16, "{waits:?}");
    }

    #[test]
    fn timeouts_pick_the_next_rung_up_and_the_usb_wait_outlasts_it() {
        // At least as long as asked, by what the rungs really wait: "300 ms" waits 262 ms, so 300 ms
        // takes the "1 s" one; "3 s" waits 4.19 s, so 4 s is still it.
        assert_eq!(timeout_code(Duration::from_millis(262)), 0xfa);
        assert_eq!(timeout_code(Duration::from_millis(300)), 0xfb);
        assert_eq!(timeout_code(Duration::from_secs(4)), 0xfc);
        assert_eq!(timeout_code(Duration::from_secs(10)), 0xfd);
        for (_, code, real) in LADDER {
            assert!(usb_wait(*code) > Duration::from_micros(*real) + Duration::from_secs(1), "{code:#04x}");
        }
    }

    #[test]
    fn a_reply_to_another_request_is_told_apart() {
        assert!(answers(&[BLK_REG_WRITE, 1], &[BLK_REG_WRITE, 0, 0x20]));
        assert!(!answers(&[BLK_REG_WRITE, 1], &[BLK_DATA_WRITE, 0, 0x20]), "a late data-write status");
        assert!(answers(&[BLK_DATA_READ], &[RSP_DATA_30, 0]));
        assert!(answers(&[BLK_DATA_READ], &[RSP_READ_STATUS, 0x20]));
        assert!(!answers(&[BLK_DATA_READ], &[BLK_REG_WRITE, 0]));
        assert!(answers(&[BLK_REG_READ], &[RSP_REG_START, 0x2c]));
        assert!(!answers(&[BLK_REG_READ], &[BLK_TAKE_CONTROL, 0]));
    }

    #[test]
    fn a_register_read_reply_is_checked_to_its_end() {
        // One value (BSR 0x2c), padded, then the end marker and the count.
        assert_eq!(parse_register_read(&[0x34, 0x2c, 0, 0, 0x35, 1, 0, 0], 1).unwrap(), vec![0x2c]);
        assert!(parse_register_read(&[0x34, 0x2c, 0, 0, 0x36, 1, 0, 0], 1).is_err(), "wrong end marker");
        assert!(parse_register_read(&[0x34, 0x2c, 0, 0, 0x35, 2, 0, 0], 1).is_err(), "wrong count");
        assert!(parse_register_read(&[0x34, 1, 2, 3, 0x34, 4, 0, 0, 0x35, 4, 0, 0], 4).is_ok());
    }

    #[test]
    fn counts_come_complemented() {
        assert_eq!(status(&[RSP_READ_STATUS, 0x20, 0x00, 0x00, 0x6e, 0xff, 0x00, 0x00]).unwrap().left, 0x92);
    }
}
