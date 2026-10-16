//! CONTEC GP-IB(USB)FL, spoken to directly over WinUSB in its own protocol. No CONTEC software is used at run time.
//!
//! Protocol rules this module keeps:
//!
//! - a transfer whose length is a multiple of the packet size is followed by a zero-length packet (`xact`),
//!   and a frame goes out in several transfers (`FIRST_CHUNK`);
//! - every long operation that was submitted is fetched, on every path (`salvage`);
//! - `0x7630` is the start signal, and its `+8` differs per operation (`WAIT_CMDS`);
//! - a stale response in the IN pipe would offset every later exchange by one (drained at open);
//! - the device's own serial poll is not used (`spoll`);
//! - a send frame carries at most `MAX_FRAME` bytes;
//! - neither direction sets the device's terminator, and payload `+28` is a flags word (`data_payload`);
//! - stopped at a count inside a message, the device takes one byte more than it returns, lost if anything
//!   else is addressed first (`reading`, `read_message`).
//!
//! The firmware sends no addressing of its own, so every read and write is preceded by interface messages assembled
//! here (`address`). The controller is address 0.
//!
//! A read always takes a whole message (or a whole batch) from the device and cuts it at the caller's count or
//! termination character, owing the rest to that instrument's next read. The caller's timeout becomes the device's GPIB
//! timeout; a timed-out read keeps what arrived. An address is probed from one bus-wide listener scan, cached briefly.
//! Not offered: SPEED and LINES.

use crate::adapter::{claim_error, open_error, Error, ReadEnd, ReadResult};
use crate::backend::{Backend, BusOps, Entry, Kind, RAW};
use crate::owed::Owed;
use nusb::transfer::{Bulk, In, Interrupt, Out, TransferError};
use nusb::MaybeFuture;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const VID: u16 = 0x06CE;
const PID: u16 = 0x8314;
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x82;
const EP_INT: u8 = 0x83;
const MAGIC: u32 = 0x7E7E_7E7E;
/// How many responses to other operations `xact` lets go before it calls the pipe out of step.
const STALE_MAX: usize = 4;

/// The bulk endpoints' packet size. When a transfer's total length is an exact multiple of it, a zero-length packet
/// must follow, or the device waits for the rest and stops answering.
const MAX_PACKET: usize = 512;

/// A frame goes out in several USB transfers, as the vendor's driver sends it: the 16-byte header plus at most
/// `FIRST_CHUNK`, then `NEXT_CHUNK` at a time.
const FIRST_CHUNK: usize = 0xdf0;
const NEXT_CHUNK: usize = 0xe00;

/// Data per send frame; a longer message goes out in several, EOI on the last only. Larger frames make the adapter stop
/// answering.
const MAX_FRAME: usize = 4000;

/// Per receive batch, and the host capacity that goes with it. A larger request overruns the reply frame.
const RECV_BATCH: usize = 2026;
const RECV_CAP: usize = 2068;
const SEND_CAP: usize = 2068;
const CMDS_CAP: usize = 1040;
/// The command bytes one `0x3631` carries: what fits in `CMDS_CAP` as u16 words after its 16-byte
/// head, with the end mark.
const CMDS_MAX: usize = (CMDS_CAP - 16 - 2) / 2;

/// USB transfer timeout, once open. Wider than any GPIB timeout, so a slow instrument comes back
/// as the device's own result code rather than as a transfer that vanished.
const USB_TIMEOUT: Duration = Duration::from_secs(20);
/// While opening: when the firmware is stuck every frame would wait out `USB_TIMEOUT`.
const OPEN_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a finished-operation packet on the interrupt endpoint is waited for per look.
const INT_WAIT: Duration = Duration::from_millis(200);
/// The host waits this much longer than the device's GPIB timeout for an operation to end, so the
/// device times out first and its state stays clean.
const HOST_MARGIN: Duration = Duration::from_secs(5);
/// A listener scan answers every `probe` for this long (a resource search probes 31 addresses).
const SCAN_FRESH: Duration = Duration::from_secs(1);

const OP_OPEN: u32 = 0x3601;
const OP_CLOSE: u32 = 0x3602;
const OP_IFC: u32 = 0x3604;
const OP_REN: u32 = 0x3605;
const OP_TIMEOUT: u32 = 0x360A;
const OP_STOP_ASYNC: u32 = 0x360D;
const OP_FIND: u32 = 0x3617;
const OP_STATUS: u32 = 0x3618;
const OP_LINES: u32 = 0x3619;
const OP_CMDS: u32 = 0x3631;
const OP_SEND: u32 = 0x3637;
const OP_RECV: u32 = 0x3638;
const OP_WAIT: u32 = 0x7630;

/// `0x7630`'s `+8`, which differs per operation started. Wrong, the operation "succeeds" with
/// result 0 and moves nothing.
const WAIT_CMDS: u32 = 2700;
const WAIT_SEND: u32 = 900;
const WAIT_RECV: u32 = 950;

/// Result codes, at `+0` of every response payload.
const ST_OK: u32 = 0x00;
/// An asynchronous operation is still running (or was submitted and never fetched).
const ST_BUSY: u32 = 0x28;
/// The previous operation was aborted by `GpibStopAsync`; reported once.
const ST_ABORTED: u32 = 0x29;
/// The receive buffer filled and the message is not over. NOT an error: ask for the next batch.
const ST_MORE: u32 = 0x2B;
/// The instrument did not answer in time.
const ST_TIMEOUT: u32 = 0x2C;

const UNL: u8 = 0x3F;
const SDC: u8 = 0x04;
const GET: u8 = 0x08;
const GTL: u8 = 0x01;
const LLO: u8 = 0x11;
const SPE: u8 = 0x18;
const SPD: u8 = 0x19;
const UNT: u8 = 0x5F;
fn lag(pad: u8) -> u8 {
    0x20 + pad
}
fn tad(pad: u8) -> u8 {
    0x40 + pad
}

