//! Serial ports: the VISA `ASRL` resources.
//!
//! The daemon opens a port for the sessions that use it, as it owns the WinUSB devices: Windows gives a COM port to one
//! process at a time. A port is `ASRL<n>`, `n` its COM number. Listing reads the registry and opens nothing (opening
//! raises DTR, which resets some devices), and nothing here probes a port for an instrument.
//!
//! The settings (baud rate, framing, flow control, DTR/RTS) belong to the port, not to a session, and are kept here;
//! how a read or a write ends belongs to the session, and comes with each request. Bytes that arrived past where a read
//! had to stop stay here for the port's next read.
//!
//! On Linux a port is a tty device (`/dev/ttyACM0`), and its number, the same `u16` everything else carries, is fixed
//! by the kernel's name (`port_number`): `ttyS<k>` is k+1, as NI-VISA numbers it, and a USB port is named by its path.
//! On macOS a port is numbered on first sight and the numbers are kept in `serialports.conf`.

use crate::adapter::{Error, ReadEnd, ReadResult};
use std::time::{Duration, Instant};

/// Flow control bits, as VISA numbers them (VI_ASRL_FLOW_*).
pub const FLOW_XON_XOFF: u8 = 1;
pub const FLOW_RTS_CTS: u8 = 2;
pub const FLOW_DTR_DSR: u8 = 4;

/// A port's settings. Parity and stop bits are numbered as VISA numbers them: parity 0 none, 1 odd,
/// 2 even, 3 mark, 4 space; stop bits 10 (one), 15 (one and a half), 20 (two).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: u8,
    pub stop_bits: u8,
    pub flow: u8,
    pub xon: u8,
    pub xoff: u8,
    pub dtr: bool,
    pub rts: bool,
}

impl Default for Config {
    /// VISA's defaults: 9600 8N1, no flow control, DTR and RTS asserted.
    fn default() -> Config {
        Config { baud: 9600, data_bits: 8, parity: 0, stop_bits: 10, flow: 0, xon: 0x11, xoff: 0x13, dtr: true, rts: true }
    }
}

/// One setting, as an ASRL_SET request names it.
pub mod field {
    pub const BAUD: u32 = 1;
    pub const DATA_BITS: u32 = 2;
    pub const PARITY: u32 = 3;
    pub const STOP_BITS: u32 = 4;
    pub const FLOW: u32 = 5;
    pub const XON: u32 = 6;
    pub const XOFF: u32 = 7;
    pub const DTR: u32 = 8;
    pub const RTS: u32 = 9;
    /// The break state: 1 held, 0 released. Not a setting that is kept.
    pub const BREAK: u32 = 10;
}

/// A setting the port cannot take (a value out of range, or one the driver refused).
fn refused() -> Error {
    Error::Status(crate::proto::ST_BAD_REQUEST)
}

impl Config {
    /// This configuration with `field` set to `v`, if that is a value it can have.
    pub fn with(mut self, f: u32, v: u32) -> Result<Config, Error> {
        match f {
            field::BAUD if v > 0 => self.baud = v,
            field::DATA_BITS if (5..=8).contains(&v) => self.data_bits = v as u8,
            field::PARITY if v <= 4 => self.parity = v as u8,
            field::STOP_BITS if matches!(v, 10 | 15 | 20) => self.stop_bits = v as u8,
            field::FLOW if v <= 7 => self.flow = v as u8,
            field::XON if v <= 0xFF => self.xon = v as u8,
            field::XOFF if v <= 0xFF => self.xoff = v as u8,
            field::DTR if v <= 1 => self.dtr = v != 0,
            field::RTS if v <= 1 => self.rts = v != 0,
            _ => return Err(refused()),
        }
        Ok(self)
    }
}

/// What ASRL_GET answers: the settings, and the state of the port now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    pub cfg: Config,
    /// Bytes received and not yet read (the driver's and ours).
    pub avail: u32,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
    pub brk: bool,
}

impl Status {
    pub fn encode(&self) -> Vec<u8> {
        let c = &self.cfg;
        let mut b = c.baud.to_le_bytes().to_vec();
        b.extend_from_slice(&[c.data_bits, c.parity, c.stop_bits, c.flow, c.xon, c.xoff, c.dtr as u8, c.rts as u8]);
        b.extend_from_slice(&self.avail.to_le_bytes());
        b.push(self.cts as u8 | (self.dsr as u8) << 1 | (self.dcd as u8) << 2 | (self.ri as u8) << 3 | (self.brk as u8) << 4);
        b
    }

    pub fn decode(b: &[u8]) -> Option<Status> {
        if b.len() < 17 {
            return None;
        }
        let cfg = Config {
            baud: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data_bits: b[4],
            parity: b[5],
            stop_bits: b[6],
            flow: b[7],
            xon: b[8],
            xoff: b[9],
            dtr: b[10] != 0,
            rts: b[11] != 0,
        };
        let l = b[16];
        Some(Status {
            cfg,
            avail: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            cts: l & 1 != 0,
            dsr: l & 2 != 0,
            dcd: l & 4 != 0,
            ri: l & 8 != 0,
            brk: l & 16 != 0,
        })
    }
}

/// viFlush's serial buffers, as a FLUSH request's `count` carries them.
pub mod flush {
    /// Throw away what was received and not read.
    pub const IN_DISCARD: u32 = 1;
    /// Throw away what was written and not yet sent.
    pub const OUT_DISCARD: u32 = 2;
    /// Wait until what was written has been sent.
    pub const OUT_DRAIN: u32 = 4;
}

/// The operating system's side of a port: what `Port` needs from it. A trait so that the logic
/// above it can be tested without a port.
pub trait Link: Send {
    fn apply(&mut self, cfg: &Config) -> Result<(), Error>;
    /// Whatever has arrived, waiting up to `wait` for the first byte: possibly nothing.
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error>;
    /// Bytes sent within `wait`.
    fn write(&mut self, data: &[u8], wait: Duration) -> Result<usize, Error>;
    /// Wait up to `wait` until what was written has left: `Timeout` if it has not.
    fn drain(&mut self, wait: Duration) -> Result<(), Error>;
    fn purge(&mut self, input: bool, output: bool) -> Result<(), Error>;
    fn set_break(&mut self, on: bool) -> Result<(), Error>;
    fn avail(&mut self) -> Result<u32, Error>;
    /// CTS, DSR, DCD, RI.
    fn lines(&mut self) -> Result<[bool; 4], Error>;
}

/// An open port.
pub struct Port {
    link: Box<dyn Link>,
    cfg: Config,
    /// Received past where a read stopped: the start of the next read.
    left: Vec<u8>,
    brk: bool,
}

/// The longest one write call waits for the port to take data, so that what arrives meanwhile is
/// taken in between (`Port::write`).
const WRITE_STEP: Duration = Duration::from_millis(20);
/// The most `Port::write` takes in and keeps for the next read.
const TAKE_IN_MAX: usize = 16 << 20;

/// How long a request's timeout lets it wait (`backend::wait_of`; 0 is 1 ms here).
fn wait_of(tmo_ms: u32) -> Duration {
    crate::backend::wait_of(tmo_ms, Duration::from_millis(1))
}

/// A request's timeout as a deadline.
fn deadline(tmo_ms: u32) -> Instant {
    Instant::now() + wait_of(tmo_ms)
}

impl Port {
    /// Open port `n` (COM`n`, or on Linux the tty `port_number` gives `n`) with these settings.
    pub fn open(n: u16, cfg: Config) -> Result<Port, Error> {
        Port::with_link(os::open(n)?, cfg)
    }

    pub fn with_link(mut link: Box<dyn Link>, cfg: Config) -> Result<Port, Error> {
        link.apply(&cfg)?;
        Ok(Port { link, cfg, left: Vec::new(), brk: false })
    }

    pub fn config(&self) -> Config {
        self.cfg
    }

