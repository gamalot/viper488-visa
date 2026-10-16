//! Prologix GPIB-USB and AR488 controllers on a serial port, as GPIB boards.
//!
//! Written from the Prologix GPIB-USB manual and AR488's source (read as a specification, no code taken from it); the
//! tests at the bottom run the protocol against a simulator of both.
//!
//! The controller owns the GPIB chip and takes orders as lines of text: a line opening `++` is for the controller,
//! anything else is data for the instrument `++addr` named. So this is a translation between the daemon's operations
//! and those lines, over a COM port -- a Prologix's own FTDI virtual port, or an Arduino's for an AR488 -- opened only
//! for a port the user configured as a bridge (`bridges.conf`). Nothing is ever sent to a port that was not.
//!
//! AR488 IS NOT A PROLOGIX: it has every Prologix command, and some of them do other things. Most of all `++savecfg`:
//! to a Prologix it switches saving to EEPROM off (it is on at every power-up, and then every `++addr` is an EEPROM
//! write); an AR488 ignores the argument and writes its EEPROM there and then. So the dialect is found out first
//! (`++ver real`: an AR488's version string can be set to impersonate a Prologix, the real one cannot), and an AR488 is
//! never sent `++savecfg`. An AR488 also sends a line longer than its 128-byte buffer in pieces, each ending with EOI,
//! so a long write to one goes in short lines, all but the last without EOI.
//!
//! Reads: the controller reads a message to its end and hands it over; EOI shows as a marker it appends (`++eot_char`):
//! a NUL from a Prologix. An AR488 prints the character as a decimal number instead -- `++eot_char 0` appends the text
//! "0" -- so it is given 255 and both "255" and the byte 0xFF are taken for the marker, the second for a firmware that
//! sends the character itself. A read that stops earlier -- at a termination character or a count -- leaves the rest of
//! the message on its way, and it is the instrument's: kept for its next read (owed.rs), never thrown away. While the
//! rest is still coming, the instrument is `talking`, and the next operation first takes it in (`settle`).

use crate::adapter::{Error, ReadEnd, ReadResult};
use crate::backend::{Backend, BusOps, Entry, Kind, RAW};
use crate::owed::Owed;
use crate::proto::ST_UNSUPPORTED;
use crate::serial::{self, Link};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Which command set a controller speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Prologix,
    Ar488,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Prologix => "prologix",
            Dialect::Ar488 => "ar488",
        }
    }

    pub fn from_name(s: &str) -> Option<Dialect> {
        [Dialect::Prologix, Dialect::Ar488].into_iter().find(|d| d.name().eq_ignore_ascii_case(s))
    }
}

// ---------------------------------------------------------------------------------------
// Which ports are bridges
// ---------------------------------------------------------------------------------------

/// `bridges.conf` in the configuration directory: `<port> [prologix|ar488]` per line, `#` comments, the port `COM<n>`
/// or on Linux a tty (`/dev/ttyACM0`, or a stable link such as `/dev/serial/by-id/...`, resolved each time it is read;
/// serial::port_number). `SYMBIOTIC_BRIDGES_CONF` names another file.
pub fn conf_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_BRIDGES_CONF") {
        return Some(PathBuf::from(p));
    }
    // The configuration directory: shared by all users after an all-users install (paths.rs).
    crate::paths::config_file("bridges.conf")
}

/// The bridges in `text`: port number and, if given, the dialect. A line that is not one is
/// ignored (a hand edit gone wrong costs that line; so does a link whose device is unplugged).
pub fn parse_conf(text: &str) -> Vec<(u16, Option<Dialect>)> {
    let mut out: Vec<(u16, Option<Dialect>)> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut w = l.split_whitespace();
        let Some(n) = w.next().and_then(serial::port_number) else { continue };
        let d = match w.next() {
            None => None,
            Some(x) => match Dialect::from_name(x) {
                Some(d) => Some(d),
                None => continue,
            },
        };
        if !out.iter().any(|(m, _)| *m == n) {
            out.push((n, d));
        }
    }
    out
}

/// The configured bridges.
pub fn configured() -> Vec<(u16, Option<Dialect>)> {
    conf_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_conf(&t)).unwrap_or_default()
}

/// Is port `n` configured as a bridge? Then it is not a VISA serial port (ASRL).
pub fn is_bridge(n: u16) -> bool {
    configured().iter().any(|(m, _)| *m == n)
}

/// Add port `n` as a bridge (replacing an entry for it), or with `None`, take it out. Comments and
/// other lines are kept. The line names the port as `given` if that names it (a stable
/// `/dev/serial/by-id/...` link stays one), else as the system does.
pub fn set_conf(n: u16, given: &str, entry: Option<Option<Dialect>>) -> std::io::Result<()> {
    let p = conf_path().ok_or_else(|| std::io::Error::other("no place for bridges.conf (LOCALAPPDATA or HOME is not set)"))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() {
            "# Serial ports that are Prologix or AR488 GPIB controllers (symbiotic-ctl bridge add/remove):\n\
             # <port> [prologix|ar488], the port COM<n> or /dev/tty...; nothing is sent to one not listed here.\n"
        } else {
            text
        };
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| parse_conf(l).first().is_none_or(|(m, _)| *m != n))
            .map(str::to_string)
            .collect();
        if let Some(d) = entry {
            let port = if serial::port_number(given) == Some(n) && !given.trim().contains(char::is_whitespace) {
                given.trim().to_string()
            } else {
                serial::port_name(n)
            };
            lines.push(match d {
                Some(d) => format!("{port} {}", d.name()),
                None => port,
            });
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// The configured bridges whose port is there, as adapters.
pub fn list() -> Result<Vec<Entry>, Error> {
    let ports = serial::list()?;
    Ok(configured()
        .into_iter()
        .filter(|(n, _)| ports.iter().any(|f| f.n == *n))
        .map(|(n, _)| Entry { kind: Kind::Prologix, serial: serial::port_name(n), pid: 0 })
        .collect())
}

// ---------------------------------------------------------------------------------------
// The line protocol
// ---------------------------------------------------------------------------------------

/// What the controller appends when it sees EOI (`++eot_char`). A Prologix: NUL, because a reply is nearly always
/// ASCII. An AR488: its `++eot_char` prints the value in decimal, so the marker is the text "255" -- or the byte 0xFF,
/// should a firmware send the character itself. Either can occur in a reply, so a marker is only believed when nothing
/// follows it for `NUL_QUIET`.
const EOT_PROLOGIX: (u8, &[&[u8]]) = (0, &[b"\0"]);
const EOT_AR488: (u8, &[&[u8]]) = (255, &[b"255", b"\xff"]);
const NUL_QUIET: Duration = Duration::from_millis(20);

/// `++read_tmo_ms` until a read sets its own: the controller's wait for one more character.
const READ_TMO_MS: u32 = 500;
/// The most `++read_tmo_ms` takes: Prologix's ceiling; the AR488 takes up to 32000.
const TMO_MAX_PROLOGIX: u32 = 3000;
const TMO_MAX_AR488: u32 = 32000;
/// And for probing an address, where most are empty.
const PROBE_TMO_MS: u32 = 25;
/// Added to the controller's own serial-poll timeout while waiting for its answer.
const SPOLL_MARGIN_MS: u64 = 60;
/// The wait for a line the controller answers at once (`++ver`, `++srq`).
const REPLY_WAIT: Duration = Duration::from_millis(1500);
/// One wait for more bytes inside a read.
const SLICE: Duration = Duration::from_millis(50);
/// An address that answered is trusted this long before a write polls it again (as the 82357B's).
const SEEN_FOR: Duration = Duration::from_secs(2);
/// How long one `++fndl` answers a search's probes: a search asks for 30 addresses one after another.
const LISTENERS_FOR: Duration = Duration::from_secs(1);
/// The wait for `++fndl`'s line.
const FNDL_WAIT: Duration = Duration::from_secs(3);
/// An AR488 takes at most this many data bytes in one line before it sends a piece with EOI.
const AR488_LINE: usize = 100;
/// How long an Arduino takes to come up after the port raised DTR and reset it.
const BOOT_WAIT: Duration = Duration::from_secs(5);

const ESC: u8 = 27;

/// A payload as one line: CR, LF, ESC and `+` are dropped by the controller unless ESC precedes
/// them (`+` so that data opening `++` is not taken for a command).
fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    for b in data {
        if matches!(b, 13 | 10 | 27 | 43) {
            out.push(ESC);
        }
        out.push(*b);
    }
    out
}