fn trace(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SYMBIOTIC_CONTEC_TRACE").is_some()) {
        crate::server::log(&format!("[contec] {}", msg()));
    }
}

/// `SYMBIOTIC_CONTEC_TRACE=2`: every frame's first bytes, both ways, for diffing against a capture.
fn trace_frames(msg: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var("SYMBIOTIC_CONTEC_TRACE").as_deref() == Ok("2")) {
        crate::server::log(&format!("[contec] {}", msg()));
    }
}

/// `SYMBIOTIC_CONTEC_TRACE=3`: how long each phase of every long operation took, one line each.
fn timing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SYMBIOTIC_CONTEC_TRACE").as_deref() == Ok("3"))
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(e.to_string())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap())).unwrap_or(0)
}

/// The caller's GPIB timeout for the device; 0 means "no preference" and gets 10 s. A longer one
/// than 65 535 ms, VI_TMO_INFINITE too, is 65 535 ms, the most the device has been given: a read
/// goes on after that runs out (`read_message`), a write or a serial poll ends there.
fn timeout_ms(ms: u32) -> u32 {
    if ms == 0 { 10_000 } else { ms.min(u16::MAX as u32) }
}

pub fn list() -> Result<Vec<Entry>, Error> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
        .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
        // The serial string is the module ID set on the thumbwheel under the case.
        .map(|d| Entry { kind: Kind::Contec, serial: d.serial_number().unwrap_or("").trim().to_string(), pid: PID })
        .collect())
}

/// How a long operation's payload is laid out: where the submit/fetch flag and the length live,
/// how much of it is sent to submit and to fetch, the host capacity, and the start value.
struct LongOp {
    op: u32,
    cap: usize,
    submit: usize,
    fetch: usize,
    flag_at: usize,
    len_at: usize,
    wait_val: u32,
}

struct Pipe {
    ep_out: nusb::Endpoint<Bulk, Out>,
    ep_in: nusb::Endpoint<Bulk, In>,
    /// The device posts one packet here when a long operation finishes.
    ep_int: nusb::Endpoint<Interrupt, In>,
    pid: u32,
    gone: bool,
    /// A frame went unanswered for a whole USB timeout: the firmware is stuck and answers nothing until the adapter is
    /// unplugged. Every later exchange then fails at once instead of waiting out the timeout again.
    stuck: bool,
    io_timeout: Duration,
    /// Who is addressed to talk and to listen, so consecutive transfers in one direction to one
    /// instrument do not re-address the bus.
    addressed: Option<(u8, bool)>,
    /// The GPIB timeout the device has now, in ms.
    dev_tmo: Option<u32>,
    /// The last listener scan and when it was made.
    scan: Option<(Instant, Vec<u8>)>,
    /// Bytes read past where a read had to stop, owed to that instrument's next read.
    owed: Owed,
    /// The instrument whose message the device stopped reading in the middle (a receive that
    /// ended "more to come"). The device has taken one byte past what it returned; continuing the
    /// read hands it over, and addressing anything else first loses it -- see `leave_read`.
    reading: Option<u8>,
    /// Instruments whose unread message lost that byte. Their next read is an error.
    broken: Vec<u8>,
    /// The control-line byte last seen, so the trace says only what changed.
    lines_seen: Option<u8>,
}

impl Pipe {
    /// About to do something that re-addresses the bus or changes the device's state. If a
    /// message was left open in the middle, the byte the device holds for it is lost now: say so
    /// on that instrument's next read rather than hand back a message with a byte missing.
    fn leave_read(&mut self) {
        if let Some(pad) = self.reading.take() {
            trace(|| format!("pad {pad}: message left open mid-batch; its next read is an error"));
            if !self.broken.contains(&pad) {
                self.broken.push(pad);
            }
        }
    }

    /// A new message to or from `pad` starts: whatever was broken about the old one is over.
    fn fresh(&mut self, pad: u8) {
        self.broken.retain(|p| *p != pad);
        self.owed.forget(pad);
    }

    fn note(&mut self, e: TransferError) -> Error {
        match e {
            TransferError::Disconnected => {
                self.gone = true;
                Error::Gone
            }
            // A transfer that timed out: the device did not answer a frame, which it does at once
            // whenever it works (a long operation answers "busy" and is asked again).
            TransferError::Cancelled => {
                if !self.stuck {
                    self.stuck = true;
                    crate::server::log("CONTEC: the adapter stopped answering; it needs unplugging");
                }
                io(e)
            }
            _ => io(e),
        }
    }

    fn head(&self, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[4..8].copy_from_slice(&self.pid.to_le_bytes());
        v
    }

    /// One request frame out, one response frame back. Returns the response payload, whatever its
    /// result code; `call` is the version that turns codes into errors.
    fn xact(&mut self, op: u32, payload: &[u8], capacity: usize) -> Result<Vec<u8>, Error> {
        if self.gone {
            return Err(Error::Gone);
        }
        if self.stuck {
            return Err(io("the adapter stopped answering; unplug it and plug it back in"));
        }
        let mut frame = Vec::with_capacity(16 + payload.len());
        frame.extend_from_slice(&MAGIC.to_le_bytes());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(capacity as u32).to_le_bytes());
        frame.extend_from_slice(payload);