    /// Change one setting (`field`). A value the port refuses leaves the settings as they were.
    pub fn set(&mut self, f: u32, v: u32) -> Result<(), Error> {
        if f == field::BREAK {
            if v > 1 {
                return Err(refused());
            }
            self.link.set_break(v != 0)?;
            self.brk = v != 0;
            return Ok(());
        }
        let c = self.cfg.with(f, v)?;
        if let Err(e) = self.link.apply(&c) {
            let _ = self.link.apply(&self.cfg);
            return Err(e);
        }
        self.cfg = c;
        Ok(())
    }

    pub fn status(&mut self) -> Result<Status, Error> {
        let [cts, dsr, dcd, ri] = self.link.lines()?;
        let avail = self.link.avail()?.saturating_add(self.left.len() as u32);
        Ok(Status { cfg: self.cfg, avail, cts, dsr, dcd, ri, brk: self.brk })
    }

    /// Write all of `data` within the timeout, then, if asked, a break of that many milliseconds.
    ///
    /// What arrives meanwhile is taken in and kept for the next read, so an instrument that answers during a long write
    /// does not fill the receive side and stall the write.
    pub fn write(&mut self, data: &[u8], tmo_ms: u32, break_ms: Option<u16>) -> Result<usize, (usize, Error)> {
        let end = deadline(tmo_ms);
        let mut sent = 0;
        while sent < data.len() {
            self.take_in().map_err(|e| (sent, e))?;
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err((sent, Error::Timeout));
            }
            match self.link.write(&data[sent..], left.min(WRITE_STEP)) {
                Ok(n) => sent += n,
                Err(e) => return Err((sent, e)),
            }
        }
        if let Some(ms) = break_ms {
            self.pulse_break(ms).map_err(|e| (sent, e))?;
        }
        Ok(sent)
    }

    /// Whatever has arrived, without waiting, onto what the next read starts with.
    fn take_in(&mut self) -> Result<(), Error> {
        if self.left.len() >= TAKE_IN_MAX {
            return Ok(()); // nobody reads: stop holding it here, the driver's buffer fills instead
        }
        let mut buf = [0u8; 4096];
        loop {
            match self.link.read(&mut buf, Duration::ZERO)? {
                0 => return Ok(()),
                n => self.left.extend_from_slice(&buf[..n]),
            }
            if self.left.len() >= TAKE_IN_MAX {
                return Ok(());
            }
        }
    }

    /// Read until the termination character (if there is one), `count` bytes, or the timeout.
    /// What arrived past the stop is kept for the next read.
    pub fn read(&mut self, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let end = deadline(tmo_ms);
        let mut out = std::mem::take(&mut self.left);
        let mut buf = vec![0u8; 4096];
        loop {
            if let Some(i) = term.and_then(|t| out.iter().take(count).position(|b| *b == t)) {
                self.left = out.split_off(i + 1);
                return Ok(ReadResult { data: out, end: ReadEnd::TermChar });
            }
            if out.len() >= count {
                self.left = out.split_off(count);
                return Ok(ReadResult { data: out, end: ReadEnd::MaxCount });
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            match self.link.read(&mut buf, left) {
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => {
                    // What had arrived is not lost with the error: the next read starts with it.
                    self.left = out;
                    return Err(e);
                }
            }
        }
    }

    /// Throw away or wait for the buffers (`flush::*`).
    pub fn flush(&mut self, what: u32, tmo_ms: u32) -> Result<(), Error> {
        if what & flush::OUT_DRAIN != 0 {
            self.link.drain(wait_of(tmo_ms))?;
        }
        let (input, output) = (what & flush::IN_DISCARD != 0, what & flush::OUT_DISCARD != 0);
        if input {
            self.left.clear();
        }
        if input || output {
            self.link.purge(input, output)?;
        }
        Ok(())
    }

    fn pulse_break(&mut self, ms: u16) -> Result<(), Error> {
        self.link.set_break(true)?;
        std::thread::sleep(Duration::from_millis(ms.max(1) as u64));
        self.link.set_break(false)?;
        self.brk = false;
        Ok(())
    }

    /// viClear: throw away what is waiting to go out, send a break -- or, for an instrument that
    /// takes IEEE 488.2 strings, `*CLS\n` -- and throw away what came in.
    pub fn clear(&mut self, strings: bool, break_ms: u16, tmo_ms: u32) -> Result<(), Error> {
        self.link.purge(false, true)?;
        if strings {
            self.write(b"*CLS\n", tmo_ms, None).map_err(|(_, e)| e)?;
            // Sent before the input is thrown away, or an answer to what came before could follow it.
            self.link.drain(wait_of(tmo_ms))?;
        } else {
            self.pulse_break(break_ms)?;
        }
        self.left.clear();
        self.link.purge(true, false)
    }

    /// viAssertTrigger for an instrument that takes IEEE 488.2 strings.
    pub fn trigger(&mut self, tmo_ms: u32) -> Result<(), Error> {
        self.write(b"*TRG\n", tmo_ms, None).map(|_| ()).map_err(|(_, e)| e)
    }

    /// viReadSTB for an instrument that takes IEEE 488.2 strings: `*STB?\n`, and the answer. Unread input is thrown
    /// away first, as the instrument drops its own unread output on a new query (IEEE 488.2), so it is not taken for
    /// the answer.
    pub fn stb(&mut self, tmo_ms: u32) -> Result<u8, Error> {
        let end = deadline(tmo_ms);
        self.left.clear();
        self.link.purge(true, false)?;
        self.write(b"*STB?\n", tmo_ms, None).map_err(|(_, e)| e)?;
        let left = if tmo_ms == crate::backend::TMO_INFINITE { tmo_ms } else { end.saturating_duration_since(Instant::now()).as_millis().clamp(1, u32::MAX as u128 - 1) as u32 };
        let r = self.read(64, Some(b'\n'), left)?;
        if r.end == ReadEnd::Timeout {
            return Err(Error::Timeout);
        }
        parse_stb(&r.data).ok_or_else(|| Error::Io(format!("*STB? answered {:?}", String::from_utf8_lossy(&r.data))))
    }
}

/// The answer to `*STB?`: a decimal number, perhaps signed or padded.
pub fn parse_stb(b: &[u8]) -> Option<u8> {
    let s = std::str::from_utf8(b).ok()?.trim();
    s.strip_prefix('+').unwrap_or(s).parse::<u8>().ok()
}

/// Port `n`, bare: for a protocol spoken over a serial line (prologix.rs) rather than a VISA
/// ASRL session. The caller applies the settings.
pub fn open_link(n: u16) -> Result<Box<dyn Link>, Error> {
    os::open(n)
}

/// A serial port the system has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub n: u16,
    /// Which kind of driver: the kernel's device name on Windows (`\Device\USBSER000`,
    /// `\Device\VCP0`, ...), the driver on Linux (`cdc_acm`, `ftdi_sio`, ...), the device's name
    /// on macOS (`usbmodem0001503224691`, `usbserial-A50285BI`).
    pub device: String,
}

/// The serial ports, by number. Opens nothing.
pub fn list() -> Result<Vec<Found>, Error> {
    let mut l = os::list()?;
    l.sort_by_key(|f| f.n);
    l.dedup_by_key(|f| f.n);
    Ok(l)
}

/// The kernel names a Linux port's number is made from: `<prefix><k>` is `base + k`, k below 1000.
/// `ttyS0` is 1 (COM1), as in NI-VISA; the rest are above 999, where no board of a PC reaches.
pub const LINUX_TTYS: [(&str, u16); 4] = [("ttyS", 1), ("ttyUSB", 1000), ("ttyACM", 2000), ("ttyAMA", 3000)];

