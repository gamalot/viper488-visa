//! What the daemon needs from an adapter, whatever it is: the operations of one VISA call, on
//! a bus held for the duration. Viper488 WinUSB adapters (adapter.rs), the NI GPIB-USB-HS (ni.rs),
//! the Keysight 82357B (ks.rs), the CONTEC GP-IB(USB)FL (contec.rs) and a Prologix or AR488 on a
//! serial port (prologix.rs) each implement it; the
//! daemon, the wire and the DLL do not know which is which.

use crate::adapter::{Error, ReadResult};
use crate::proto::ST_UNSUPPORTED;
use std::sync::Arc;
use std::time::Duration;

/// VISA's VI_TMO_INFINITE as a request carries it (timeouts are u32 milliseconds, as in VISA).
pub const TMO_INFINITE: u32 = u32::MAX;

/// Whether a read whose adapter-side wait ran out should ask again, keeping what came: adapters cap their own wait
/// (65.5 s for the Viper488 firmware and the CONTEC, 1000 s for the NI), the caller's timeout need not be capped.
pub fn read_on(deadline: std::time::Instant) -> bool {
    deadline.saturating_duration_since(std::time::Instant::now()) >= Duration::from_secs(1)
}

/// A timeout for a protocol with u16 milliseconds: TMO_INFINITE becomes u16::MAX, and a finite timeout is capped at 65
/// 534 ms so it never becomes infinite.
pub fn u16_ms(tmo_ms: u32) -> u16 {
    if tmo_ms == TMO_INFINITE { u16::MAX } else { tmo_ms.min(u16::MAX as u32 - 1) as u16 }
}

/// What a protocol of u16 milliseconds means as a request's timeout: the inverse of `u16_ms`.
pub fn from_u16_ms(ms: u16) -> u32 {
    if ms == u16::MAX { TMO_INFINITE } else { ms as u32 }
}

/// How long a request's timeout lets a backend that keeps its own deadline wait: TMO_INFINITE is a
/// year, which is forever to one request and still a deadline an `Instant` can hold; 0 ("no
/// preference") is `default`.
pub fn wait_of(tmo_ms: u32, default: Duration) -> Duration {
    match tmo_ms {
        TMO_INFINITE => Duration::from_secs(365 * 24 * 3600),
        0 => default,
        ms => Duration::from_millis(ms as u64),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Viper = 0,
    Ni = 1,
    Keysight = 2,
    Contec = 3,
    /// A Prologix or AR488 on a configured serial port (prologix.rs).
    Prologix = 4,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Viper => "viper",
            Kind::Ni => "ni",
            Kind::Keysight => "keysight",
            Kind::Contec => "contec",
            Kind::Prologix => "prologix",
        }
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        [Kind::Viper, Kind::Ni, Kind::Keysight, Kind::Contec, Kind::Prologix].into_iter().find(|k| k.name() == s)
    }
}

/// An adapter present on the machine. Its board number is its position in `list()`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: Kind,
    pub serial: String,
    pub pid: u16,
}

impl Entry {
    /// The key the daemon caches an open adapter under.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.name(), self.serial)
    }
}

/// Every adapter: Viper488 first, then NI, Keysight and CONTEC, each kind by serial, then the configured Prologix/AR488
/// bridges by COM number, so the board numbering does not depend on plug order.
pub fn list() -> Result<Vec<Entry>, Error> {
    let mut v: Vec<Entry> = crate::adapter::list()?
        .into_iter()
        .map(|d| Entry { kind: Kind::Viper, serial: d.serial, pid: d.pid })
        .collect();
    v.extend(crate::ni::list()?);
    v.extend(crate::ks::list()?);
    v.extend(crate::contec::list()?);
    v.extend(crate::prologix::list()?);
    // A bridge's "serial" is its port, COM<n>: by number, so COM10 comes after COM9.
    let order = |e: &Entry| (e.kind, crate::serial::port_number(&e.serial).unwrap_or(0), e.serial.clone());
    v.sort_by_key(order);
    Ok(v)
}