        trace_frames(|| format!("-> {op:#06x} {:02x?}", &payload[..payload.len().min(48)]));
        let mut sent = 0usize;
        while sent < frame.len() {
            let n = if sent == 0 { (16 + FIRST_CHUNK).min(frame.len()) } else { NEXT_CHUNK.min(frame.len() - sent) };
            let mut buf = self.ep_out.allocate(n);
            buf.extend_from_slice(&frame[sent..sent + n]);
            if let Err(e) = self.ep_out.transfer_blocking(buf, self.io_timeout).status {
                return Err(self.note(e));
            }
            sent += n;
        }
        // The load-bearing empty packet; see MAX_PACKET.
        if frame.len() % MAX_PACKET == 0 {
            let buf = self.ep_out.allocate(0);
            if let Err(e) = self.ep_out.transfer_blocking(buf, self.io_timeout).status {
                return Err(self.note(e));
            }
        }

        // A response names the operation it answers (+4). One for another operation is a stale one left in the pipe,
        // which would put every later exchange one behind: it is skipped and the next one read. A stale answer to the
        // same operation cannot be told apart.
        for _ in 0..=STALE_MAX {
            let acc = self.read_frame(capacity)?;
            // The header's words after the magic, for the trace: what the device says at +4, +8, +12.
            let head = (u32_at(&acc, 4), u32_at(&acc, 8), u32_at(&acc, 12));
            match frame_body(acc, op)? {
                Ok(body) => {
                    trace_frames(|| format!("<- {op:#06x} head {:#06x} {:#x} {:#x} {:02x?}", head.0, head.1, head.2, &body[..body.len().min(48)]));
                    return Ok(body);
                }
                Err(other) => trace(|| format!("{op:#06x}: a response to {other:#06x} was in the pipe; let go")),
            }
        }
        Err(io(format!("{op:#06x}: the adapter keeps answering other operations: the pipe is out of step")))
    }

    /// One response frame from the IN pipe. Its real length is in len2 (+12), not len (+8). Read at the
    /// same granularity, rounded up to the packet size: WinUSB rejects any other bulk read length.
    fn read_frame(&mut self, capacity: usize) -> Result<Vec<u8>, Error> {
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let want = if acc.is_empty() { 16 + capacity.min(FIRST_CHUNK) } else { NEXT_CHUNK };
            let want = want.div_ceil(MAX_PACKET) * MAX_PACKET;
            let b = self.ep_in.allocate(want);
            let c = self.ep_in.transfer_blocking(b, self.io_timeout);
            if let Err(e) = c.status {
                return Err(self.note(e));
            }
            acc.extend_from_slice(&c.buffer[..c.actual_len]);
            if frame_done(&mut acc) || c.actual_len == 0 {
                return Ok(acc);
            }
        }
    }

    /// `xact` for a short operation: a result code other than success is an error. `0x29` is the
    /// one-shot leftover of an abort and is retried once.
    fn call(&mut self, op: u32, payload: &[u8], capacity: usize) -> Result<Vec<u8>, Error> {
        let mut r = self.xact(op, payload, capacity)?;
        if u32_at(&r, 0) == ST_ABORTED {
            r = self.xact(op, payload, capacity)?;
        }
        match u32_at(&r, 0) {
            ST_OK => Ok(r),
            ST_TIMEOUT => Err(Error::Timeout),
            c => Err(io(format!("{op:#06x}: the adapter returned result code {c:#x}"))),
        }
    }

    fn start(&mut self, wait_val: u32) -> Result<Vec<u8>, Error> {
        let mut w = self.head(12);
        w[8..12].copy_from_slice(&wait_val.to_le_bytes());
        self.xact(OP_WAIT, &w, 12)
    }

    /// Wait for the device to say the operation is done: one packet on the interrupt endpoint. A
    /// fetch sent too early voids the operation silently (success, zero bytes), so this is not
    /// optional. If nothing comes within `INT_WAIT` the fetch goes out anyway and a `0x28` answer
    /// sends it round again.
    /// True when the packet came; false when it did not and the fallback sleep ran.
    fn wait_done(&mut self) -> bool {
        let b = self.ep_int.allocate(64);
        if self.ep_int.transfer_blocking(b, INT_WAIT).status.is_err() {
            std::thread::sleep(Duration::from_millis(5));
            return false;
        }
        true
    }

    /// A long operation, three frames: submit (flag 0), `0x7630` to start it, fetch (flag 1,
    /// length 0) until it is no longer running. Returns the fetched payload and its result code;
    /// codes are the caller's to judge. `limit` is how long the host waits before collecting it
    /// by force, and must be wider than the device's own GPIB timeout.
    fn long_op(&mut self, o: &LongOp, mut payload: Vec<u8>, limit: Duration) -> Result<(u32, Vec<u8>), Error> {
        let t0 = Instant::now();
        payload[o.flag_at..o.flag_at + 2].copy_from_slice(&0u16.to_le_bytes());
        let r = self.xact(o.op, &payload[..o.submit], o.cap)?;
        let mut code = u32_at(&r, 0);
        if code == ST_ABORTED {
            code = u32_at(&self.xact(o.op, &payload[..o.submit], o.cap)?, 0);
        }
        if code != ST_OK {
            // Not accepted, so there is nothing to fetch.
            return Err(io(format!("{:#06x}: submit refused with result code {code:#x}", o.op)));
        }
        payload[o.flag_at..o.flag_at + 2].copy_from_slice(&1u16.to_le_bytes());
        payload[o.len_at..o.len_at + 4].copy_from_slice(&0u32.to_le_bytes());

        let deadline = Instant::now() + limit;
        let t_submit = t0.elapsed();
        let mut outcome = self.start(o.wait_val).map(|_| ());
        let t_start = t0.elapsed();
        let (mut t_wait, mut ints, mut busy) = (Duration::ZERO, 0u32, 0u32);
        while outcome.is_ok() {
            let w = Instant::now();
            ints += self.wait_done() as u32;
            t_wait += w.elapsed();
            match self.xact(o.op, &payload[..o.fetch], o.cap) {
                Ok(r) if u32_at(&r, 0) == ST_BUSY => {
                    busy += 1;
                    if Instant::now() >= deadline {
                        outcome = Err(Error::Timeout);
                        break;
                    }
                    outcome = self.start(o.wait_val).map(|_| ());
                }
                Ok(r) => {
                    if timing() {
                        let all = t0.elapsed();
                        crate::server::log(&format!(
                            "[contec-t] {:#06x} len {} submit {} start {} wait {} fetch {} total {} us, int {ints} busy {busy}",
                            o.op, o.submit, t_submit.as_micros(), (t_start - t_submit).as_micros(), t_wait.as_micros(),
                            (all - t_start - t_wait).as_micros(), all.as_micros()
                        ));
                    }
                    return Ok((u32_at(&r, 0), r));
                }
                Err(e) => outcome = Err(e),
            }
        }
        let e = outcome.unwrap_err();
        trace(|| format!("{:#06x} not collected ({e}); salvaging", o.op));
        self.salvage(o, &payload);
        Err(e)
    }

    /// Collect an operation that was submitted and could not be fetched. Leave one behind and
    /// every later submit returns `0x28`, surviving a process restart and a re-enumeration. Ask
    /// for a few seconds, then abort with `GpibStopAsync` and ask again; when even that fails the
    /// cable has to come out, and the log says so.
    fn salvage(&mut self, o: &LongOp, fetch: &[u8]) {
        if self.gone || self.stuck {
            return;
        }
        let mut stopped = false;
        let mut deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let _ = self.start(o.wait_val);
            match self.xact(o.op, &fetch[..o.fetch], o.cap) {
                Ok(r) if u32_at(&r, 0) == ST_BUSY => {}
                Err(Error::Gone) => return,
                _ => return, // any terminal answer: it is over
            }
            if Instant::now() >= deadline {
                if stopped {
                    crate::server::log(&format!(
                        "CONTEC: operation {:#06x} survived GpibStopAsync; the adapter needs unplugging", o.op
                    ));
                    return;
                }
                let v = self.head(12);
                let _ = self.xact(OP_STOP_ASYNC, &v, 12);
                stopped = true;
                deadline = Instant::now() + Duration::from_secs(6);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `0x3618`: `+4` data, `+8` process id, `+12` select, `+14` 1 to write.
    fn status(&mut self, select: u16, write: bool) -> Result<Vec<u8>, Error> {
        let mut v = vec![0u8; 16];
        v[8..12].copy_from_slice(&self.pid.to_le_bytes());
        v[12..14].copy_from_slice(&select.to_le_bytes());
        v[14..16].copy_from_slice(&(write as u16).to_le_bytes());
        self.call(OP_STATUS, &v, 16)
    }

    /// The device's GPIB timeout. Setting it between addressing and a transfer disturbs the
    /// talker/listener state just set up, so a change forgets the addressing.
    fn set_timeout(&mut self, ms: u32) -> Result<(), Error> {
        if self.dev_tmo == Some(ms) {
            return Ok(());
        }
        self.leave_read();
        let mut v = self.head(16);
        v[8..12].copy_from_slice(&ms.to_le_bytes());
        v[12..16].copy_from_slice(&1u32.to_le_bytes());
        self.call(OP_TIMEOUT, &v, 16)?;
        self.dev_tmo = Some(ms);
        self.addressed = None;
        Ok(())
    }

    fn limit(&self) -> Duration {
        Duration::from_millis(self.dev_tmo.unwrap_or(10_000) as u64) + HOST_MARGIN
    }

    /// `0x3631`: command bytes as u16 words at `+16`, ended by `0xFFFF`, the count (including that
    /// end mark) at `+4`; the length sent is rounded up to a multiple of four or it is refused. One
    /// operation's payload holds `CMDS_MAX` of them; more go in several, one after another, as the
    /// interface messages they are (ATN stays asserted between them).
    fn commands(&mut self, cmds: &[u8]) -> Result<(), Error> {
        self.leave_read();
        for part in cmds.chunks(CMDS_MAX) {
            self.command_op(part)?;
        }
        Ok(())
    }

    fn command_op(&mut self, cmds: &[u8]) -> Result<(), Error> {
        let v = cmd_payload(cmds, self.pid);
        let end = 16 + cmds.len() * 2;
        let o = LongOp {
            op: OP_CMDS, cap: CMDS_CAP, submit: (end + 2 + 3) & !3, fetch: 16, flag_at: 14, len_at: 4,
            wait_val: WAIT_CMDS,
        };
        let limit = self.limit();
        match self.long_op(&o, v, limit)? {
            (ST_OK, _) => Ok(()),
            (ST_TIMEOUT, _) => Err(Error::Timeout),
            (c, _) => Err(io(format!("interface messages: result code {c:#x}"))),
        }
    }

    /// Make the host talk and `pad` listen, or the other way round; skipped when already so.
    fn address(&mut self, pad: u8, host_talks: bool) -> Result<(), Error> {
        if self.addressed == Some((pad, host_talks)) {
            return Ok(());
        }
        self.addressed = None;
        if host_talks {
            self.commands(&[UNL, lag(pad), tad(0)])?;
        } else {
            // One string. A UNL+LAG(pad) in front would make the instrument a listener just before
            // it is asked to talk, and the read then never goes out.
            self.commands(&[UNL, lag(0), tad(pad)])?;
        }
        self.addressed = Some((pad, host_talks));
        Ok(())
    }

    /// Interface messages that leave nobody usefully addressed.
    fn commands_unaddressed(&mut self, cmds: &[u8]) -> Result<(), Error> {
        self.addressed = None;
        self.commands(cmds)
    }

    /// The payload `0x3637` (send) and `0x3638` (receive) share: 40 bytes of header, then data.
    ///
    /// `+28` is a flags word, `0x1000` meaning "no terminator". It is NOT the GPIB address: read
    /// that way once, instruments at address 20 (0x14) worked by coincidence and the rest saw a
    /// 0x00 in their data end the message. The terminator fields stay zero in both directions:
    /// on a send the device appends it to every frame, on a receive it ends the transfer at the
    /// first matching byte and reports a normal completion -- binary data cut short, silently.
    fn data_payload(&self, len: usize, sending: bool, eoi: bool) -> Vec<u8> {
        let mut v = vec![0u8; 40 + len];
        v[12..16].copy_from_slice(&(len as u32).to_le_bytes());
        v[16..20].copy_from_slice(&self.pid.to_le_bytes());
        v[20..22].copy_from_slice(&7u16.to_le_bytes());
        v[24..28].copy_from_slice(&(sending as u32).to_le_bytes());
        v[28..30].copy_from_slice(&0x1000u16.to_le_bytes());
        v[30..32].copy_from_slice(&(eoi as u16).to_le_bytes());
        v
    }

    /// The addresses that answer as listeners, from one bus-wide scan (`0x3617`): the capacity at
    /// `+12` in, the count at `+12` and the addresses as u16 from `+16` out.
    fn listeners(&mut self) -> Result<Vec<u8>, Error> {
        if let Some((at, found)) = &self.scan {
            if at.elapsed() < SCAN_FRESH {
                return Ok(found.clone());
            }
        }
        self.leave_read();
        self.addressed = None;
        let mut v = self.head(80);
        v[8..10].copy_from_slice(&(-1i16).to_le_bytes());
        v[12..16].copy_from_slice(&31u32.to_le_bytes());
        let r = self.call(OP_FIND, &v, 80)?;
        let n = (u32_at(&r, 12) as usize).min(31);
        let found: Vec<u8> = (0..n)
            .filter_map(|i| r.get(16 + i * 2..18 + i * 2))
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u8)
            .filter(|p| (1..=30).contains(p))
            .collect();
        trace(|| format!("listeners: {found:?}"));
        self.scan = Some((Instant::now(), found.clone()));
        Ok(found)
    }

    /// Whether anything listens now: NRFD or NDAC asserted (`0x3619`'s `+8`, bits 2 and 1). Data sent with no listener
    /// never completes its handshake, and the device then stops answering altogether when its GPIB timeout runs out; so
    /// the "no listeners" condition is checked first, as an IEEE 488 talker does.
    fn listening(&mut self) -> Result<bool, Error> {
        let v = self.head(12);
        let r = self.call(OP_LINES, &v, 12)?;
        let b = r.get(8).copied().unwrap_or(0);
        trace(|| format!("before sending: lines {b:#04x}"));
        Ok(b & 0x06 != 0)
    }

    /// A serial poll, assembled from interface messages and a one-byte receive: UNL, our listen address, SPE, the
    /// instrument's talk address; one byte; SPD, UNT. The device's own poll (`0x3632`) is not used: it leaves the
    /// instrument in a bad state.
    ///
    /// The receive asks for one byte. In serial-poll mode the instrument keeps offering its status byte; the byte the
    /// device takes past the count (see `reading`) is another copy, which SPD makes harmless.
    fn spoll(&mut self, pad: u8) -> Result<u8, Error> {
        self.commands_unaddressed(&[UNL, lag(0), SPE, tad(pad)])?;
        let v = self.data_payload(1, false, true);
        let o = LongOp { op: OP_RECV, cap: RECV_CAP, submit: 40, fetch: 40, flag_at: 22, len_at: 12, wait_val: WAIT_RECV };
        let limit = self.limit();
        let got = self.long_op(&o, v, limit);
        // Always leave serial-poll mode, whatever the poll did.
        let done = self.commands_unaddressed(&[SPD, UNT]);
        let (code, r) = got?;
        trace(|| format!("spoll pad {pad}: code {code:#x}, {} bytes", u32_at(&r, 4)));
        match code {
            ST_OK | ST_MORE if u32_at(&r, 4) >= 1 && r.len() > 40 => {
                done?;
                Ok(r[40])
            }
            ST_OK | ST_MORE | ST_TIMEOUT => Err(Error::Timeout),
            c => Err(io(format!("serial poll: result code {c:#x}"))),
        }
    }

    /// The vendor's open sequence, step for step.
    fn bring_up(&mut self) -> Result<(), Error> {
        let mut v = self.head(32);
        // Eight bytes that look like a leftover pointer; zeroed, the data path stops working.
        v[8..16].copy_from_slice(&[0x00, 0x00, 0x64, 0x00, 0xf7, 0x7f, 0x00, 0x00]);
        v[16..18].copy_from_slice(&1u16.to_le_bytes());
        v[18..20].copy_from_slice(&7u16.to_le_bytes());
        v[22..24].copy_from_slice(&500u16.to_le_bytes());
        v[24..28].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        self.call(OP_OPEN, &v, 32).map_err(|e| io(format!("open frame: {e}")))?;
        self.status(0x3C, true)?;
        // Without the status read after IFC, every later long operation reports one in progress.
        let _ = self.status(0x0A, false);
        let _ = self.status(0x29, false);
        let mut v = self.head(12);
        v[8..10].copy_from_slice(&1u16.to_le_bytes());
        self.call(OP_IFC, &v, 12).map_err(|e| io(format!("IFC: {e}")))?;
        let _ = self.status(0x29, false);
        let mut v = self.head(12);
        v[8..10].copy_from_slice(&1u16.to_le_bytes());
        self.call(OP_REN, &v, 12).map_err(|e| io(format!("REN: {e}")))?;
        let v = self.head(16);
        let _ = self.xact(OP_TIMEOUT, &v, 16); // the vendor reads it before writing it
        self.set_timeout(100)?;
        // A crashed run leaves an operation submitted and never fetched, after which every submit
        // returns 0x28. One fetch of each long operation collects it; this has to come after
        // 0x3601 (before it the answer is 0x17 and an absurd length).
        for (op, flen, flag_at, cap) in [(OP_CMDS, 16usize, 14usize, CMDS_CAP), (OP_SEND, 40, 22, SEND_CAP), (OP_RECV, 40, 22, RECV_CAP)] {
            let mut v = [0u8; 40];
            if op == OP_CMDS {
                v[8..12].copy_from_slice(&self.pid.to_le_bytes());
                v[12..14].copy_from_slice(&1u16.to_le_bytes());
            } else {
                v[16..20].copy_from_slice(&self.pid.to_le_bytes());
                v[20..22].copy_from_slice(&7u16.to_le_bytes());
            }
            v[flag_at..flag_at + 2].copy_from_slice(&1u16.to_le_bytes());
            let _ = self.xact(op, &v[..flen], cap);
        }
        Ok(())
    }
}