/// A port's number from what a user or a file calls it: `COM<n>` anywhere; on Linux also a tty
/// device, `/dev/ttyACM0` or `ttyACM0`, or a link to one (`/dev/serial/by-id/...`, resolved now).
pub fn port_number(name: &str) -> Option<u16> {
    let name = name.trim();
    if let Some(n) = com_number(&name.to_ascii_uppercase()) {
        return Some(n);
    }
    if cfg!(target_os = "macos") {
        #[cfg(target_os = "macos")]
        return mac::number_of(name);
    }
    if !cfg!(target_os = "linux") {
        return None;
    }
    linux_number(name)
}

/// A Linux tty's number from its path or name (`port_number`).
fn linux_number(name: &str) -> Option<u16> {
    let kernel = if name.contains('/') {
        let p = std::path::Path::new(name);
        let real = if name.starts_with("/dev/") && !name[5..].contains('/') { p.to_path_buf() } else { std::fs::canonicalize(p).ok()? };
        real.strip_prefix("/dev").ok()?.to_str()?.to_string()
    } else {
        name.to_string()
    };
    tty_number(&kernel)
}

/// `ttyACM0` to its number (`LINUX_TTYS`); anything else, None.
pub fn tty_number(kernel: &str) -> Option<u16> {
    LINUX_TTYS.iter().find_map(|(prefix, base)| {
        let d = kernel.strip_prefix(prefix)?;
        if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let k: u16 = d.parse().ok().filter(|k| *k < 999)?;
        Some(base + k)
    })
}

/// Port `n` as the system names it: `COM3` on Windows, `/dev/ttyACM0` on Linux,
/// `/dev/cu.usbmodem0001503224691` on macOS (`serialports.conf`).
pub fn port_name(n: u16) -> String {
    #[cfg(target_os = "macos")]
    if let Some(p) = mac::path_of(n) {
        return p;
    }
    if cfg!(target_os = "linux") {
        if let Some(k) = tty_name(n) {
            return format!("/dev/{k}");
        }
    }
    format!("COM{n}")
}

/// The Linux kernel name for number `n`, if it is one of `LINUX_TTYS`.
pub fn tty_name(n: u16) -> Option<String> {
    // The highest base not above n: 999..1000 belongs to nothing (ttyS stops at 998).
    let (prefix, base) = LINUX_TTYS.iter().rev().find(|(_, base)| n >= *base)?;
    let k = n - base;
    (k < 999).then(|| format!("{prefix}{k}"))
}

/// macOS names a serial port by its device (`/dev/cu.usbmodem0001503224691`), with no number in it. A port is given the
/// lowest free number the first time it is seen, and keeps it: the numbers are written down in `serialports.conf` in
/// the configuration directory, `<n> = <path>` a line, which a user may edit (`SYMBIOTIC_SERIALPORTS_CONF` names
/// another file).
pub fn parse_port_map(text: &str) -> Vec<(u16, String)> {
    let mut out: Vec<(u16, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((n, path)) = line.split_once('=') else { continue };
        let (Ok(n), path) = (n.trim().parse::<u16>(), path.trim()) else { continue };
        // The first line for a number or a path wins; one that repeats either is left out.
        if n > 0 && path.starts_with('/') && !out.iter().any(|(m, p)| *m == n || p == path) {
            out.push((n, path.to_string()));
        }
    }
    out
}

/// `map` with every one of `paths` in it: one not there yet gets the lowest number no port has.
/// Whether it changed.
pub fn number_ports(map: &mut Vec<(u16, String)>, paths: &[String]) -> bool {
    let mut changed = false;
    for p in paths {
        if map.iter().any(|(_, q)| q == p) {
            continue;
        }
        let Some(n) = (1..=u16::MAX).find(|n| !map.iter().any(|(m, _)| m == n)) else { break };
        map.push((n, p.clone()));
        changed = true;
    }
    changed
}

/// The text of `serialports.conf` for `map`.
pub fn port_map_text(map: &[(u16, String)]) -> String {
    let mut m = map.to_vec();
    m.sort();
    let mut t = String::from("# Serial port numbers on macOS: ASRL<n>::INSTR is the device on its line.\n# Given the first time a port is seen; edit a number here to change it.\n");
    for (n, p) in m {
        t.push_str(&format!("{n} = {p}\n"));
    }
    t
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    fn file() -> Option<PathBuf> {
        std::env::var_os("SYMBIOTIC_SERIALPORTS_CONF").map(PathBuf::from).or_else(|| crate::paths::config_file("serialports.conf"))
    }

    pub fn map() -> Vec<(u16, String)> {
        file().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| super::parse_port_map(&t)).unwrap_or_default()
    }

    /// The numbers of `paths`, a number given (and written down) to each that has none yet.
    pub fn number(paths: &[String]) -> Vec<(u16, String)> {
        let mut map = map();
        if paths.iter().all(|p| map.iter().any(|(_, q)| q == p)) {
            return map;
        }
        if let Some(f) = file() {
            let _ = crate::conffile::update(&f, |text| {
                map = super::parse_port_map(text);
                super::number_ports(&mut map, paths).then(|| super::port_map_text(&map))
            });
        }
        map
    }

    /// `/dev/cu.X`, `cu.X`, `/dev/tty.X` or `tty.X` as the callout device's path (the one opened: it
    /// does not wait for carrier detect).
    pub fn device_path(name: &str) -> Option<String> {
        let base = name.strip_prefix("/dev/").unwrap_or(name);
        let rest = base.strip_prefix("cu.").or_else(|| base.strip_prefix("tty."))?;
        (!rest.is_empty() && !rest.contains('/')).then(|| format!("/dev/cu.{rest}"))
    }

    /// A port's number by its device: the one written down, or a new one if the device is there.
    pub fn number_of(name: &str) -> Option<u16> {
        let path = device_path(name)?;
        if let Some((n, _)) = map().into_iter().find(|(_, p)| *p == path) {
            return Some(n);
        }
        if !std::path::Path::new(&path).exists() {
            return None;
        }
        number(std::slice::from_ref(&path)).into_iter().find(|(_, p)| *p == path).map(|(n, _)| n)
    }

    pub fn path_of(n: u16) -> Option<String> {
        map().into_iter().find(|(m, _)| *m == n).map(|(_, p)| p)
    }
}

/// `COM<n>` to `n`.
pub fn com_number(name: &str) -> Option<u16> {
    let d = name.strip_prefix("COM").or_else(|| name.strip_prefix("com"))?;
    if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    d.parse().ok().filter(|n| *n > 0)
}