fn io(s: impl Into<String>) -> Error {
    Error::Io(s.into())
}

struct Pipe {
    link: Box<dyn Link>,
    dialect: Dialect,
    version: String,
    addr: Option<u8>,
    eoi: bool,
    tmo: u32,
    seen: HashMap<u8, Instant>,
    owed: Owed,
    /// An instrument whose message is still arriving: a read stopped before its end.
    talking: Option<u8>,
    /// When the controller last heard from it (or was told to `++read`): its own wait runs from there.
    talk_last: Instant,
    /// Status bytes with RQS that our own serial polls read (`take_polled`).
    polled: Vec<(u8, u8)>,
    /// The primary addresses `++fndl` found listening, and when.
    listeners: Option<(Instant, Vec<u8>)>,
    /// False once the controller said it does not know `++fndl` (an AR488 older than it).
    fndl: bool,
}

impl Pipe {
    /// Bytes to the controller, all of them within `wait`.
    fn send(&mut self, bytes: &[u8], wait: Duration) -> Result<(), Error> {
        let end = Instant::now() + wait;
        let mut sent = 0;
        while sent < bytes.len() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            sent += self.link.write(&bytes[sent..], left)?;
        }
        Ok(())
    }

    fn line(&mut self, text: &str) -> Result<(), Error> {
        let mut b = text.as_bytes().to_vec();
        b.push(b'\n');
        self.send(&b, REPLY_WAIT)
    }

    /// A command to the controller: whatever is still arriving is taken in first, or it would be
    /// read as this command's answer.
    fn cmd(&mut self, text: &str) -> Result<(), Error> {
        self.settle()?;
        self.line(text)
    }

    /// What arrives within `wait`, appended to `buf`: whether anything did.
    fn recv(&mut self, buf: &mut Vec<u8>, wait: Duration) -> Result<bool, Error> {
        let mut b = [0u8; 4096];
        let n = self.link.read(&mut b, wait.max(Duration::from_millis(1)))?;
        buf.extend_from_slice(&b[..n]);
        Ok(n > 0)
    }

    /// One line of the controller's own, trimmed.
    fn ask(&mut self, text: &str, wait: Duration) -> Result<String, Error> {
        self.cmd(text)?;
        self.answer(wait)
    }

    fn answer(&mut self, wait: Duration) -> Result<String, Error> {
        let end = Instant::now() + wait;
        let mut buf = Vec::new();
        loop {
            // Empty lines are skipped: a CR LF split across reads, or the echo of an empty line.
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                let s = String::from_utf8_lossy(&line).trim().to_string();
                if !s.is_empty() {
                    return Ok(s);
                }
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            self.recv(&mut buf, left.min(SLICE))?;
        }
    }

    /// Does `buf` end the message: the EOI marker last, and nothing after it for NUL_QUIET? The
    /// marker is taken off if so; what came after it, if anything, is added.
    fn ends(&mut self, buf: &mut Vec<u8>) -> Result<bool, Error> {
        let Some(m) = self.eot().1.iter().find(|m| buf.ends_with(m)) else { return Ok(false) };
        let mut more = Vec::new();
        self.recv(&mut more, NUL_QUIET)?;
        if more.is_empty() {
            buf.truncate(buf.len() - m.len());
            Ok(true)
        } else {
            buf.extend_from_slice(&more);
            Ok(false)
        }
    }

    fn eot(&self) -> (u8, &'static [&'static [u8]]) {
        match self.dialect {
            Dialect::Prologix => EOT_PROLOGIX,
            Dialect::Ar488 => EOT_AR488,
        }
    }

    /// The controller gives up on a read after `++read_tmo_ms` of silence.
    fn quiet(&self) -> Duration {
        Duration::from_millis(self.tmo as u64 + 100)
    }

    /// Take in the rest of the message still arriving, for its instrument's next read.
    fn settle(&mut self) -> Result<(), Error> {
        let Some(pad) = self.talking.take() else { return Ok(()) };
        let mut buf = Vec::new();
        // From when the controller last heard anything, not from now: after a read that timed out
        // its wait is mostly over, and a read's wait can be long (read_message).
        let mut last = self.talk_last;
        let ended = loop {
            if self.ends(&mut buf)? {
                break true;
            }
            if self.recv(&mut buf, SLICE)? {
                last = Instant::now();
            } else if last.elapsed() >= self.quiet() {
                break false; // the controller stopped reading; the rest is still in the instrument
            }
        };
        if !buf.is_empty() || ended {
            self.owed.put(pad, buf, ended);
        }
        Ok(())
    }

    /// Make the controller ours: find out what it is, then set it up. The order matters for a
    /// Prologix, whose settings go to EEPROM until `++savecfg 0`.
    fn bring_up(&mut self, forced: Option<Dialect>) -> Result<(), Error> {
        // An Arduino resets when the port opens; ask until it answers. An empty line first ends
        // whatever half line an earlier user left in the controller.
        let end = Instant::now() + BOOT_WAIT;
        let version = loop {
            let _ = self.line("");
            match self.ask("++ver real", Duration::from_millis(700)) {
                Ok(v) if !v.is_empty() => break v,
                _ if Instant::now() >= end => {
                    return Err(io("it does not answer ++ver: is this port a Prologix or AR488 controller?"))
                }
                _ => {}
            }
        };
        let mut version = version;
        // Verbose on (a board that does not reset kept it from before): it echoes what it is sent.
        if version.contains("++ver") {
            self.line("++verbose")?;
            std::thread::sleep(Duration::from_millis(200));
            let _ = self.recv(&mut Vec::new(), Duration::from_millis(200));
            version = self.ask("++ver real", REPLY_WAIT)?;
            // Still our own words: not a controller in verbose mode but a line that echoes -- a
            // loopback, a modem, a terminal. Nothing more is sent to it.
            if version.contains("++ver") {
                return Err(io("it echoes what it is sent (a loopback or a modem?): not a Prologix or AR488 controller"));
            }
        }
        self.dialect = forced.unwrap_or(if version.to_ascii_uppercase().contains("AR488") { Dialect::Ar488 } else { Dialect::Prologix });
        self.version = version;
        match self.dialect {
            // First, before anything that would be saved; the setting itself is not saved.
            Dialect::Prologix => self.cmd("++savecfg 0")?,
            // Never ++savecfg (it writes the EEPROM). What an earlier user may have saved: the
            // controller answering *IDN? itself, and polling on SRQ by itself.
            Dialect::Ar488 => {
                self.cmd("++idn 0")?;
                self.cmd("++srqauto 0")?;
            }
        }
        // Controller; no read-after-write (a read is the caller's own operation); nothing appended
        // to what is sent (the caller's message has its own terminator); EOI on the last byte and a
        // NUL after a read that saw it.
        for c in ["++mode 1", "++auto 0", "++eos 3", "++eoi 1", "++eot_enable 1"] {
            self.cmd(c)?;
        }
        self.cmd(&format!("++eot_char {}", self.eot().0))?;
        self.eoi = true;
        self.tmo = 0;
        self.set_tmo(READ_TMO_MS)?;
        self.addr = None;
        Ok(())
    }

    fn set_tmo(&mut self, ms: u32) -> Result<(), Error> {
        let max = match self.dialect {
            Dialect::Prologix => TMO_MAX_PROLOGIX,
            Dialect::Ar488 => TMO_MAX_AR488,
        };
        let ms = ms.clamp(1, max);
        if self.tmo != ms {
            self.cmd(&format!("++read_tmo_ms {ms}"))?;
            self.tmo = ms;
        }
        Ok(())
    }

    fn set_addr(&mut self, pad: u8) -> Result<(), Error> {
        if self.addr != Some(pad) {
            self.cmd(&format!("++addr {pad}"))?;
            self.addr = Some(pad);
        }
        Ok(())
    }

    fn set_eoi(&mut self, on: bool) -> Result<(), Error> {
        if self.eoi != on {
            self.cmd(if on { "++eoi 1" } else { "++eoi 0" })?;
            self.eoi = on;
        }
        Ok(())
    }

    /// Serial poll: Timeout when nobody answers. The controller waits `++read_tmo_ms` for it; the
    /// margin is the line's latency (an FTDI virtual port holds a short reply up to 16 ms). It is
    /// paid once per empty address when a search probes all 30, so it is kept small.
    fn spoll(&mut self, pad: u8, box_tmo: u32) -> Result<u8, Error> {
        self.set_tmo(box_tmo)?;
        let r = self.ask(&format!("++spoll {pad}"), Duration::from_millis(self.tmo as u64 + SPOLL_MARGIN_MS));
        match r {
            Ok(s) => s.parse::<u8>().map_err(|_| io(format!("++spoll {pad} answered {s:?}"))),
            Err(e) => Err(e),
        }
    }

    /// Is anything at `pad`? A serial poll that is answered; one with RQS is kept (it took the
    /// request from the instrument).
    fn present(&mut self, pad: u8) -> Result<bool, Error> {
        match self.spoll(pad, PROBE_TMO_MS) {
            Ok(stb) => {
                self.seen.insert(pad, Instant::now());
                if stb & 0x40 != 0 {
                    self.polled.retain(|(p, _)| *p != pad);
                    self.polled.push((pad, stb));
                }
                Ok(true)
            }
            Err(Error::Timeout) => {
                self.seen.remove(&pad);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Is anything at `pad`, for a search or an open. An AR488 finds every listener on the bus in
    /// one `++fndl` (addresses each to listen and looks at NDAC, as the 82357B's `listens`) in about
    /// the time one empty address costs a serial poll; a search asks 30 times in a row, so one
    /// answer serves a second. A Prologix has no such command: it is polled.
    fn listens(&mut self, pad: u8) -> Result<bool, Error> {
        if self.dialect != Dialect::Ar488 || !self.fndl {
            return self.present(pad);
        }
        let fresh = self.listeners.as_ref().filter(|(t, _)| t.elapsed() < LISTENERS_FOR).map(|(_, l)| l.contains(&pad));
        let found = match fresh {
            Some(f) => f,
            None => match self.find_listeners()? {
                Some(l) => {
                    let f = l.contains(&pad);
                    self.listeners = Some((Instant::now(), l));
                    f
                }
                None => return self.present(pad),
            },
        };
        if found {
            self.seen.insert(pad, Instant::now());
        } else {
            self.seen.remove(&pad);
        }
        Ok(found)
    }

    /// `++fndl 1-30`: a line of the primary addresses that listen, comma separated, empty when none do. `pad:sad`
    /// entries (an instrument that listens at a secondary address only) are left out: a bridge does not do secondary
    /// addresses. "Transmit failed!" (then an empty line) is the addressing itself failing: nothing on the bus takes
    /// part in the handshake. None: the controller does not know the command.
    ///
    /// AR488 firmware may leave its "a device is addressed to listen" flag set after `++fndl` (the last address it
    /// tried, 30), so the next message would be sent without addressing and go to nobody. `++unl` clears the flag, and
    /// unaddresses the last listener found; it answers nothing.
    fn find_listeners(&mut self) -> Result<Option<Vec<u8>>, Error> {
        self.cmd("++fndl 1-30")?;
        let line = self.raw_line(FNDL_WAIT)?;
        if !line.starts_with("Transmit failed") && !line.eq_ignore_ascii_case("Unrecognized command") {
            self.cmd("++unl")?;
        }
        if line.eq_ignore_ascii_case("Unrecognized command") {
            crate::server::log("AR488: no ++fndl in this firmware; serial polls find instruments instead");
            self.fndl = false;
            return Ok(None);
        }
        if line.starts_with("Transmit failed") {
            let _ = self.raw_line(Duration::from_millis(200)); // the empty line after it
            return Err(Error::NoDevice);
        }
        let mut found = Vec::new();
        for entry in line.split(',').map(str::trim).filter(|e| !e.is_empty() && !e.contains(':')) {
            found.push(entry.parse::<u8>().map_err(|_| io(format!("++fndl answered {line:?}")))?);
        }
        Ok(Some(found))
    }

    /// One line from the controller, trimmed, an empty one too (unlike `answer`).
    fn raw_line(&mut self, wait: Duration) -> Result<String, Error> {
        let end = Instant::now() + wait;
        let mut buf = Vec::new();
        loop {
            if let Some(i) = buf.iter().position(|b| *b == b'\n') {
                return Ok(String::from_utf8_lossy(&buf[..i]).trim().to_string());
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            self.recv(&mut buf, left.min(SLICE))?;
        }
    }

    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, Error> {
        // A write to an address nobody listens at is accepted and goes nowhere: check first.
        if !self.seen.get(&pad).is_some_and(|t| t.elapsed() < SEEN_FOR) && !self.present(pad)? {
            return Err(Error::NoDevice);
        }
        self.settle()?;
        // A new message: the instrument drops what it had not sent, and so is nothing owed.
        self.owed.forget(pad);
        self.set_addr(pad)?;
        let wait = crate::backend::wait_of(tmo_ms, Duration::from_millis(1));
        let pieces: Vec<&[u8]> = match self.dialect {
            Dialect::Ar488 => data.chunks(AR488_LINE).collect(),
            Dialect::Prologix => vec![data],
        };
        let last = pieces.len().saturating_sub(1);
        for (i, piece) in pieces.into_iter().enumerate() {
            self.set_eoi(eoi && i == last)?;
            let mut line = escape(piece);
            line.push(b'\n');
            self.send(&line, wait)?;
        }
        Ok(data.len())
    }

    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let deadline = Instant::now() + crate::backend::wait_of(tmo_ms, Duration::from_millis(1));
        let (mut out, ended) = self.owed.take(pad);
        if let Some(end) = self.owed.cut(pad, &mut out, count, term, ended) {
            return Ok(ReadResult { data: out, end });
        }
        if ended {
            return Ok(ReadResult { data: out, end: ReadEnd::End });
        }
        if self.talking != Some(pad) {
            self.settle()?;
            // What settle took in may be this instrument's own: no, it was another's (talking is one instrument at a
            // time), so this one is owed nothing more than `out` already has. The controller waits as long as the
            // caller does, so the instrument is addressed to talk once per read, as a GPIB adapter does it. Each
            // `++read` addresses it again, and an instrument addressed with nothing to say may log an error (-420).
            self.set_tmo(tmo_ms.max(1))?;
            self.set_addr(pad)?;
            self.line("++read eoi")?;
            self.talking = Some(pad);
            self.talk_last = Instant::now();
        }
        let mut last = self.talk_last;
        loop {
            if self.ends(&mut out)? {
                self.talking = None;
                self.seen.insert(pad, Instant::now());
                let end = self.owed.cut(pad, &mut out, count, term, true).unwrap_or(ReadEnd::End);
                return Ok(ReadResult { data: out, end });
            }
            // Stopped before the end: the rest keeps coming, and is this instrument's.
            if let Some(end) = self.owed.cut(pad, &mut out, count, term, false) {
                self.seen.insert(pad, Instant::now());
                return Ok(ReadResult { data: out, end });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            if self.recv(&mut out, left.min(SLICE))? {
                last = Instant::now();
                self.talk_last = last;
            } else if last.elapsed() >= self.quiet() {
                // The controller has stopped waiting and the caller has not: a wait longer than the
                // controller takes (Prologix: 3 s). An instrument part-way through its answer is
                // still addressed to talk, so this continues it.
                self.line("++read eoi")?;
                last = Instant::now();
                self.talk_last = last;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------

pub struct PrologixAdapter {
    pipe: Mutex<Pipe>,
}

pub struct PrologixBus<'a> {
    pipe: MutexGuard<'a, Pipe>,
}

impl PrologixAdapter {
    /// Open the bridge on `serial` (`COM<n>`, `/dev/ttyACM0`), which must be a configured one.
    pub fn open(serial: &str) -> Result<PrologixAdapter, Error> {
        let n = serial::port_number(serial).ok_or(Error::NoBoard)?;
        let forced = configured().into_iter().find(|(m, _)| *m == n).ok_or(Error::NoBoard)?.1;
        let link = serial::open_link(n)?;
        let a = PrologixAdapter::start(link, forced)?;
        let p = a.pipe.lock().unwrap();
        crate::server::log(&format!("{serial}: {} ({})", p.version, p.dialect.name()));
        drop(p);
        Ok(a)
    }

    fn start(mut link: Box<dyn Link>, forced: Option<Dialect>) -> Result<PrologixAdapter, Error> {
        // AR488's own rate; a Prologix's USB port ignores it.
        link.apply(&serial::Config { baud: 115200, ..Default::default() })?;
        let mut pipe = Pipe {
            link,
            dialect: Dialect::Prologix,
            version: String::new(),
            addr: None,
            eoi: true,
            tmo: 0,
            seen: HashMap::new(),
            owed: Owed::default(),
            talking: None,
            talk_last: Instant::now(),
            polled: Vec::new(),
            listeners: None,
            fndl: true,
        };
        pipe.bring_up(forced)?;
        Ok(PrologixAdapter { pipe: Mutex::new(pipe) })
    }
}

impl Backend for PrologixAdapter {
    fn lock(&self) -> Box<dyn BusOps + '_> {
        Box::new(PrologixBus { pipe: self.pipe.lock().unwrap_or_else(|e| e.into_inner()) })
    }
    fn try_lock(&self) -> Option<Box<dyn BusOps + '_>> {
        Some(Box::new(PrologixBus { pipe: self.pipe.try_lock().ok()? }))
    }
}

fn unsupported<T>() -> Result<T, Error> {
    Err(Error::Status(ST_UNSUPPORTED))
}

impl BusOps for PrologixBus<'_> {
    fn probe(&mut self, pad: u8) -> Result<bool, Error> {
        if pad == 0 || pad == RAW {
            return Ok(false); // the controller's own address
        }
        self.pipe.listens(pad)
    }

    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        if pad == RAW {
            return Err((0, Error::Status(ST_UNSUPPORTED)));
        }
        self.pipe.write(pad, data, eoi, tmo_ms).map_err(|e| (0, e))
    }

    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        if pad == RAW {
            return unsupported();
        }
        self.pipe.read_message(pad, count, term, tmo_ms)
    }

    fn clear(&mut self, pad: u8) -> Result<(), Error> {
        let p = &mut self.pipe;
        p.settle()?;
        p.set_addr(pad)?;
        p.cmd("++clr")?;
        p.owed.forget(pad);
        Ok(())
    }

    fn trigger(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.cmd(&format!("++trg {pad}"))
    }

    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error> {
        self.pipe.spoll(pad, tmo_ms)
    }

    fn local(&mut self, pad: u8) -> Result<(), Error> {
        self.pipe.set_addr(pad)?;
        self.pipe.cmd("++loc")
    }

    fn llo(&mut self) -> Result<(), Error> {
        // VISA's LLO is the universal message. AR488 sends it with `all`; a Prologix's `++llo` is
        // documented as disabling the addressed instrument's front panel.
        match self.pipe.dialect {
            Dialect::Ar488 => self.pipe.cmd("++llo all"),
            Dialect::Prologix => self.pipe.cmd("++llo"),
        }
    }

    fn ren(&mut self, enable: bool) -> Result<(), Error> {
        match (self.pipe.dialect, enable) {
            (Dialect::Ar488, _) => self.pipe.cmd(if enable { "++ren 1" } else { "++ren 0" }),
            // A Prologix holds REN asserted as controller and has no command to release it.
            (Dialect::Prologix, true) => Ok(()),
            (Dialect::Prologix, false) => unsupported(),
        }
    }

    fn ifc(&mut self) -> Result<(), Error> {
        self.pipe.cmd("++ifc")
    }

    fn srq_asserted(&mut self) -> Result<bool, Error> {
        Ok(self.pipe.ask("++srq", REPLY_WAIT)? == "1")
    }

    /// `++srq` is SRQ's level on both; an AR488's `++ren` without an argument says REN's. Nothing
    /// tells the rest.
    fn lines_known(&mut self) -> Result<(u16, u16), Error> {
        use crate::proto::{BUS_REN, BUS_SRQ};
        let mut lines = if self.srq_asserted()? { BUS_SRQ } else { 0 };
        let mut known = BUS_SRQ;
        if self.pipe.dialect == Dialect::Ar488 {
            if self.pipe.ask("++ren", REPLY_WAIT)? == "1" {
                lines |= BUS_REN;
            }
            known |= BUS_REN;
        }
        Ok((lines, known))
    }

    fn take_polled(&mut self) -> Vec<(u8, u8)> {
        std::mem::take(&mut self.pipe.polled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// One instrument behind the simulated controller: it answers `*IDN?`, `LONG?` (1000 bytes),
    /// `BIN?` (binary with NULs in it) and `CSV?`, and records every message it was sent.
    #[derive(Default)]
    struct Inst {
        got: Vec<Vec<u8>>,
        partial: Vec<u8>,
        output: Vec<u8>,
        stb: u8,
        triggered: u32,
    }

    #[derive(Default)]
    struct Box488 {
        dialect: Option<Dialect>,
        /// What `++ver` says (an AR488 can be made to say anything).
        verstr: Option<String>,
        verbose: bool,
        autosave: bool,
        eeprom_writes: u32,
        commands: Vec<String>,
        addr: u8,
        eoi: bool,
        eos: u8,
        eot: bool,
        eot_char: u8,
        inst: HashMap<u8, Inst>,
        line: Vec<u8>,
        esc: bool,
        /// The line's first two characters came escaped: data, not a command.
        plus_escaped: bool,
        to_host: Vec<u8>,
        /// Hand the host at most this many bytes per read, to split replies across reads.
        dribble: usize,
        ren: Option<bool>,
        srq: bool,
        /// An AR488 that sends the `++eot_char` character itself, as the Prologix manual has it; others print its value
        /// in decimal.
        eot_as_byte: bool,
        /// An AR488 from before `++fndl`.
        no_fndl: bool,
        /// Addresses that listen only at a secondary address, as `++fndl` reports them.
        sad_only: Vec<(u8, u8)>,
        /// AR488 after `++fndl`: it believes address 30 is addressed to listen, so the next message goes there, until
        /// `++unl`.
        stale_listener: bool,
    }

    const LONG: usize = 1000;

    fn bin() -> Vec<u8> {
        (0..300u32).map(|i| if i % 64 == 63 { 0 } else { (i % 250) as u8 + 1 }).collect()
    }

    impl Box488 {
        fn new(d: Dialect) -> Box488 {
            let mut b = Box488 { dialect: Some(d), autosave: d == Dialect::Prologix, eoi: true, dribble: 4096, ..Default::default() };
            for pad in [5, 7] {
                b.inst.insert(pad, Inst::default());
            }
            b
        }

        fn ar488(&self) -> bool {
            self.dialect == Some(Dialect::Ar488)
        }

        fn config_changed(&mut self) {
            if !self.ar488() && self.autosave {
                self.eeprom_writes += 1;
            }
        }

        fn say(&mut self, s: &str) {
            self.to_host.extend_from_slice(s.as_bytes());
            self.to_host.extend_from_slice(b"\r\n");
        }

        fn deliver(&mut self, data: &[u8]) {
            // An AR488 sends a line longer than its buffer in pieces, each ended as a whole line is.
            let pieces: Vec<Vec<u8>> =
                if self.ar488() { data.chunks(128).map(|c| c.to_vec()).collect() } else { vec![data.to_vec()] };
            let (addr, eoi) = (if std::mem::take(&mut self.stale_listener) { 30 } else { self.addr }, self.eoi);
            let Some(i) = self.inst.get_mut(&addr) else { return };
            for p in pieces {
                i.partial.extend_from_slice(&p);
                if eoi {
                    let m = std::mem::take(&mut i.partial);
                    // A new message: unread output is dropped (IEEE 488.2).
                    i.output = match m.as_slice() {
                        b"*IDN?\n" => format!("SIM,{addr}\n").into_bytes(),
                        b"LONG?\n" => (0..LONG).map(|k| b'a' + (k % 26) as u8).collect(),
                        b"BIN?\n" => bin(),
                        b"CSV?\n" => b"1,2,3\n".to_vec(),
                        // What an AR488's EOI marker looks like, inside a reply.
                        b"T255?\n" => b"a,255,b,255\n".to_vec(),
                        b"FF?\n" => vec![0xFF, 1, 0xFF, 2, 0xFF],
                        _ => Vec::new(),
                    };
                    i.got.push(m);
                }
            }
        }

        fn command(&mut self, c: &str) {
            self.commands.push(c.to_string());
            let mut w = c[2..].split_whitespace();
            let name = w.next().unwrap_or("");
            let arg = w.next();
            match (name, arg) {
                ("ver", Some("real")) if self.ar488() => self.say("AR488 GPIB controller, ver. 0.53.46, 22/05/2026"),
                ("ver", _) => {
                    let v = self.verstr.clone().unwrap_or_else(|| {
                        if self.ar488() { "AR488 GPIB controller, ver. 0.53.46, 22/05/2026".into() } else { "Prologix GPIB-USB Controller version 6.107".into() }
                    });
                    self.say(&v)
                }
                ("savecfg", a) if self.ar488() => {
                    let _ = a;
                    self.eeprom_writes += 1; // any argument: saved now
                }
                ("savecfg", Some(v)) => self.autosave = v == "1",
                ("savecfg", None) => {
                    let s = if self.autosave { "1" } else { "0" };
                    self.say(s)
                }
                ("verbose", None) if self.ar488() => {
                    self.verbose = !self.verbose;
                    let s = format!("Verbose: {}", if self.verbose { "ON" } else { "OFF" });
                    self.say(&s)
                }
                ("mode" | "auto" | "eos" | "eoi" | "eot_enable" | "eot_char" | "read_tmo_ms" | "addr", Some(v)) => {
                    let n: u32 = v.parse().unwrap();
                    match name {
                        "eos" => self.eos = n as u8,
                        "eoi" => self.eoi = n == 1,
                        "eot_enable" => self.eot = n == 1,
                        "eot_char" => self.eot_char = n as u8,
                        "addr" => self.addr = n as u8,
                        _ => {}
                    }
                    self.config_changed();
                }
                ("idn" | "srqauto", Some(_)) if self.ar488() => {}
                ("read", Some("eoi")) => {
                    let addr = self.addr;
                    if let Some(i) = self.inst.get_mut(&addr) {
                        let out = std::mem::take(&mut i.output);
                        if !out.is_empty() {
                            self.to_host.extend_from_slice(&out);
                            if self.eot {
                                if self.ar488() && !self.eot_as_byte {
                                    let text = self.eot_char.to_string();
                                    self.to_host.extend_from_slice(text.as_bytes());
                                } else {
                                    self.to_host.push(self.eot_char);
                                }
                            }
                        }
                    }
                }
                ("spoll", Some(p)) => {
                    let p: u8 = p.parse().unwrap();
                    if let Some(i) = self.inst.get_mut(&p) {
                        let s = i.stb;
                        i.stb &= !0x40;
                        self.srq = false;
                        self.say(&s.to_string());
                    } // nobody there: nothing, after the timeout
                }
                ("fndl", Some("1-30")) if self.ar488() && !self.no_fndl => {
                    if self.inst.is_empty() && self.sad_only.is_empty() {
                        self.say("Transmit failed!");
                        self.say("");
                    } else {
                        let mut pads: Vec<u8> = self.inst.keys().copied().collect();
                        pads.sort();
                        let mut e: Vec<String> = pads.iter().map(|p| p.to_string()).collect();
                        e.extend(self.sad_only.iter().map(|(p, s)| format!("{p}:{}", 0x60 + s)));
                        let l = e.join(",");
                        self.say(&l);
                        self.stale_listener = true;
                    }
                }
                ("unl", None) if self.ar488() => self.stale_listener = false,
                ("srq", None) => {
                    let s = if self.srq { "1" } else { "0" };
                    self.say(s)
                }
                ("clr", None) => {
                    let a = self.addr;
                    if let Some(i) = self.inst.get_mut(&a) {
                        i.output.clear();
                    }
                }
                ("trg", Some(p)) => {
                    if let Some(i) = self.inst.get_mut(&p.parse::<u8>().unwrap()) {
                        i.triggered += 1;
                    }
                }
                ("ren", Some(v)) if self.ar488() => self.ren = Some(v == "1"),
                ("ren", None) if self.ar488() => {
                    let s = if self.ren != Some(false) { "1" } else { "0" }; // asserted as controller
                    self.say(s)
                }
                ("loc" | "ifc", None) | ("llo", _) => {}
                _ => self.say("Unrecognized command"),
            }
        }

        fn take(&mut self, b: u8) {
            if self.verbose && b != b'\n' {
                self.to_host.push(b);
            }
            if self.esc {
                if self.line.len() < 2 {
                    self.plus_escaped = true;
                }
                self.line.push(b);
                self.esc = false;
                return;
            }
            match b {
                ESC => self.esc = true,
                b'\r' | b'\n' => {
                    let l = std::mem::take(&mut self.line);
                    let escaped = std::mem::take(&mut self.plus_escaped);
                    if l.is_empty() {
                        return;
                    }
                    if l.starts_with(b"++") && !escaped {
                        self.command(&String::from_utf8_lossy(&l));
                    } else {
                        self.deliver(&l);
                    }
                }
                _ => self.line.push(b),
            }
        }
    }

    struct Sim(Arc<Mutex<Box488>>);

    impl Link for Sim {
        fn apply(&mut self, _c: &serial::Config) -> Result<(), Error> {
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error> {
            let mut b = self.0.lock().unwrap();
            if b.to_host.is_empty() {
                drop(b);
                std::thread::sleep(wait.min(Duration::from_millis(2)));
                return Ok(0);
            }
            let n = b.to_host.len().min(buf.len()).min(b.dribble);
            buf[..n].copy_from_slice(&b.to_host[..n]);
            b.to_host.drain(..n);
            Ok(n)
        }
        fn write(&mut self, data: &[u8], _wait: Duration) -> Result<usize, Error> {
            let mut b = self.0.lock().unwrap();
            for x in data {
                b.take(*x);
            }
            Ok(data.len())
        }
        fn drain(&mut self, _wait: Duration) -> Result<(), Error> {
            Ok(())
        }
        fn purge(&mut self, _i: bool, _o: bool) -> Result<(), Error> {
            Ok(())
        }
        fn set_break(&mut self, _on: bool) -> Result<(), Error> {
            Ok(())
        }
        fn avail(&mut self) -> Result<u32, Error> {
            Ok(0)
        }
        fn lines(&mut self) -> Result<[bool; 4], Error> {
            Ok([false; 4])
        }
    }

    fn up(b: Box488, forced: Option<Dialect>) -> (PrologixAdapter, Arc<Mutex<Box488>>) {
        let s = Arc::new(Mutex::new(b));
        let a = PrologixAdapter::start(Box::new(Sim(s.clone())), forced).unwrap();
        (a, s)
    }

    fn query(a: &PrologixAdapter, pad: u8, q: &[u8], count: usize, term: Option<u8>) -> ReadResult {
        let mut bus = a.lock();
        bus.write(pad, q, true, 1000).unwrap();
        bus.read_message(pad, count, term, 1000).unwrap()
    }

    #[test]
    fn a_prologix_turns_saving_off_first_and_writes_no_eeprom() {
        let (a, s) = up(Box488::new(Dialect::Prologix), None);
        assert_eq!(a.pipe.lock().unwrap().dialect, Dialect::Prologix);
        let b = s.lock().unwrap();
        let first_setting = b.commands.iter().position(|c| c.starts_with("++mode")).unwrap();
        let savecfg = b.commands.iter().position(|c| c == "++savecfg 0").unwrap();
        assert!(savecfg < first_setting, "savecfg 0 before any setting: {:?}", b.commands);
        assert_eq!(b.eeprom_writes, 0);
    }

    #[test]
    fn an_ar488_is_never_sent_savecfg_even_when_it_says_it_is_a_prologix() {
        let mut b = Box488::new(Dialect::Ar488);
        b.verstr = Some("Prologix GPIB-USB Controller version 6.107".into());
        let (a, s) = up(b, None);
        assert_eq!(a.pipe.lock().unwrap().dialect, Dialect::Ar488, "++ver real sees through it");
        let b = s.lock().unwrap();
        assert!(!b.commands.iter().any(|c| c.starts_with("++savecfg")), "{:?}", b.commands);
        assert_eq!(b.eeprom_writes, 0);
        assert!(b.commands.contains(&"++idn 0".to_string()) && b.commands.contains(&"++srqauto 0".to_string()));
    }

    #[test]
    fn an_ar488_left_in_verbose_mode_is_put_right() {
        let mut b = Box488::new(Dialect::Ar488);
        b.verbose = true;
        let (a, s) = up(b, None);
        assert!(!s.lock().unwrap().verbose);
        assert_eq!(query(&a, 5, b"*IDN?\n", 100, None).data, b"SIM,5\n");
    }

    #[test]
    fn a_query_reads_to_eoi() {
        for d in [Dialect::Prologix, Dialect::Ar488] {
            let (a, _) = up(Box488::new(d), None);
            let r = query(&a, 5, b"*IDN?\n", 100, None);
            assert_eq!((r.data.as_slice(), r.end), (&b"SIM,5\n"[..], ReadEnd::End), "{d:?}");
        }
    }

    #[test]
    fn a_read_that_times_out_addresses_the_instrument_once() {
        // Each ++read addresses the instrument to talk, and one with nothing to say logs -420 each
        // time: the controller is made to wait as long as the caller, not ++read again and again.
        for d in [Dialect::Prologix, Dialect::Ar488] {
            let (a, s) = up(Box488::new(d), None);
            let before = s.lock().unwrap().commands.len();
            let r = a.lock().read_message(5, 100, None, 1500).unwrap();
            assert_eq!(r.end, ReadEnd::Timeout, "{d:?}");
            let b = s.lock().unwrap();
            let sent = &b.commands[before..];
            assert_eq!(sent.iter().filter(|c| c.starts_with("++read ")).count(), 1, "{d:?}: {sent:?}");
            assert!(sent.contains(&"++read_tmo_ms 1500".to_string()), "{d:?}: {sent:?}");
        }
    }

    #[test]
    fn an_ar488s_eoi_marker_is_its_value_in_decimal_or_the_byte() {
        // AR488 firmware prints ++eot_char's value as a number; a firmware that sends the character itself sends 0xFF.
        // Either way the reply comes back whole, even when it holds the marker's own bytes and the marker arrives a
        // byte at a time.
        for as_byte in [false, true] {
            let mut b = Box488::new(Dialect::Ar488);
            b.eot_as_byte = as_byte;
            b.dribble = 1;
            let (a, s) = up(b, None);
            assert!(s.lock().unwrap().commands.contains(&"++eot_char 255".to_string()));
            for (q, want) in [(&b"*IDN?\n"[..], &b"SIM,5\n"[..]), (b"T255?\n", b"a,255,b,255\n"), (b"FF?\n", &[0xFF, 1, 0xFF, 2, 0xFF])] {
                let r = query(&a, 5, q, 100, None);
                assert_eq!((r.data.as_slice(), r.end), (want, ReadEnd::End), "as_byte {as_byte}, {q:?}");
            }
        }
        // A Prologix keeps its NUL.
        let (_, s) = up(Box488::new(Dialect::Prologix), None);
        assert!(s.lock().unwrap().commands.contains(&"++eot_char 0".to_string()));
    }

    #[test]
    fn the_lines_a_bridge_can_tell_are_srq_and_on_an_ar488_ren() {
        use crate::proto::{BUS_REN, BUS_SRQ};
        let (a, s) = up(Box488::new(Dialect::Ar488), None);
        assert_eq!(a.lock().lines_known().unwrap(), (BUS_REN, BUS_SRQ | BUS_REN));
        s.lock().unwrap().srq = true;
        a.lock().ren(false).unwrap();
        assert_eq!(a.lock().lines_known().unwrap(), (BUS_SRQ, BUS_SRQ | BUS_REN));
        let (a, _) = up(Box488::new(Dialect::Prologix), None);
        assert_eq!(a.lock().lines_known().unwrap(), (0, BUS_SRQ));
    }

    #[test]
    fn what_a_read_stops_short_of_is_the_instruments_next_read() {
        let mut b = Box488::new(Dialect::Ar488);
        b.dribble = 3;
        let (a, s) = up(b, None);
        let r = query(&a, 5, b"CSV?\n", 100, Some(b','));
        assert_eq!((r.data.as_slice(), r.end), (&b"1,"[..], ReadEnd::TermChar));
        // Another instrument in between: the rest of 5's answer is taken in, not read as 7's.
        assert_eq!(query(&a, 7, b"*IDN?\n", 100, None).data, b"SIM,7\n");
        let mut bus = a.lock();
        let r = bus.read_message(5, 100, Some(b','), 1000).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"2,"[..], ReadEnd::TermChar));
        let r = bus.read_message(5, 100, Some(b','), 1000).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"3\n"[..], ReadEnd::End));
        drop(bus);
        assert_eq!(s.lock().unwrap().inst[&5].got, vec![b"CSV?\n".to_vec()], "one question, answered in pieces");
    }

    #[test]
    fn a_long_answer_read_in_counts_comes_back_whole() {
        let mut b = Box488::new(Dialect::Prologix);
        b.dribble = 64;
        let (a, _) = up(b, None);
        let mut bus = a.lock();
        bus.write(5, b"LONG?\n", true, 1000).unwrap();
        let mut all = Vec::new();
        loop {
            let r = bus.read_message(5, 128, None, 1000).unwrap();
            all.extend_from_slice(&r.data);
            match r.end {
                ReadEnd::MaxCount => continue,
                ReadEnd::End => break,
                e => panic!("{e:?}"),
            }
        }
        let want: Vec<u8> = (0..LONG).map(|k| b'a' + (k % 26) as u8).collect();
        assert_eq!(all, want);
    }

    #[test]
    fn a_nul_in_binary_is_not_taken_for_the_end() {
        let mut b = Box488::new(Dialect::Ar488);
        b.dribble = 64; // every 64th byte is a NUL and lands at the end of a piece
        let (a, _) = up(b, None);
        let r = query(&a, 5, b"BIN?\n", 1000, None);
        assert_eq!((r.data, r.end), (bin(), ReadEnd::End));
    }

    #[test]
    fn a_long_write_to_an_ar488_is_one_message() {
        let (a, s) = up(Box488::new(Dialect::Ar488), None);
        let mut msg: Vec<u8> = b"DATA ".to_vec();
        msg.extend((0..400).map(|i| b"0123456789,"[i % 11]));
        msg.push(b'\n');
        a.lock().write(5, &msg, true, 1000).unwrap();
        assert_eq!(s.lock().unwrap().inst[&5].got, vec![msg], "one message, not one per 128 bytes");
    }

    #[test]
    fn data_that_looks_like_commands_or_line_ends_arrives_intact() {
        for d in [Dialect::Prologix, Dialect::Ar488] {
            let (a, s) = up(Box488::new(d), None);
            let msg = b"++ver\r\n\x1b+x\x1b".to_vec();
            a.lock().write(5, &msg, true, 1000).unwrap();
            let b = s.lock().unwrap();
            assert_eq!(b.inst[&5].got, vec![msg], "{d:?}");
            assert_eq!(b.commands.iter().filter(|c| c.starts_with("++ver")).count(), 1, "only bring_up's");
        }
    }

    #[test]
    fn nobody_there_is_no_device() {
        let (a, _) = up(Box488::new(Dialect::Prologix), None);
        let mut bus = a.lock();
        assert!(!bus.probe(9).unwrap());
        assert!(bus.probe(5).unwrap());
        assert!(matches!(bus.write(9, b"*RST\n", true, 1000), Err((0, Error::NoDevice))));
    }

    fn probe_all(a: &PrologixAdapter) -> Vec<u8> {
        let mut bus = a.lock();
        (1..=30).filter(|p| bus.probe(*p).unwrap()).collect()
    }

    fn sent(s: &Arc<Mutex<Box488>>, prefix: &str) -> usize {
        s.lock().unwrap().commands.iter().filter(|c| c.starts_with(prefix)).count()
    }

    #[test]
    fn an_ar488_search_asks_for_the_listeners_once() {
        let mut b = Box488::new(Dialect::Ar488);
        b.inst.insert(12, Inst::default());
        b.sad_only.push((20, 3)); // "20:99": not a primary address that listens
        let (a, s) = up(b, None);
        assert_eq!(probe_all(&a), vec![5, 7, 12]);
        assert_eq!(sent(&s, "++fndl"), 1);
        assert_eq!(sent(&s, "++spoll"), 0);
        // Found by ++fndl counts as seen: the write that follows does not poll, and it arrives.
        assert_eq!(query(&a, 12, b"*IDN?\n", 100, None).data, b"SIM,12\n");
        assert_eq!(sent(&s, "++spoll"), 0);
    }

    #[test]
    fn a_stale_list_of_listeners_is_asked_for_again() {
        let (a, s) = up(Box488::new(Dialect::Ar488), None);
        assert!(!a.lock().probe(9).unwrap());
        s.lock().unwrap().inst.insert(9, Inst::default());
        assert!(!a.lock().probe(9).unwrap(), "still the answer of a moment ago");
        a.pipe.lock().unwrap().listeners.as_mut().unwrap().0 -= LISTENERS_FOR;
        assert!(a.lock().probe(9).unwrap());
        assert_eq!(sent(&s, "++fndl"), 2);
    }

    #[test]
    fn an_ar488_without_fndl_is_polled() {
        let mut b = Box488::new(Dialect::Ar488);
        b.no_fndl = true;
        let (a, s) = up(b, None);
        assert_eq!(probe_all(&a), vec![5, 7]);
        assert_eq!(probe_all(&a), vec![5, 7]);
        assert_eq!(sent(&s, "++fndl"), 1, "asked once, then never again");
    }

    #[test]
    fn a_prologix_is_polled() {
        let (a, s) = up(Box488::new(Dialect::Prologix), None);
        assert_eq!(probe_all(&a), vec![5, 7]);
        assert_eq!(sent(&s, "++fndl"), 0);
        assert_eq!(sent(&s, "++spoll"), 30);
    }

    #[test]
    fn an_empty_bus_is_no_device_and_the_bridge_goes_on_answering() {
        let mut b = Box488::new(Dialect::Ar488);
        b.inst.clear();
        let (a, s) = up(b, None);
        assert!(matches!(a.lock().probe(5), Err(Error::NoDevice)));
        assert!(s.lock().unwrap().to_host.is_empty(), "the empty line after it was taken in");
        s.lock().unwrap().srq = true;
        assert!(a.lock().srq_asserted().unwrap());
    }

    #[test]
    fn a_request_for_service_that_the_address_check_took_is_handed_over() {
        let (a, s) = up(Box488::new(Dialect::Ar488), None);
        s.lock().unwrap().inst.get_mut(&5).unwrap().stb = 0x50;
        let mut bus = a.lock();
        bus.write(5, b"*CLS\n", true, 1000).unwrap(); // polls first: never seen
        assert_eq!(bus.take_polled(), vec![(5, 0x50)]);
        assert!(bus.take_polled().is_empty());
    }

    #[test]
    fn ren_and_the_rest() {
        let (a, s) = up(Box488::new(Dialect::Ar488), None);
        a.lock().ren(false).unwrap();
        assert_eq!(s.lock().unwrap().ren, Some(false));
        a.lock().trigger(7).unwrap();
        assert_eq!(s.lock().unwrap().inst[&7].triggered, 1);
        let (p, _) = up(Box488::new(Dialect::Prologix), None);
        assert!(matches!(p.lock().ren(false), Err(Error::Status(ST_UNSUPPORTED))));
        assert!(p.lock().ren(true).is_ok());
        // Nothing unrecognized was sent to either.
        assert!(!String::from_utf8_lossy(&s.lock().unwrap().to_host).contains("Unrecognized"));
    }

    #[test]
    fn a_clear_drops_what_was_owed() {
        let (a, _) = up(Box488::new(Dialect::Prologix), None);
        let r = query(&a, 5, b"CSV?\n", 100, Some(b','));
        assert_eq!(r.data, b"1,");
        let mut bus = a.lock();
        bus.clear(5).unwrap();
        let r = bus.read_message(5, 100, None, 700).unwrap();
        assert_eq!(r.end, ReadEnd::Timeout, "nothing left after a clear: {:?}", r.data);
    }

    /// A port whose TX is wired to its RX: everything comes back.
    struct Loopback(Vec<u8>);

    impl Link for Loopback {
        fn apply(&mut self, _c: &serial::Config) -> Result<(), Error> {
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<usize, Error> {
            if self.0.is_empty() {
                std::thread::sleep(wait.min(Duration::from_millis(2)));
                return Ok(0);
            }
            let n = self.0.len().min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
        fn write(&mut self, data: &[u8], _wait: Duration) -> Result<usize, Error> {
            self.0.extend_from_slice(data);
            Ok(data.len())
        }
        fn drain(&mut self, _wait: Duration) -> Result<(), Error> {
            Ok(())
        }
        fn purge(&mut self, _i: bool, _o: bool) -> Result<(), Error> {
            Ok(())
        }
        fn set_break(&mut self, _on: bool) -> Result<(), Error> {
            Ok(())
        }
        fn avail(&mut self) -> Result<u32, Error> {
            Ok(0)
        }
        fn lines(&mut self) -> Result<[bool; 4], Error> {
            Ok([false; 4])
        }
    }

    #[test]
    fn a_port_that_echoes_is_not_taken_for_a_controller() {
        match PrologixAdapter::start(Box::new(Loopback(Vec::new())), None) {
            Err(Error::Io(m)) => assert!(m.contains("echoes"), "{m}"),
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("a loopback was taken for a controller"),
        }
    }

    #[test]
    fn the_configuration_file() {
        let t = "# c\nCOM5\ncom7 ar488\nCOM9 prologix\nCOM3 banana\nnonsense\nCOM5 ar488\n";
        assert_eq!(parse_conf(t), vec![(5, None), (7, Some(Dialect::Ar488)), (9, Some(Dialect::Prologix))]);
    }
}