/// Has `acc` the whole response frame? Its real length is in len2 (+12), not len (+8); anything
/// after it is cut off.
pub(crate) fn frame_done(acc: &mut Vec<u8>) -> bool {
    if acc.len() < 16 {
        return false;
    }
    let need = 16usize.saturating_add(u32_at(acc, 12) as usize);
    if acc.len() < need {
        return false;
    }
    acc.truncate(need);
    true
}

/// A response frame's payload, if its header is one: `Ok(Ok(payload))` when it answers `op`,
/// `Ok(Err(other))` when it answers operation `other` instead.
pub(crate) fn frame_body(mut acc: Vec<u8>, op: u32) -> Result<Result<Vec<u8>, u32>, Error> {
    if acc.len() < 16 || u32_at(&acc, 0) != MAGIC {
        return Err(io("the response frame header is wrong"));
    }
    match u32_at(&acc, 4) {
        o if o == op => Ok(Ok(acc.split_off(16))),
        other => Ok(Err(other)),
    }
}

/// The payload of one `0x3631` for `cmds` (at most `CMDS_MAX` of them), as `Pipe::commands` sends it.
fn cmd_payload(cmds: &[u8], pid: u32) -> Vec<u8> {
    debug_assert!(cmds.len() <= CMDS_MAX);
    let mut v = vec![0u8; CMDS_CAP];
    v[4..8].copy_from_slice(&((cmds.len() + 1) as u32).to_le_bytes());
    v[8..12].copy_from_slice(&pid.to_le_bytes());
    v[12..14].copy_from_slice(&1u16.to_le_bytes());
    for (i, c) in cmds.iter().enumerate() {
        v[16 + i * 2..18 + i * 2].copy_from_slice(&(*c as u16).to_le_bytes());
    }
    let end = 16 + cmds.len() * 2;
    v[end..end + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
    v
}

pub struct ContecAdapter {
    pipe: Mutex<Pipe>,
}

pub struct ContecBus<'a> {
    pipe: MutexGuard<'a, Pipe>,
}