/// Open the adapter `e` names, and say which one was opened: `e` itself, except a blank 82357B, which
/// is woken first and is then the running one with the serial it came back with.
pub fn open(e: &Entry) -> Result<(Arc<dyn Backend>, Entry), Error> {
    let mut opened = e.clone();
    let a: Arc<dyn Backend> = match e.kind {
        Kind::Viper => Arc::new(crate::adapter::Adapter::open(&e.serial)?),
        Kind::Ni => Arc::new(crate::ni::NiAdapter::open(&e.serial)?),
        Kind::Contec => Arc::new(crate::contec::ContecAdapter::open(&e.serial)?),
        Kind::Prologix => Arc::new(crate::prologix::PrologixAdapter::open(&e.serial)?),
        Kind::Keysight => {
            // A blank 82357B is woken first (two uploads, two enumerations); once it runs it has a
            // serial, which the entry for the blank one did not.
            let serial = if e.pid == crate::ks::PID_BLANK {
                match crate::ks::wake() {
                    Ok(serial) => serial,
                    Err(err) => {
                        // Report the reason once, not at every retry.
                        static SAID: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
                        let mut said = SAID.lock().unwrap_or_else(|p| p.into_inner());
                        if *said != err.to_string() {
                            *said = err.to_string();
                            crate::server::log(&format!("82357B: the firmware could not be loaded: {err}"));
                        }
                        return Err(err);
                    }
                }
            } else {
                e.serial.clone()
            };
            opened = Entry { kind: Kind::Keysight, serial: serial.clone(), pid: crate::ks::PID_RUN };
            // The open can fail for a while: just after a blank 82357B is woken (Windows is still binding its driver),
            // and while Keysight's IO Libraries hold the device. Keep trying for a while and report once why.
            let woke = e.pid == crate::ks::PID_BLANK;
            let start = std::time::Instant::now();
            let mut tries = 0u32;
            let mut told = false;
            loop {
                match crate::ks::KsAdapter::open(&serial) {
                    Ok(a) => {
                        if told {
                            crate::server::log(&format!("82357B: opened after {} s", start.elapsed().as_secs()));
                        }
                        break Arc::new(a);
                    }
                    Err(Error::Busy) if start.elapsed() < std::time::Duration::from_secs(45) => {
                        if !told {
                            crate::server::log("82357B: held by another program; waiting for it to let go (up to 45 s)");
                            told = true;
                        }
                    }
                    Err(e) if woke && tries < 24 => {
                        tries += 1;
                        crate::server::log(&format!("82357B: opening it just after the firmware load: {e}"));
                    }
                    Err(e) => return Err(e),
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    };
    Ok((a, opened))
}

/// The `pad` that means the interface level (a VISA GPIB INTFC session) to a backend's own write
/// and read: address nobody, the caller's commands did. Also the wire's "no address".
pub const RAW: u8 = 0xFF;

/// No secondary address: the instrument is its primary address alone.
pub const NO_SAD: u8 = 0xFF;

/// Who the interface level's read-ahead is for (`Owed::raw_for`): an INTFC session, a serial poll
/// through it, or a secondary address (`sad_owner`).
pub const RAW_INTFC: u16 = 0xFFFF;
pub const RAW_POLL: u16 = 0xFFFE;
pub fn sad_owner(pad: u8, sad: u8) -> u16 {
    (pad as u16) << 8 | sad as u16
}

// IEEE 488.1 interface messages the secondary-address operations send. The controller is at
// primary address 0 on every adapter here (the INTFC sessions and each backend's own addressing use
// it too).
const UNL: u8 = 0x3F;
const UNT: u8 = 0x5F;
const SPE: u8 = 0x18;
const SPD: u8 = 0x19;
const SDC: u8 = 0x04;
const GET: u8 = 0x08;
const GTL: u8 = 0x01;
const CONTROLLER: u8 = 0;
fn lag(pad: u8) -> u8 {
    0x20 | pad
}
fn tag(pad: u8) -> u8 {
    0x40 | pad
}
fn sag(sad: u8) -> u8 {
    0x60 | sad
}

pub trait Backend: Send + Sync {
    /// Hold the bus: everything through the returned guard is back to back.
    fn lock(&self) -> Box<dyn BusOps + '_>;
    /// The bus only if nobody has it right now. Watching for service requests uses this: it must
    /// never make a caller wait, or interleave with one.
    fn try_lock(&self) -> Option<Box<dyn BusOps + '_>>;
    /// What the adapter says it is, asked of the adapter itself (a Viper488's IDENTIFY board name, such as "Viper488
    /// Classic"); None where the kind already says it.
    fn model(&self) -> Option<String> {
        None
    }
}

pub trait BusOps {
    fn probe(&mut self, pad: u8) -> Result<bool, Error>;
    /// Bytes taken by the adapter; on error also how many were taken before it.
    fn write(&mut self, pad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)>;
    fn read_message(&mut self, pad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error>;
    fn clear(&mut self, pad: u8) -> Result<(), Error>;
    fn trigger(&mut self, pad: u8) -> Result<(), Error>;
    fn spoll(&mut self, pad: u8, tmo_ms: u32) -> Result<u8, Error>;
    fn local(&mut self, pad: u8) -> Result<(), Error>;
    fn llo(&mut self) -> Result<(), Error>;
    fn ren(&mut self, enable: bool) -> Result<(), Error>;
    fn ifc(&mut self) -> Result<(), Error>;
    /// The T1 level (SPEED_*; SPEED_QUERY only reports it): Viper488 boards with CAP_SPEED, the NI
    /// and the 82357B.
    fn speed(&mut self, _level: u8) -> Result<u8, Error> {
        Err(Error::Status(ST_UNSUPPORTED))
    }
    fn lines(&mut self) -> Result<u16, Error> {
        Err(Error::Status(ST_UNSUPPORTED))
    }
    /// The lines and which of them the adapter can tell (set = known): all of them, where `lines`
    /// works at all; a Prologix or AR488 can say SRQ (and an AR488 REN) and nothing else.
    fn lines_known(&mut self) -> Result<(u16, u16), Error> {
        self.lines().map(|l| (l, 0xFFFF))
    }
    /// The interface level, for a VISA GPIB INTFC session: the caller addresses the bus itself.
    /// `command` sends interface messages with ATN and leaves ATN asserted; `raw_write` and
    /// `raw_read` move data as whatever those made the controller, addressing nobody. None of
    /// them sends UNL/UNT on its own, not at EOI and not on failure.
    fn command(&mut self, _cmds: &[u8], _tmo_ms: u32) -> Result<(), Error> {
        Err(Error::Status(ST_UNSUPPORTED))
    }
    fn raw_write(&mut self, _data: &[u8], _eoi: bool, _tmo_ms: u32) -> Result<usize, (usize, Error)> {
        Err((0, Error::Status(ST_UNSUPPORTED)))
    }
    fn raw_read(&mut self, _count: usize, _term: Option<u8>, _tmo_ms: u32) -> Result<ReadResult, Error> {
        Err(Error::Status(ST_UNSUPPORTED))
    }
    /// Is SRQ asserted? Reads the adapter's own view of the line (a level, not an event: SRQ is
    /// wired-OR, and a second requester makes no edge) and addresses nobody.
    fn srq_asserted(&mut self) -> Result<bool, Error> {
        Ok(false)
    }
    /// Status bytes with RQS set that the adapter read on its own (the 82357B polls an address to check whether anyone
    /// is there), as (pad, byte). Each is handed over once, and the daemon delivers it as if it had polled for it.
    fn take_polled(&mut self) -> Vec<(u8, u8)> {
        Vec::new()
    }

    /// Does this adapter have the interface level (`command`, `raw_write`, `raw_read`)? The
    /// secondary-address operations below are made of it.
    fn interface_level(&self) -> bool {
        false
    }
    /// Who the next interface-level read is for (`Owed::raw_for`); an adapter that keeps no
    /// read-ahead has nothing to do.
    fn raw_for(&mut self, _who: u16, _keep: bool) {}

    // Instruments at a secondary address (`GPIBn::pad::sad::INSTR`, sad 0..=30), built from the interface level: the
    // primary address followed at once by the secondary (SCG, 0x60 | sad). An adapter without the interface level
    // answers UNSUPPORTED.

    /// The adapter can reach secondary addresses. Whether an instrument answers at one is not checked; the first
    /// operation tells.
    fn probe_sad(&mut self, _pad: u8, _sad: u8) -> Result<bool, Error> {
        if self.interface_level() { Ok(true) } else { Err(Error::Status(ST_UNSUPPORTED)) }
    }
    fn write_sad(&mut self, pad: u8, sad: u8, data: &[u8], eoi: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        self.raw_for(sad_owner(pad, sad), false); // a new message: IEEE 488.2 drops unread output
        self.command(&[UNL, lag(pad), sag(sad), tag(CONTROLLER)], tmo_ms).map_err(|e| (0, e))?;
        self.raw_write(data, eoi, tmo_ms)
    }
    fn read_sad(&mut self, pad: u8, sad: u8, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        self.raw_for(sad_owner(pad, sad), true);
        self.command(&[UNL, lag(CONTROLLER), tag(pad), sag(sad)], tmo_ms)?;
        self.raw_read(count, term, tmo_ms)
    }
    fn clear_sad(&mut self, pad: u8, sad: u8) -> Result<(), Error> {
        self.raw_for(sad_owner(pad, sad), false);
        self.command(&[UNL, lag(pad), sag(sad), SDC], 1000)
    }
    fn trigger_sad(&mut self, pad: u8, sad: u8) -> Result<(), Error> {
        self.command(&[UNL, lag(pad), sag(sad), GET], 1000)
    }
    fn local_sad(&mut self, pad: u8, sad: u8) -> Result<(), Error> {
        self.command(&[UNL, lag(pad), sag(sad), GTL], 1000)
    }
    /// A serial poll. SPD and UNT are sent on failure too, so the instrument does not answer later reads with its
    /// status byte. Any interface-level read-ahead for it is dropped first.
    fn spoll_sad(&mut self, pad: u8, sad: u8, tmo_ms: u32) -> Result<u8, Error> {
        self.raw_for(RAW_POLL, false);
        self.command(&[UNL, SPE, lag(CONTROLLER), tag(pad), sag(sad)], tmo_ms)?;
        let r = self.raw_read(1, None, tmo_ms);
        let restored = self.command(&[SPD, UNT], tmo_ms);
        let r = r?;
        restored?;
        r.data.first().copied().ok_or(Error::Timeout)
    }
}

/// The bus status register of a TMS9914 (82357B) or a TNT4882 (NI), which lay the lines out alike
/// -- REN, IFC, SRQ, EOI, NRFD, NDAC, DAV, ATN from bit 0 up, set = asserted -- as `lines()` reports
/// them.
pub fn lines_from_bsr(bsr: u8) -> u16 {
    use crate::proto::{BUS_ATN, BUS_DAV, BUS_EOI, BUS_IFC, BUS_NDAC, BUS_NRFD, BUS_REN, BUS_SRQ};
    let map = [(0, BUS_REN), (1, BUS_IFC), (2, BUS_SRQ), (3, BUS_EOI), (4, BUS_NRFD), (5, BUS_NDAC), (6, BUS_DAV), (7, BUS_ATN)];
    map.iter().filter(|(bit, _)| bsr >> bit & 1 != 0).fold(0, |acc, (_, f)| acc | f)
}

#[cfg(test)]
mod bsr_tests {
    use super::*;
    use crate::proto::*;

    #[test]
    fn the_bus_status_register_maps_line_by_line() {
        // Read on an 82357B: SRQ held by an instrument, the bus idle (NDAC, EOI).
        assert_eq!(lines_from_bsr(0x2c), BUS_NDAC | BUS_EOI | BUS_SRQ);
        assert_eq!(lines_from_bsr(0x80), BUS_ATN);
        assert_eq!(lines_from_bsr(0x01), BUS_REN);
        assert_eq!(lines_from_bsr(0xff).count_ones(), 8);
    }
}

#[cfg(test)]
mod secondary_tests {
    use super::*;
    use crate::adapter::ReadEnd;

    /// An interface level that records what goes out and answers reads from a script.
    #[derive(Default)]
    struct Rec {
        cmds: Vec<Vec<u8>>,
        written: Vec<(Vec<u8>, bool)>,
        replies: Vec<Result<ReadResult, Error>>,
        owners: Vec<(u16, bool)>,
        level: bool,
    }
    impl BusOps for Rec {
        fn probe(&mut self, _: u8) -> Result<bool, Error> { unreachable!() }
        fn write(&mut self, _: u8, _: &[u8], _: bool, _: u32) -> Result<usize, (usize, Error)> { unreachable!() }
        fn read_message(&mut self, _: u8, _: usize, _: Option<u8>, _: u32) -> Result<ReadResult, Error> { unreachable!() }
        fn clear(&mut self, _: u8) -> Result<(), Error> { unreachable!() }
        fn trigger(&mut self, _: u8) -> Result<(), Error> { unreachable!() }
        fn spoll(&mut self, _: u8, _: u32) -> Result<u8, Error> { unreachable!() }
        fn local(&mut self, _: u8) -> Result<(), Error> { unreachable!() }
        fn llo(&mut self) -> Result<(), Error> { unreachable!() }
        fn ren(&mut self, _: bool) -> Result<(), Error> { unreachable!() }
        fn ifc(&mut self) -> Result<(), Error> { unreachable!() }
        fn command(&mut self, cmds: &[u8], _: u32) -> Result<(), Error> {
            self.cmds.push(cmds.to_vec());
            Ok(())
        }
        fn raw_write(&mut self, data: &[u8], eoi: bool, _: u32) -> Result<usize, (usize, Error)> {
            self.written.push((data.to_vec(), eoi));
            Ok(data.len())
        }
        fn raw_read(&mut self, _: usize, _: Option<u8>, _: u32) -> Result<ReadResult, Error> {
            self.replies.remove(0)
        }
        fn interface_level(&self) -> bool {
            self.level
        }
        fn raw_for(&mut self, who: u16, keep: bool) {
            self.owners.push((who, keep));
        }
    }
    fn reply(data: &[u8]) -> Result<ReadResult, Error> {
        Ok(ReadResult { data: data.to_vec(), end: ReadEnd::End })
    }

    #[test]
    fn a_secondary_address_follows_its_primary() {
        let mut b = Rec { level: true, ..Default::default() };
        assert_eq!(b.probe_sad(5, 3), Ok(true));
        b.write_sad(5, 3, b"*IDN?\n", true, 1000).unwrap();
        b.replies.push(reply(b"ID\n"));
        assert_eq!(b.read_sad(5, 3, 100, Some(b'\n'), 1000).unwrap().data, b"ID\n");
        b.clear_sad(5, 3).unwrap();
        b.trigger_sad(5, 3).unwrap();
        b.local_sad(5, 3).unwrap();
        assert_eq!(b.cmds, vec![
            vec![UNL, 0x25, 0x63, 0x40], // listen 5, secondary 3, the controller talks
            vec![UNL, 0x20, 0x45, 0x63], // the controller listens, 5 talks, secondary 3
            vec![UNL, 0x25, 0x63, SDC],
            vec![UNL, 0x25, 0x63, GET],
            vec![UNL, 0x25, 0x63, GTL],
        ]);
        assert_eq!(b.written, vec![(b"*IDN?\n".to_vec(), true)]);
        let me = sad_owner(5, 3);
        assert_eq!(b.owners, vec![(me, false), (me, true), (me, false)], "a write and a clear drop the tail, a read keeps its own");
    }

    #[test]
    fn a_serial_poll_is_undone_even_when_it_fails() {
        let mut b = Rec { level: true, ..Default::default() };
        b.replies.push(reply(&[0x50]));
        assert_eq!(b.spoll_sad(7, 0, 100), Ok(0x50));
        b.replies.push(Ok(ReadResult { data: vec![], end: ReadEnd::Timeout }));
        assert_eq!(b.spoll_sad(7, 30, 100), Err(Error::Timeout));
        b.replies.push(Err(Error::Gone));
        assert_eq!(b.spoll_sad(7, 30, 100), Err(Error::Gone));
        assert_eq!(b.cmds[0], vec![UNL, SPE, 0x20, 0x47, 0x60]);
        assert_eq!(b.cmds[1], vec![SPD, UNT]);
        assert_eq!(b.cmds[3], vec![SPD, UNT]);
        assert_eq!(b.cmds[5], vec![SPD, UNT]);
        assert!(b.owners.iter().all(|o| *o == (RAW_POLL, false)), "a status byte never takes a tail");
    }

    #[test]
    fn without_the_interface_level_there_are_no_secondary_addresses() {
        let mut b = Rec::default();
        assert_eq!(b.probe_sad(5, 3), Err(Error::Status(ST_UNSUPPORTED)));
    }
}