#[cfg(windows)]
mod os {
    use super::{com_number, Config, Found, Link, FLOW_DTR_DSR, FLOW_RTS_CTS, FLOW_XON_XOFF};
    use crate::adapter::Error;
    use std::time::Duration;
    use windows_sys::Win32::Devices::Communication::{
        ClearCommBreak, ClearCommError, GetCommModemStatus, GetCommState, PurgeComm, SetCommBreak, SetCommState,
        SetCommTimeouts, COMMTIMEOUTS, COMSTAT, DCB, MS_CTS_ON, MS_DSR_ON, MS_RING_ON, MS_RLSD_ON, PURGE_RXABORT,
        PURGE_RXCLEAR, PURGE_TXABORT, PURGE_TXCLEAR,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_BAD_COMMAND, ERROR_DEVICE_NOT_CONNECTED,
        ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, OPEN_EXISTING};
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};

    struct Com {
        h: HANDLE,
    }

    // SAFETY: a file handle may be used from any thread; the daemon uses it from one at a time.
    unsafe impl Send for Com {}

    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: our own handle, closed once.
            unsafe { CloseHandle(self.h) };
        }
    }

    /// A failed call's error. A USB serial adapter that was unplugged fails every call with one of
    /// these, depending on its driver: that is the port gone, not an I/O error to retry.
    fn last(what: &str) -> Error {
        // SAFETY: reads the calling thread's last error.
        let e = unsafe { GetLastError() };
        match e {
            ERROR_ACCESS_DENIED | ERROR_BAD_COMMAND | ERROR_GEN_FAILURE | ERROR_DEVICE_NOT_CONNECTED
            | ERROR_OPERATION_ABORTED | ERROR_FILE_NOT_FOUND => Error::Gone,
            _ => Error::Io(format!("{what}: Windows error {e}")),
        }
    }

    pub fn open(n: u16) -> Result<Box<dyn Link>, Error> {
        let path: Vec<u16> = format!(r"\\.\COM{n}").encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; no security attributes, no template.
        let h = unsafe {
            CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: as in `last`.
            return Err(match unsafe { GetLastError() } {
                ERROR_FILE_NOT_FOUND => Error::NoBoard,
                ERROR_ACCESS_DENIED => Error::Busy, // another program has it open
                e => Error::Io(format!("cannot open COM{n}: Windows error {e}")),
            });
        }
        Ok(Box::new(Com { h }))
    }

    impl Com {
        fn comstat(&self) -> Result<COMSTAT, Error> {
            let mut errors = 0u32;
            // SAFETY: plain data, filled by the call.
            let mut s: COMSTAT = unsafe { std::mem::zeroed() };
            if unsafe { ClearCommError(self.h, &mut errors, &mut s) } == 0 {
                return Err(last("ClearCommError"));
            }
            Ok(s)
        }

        fn queued(&self) -> Result<u32, Error> {
            Ok(self.comstat()?.cbOutQue)
        }

        fn timeouts(&self, read_ms: u32, write_ms: u32) -> Result<(), Error> {
            // A read returns at once with what is there, or waits up to read_ms for the first byte.
            let t = COMMTIMEOUTS {
                ReadIntervalTimeout: u32::MAX,
                ReadTotalTimeoutMultiplier: u32::MAX,
                ReadTotalTimeoutConstant: read_ms.clamp(1, u32::MAX - 1),
                WriteTotalTimeoutMultiplier: 0,
                WriteTotalTimeoutConstant: write_ms.max(1),
            };
            // SAFETY: our handle and a valid structure.
            if unsafe { SetCommTimeouts(self.h, &t) } == 0 {
                return Err(last("SetCommTimeouts"));
            }
            Ok(())
        }
    }

    fn ms(d: Duration) -> u32 {
        d.as_millis().min(u32::MAX as u128 - 1) as u32
    }

    impl Link for Com {
        fn apply(&mut self, c: &Config) -> Result<(), Error> {
            // SAFETY: DCB is plain data; GetCommState fills it.
            let mut d: DCB = unsafe { std::mem::zeroed() };
            d.DCBlength = std::mem::size_of::<DCB>() as u32;
            if unsafe { GetCommState(self.h, &mut d) } == 0 {
                return Err(last("GetCommState"));
            }
            d.BaudRate = c.baud;
            d.ByteSize = c.data_bits;
            d.Parity = c.parity; // NOPARITY..SPACEPARITY are 0..4, as VISA numbers them
            d.StopBits = match c.stop_bits {
                15 => 1,
                20 => 2,
                _ => 0,
            };
            d.XonChar = c.xon as i8;
            d.XoffChar = c.xoff as i8;
            if d.XonLim == 0 || d.XoffLim == 0 {
                d.XonLim = 2048;
                d.XoffLim = 512;
            }
            let dtr = if c.flow & FLOW_DTR_DSR != 0 { 2 } else { c.dtr as u32 };
            let rts = if c.flow & FLOW_RTS_CTS != 0 { 2 } else { c.rts as u32 };
            let x = (c.flow & FLOW_XON_XOFF != 0) as u32;
            // fBinary, fParity, fOutxCtsFlow, fOutxDsrFlow, fDtrControl (2 bits), fOutX, fInX,
            // fRtsControl (2 bits); fDsrSensitivity, fErrorChar, fNull and fAbortOnError off.
            d._bitfield = 1
                | ((c.parity != 0) as u32) << 1
                | ((c.flow & FLOW_RTS_CTS != 0) as u32) << 2
                | ((c.flow & FLOW_DTR_DSR != 0) as u32) << 3
                | dtr << 4
                | 1 << 7 // fTXContinueOnXoff
                | x << 8
                | x << 9
                | rts << 12;
            // SAFETY: our handle and a filled structure.
            if unsafe { SetCommState(self.h, &d) } == 0 {
                // SAFETY: as in `last`.
                let e = unsafe { GetLastError() };
                return Err(if e == windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER {
                    super::refused() // a setting the driver does not take (a baud rate it lacks)
                } else {
                    Error::Io(format!("SetCommState: Windows error {e}"))
                });
            }
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error> {
            self.timeouts(ms(wait), 1)?;
            let mut n = 0u32;
            // SAFETY: the buffer and its length; synchronous, no OVERLAPPED.
            if unsafe { ReadFile(self.h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 {
                return Err(last("ReadFile"));
            }
            Ok(n as usize)
        }

        fn write(&mut self, data: &[u8], wait: Duration) -> Result<usize, Error> {
            self.timeouts(1, ms(wait))?;
            let mut n = 0u32;
            let len = data.len().min(1 << 20) as u32;
            // SAFETY: the data and its length; synchronous.
            if unsafe { WriteFile(self.h, data.as_ptr(), len, &mut n, std::ptr::null_mut()) } == 0 {
                return Err(last("WriteFile"));
            }
            Ok(n as usize)
        }

        fn drain(&mut self, wait: Duration) -> Result<(), Error> {
            // Not FlushFileBuffers: it waits for as long as it takes, and with the other side holding
            // off flow control that is forever. The driver's output queue, watched until empty.
            let end = std::time::Instant::now() + wait;
            loop {
                if self.queued()? == 0 {
                    return Ok(());
                }
                if std::time::Instant::now() >= end {
                    return Err(Error::Timeout);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn purge(&mut self, input: bool, output: bool) -> Result<(), Error> {
            let f = if input { PURGE_RXABORT | PURGE_RXCLEAR } else { 0 } | if output { PURGE_TXABORT | PURGE_TXCLEAR } else { 0 };
            // SAFETY: our handle.
            if f != 0 && unsafe { PurgeComm(self.h, f) } == 0 {
                return Err(last("PurgeComm"));
            }
            Ok(())
        }

        fn set_break(&mut self, on: bool) -> Result<(), Error> {
            // SAFETY: our handle.
            let ok = unsafe { if on { SetCommBreak(self.h) } else { ClearCommBreak(self.h) } };
            if ok == 0 {
                return Err(last("SetCommBreak"));
            }
            Ok(())
        }

        fn avail(&mut self) -> Result<u32, Error> {
            Ok(self.comstat()?.cbInQue)
        }

        fn lines(&mut self) -> Result<[bool; 4], Error> {
            let mut m = 0u32;
            // SAFETY: our handle.
            if unsafe { GetCommModemStatus(self.h, &mut m) } == 0 {
                return Err(last("GetCommModemStatus"));
            }
            Ok([m & MS_CTS_ON != 0, m & MS_DSR_ON != 0, m & MS_RLSD_ON != 0, m & MS_RING_ON != 0])
        }
    }

    /// HKLM\HARDWARE\DEVICEMAP\SERIALCOMM: one value per port, the kernel's device name holding the
    /// port name. Present ports only; nothing is opened.
    pub fn list() -> Result<Vec<Found>, Error> {
        let key: Vec<u16> = r"HARDWARE\DEVICEMAP\SERIALCOMM".encode_utf16().chain(Some(0)).collect();
        let mut hk: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated key name; hk receives the handle.
        if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut hk) } != 0 {
            return Ok(Vec::new()); // no serial ports at all: the key does not exist
        }
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut data = [0u16; 128];
            let (mut nl, mut dl, mut ty) = (name.len() as u32, (data.len() * 2) as u32, 0u32);
            // SAFETY: buffers and their lengths (characters for the name, bytes for the data).
            let r = unsafe {
                RegEnumValueW(hk, i, name.as_mut_ptr(), &mut nl, std::ptr::null(), &mut ty, data.as_mut_ptr() as *mut u8, &mut dl)
            };
            if r != 0 {
                break; // ERROR_NO_MORE_ITEMS, or a value too long to be a port name
            }
            let port = String::from_utf16_lossy(&data[..(dl as usize / 2)]);
            if let Some(n) = com_number(port.trim_end_matches('\0')) {
                out.push(Found { n, device: String::from_utf16_lossy(&name[..nl as usize]) });
            }
        }
        // SAFETY: the key opened above.
        unsafe { RegCloseKey(hk) };
        Ok(out)
    }
}

/// The tty requests the shared Unix code below makes. libc has them for Linux; for macOS it has few of
/// them, so they are made here from `<sys/ttycom.h>`'s `_IO`, `_IOR` and `_IOW`.
#[cfg(target_os = "linux")]
mod tc {
    pub type Req = libc::Ioctl;
    pub use libc::{FIONREAD, TIOCCBRK, TIOCEXCL, TIOCMBIC, TIOCMBIS, TIOCMGET, TIOCOUTQ, TIOCSBRK};
    pub use libc::{TIOCM_CAR, TIOCM_CTS, TIOCM_DSR, TIOCM_DTR, TIOCM_RNG, TIOCM_RTS};
}

#[cfg(target_os = "macos")]
mod tc {
    pub type Req = libc::c_ulong;
    pub const FIONREAD: Req = libc::FIONREAD;
    pub const TIOCEXCL: Req = 0x2000_740d; // _IO('t', 13)
    pub const TIOCOUTQ: Req = 0x4004_7473; // _IOR('t', 115, int)
    pub const TIOCMBIS: Req = 0x8004_746c; // _IOW('t', 108, int)
    pub const TIOCMBIC: Req = 0x8004_746b; // _IOW('t', 107, int)
    pub const TIOCMGET: Req = 0x4004_746a; // _IOR('t', 106, int)
    pub const TIOCSBRK: Req = 0x2000_747b; // _IO('t', 123)
    pub const TIOCCBRK: Req = 0x2000_747a; // _IO('t', 122)
    /// `_IOW('T', 2, speed_t)`, from IOKit's serial headers: any rate, where termios takes only its table's.
    pub const IOSSIOSPEED: Req = 0x8008_5402;
    pub const TIOCM_DTR: libc::c_int = 0x002;
    pub const TIOCM_RTS: libc::c_int = 0x004;
    pub const TIOCM_CTS: libc::c_int = 0x020;
    pub const TIOCM_CAR: libc::c_int = 0x040;
    pub const TIOCM_RNG: libc::c_int = 0x080;
    pub const TIOCM_DSR: libc::c_int = 0x100;
}

/// Linux and macOS: a tty through termios, poll and the tty ioctls. What differs -- how a rate is
/// set, which ports there are and what they are called -- is marked.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod os {
    #[cfg(target_os = "linux")]
    use super::{tty_name, tty_number};
    use super::tc;
    use super::{Config, Found, Link, FLOW_DTR_DSR, FLOW_RTS_CTS, FLOW_XON_XOFF};
    use crate::adapter::Error;
    use std::time::{Duration, Instant};

    struct Tty {
        fd: libc::c_int,
        name: String,
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            // SAFETY: our own descriptor, closed once.
            unsafe { libc::close(self.fd) };
        }
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    impl Tty {
        /// A failed call's error. An unplugged USB serial adapter fails every call with one of
        /// these: that is the port gone, not an I/O error to retry.
        fn last(&self, what: &str) -> Error {
            match errno() {
                libc::EIO | libc::ENXIO | libc::ENODEV | libc::EBADF => Error::Gone,
                e => Error::Io(format!("{what} on {}: {}", self.name, std::io::Error::from_raw_os_error(e))),
            }
        }

        fn ioctl_int(&self, req: tc::Req, what: &str) -> Result<libc::c_int, Error> {
            let mut v: libc::c_int = 0;
            // SAFETY: our descriptor; the request writes one int.
            if unsafe { libc::ioctl(self.fd, req, &mut v) } < 0 {
                return Err(self.last(what));
            }
            Ok(v)
        }

        fn modem(&self, bits: libc::c_int, on: bool) -> Result<(), Error> {
            let req = if on { tc::TIOCMBIS } else { tc::TIOCMBIC };
            // SAFETY: our descriptor; the request reads one int.
            if unsafe { libc::ioctl(self.fd, req, &bits) } < 0 {
                return Err(self.last("TIOCMBIS"));
            }
            Ok(())
        }

        /// Wait up to `wait` for `events`; false on the timeout.
        fn poll(&self, events: libc::c_short, wait: Duration) -> Result<bool, Error> {
            let end = Instant::now() + wait;
            loop {
                let mut p = libc::pollfd { fd: self.fd, events, revents: 0 };
                let ms = end.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32;
                // SAFETY: one pollfd.
                let r = unsafe { libc::poll(&mut p, 1, ms) };
                if r < 0 {
                    if errno() == libc::EINTR {
                        continue;
                    }
                    return Err(self.last("poll"));
                }
                if r == 0 {
                    return Ok(false);
                }
                // A USB port that went away answers POLLHUP / POLLERR (or POLLNVAL) and nothing else.
                if p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && p.revents & events == 0 {
                    return Err(Error::Gone);
                }
                return Ok(true);
            }
        }
    }

    /// Port `n`'s device.
    fn device(n: u16) -> Option<String> {
        #[cfg(target_os = "linux")]
        return tty_name(n).map(|k| format!("/dev/{k}"));
        #[cfg(target_os = "macos")]
        return super::mac::path_of(n);
    }

    pub fn open(n: u16) -> Result<Box<dyn Link>, Error> {
        let Some(name) = device(n) else { return Err(Error::NoBoard) };
        let path = std::ffi::CString::new(name.clone()).expect("no NUL in a tty name");
        // Non-blocking: poll() does the waiting, and opening does not wait for DCD.
        // SAFETY: a NUL-terminated path.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(match errno() {
                libc::ENOENT | libc::ENODEV | libc::ENXIO => Error::NoBoard,
                libc::EBUSY => Error::Busy,
                libc::EACCES | libc::EPERM => Error::Io(if cfg!(target_os = "linux") {
                    format!("cannot open {name}: permission denied (the user must be in the group that owns it, dialout on most distributions)")
                } else {
                    format!("cannot open {name}: permission denied")
                }),
                e => Error::Io(format!("cannot open {name}: {}", std::io::Error::from_raw_os_error(e))),
            });
        }
        let t = Tty { fd, name };
        // One process at a time, as Windows gives a COM port. Two conventions, both kept: flock,
        // which pyserial's exclusive=True takes (and so a port it holds is Busy here), and
        // TIOCEXCL, which makes any other open fail with EBUSY while we hold the port.
        // SAFETY: our descriptor.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(if errno() == libc::EWOULDBLOCK { Error::Busy } else { t.last("flock") });
        }
        // SAFETY: our descriptor; the request takes no argument.
        if unsafe { libc::ioctl(fd, tc::TIOCEXCL) } < 0 {
            return Err(t.last("TIOCEXCL"));
        }
        Ok(Box::new(t))
    }

    impl Link for Tty {
        #[cfg(target_os = "linux")]
        fn apply(&mut self, c: &Config) -> Result<(), Error> {
            if c.flow & FLOW_DTR_DSR != 0 {
                return Err(super::refused()); // Linux's tty layer has no DTR/DSR flow control
            }
            // termios2: any baud rate (BOTHER), not only the Bxxx table.
            // SAFETY: plain data, filled by TCGETS2.
            let mut t: libc::termios2 = unsafe { std::mem::zeroed() };
            // SAFETY: our descriptor and a termios2.
            if unsafe { libc::ioctl(self.fd, libc::TCGETS2, &mut t) } < 0 {
                return Err(self.last("TCGETS2"));
            }
            // Raw: no line editing, no translation, no signals, no echo.
            t.c_iflag &= !(libc::IGNBRK | libc::BRKINT | libc::PARMRK | libc::ISTRIP | libc::INLCR | libc::IGNCR | libc::ICRNL | libc::IXON | libc::IXOFF | libc::IXANY | libc::INPCK);
            t.c_oflag &= !libc::OPOST;
            t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CMSPAR | libc::CSTOPB | libc::CRTSCTS | libc::CBAUD | libc::HUPCL);
            t.c_cflag |= libc::CREAD | libc::CLOCAL | libc::BOTHER;
            t.c_cflag |= match c.data_bits {
                5 => libc::CS5,
                6 => libc::CS6,
                7 => libc::CS7,
                _ => libc::CS8,
            };
            t.c_cflag |= match c.parity {
                1 => libc::PARENB | libc::PARODD,
                2 => libc::PARENB,
                3 => libc::PARENB | libc::PARODD | libc::CMSPAR,
                4 => libc::PARENB | libc::CMSPAR,
                _ => 0,
            };
            // CSTOPB is two stop bits, or one and a half with five data bits; 1.5 with more is refused.
            match (c.stop_bits, c.data_bits) {
                (20, _) | (15, 5) => t.c_cflag |= libc::CSTOPB,
                (15, _) => return Err(super::refused()),
                _ => {}
            }
            if c.flow & FLOW_RTS_CTS != 0 {
                t.c_cflag |= libc::CRTSCTS;
            }
            if c.flow & FLOW_XON_XOFF != 0 {
                t.c_iflag |= libc::IXON | libc::IXOFF;
            }
            t.c_cc[libc::VSTART] = c.xon;
            t.c_cc[libc::VSTOP] = c.xoff;
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            t.c_ispeed = c.baud;
            t.c_ospeed = c.baud;
            // SAFETY: our descriptor and a filled termios2.
            if unsafe { libc::ioctl(self.fd, libc::TCSETS2, &t) } < 0 {
                return Err(if errno() == libc::EINVAL { super::refused() } else { self.last("TCSETS2") });
            }
            // A driver that cannot do a rate may set another one without failing: read it back.
            // SAFETY: as above.
            let mut back: libc::termios2 = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(self.fd, libc::TCGETS2, &mut back) } == 0 && back.c_ospeed != 0 {
                let (want, got) = (c.baud as u64, back.c_ospeed as u64);
                if got * 100 < want * 97 || got * 100 > want * 103 {
                    return Err(super::refused());
                }
            }
            // The lines flow control does not drive.
            self.modem(tc::TIOCM_DTR, c.dtr)?;
            if c.flow & FLOW_RTS_CTS == 0 {
                self.modem(tc::TIOCM_RTS, c.rts)?;
            }
            Ok(())
        }

        /// termios has only its table of rates on macOS; another is set after it with IOSSIOSPEED,
        /// as Apple's serial drivers take any. There is no mark or space parity (no CMSPAR) and no
        /// DTR/DSR flow control: refused, as settings the port cannot have.
        #[cfg(target_os = "macos")]
        fn apply(&mut self, c: &Config) -> Result<(), Error> {
            const TABLE: [u32; 18] = [50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400];
            if c.flow & FLOW_DTR_DSR != 0 || c.parity >= 3 {
                return Err(super::refused());
            }
            // SAFETY: plain data, filled by tcgetattr.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: our descriptor and a termios.
            if unsafe { libc::tcgetattr(self.fd, &mut t) } < 0 {
                return Err(self.last("tcgetattr"));
            }
            t.c_iflag &= !(libc::IGNBRK | libc::BRKINT | libc::PARMRK | libc::ISTRIP | libc::INLCR | libc::IGNCR | libc::ICRNL | libc::IXON | libc::IXOFF | libc::IXANY | libc::INPCK);
            t.c_oflag &= !libc::OPOST;
            t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS | libc::HUPCL);
            t.c_cflag |= libc::CREAD | libc::CLOCAL;
            t.c_cflag |= match c.data_bits {
                5 => libc::CS5,
                6 => libc::CS6,
                7 => libc::CS7,
                _ => libc::CS8,
            };
            t.c_cflag |= match c.parity {
                1 => libc::PARENB | libc::PARODD,
                2 => libc::PARENB,
                _ => 0,
            };
            match (c.stop_bits, c.data_bits) {
                (20, _) | (15, 5) => t.c_cflag |= libc::CSTOPB,
                (15, _) => return Err(super::refused()),
                _ => {}
            }
            if c.flow & FLOW_RTS_CTS != 0 {
                t.c_cflag |= libc::CRTSCTS;
            }
            if c.flow & FLOW_XON_XOFF != 0 {
                t.c_iflag |= libc::IXON | libc::IXOFF;
            }
            t.c_cc[libc::VSTART] = c.xon;
            t.c_cc[libc::VSTOP] = c.xoff;
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 0;
            let in_table = TABLE.contains(&c.baud);
            // SAFETY: a termios of our own.
            unsafe { libc::cfsetspeed(&mut t, if in_table { c.baud } else { 9600 } as libc::speed_t) };
            // SAFETY: our descriptor and a filled termios.
            if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &t) } < 0 {
                return Err(if errno() == libc::EINVAL { super::refused() } else { self.last("tcsetattr") });
            }
            if !in_table {
                let speed = c.baud as libc::speed_t;
                // SAFETY: our descriptor; the request reads one speed_t.
                if unsafe { libc::ioctl(self.fd, tc::IOSSIOSPEED, &speed) } < 0 {
                    return Err(super::refused());
                }
            }
            self.modem(tc::TIOCM_DTR, c.dtr)?;
            if c.flow & FLOW_RTS_CTS == 0 {
                self.modem(tc::TIOCM_RTS, c.rts)?;
            }
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error> {
            if !self.poll(libc::POLLIN, wait)? {
                return Ok(0);
            }
            // SAFETY: the buffer and its length.
            let r = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r < 0 {
                return match errno() {
                    libc::EAGAIN | libc::EINTR => Ok(0),
                    _ => Err(self.last("read")),
                };
            }
            if r == 0 {
                // Readable and nothing to read: the device is gone (hang-up).
                return Err(Error::Gone);
            }
            Ok(r as usize)
        }

        fn write(&mut self, data: &[u8], wait: Duration) -> Result<usize, Error> {
            if !self.poll(libc::POLLOUT, wait)? {
                return Ok(0);
            }
            // SAFETY: the data and its length.
            let r = unsafe { libc::write(self.fd, data.as_ptr().cast(), data.len().min(1 << 20)) };
            if r < 0 {
                return match errno() {
                    libc::EAGAIN | libc::EINTR => Ok(0),
                    _ => Err(self.last("write")),
                };
            }
            Ok(r as usize)
        }

        fn drain(&mut self, wait: Duration) -> Result<(), Error> {
            // Not tcdrain: it waits for as long as it takes, and with the other side holding off
            // flow control that is forever. The output queue, watched until empty.
            let end = Instant::now() + wait;
            loop {
                if self.ioctl_int(tc::TIOCOUTQ, "TIOCOUTQ")? == 0 {
                    return Ok(());
                }
                if Instant::now() >= end {
                    return Err(Error::Timeout);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn purge(&mut self, input: bool, output: bool) -> Result<(), Error> {
            let q = match (input, output) {
                (true, true) => libc::TCIOFLUSH,
                (true, false) => libc::TCIFLUSH,
                (false, true) => libc::TCOFLUSH,
                (false, false) => return Ok(()),
            };
            // SAFETY: our descriptor.
            if unsafe { libc::tcflush(self.fd, q) } < 0 {
                return Err(self.last("tcflush"));
            }
            Ok(())
        }

        fn set_break(&mut self, on: bool) -> Result<(), Error> {
            let req = if on { tc::TIOCSBRK } else { tc::TIOCCBRK };
            // SAFETY: our descriptor; the request takes no argument.
            if unsafe { libc::ioctl(self.fd, req) } < 0 {
                // cdc_acm answers EOPNOTSUPP for a device that does not advertise that it can send a break.
                if errno() == libc::EOPNOTSUPP {
                    return Err(Error::Status(crate::proto::ST_UNSUPPORTED));
                }
                return Err(self.last("TIOCSBRK"));
            }
            Ok(())
        }

        fn avail(&mut self) -> Result<u32, Error> {
            Ok(self.ioctl_int(tc::FIONREAD, "FIONREAD")?.max(0) as u32)
        }

        fn lines(&mut self) -> Result<[bool; 4], Error> {
            let m = self.ioctl_int(tc::TIOCMGET, "TIOCMGET")?;
            Ok([m & tc::TIOCM_CTS != 0, m & tc::TIOCM_DSR != 0, m & tc::TIOCM_CAR != 0, m & tc::TIOCM_RNG != 0])
        }
    }

    /// /dev's callout devices, numbered as `serialports.conf` has them (a new one gets the next free
    /// number, written down). The Mac's own -- the Bluetooth port, the debug consoles -- are left out.
    /// Nothing is opened.
    #[cfg(target_os = "macos")]
    pub fn list() -> Result<Vec<Found>, Error> {
        const OWN: [&str; 3] = ["Bluetooth-Incoming-Port", "debug-console", "wlan-debug"];
        let Ok(dir) = std::fs::read_dir("/dev") else { return Ok(Vec::new()) };
        let mut paths: Vec<String> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let rest = name.strip_prefix("cu.")?;
                (!OWN.contains(&rest)).then(|| format!("/dev/{name}"))
            })
            .collect();
        paths.sort();
        let map = super::mac::number(&paths);
        Ok(map
            .into_iter()
            .filter(|(_, p)| paths.contains(p))
            .map(|(n, p)| Found { n, device: p.trim_start_matches("/dev/cu.").to_string() })
            .collect())
    }

    /// /sys/class/tty: the ttys with a device behind them, named as `LINUX_TTYS` has them. The
    /// 8250 driver makes ttyS0..31 whether or not a UART is there; those on the platform bus
    /// with no I/O port are left out (a PCI or ACPI UART stays). Nothing is opened.
    #[cfg(target_os = "linux")]
    pub fn list() -> Result<Vec<Found>, Error> {
        let Ok(dir) = std::fs::read_dir("/sys/class/tty") else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(n) = tty_number(&name) else { continue };
            let dev = e.path().join("device");
            let Ok(real) = std::fs::canonicalize(&dev) else { continue };
            let driver = std::fs::read_link(dev.join("driver"))
                .ok()
                .and_then(|l| l.file_name().map(|f| f.to_string_lossy().to_string()))
                .unwrap_or_default();
            if name.starts_with("ttyS") && is_phantom_8250(&e.path(), &real) {
                continue;
            }
            out.push(Found { n, device: driver });
        }
        Ok(out)
    }

    /// A ttyS the 8250 driver made for a UART that is not there: on the platform bus (serial8250),
    /// with port type 0 (PORT_UNKNOWN).
    #[cfg(target_os = "linux")]
    fn is_phantom_8250(class: &std::path::Path, device: &std::path::Path) -> bool {
        let platform = device.to_string_lossy().contains("/serial8250");
        let unknown = std::fs::read_to_string(class.join("type")).map(|t| t.trim() == "0").unwrap_or(false);
        platform && unknown
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod os {
    use super::{Found, Link};
    use crate::adapter::Error;

    pub fn open(_n: u16) -> Result<Box<dyn Link>, Error> {
        Err(Error::NoBoard)
    }

    pub fn list() -> Result<Vec<Found>, Error> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infinite_is_infinite_here_too() {
        use crate::backend::TMO_INFINITE;
        assert!(wait_of(TMO_INFINITE) > Duration::from_secs(24 * 3600), "VI_TMO_INFINITE, as on the adapters");
        assert_eq!(wait_of(120_000), Duration::from_millis(120_000), "longer than the u16 the protocol had");
        assert_eq!(wait_of(0), Duration::from_millis(1));
        let _ = deadline(TMO_INFINITE); // representable
    }
    use std::sync::{Arc, Mutex};

    /// A port whose input is a script of chunks, one per read, and whose output is recorded.
    #[derive(Default)]
    struct Fake {
        input: Arc<Mutex<Vec<Vec<u8>>>>,
        /// Answers: put into the input as each write is made.
        replies: Arc<Mutex<Vec<Vec<u8>>>>,
        sent: Arc<Mutex<Vec<u8>>>,
        applied: Arc<Mutex<Vec<Config>>>,
        refuse_baud: Option<u32>,
        breaks: Arc<Mutex<Vec<bool>>>,
    }

    impl Link for Fake {
        fn apply(&mut self, c: &Config) -> Result<(), Error> {
            if Some(c.baud) == self.refuse_baud {
                return Err(refused());
            }
            self.applied.lock().unwrap().push(*c);
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error> {
            let mut i = self.input.lock().unwrap();
            if i.is_empty() {
                std::thread::sleep(wait.min(Duration::from_millis(5)));
                return Ok(0);
            }
            let chunk = i.remove(0);
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            if n < chunk.len() {
                i.insert(0, chunk[n..].to_vec());
            }
            Ok(n)
        }
        fn write(&mut self, data: &[u8], _wait: Duration) -> Result<usize, Error> {
            self.sent.lock().unwrap().extend_from_slice(data);
            let mut r = self.replies.lock().unwrap();
            if !r.is_empty() {
                let a = r.remove(0);
                self.input.lock().unwrap().push(a);
            }
            Ok(data.len())
        }
        fn drain(&mut self, _wait: Duration) -> Result<(), Error> {
            Ok(())
        }
        fn purge(&mut self, input: bool, _output: bool) -> Result<(), Error> {
            if input {
                self.input.lock().unwrap().clear();
            }
            Ok(())
        }
        fn set_break(&mut self, on: bool) -> Result<(), Error> {
            self.breaks.lock().unwrap().push(on);
            Ok(())
        }
        fn avail(&mut self) -> Result<u32, Error> {
            Ok(self.input.lock().unwrap().iter().map(|c| c.len() as u32).sum())
        }
        fn lines(&mut self) -> Result<[bool; 4], Error> {
            Ok([true, false, false, false])
        }
    }

    fn port(chunks: &[&[u8]]) -> (Port, Arc<Mutex<Vec<u8>>>, Arc<Mutex<Vec<bool>>>) {
        let f = Fake { input: Arc::new(Mutex::new(chunks.iter().map(|c| c.to_vec()).collect())), ..Default::default() };
        let (sent, breaks) = (f.sent.clone(), f.breaks.clone());
        (Port::with_link(Box::new(f), Config::default()).unwrap(), sent, breaks)
    }

    #[test]
    fn a_read_stops_at_the_character_and_keeps_the_rest() {
        let (mut p, _, _) = port(&[b"1.5\n2.5\n3", b".5\n"]);
        let r = p.read(100, Some(b'\n'), 100).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"1.5\n"[..], ReadEnd::TermChar));
        let r = p.read(100, Some(b'\n'), 100).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"2.5\n"[..], ReadEnd::TermChar), "the second line was already here");
        let r = p.read(100, Some(b'\n'), 100).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"3.5\n"[..], ReadEnd::TermChar), "joined across reads from the port");
    }

    #[test]
    fn the_count_comes_first_when_the_character_lies_beyond_it() {
        let (mut p, _, _) = port(&[b"abcdef\n"]);
        let r = p.read(3, Some(b'\n'), 100).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"abc"[..], ReadEnd::MaxCount));
        let r = p.read(10, Some(b'\n'), 100).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"def\n"[..], ReadEnd::TermChar));
    }

    #[test]
    fn without_a_character_a_read_ends_at_the_count_or_times_out_with_what_came() {
        let (mut p, _, _) = port(&[b"ab\ncd"]);
        let r = p.read(10, None, 30).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"ab\ncd"[..], ReadEnd::Timeout));
        let r = p.read(10, None, 30).unwrap();
        assert!(r.data.is_empty() && r.end == ReadEnd::Timeout, "nothing is kept after a timeout returned it all");
    }

    #[test]
    fn a_discarded_input_includes_what_was_kept() {
        let (mut p, _, _) = port(&[b"one\ntwo\n"]);
        p.read(100, Some(b'\n'), 100).unwrap();
        assert_eq!(p.status().unwrap().avail, 4, "two\\n is kept, and counts as available");
        p.flush(flush::IN_DISCARD, 10).unwrap();
        assert_eq!(p.status().unwrap().avail, 0);
        assert_eq!(p.read(100, Some(b'\n'), 20).unwrap().end, ReadEnd::Timeout);
    }

    #[test]
    fn clear_sends_a_break_or_star_cls() {
        let (mut p, sent, breaks) = port(&[b"stale\n"]);
        p.clear(false, 1, 100).unwrap();
        assert_eq!(*breaks.lock().unwrap(), vec![true, false]);
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(p.status().unwrap().avail, 0, "what had come in is gone");
        p.clear(true, 1, 100).unwrap();
        assert_eq!(sent.lock().unwrap().as_slice(), b"*CLS\n");
        assert_eq!(breaks.lock().unwrap().len(), 2, "no second break");
    }

    #[test]
    fn macos_ports_keep_the_numbers_they_were_given() {
        let text = "# c\n2 = /dev/cu.usbmodem1\nx = /dev/cu.bad\n3 = /dev/cu.usbmodem1\n2 = /dev/cu.other\n0 = /dev/cu.zero\n5 = relative\n";
        let mut map = parse_port_map(text);
        assert_eq!(map, [(2, "/dev/cu.usbmodem1".to_string())], "a repeated number or path, a bad line: left out");
        let paths = ["/dev/cu.usbmodem1".to_string(), "/dev/cu.usbserial-A".to_string(), "/dev/cu.usbserial-B".to_string()];
        assert!(number_ports(&mut map, &paths));
        assert_eq!(map, [(2, "/dev/cu.usbmodem1".to_string()), (1, "/dev/cu.usbserial-A".to_string()), (3, "/dev/cu.usbserial-B".to_string())], "the lowest free numbers");
        assert!(!number_ports(&mut map, &paths), "nothing new: unchanged");
        // Written and read back: the same numbers, a port that is unplugged keeping its own.
        let again = parse_port_map(&port_map_text(&map));
        let mut sorted = map.clone();
        sorted.sort();
        assert_eq!(again, sorted);
        let mut m = again;
        number_ports(&mut m, &["/dev/cu.new".to_string()]);
        assert_eq!(m.last().unwrap(), &(4, "/dev/cu.new".to_string()));
    }

    #[test]
    fn a_status_byte_is_read_as_a_decimal_line() {
        // Output nobody read is in the stream ahead of the answer, part of it already taken in.
        let f = Fake {
            input: Arc::new(Mutex::new(vec![b"1.234E+0".to_vec(), b"0\n".to_vec()])),
            replies: Arc::new(Mutex::new(vec![b"+72\r\n".to_vec()])),
            ..Default::default()
        };
        let sent = f.sent.clone();
        let mut p = Port::with_link(Box::new(f), Config::default()).unwrap();
        assert_eq!(p.read(3, None, 100).unwrap().data, b"1.2", "a read that left the rest unread");
        assert_eq!(p.stb(100).unwrap(), 72, "the unread output is not taken for the answer");
        assert_eq!(sent.lock().unwrap().as_slice(), b"*STB?\n");
        assert_eq!(parse_stb(b" 0\n"), Some(0));
        assert_eq!(parse_stb(b"256\n"), None);
        assert_eq!(parse_stb(b"x\n"), None);
    }

    #[test]
    fn a_write_can_end_with_a_break() {
        let (mut p, sent, breaks) = port(&[]);
        assert_eq!(p.write(b"OUTP ON", 100, Some(1)).unwrap(), 7);
        assert_eq!(sent.lock().unwrap().as_slice(), b"OUTP ON");
        assert_eq!(*breaks.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn a_setting_the_port_refuses_leaves_the_old_one() {
        let f = Fake { refuse_baud: Some(12345), ..Default::default() };
        let applied = f.applied.clone();
        let mut p = Port::with_link(Box::new(f), Config::default()).unwrap();
        p.set(field::BAUD, 115200).unwrap();
        assert_eq!(p.config().baud, 115200);
        assert!(p.set(field::BAUD, 12345).is_err());
        assert_eq!(p.config().baud, 115200, "unchanged");
        assert_eq!(applied.lock().unwrap().last().unwrap().baud, 115200, "and put back on the port");
        assert!(p.set(field::DATA_BITS, 9).is_err(), "out of range, never reaches the port");
        assert!(p.set(field::STOP_BITS, 12).is_err());
        p.set(field::STOP_BITS, 20).unwrap();
        p.set(field::FLOW, (FLOW_RTS_CTS | FLOW_XON_XOFF) as u32).unwrap();
        assert_eq!(p.config().flow, 3);
    }

    #[test]
    fn a_status_goes_over_the_wire_intact() {
        let s = Status {
            cfg: Config { baud: 115200, data_bits: 7, parity: 2, stop_bits: 20, flow: 2, xon: 1, xoff: 2, dtr: false, rts: true },
            avail: 1234,
            cts: true,
            dsr: false,
            dcd: true,
            ri: false,
            brk: true,
        };
        assert_eq!(Status::decode(&s.encode()), Some(s));
        assert_eq!(Status::decode(&[0; 5]), None);
    }

    #[test]
    fn com_names() {
        assert_eq!(com_number("COM3"), Some(3));
        assert_eq!(com_number("com12"), Some(12));
        assert_eq!(com_number("COM0"), None);
        assert_eq!(com_number("CNCA0"), None);
        assert_eq!(com_number("COM"), None);
        assert_eq!(com_number("COM1a"), None);
    }

    #[test]
    fn linux_port_numbers_go_both_ways() {
        assert_eq!(tty_number("ttyS0"), Some(1));
        assert_eq!(tty_number("ttyUSB3"), Some(1003));
        assert_eq!(tty_number("ttyACM0"), Some(2000));
        assert_eq!(tty_number("ttyAMA1"), Some(3001));
        assert_eq!(tty_number("tty0"), None);
        assert_eq!(tty_number("ttyACM"), None);
        assert_eq!(tty_number("ttyACMx"), None);
        assert_eq!(tty_number("ttyUSB999"), None);
        for n in [1, 998, 999, 1000, 1998, 2000, 3000, 3998] {
            let k = tty_name(n).unwrap();
            assert_eq!(tty_number(&k), Some(n), "{n} -> {k}");
        }
        assert_eq!(tty_name(0), None);
        assert_eq!(tty_name(4000), None);
        assert_eq!(port_number("COM3"), Some(3));
        if cfg!(target_os = "linux") {
            assert_eq!(port_number("/dev/ttyACM0"), Some(2000));
            assert_eq!(port_number("ttyUSB1"), Some(1001));
            assert_eq!(port_name(2000), "/dev/ttyACM0");
            assert_eq!(port_number("/dev/serial/by-id/not-there"), None);
        } else {
            assert_eq!(port_number("/dev/ttyACM0"), None);
            assert_eq!(port_name(3), "COM3");
        }
    }
}