impl ContecAdapter {
    /// Open the adapter whose module ID (the USB serial string) is `serial`.
    pub fn open(serial: &str) -> Result<ContecAdapter, Error> {
        let info = nusb::list_devices()
            .wait()
            .map_err(|e| io(format!("cannot enumerate USB devices: {e}")))?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.serial_number().unwrap_or("").trim() == serial)
            .ok_or(Error::Gone)?;
        let device = info.open().wait().map_err(|e| open_error("open", e))?;
        crate::adapter::configure(&device);
        let iface = device.claim_interface(0).wait().map_err(|e| claim_error(&info, 0, "claim interface 0", e))?;
        let mut pipe = Pipe {
            ep_out: iface.endpoint::<Bulk, Out>(EP_OUT).map_err(|e| io(format!("bulk out: {e}")))?,
            ep_in: iface.endpoint::<Bulk, In>(EP_IN).map_err(|e| io(format!("bulk in: {e}")))?,
            ep_int: iface.endpoint::<Interrupt, In>(EP_INT).map_err(|e| io(format!("interrupt in: {e}")))?,
            pid: std::process::id() & 0x7FFF,
            gone: false,
            stuck: false,
            io_timeout: OPEN_TIMEOUT,
            addressed: None,
            dev_tmo: None,
            scan: None,
            owed: Owed::default(),
            reading: None,
            broken: Vec::new(),
            lines_seen: None,
        };
        // Killed hard, a previous run leaves the endpoints stalled and a response frame in the IN
        // pipe; that frame would offset every later exchange by one. Clear, then drain.
        let _ = pipe.ep_out.clear_halt().wait();
        let _ = pipe.ep_in.clear_halt().wait();
        for _ in 0..8 {
            let b = pipe.ep_in.allocate(4096);
            let c = pipe.ep_in.transfer_blocking(b, Duration::from_millis(60));
            if c.status.is_err() || c.actual_len == 0 {
                break;
            }
        }
        pipe.bring_up().map_err(|e| {
            io(format!("{e} -- if this persists the adapter's firmware is stuck; unplug it and plug it back in"))
        })?;
        pipe.io_timeout = USB_TIMEOUT;
        Ok(ContecAdapter { pipe: Mutex::new(pipe) })
    }
}

impl Drop for ContecAdapter {
    fn drop(&mut self) {
        if let Ok(mut p) = self.pipe.lock() {
            if !p.gone && !p.stuck {
                let v = p.head(12);
                let _ = p.xact(OP_CLOSE, &v, 12);
            }
        }
    }
}

impl Backend for ContecAdapter {
    fn lock(&self) -> Box<dyn BusOps + '_> {
        Box::new(ContecBus { pipe: self.pipe.lock().unwrap_or_else(|e| e.into_inner()) })
    }
    fn try_lock(&self) -> Option<Box<dyn BusOps + '_>> {
        Some(Box::new(ContecBus { pipe: self.pipe.try_lock().ok()? }))
    }
}

impl BusOps for ContecBus<'_> {
    fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        if pad == 0 {
            return Ok(false); // the adapter's own address
        }
        Ok(self.pipe.listeners()?.contains(&pad))
    }

    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        let p = &mut self.pipe;
        p.fresh(pad);
        p.set_timeout(timeout_ms(tmo_ms)).map_err(|e| (0, e))?;
        // At the interface level (RAW) the caller's commands addressed the bus; nobody is here.
        if pad != RAW {
            p.address(pad, true).map_err(|e| (0, e))?;
        }
        if !p.listening().map_err(|e| (0, e))? {
            return Err((0, Error::NoDevice));
        }
        let limit = p.limit();
        let mut sent = 0usize;
        for part in data.chunks(MAX_FRAME) {
            let last = sent + part.len() >= data.len();
            let mut v = p.data_payload(part.len(), true, eoi && last);
            v[40..].copy_from_slice(part);
            let o = LongOp {
                op: OP_SEND, cap: (40 + part.len()).max(SEND_CAP), submit: 40 + part.len(), fetch: 40, flag_at: 22,
                len_at: 12, wait_val: WAIT_SEND,
            };
            let (code, r) = p.long_op(&o, v, limit).map_err(|e| (sent, e))?;
            let n = (u32_at(&r, 4) as usize).min(part.len());
            sent += n;
            match code {
                ST_OK if n == part.len() => {}
                ST_OK => return Err((sent, io(format!("the adapter sent {sent} of {} bytes", data.len())))),
                ST_TIMEOUT => return Err((sent, Error::Timeout)),
                c => return Err((sent, io(format!("send: result code {c:#x}")))),
            }
        }
        Ok(sent)
    }

    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let p = &mut self.pipe;
        if p.broken.contains(&pad) {
            p.fresh(pad);
            return Err(io(
                "this message was left unfinished while another operation used the bus, and the \
                 adapter lost a byte of it then; ask again",
            ));
        }
        let (mut out, ended) = p.owed.take(pad);
        // The timeout covers the whole read. The device's own timeout only ends a batch that stops coming, and it
        // cannot be changed inside a message (that loses a byte), so the read ends between batches, leaving the message
        // open (`reading`) for the next read to continue, as when the caller's count ends it.
        let deadline = std::time::Instant::now() + crate::backend::wait_of(tmo_ms, Duration::from_secs(10));
        let mut first = true;
        let mut dev_ms = timeout_ms(tmo_ms);
        loop {
            if let Some(end) = p.owed.cut(pad, &mut out, count, term, ended) {
                return Ok(ReadResult { data: out, end });
            }
            if ended {
                // What was owed was the rest of a message that EOI already closed.
                return Ok(ReadResult { data: out, end: ReadEnd::End });
            }
            if !first && std::time::Instant::now() >= deadline {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            first = false;
            // A new timeout is a state change that would lose the byte of an open message, so it
            // waits for the next one.
            if p.reading != Some(pad) {
                p.set_timeout(dev_ms)?;
            }
            if pad != RAW {
                p.address(pad, false)?;
            }
            // Always a whole batch, whatever the caller's count: stopped at a count inside a
            // message, the device takes one byte more than it returns, and that byte is lost the
            // moment anything else is addressed. Read to EOI (or a full batch) and owe the rest.
            let v = p.data_payload(RECV_BATCH, false, true);
            let o = LongOp {
                op: OP_RECV, cap: RECV_CAP, submit: 40, fetch: 40, flag_at: 22, len_at: 12, wait_val: WAIT_RECV,
            };
            let limit = p.limit();
            let (code, r) = p.long_op(&o, v, limit)?;
            // A receive's answer has its 40-byte head: shorter is not this operation's answer (one the
            // pipe slipped by, say), and is said so rather than cut into.
            if r.len() < 40 {
                return Err(io(format!("receive: a {}-byte answer, short of its 40-byte head", r.len())));
            }
            let n = (u32_at(&r, 4) as usize).min(r.len() - 40);
            out.extend_from_slice(&r[40..40 + n]);
            trace(|| format!("read pad {pad}: got {n}, code {code:#x}"));
            // The device says neither "EOI" nor "count": a batch that came back short without
            // "more to come" ended on EOI. It sometimes fills a batch and still reports 0, so a
            // full batch counts as "more" too.
            let more = code == ST_MORE || (code == ST_OK && n >= RECV_BATCH);
            p.reading = more.then_some(pad);
            match code {
                // The device's wait ran out and the caller's has not: ask again, keeping what came.
                ST_TIMEOUT if crate::backend::read_on(deadline) => {
                    // No message is open after a timeout, so the device's wait can be what is left.
                    let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis();
                    dev_ms = left.min(u16::MAX as u128) as u32;
                }
                ST_TIMEOUT => return Ok(ReadResult { data: out, end: ReadEnd::Timeout }), // what arrived stays
                ST_OK if !more => {
                    // The whole message is in: what the caller's count or termination character does not take is owed,
                    // marked as ending the message. A termination character inside the message is a stop, as VISA means
                    // it.
                    return Ok(match p.owed.cut(pad, &mut out, count, term, true) {
                        Some(end) => ReadResult { data: out, end },
                        None => ReadResult { data: out, end: ReadEnd::End },
                    });
                }
                ST_OK | ST_MORE => {}
                c => return Err(io(format!("receive: result code {c:#x}"))),
            }
        }
    }

    fn clear(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.fresh(pad);
        self.pipe.commands_unaddressed(&[UNL, lag(pad), SDC])
    }

    fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.commands_unaddressed(&[UNL, lag(pad), GET])
    }

    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error> {
        self.pipe.set_timeout(timeout_ms(tmo_ms))?;
        self.pipe.spoll(pad)
    }

    fn local(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.commands_unaddressed(&[UNL, lag(pad), GTL])
    }

    fn llo(&mut self) -> Result<(), Error> {
        self.pipe.commands_unaddressed(&[LLO])
    }

    fn ren(&mut self, enable: bool) -> Result<(), Error> {
        let mut v = self.pipe.head(12);
        v[8..10].copy_from_slice(&(enable as u16).to_le_bytes());
        self.pipe.call(OP_REN, &v, 12).map(|_| ())
    }

    fn ifc(&mut self) -> Result<(), Error> {
        let p = &mut self.pipe;
        p.leave_read();
        p.addressed = None;
        p.scan = None;
        let mut v = p.head(12);
        v[8..10].copy_from_slice(&1u16.to_le_bytes());
        p.call(OP_IFC, &v, 12).map(|_| ())
    }

    fn command(&mut self, cmds: &[u8], tmo_ms: u32) -> Result<(), Error> {
        self.pipe.set_timeout(timeout_ms(tmo_ms))?;
        self.pipe.commands_unaddressed(cmds)
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

    /// A message another talker left broken at the interface level is not this one's to report.
    fn raw_for(&mut self, who: u16, keep: bool) {
        if self.pipe.owed.raw_for(who, keep) {
            self.pipe.broken.retain(|p| *p != RAW);
        }
    }

    /// The control lines in the Viper488 wire's bit layout (proto::BUS_*), from `0x3619`'s `+8`
    /// (EOI SRQ ATN REN IFC NRFD NDAC DAV from bit 7 down).
    fn lines(&mut self) -> Result<u16, Error> {
        use crate::proto::{BUS_ATN, BUS_DAV, BUS_EOI, BUS_IFC, BUS_NDAC, BUS_NRFD, BUS_REN, BUS_SRQ};
        let v = self.pipe.head(12);
        let r = self.pipe.call(OP_LINES, &v, 12)?;
        let b = r.get(8).copied().unwrap_or(0);
        let map = [(7, BUS_EOI), (6, BUS_SRQ), (5, BUS_ATN), (4, BUS_REN), (3, BUS_IFC), (2, BUS_NRFD), (1, BUS_NDAC), (0, BUS_DAV)];
        Ok(map.iter().filter(|(bit, _)| b >> bit & 1 != 0).fold(0, |acc, (_, f)| acc | f))
    }

    /// `0x3619` reads the control lines; `+8` is EOI SRQ ATN REN IFC NRFD NDAC DAV from bit 7 down.
    /// The interrupt endpoint is no SRQ channel (it only speaks during an operation).
    fn srq_asserted(&mut self) -> Result<bool, Error> {
        let v = self.pipe.head(12);
        let r = self.pipe.call(OP_LINES, &v, 12)?;
        let b = r.get(8).copied().unwrap_or(0);
        if self.pipe.lines_seen != Some(b) {
            self.pipe.lines_seen = Some(b);
            trace(|| format!("lines {b:#04x} {:02x?}", &r[..r.len().min(16)]));
        }
        Ok(b & 0x40 != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len2: u32, body: &[u8]) -> Vec<u8> {
        let mut v = MAGIC.to_le_bytes().to_vec();
        v.extend_from_slice(&0x7630u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&len2.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn a_response_frame_is_as_long_as_its_second_length_says() {
        let mut f = frame(4, &[0, 0, 0, 0]);
        f.extend_from_slice(&[0xEE; 60]); // the rest of the packet
        let mut acc = f[..17].to_vec();
        assert!(!frame_done(&mut acc), "four bytes of body are due, one has come");
        let mut acc = f.clone();
        assert!(frame_done(&mut acc));
        assert_eq!(frame_body(acc, 0x7630).unwrap(), Ok(vec![0, 0, 0, 0]));
        // A length the adapter could not mean does not overflow; the read ends on an empty packet.
        let mut acc = frame(u32::MAX, &[]);
        assert!(!frame_done(&mut acc));
    }

    #[test]
    fn a_frame_without_the_magic_or_a_header_is_refused() {
        let mut f = frame(0, &[]);
        f[0] ^= 1;
        assert!(frame_body(f, 0x7630).is_err());
        assert!(frame_body(vec![0x7E; 15], 0x7630).is_err());
    }

    #[test]
    fn a_response_to_another_operation_is_told_apart() {
        // `frame` builds a response to 0x7630 (the wait); a stale one in front of a 0x3638 fetch.
        assert_eq!(frame_body(frame(4, &[1, 0, 0, 0]), 0x3638).unwrap(), Err(0x7630));
        assert_eq!(frame_body(frame(4, &[1, 0, 0, 0]), 0x7630).unwrap(), Ok(vec![1, 0, 0, 0]));
    }

    #[test]
    fn any_number_of_interface_messages_fits_in_its_operations() {
        // The most one operation takes: its end mark lands on the payload's last two bytes.
        let v = cmd_payload(&[0x3F; CMDS_MAX], 7);
        assert_eq!(v.len(), CMDS_CAP);
        assert_eq!(&v[CMDS_CAP - 2..], &[0xFF, 0xFF]);
        assert_eq!(u32_at(&v, 4) as usize, CMDS_MAX + 1, "the count holds the end mark");
        assert_eq!(u16::from_le_bytes([v[16], v[17]]), 0x3F);
        // A viGpibCommand of 512 bytes (once an out-of-range write here) is two operations.
        let long = vec![0x20u8; 512];
        let parts: Vec<_> = long.chunks(CMDS_MAX).map(|c| cmd_payload(c, 7)).collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(u32_at(&parts[1], 4), 2, "one byte and the end mark");
    }
}
