//! USBTMC instruments (USBTMC 1.0 with the USB488 subclass), owned by the daemon like the adapters.
//!
//! A USBTMC device is an instrument, not a bus: no GPIB address, no interface level. So it is not a board; the daemon
//! numbers USBTMC instruments separately (`Slots`), by `VID:PID:serial`.
//!
//! The protocol is done here, over a `Link` that only moves bytes: nusb for a device we can claim (on Windows, one
//! bound to WinUSB); on Windows the interface IVI-6.2 specifies for a device on a vendor's USBTMC driver
//! (`usbtmc_ivi.rs`); on Linux the kernel usbtmc driver's pass-through ioctls (`usbtmc_linux.rs`). Those drivers let
//! other programs open the instrument at the same time, which WinUSB and usbfs do not. A device with none of these is
//! left alone and `list_all` says why. Listing reads what the OS has cached, so it opens nothing.
//!
//! References: the USBTMC and USBTMC-USB488 specifications, and pyvisa-py's `protocols/usbtmc.py` (MIT). The USB488
//! interface capabilities are byte 14 of the GET_CAPABILITIES answer (12..14 is bcdUSB488), and REN_CONTROL,
//! GO_TO_LOCAL and LOCAL_LOCKOUT are control IN requests that answer one status byte.

use crate::adapter::{Error, ReadEnd, ReadResult};
use crate::proto::ST_UNSUPPORTED;
use nusb::descriptors::TransferType;
use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, Direction, In, Interrupt, Out, Recipient, TransferError};
use nusb::MaybeFuture;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const CLASS_APP_SPECIFIC: u8 = 0xFE;
const SUBCLASS_USBTMC: u8 = 0x03;
const PROTOCOL_USB488: u8 = 0x01;

// Bulk messages.
const DEV_DEP_MSG_OUT: u8 = 1;
const REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const DEV_DEP_MSG_IN: u8 = 2;
const TRIGGER: u8 = 128;
const HEADER: usize = 12;
/// bmTransferAttributes: end of message (OUT and IN); on a read request, stop on TermChar; on
/// an answer, it ended on TermChar.
const EOM: u8 = 0x01;
const TERM_CHAR: u8 = 0x02;

// Class requests. The abort ones are addressed to the endpoint, the rest to the interface.
const INITIATE_ABORT_BULK_OUT: u8 = 1;
const CHECK_ABORT_BULK_OUT_STATUS: u8 = 2;
const INITIATE_ABORT_BULK_IN: u8 = 3;
const CHECK_ABORT_BULK_IN_STATUS: u8 = 4;
const INITIATE_CLEAR: u8 = 5;
const CHECK_CLEAR_STATUS: u8 = 6;
const GET_CAPABILITIES: u8 = 7;
const READ_STATUS_BYTE: u8 = 128;
const REN_CONTROL: u8 = 160;
const GO_TO_LOCAL: u8 = 161;
const LOCAL_LOCKOUT: u8 = 162;

const STATUS_SUCCESS: u8 = 0x01;
const STATUS_PENDING: u8 = 0x02;

/// An interrupt-IN notification: 0x81 is a service request (the status byte follows); 0x80 | bTag
/// answers READ_STATUS_BYTE when the device has the endpoint.
const NOTIFY_SRQ: u8 = 0x81;

const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long one drain read waits before deciding the IN pipe is empty.
const DRAIN_WAIT: Duration = Duration::from_millis(50);
/// The most one read request asks for.
const CHUNK: usize = 1 << 20;
/// How long an abort is given to finish (pyvisa-py uses the same).
const ABORT_TIMEOUT: Duration = Duration::from_secs(5);
const SRQ_QUEUE_MAX: usize = 50;

/// A USBTMC device the machine has, whether or not it is ours to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    pub iface: u8,
    pub usb488: bool,
    pub manufacturer: String,
    pub product: String,
    /// The driver bound to it (Windows); empty where the OS does not say.
    pub driver: String,
    /// Why it is not used, if it is not.
    pub not_ours: Option<String>,
    /// The driver it is used through, when that is the way: on Windows the vendor USBTMC driver's
    /// device interface (IVI-6.2), on Linux the kernel usbtmc driver's `/dev/usbtmcN`. None for one
    /// claimed through nusb.
    pub through: Option<String>,
}

impl Found {
    /// What the daemon knows it by, and numbers it by.
    pub fn key(&self) -> String {
        key(self.vid, self.pid, &self.serial)
    }
}

pub fn key(vid: u16, pid: u16, serial: &str) -> String {
    format!("{vid:04X}:{pid:04X}:{serial}")
}

/// Every USBTMC device the OS lists, with why one is not used: no serial number (nothing stable
/// to know it by); on Windows, no driver at all, or a driver other than WinUSB that offers no enabled
/// IVI-6.2 interface; on Linux a kernel driver on its USBTMC interface other than usbtmc, or usbtmc
/// without its device file.
pub fn list_all() -> Result<Vec<Found>, Error> {
    let all = nusb::list_devices().wait().map_err(|e| Error::Io(format!("cannot enumerate USB devices: {e}")))?;
    let mut v = Vec::new();
    for d in all {
        let Some(i) = d.interfaces().find(|i| i.class() == CLASS_APP_SPECIFIC && i.subclass() == SUBCLASS_USBTMC) else {
            continue;
        };
        let serial = d.serial_number().unwrap_or("").to_string();
        let driver = driver_of(&d, i.interface_number());
        let through = driver_device(&d, i.interface_number(), &driver);
        let not_ours = if serial.is_empty() {
            Some("it reports no serial number, so it has no stable name".to_string())
        } else if cfg!(windows) && driver.is_empty() {
            // The installer's UsbtmcWinUsb feature binds every instrument that has no driver to WinUSB.
            Some("it has no driver (the installer's \"USBTMC instruments on WinUSB\" feature binds it; repair the installation with that feature)".to_string())
        } else if cfg!(windows) && !driver.eq_ignore_ascii_case("winusb") && through.is_none() {
            // A driver that just took the device over may report it started with its interface not yet enabled, until
            // the device is plugged in again or Windows restarts.
            Some(format!(
                "it is bound to {driver}, which offers no enabled USBTMC interface (IVI-6.2): unplug it and plug it in again, or restart Windows; windows\\winusb-bind.ps1 -VidPid {:04X}:{:04X} moves it to WinUSB instead",
                d.vendor_id(),
                d.product_id()
            ))
        } else if cfg!(target_os = "linux") && driver == "usbtmc" && through.is_none() {
            Some("the kernel's usbtmc driver has it, but there is no /dev/usbtmc device file for it".to_string())
        } else if cfg!(target_os = "linux") && !driver.is_empty() && driver != "usbtmc" {
            Some(format!(
                "its USBTMC interface belongs to the kernel driver {driver} (echo <interface> | sudo tee /sys/bus/usb/drivers/{driver}/unbind hands it over)"
            ))
        } else {
            None
        };
        v.push(Found {
            vid: d.vendor_id(),
            pid: d.product_id(),
            serial,
            iface: i.interface_number(),
            usb488: i.protocol() == PROTOCOL_USB488,
            manufacturer: d.manufacturer_string().unwrap_or("").to_string(),
            product: d.product_string().unwrap_or("").to_string(),
            driver,
            not_ours,
            through,
        });
    }
    v.sort_by_key(|a| a.key());
    Ok(v)
}

/// The driver that has a device: on Windows the device's (nusb says), on Linux the kernel driver
/// bound to its USBTMC interface (sysfs); elsewhere nothing is known.
#[cfg(windows)]
fn driver_of(d: &nusb::DeviceInfo, _iface: u8) -> String {
    d.driver().unwrap_or("").to_string()
}

#[cfg(not(windows))]
fn driver_of(d: &nusb::DeviceInfo, iface: u8) -> String {
    crate::adapter::kernel_driver(d, iface).unwrap_or_default()
}

/// What a device on a USBTMC driver is used through: on Windows the enabled IVI-6.2 device interface
/// of a driver other than WinUSB; on Linux the kernel usbtmc driver's device file.
#[cfg(windows)]
fn driver_device(d: &nusb::DeviceInfo, _iface: u8, driver: &str) -> Option<String> {
    if driver.is_empty() || driver.eq_ignore_ascii_case("winusb") {
        return None;
    }
    crate::usbtmc_ivi::interface_path(d.instance_id())
}

#[cfg(target_os = "linux")]
fn driver_device(d: &nusb::DeviceInfo, iface: u8, driver: &str) -> Option<String> {
    if driver != "usbtmc" {
        return None;
    }
    crate::usbtmc_linux::device_file(d.sysfs_path(), iface)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn driver_device(_: &nusb::DeviceInfo, _: u8, _: &str) -> Option<String> {
    None
}

/// The USBTMC instruments that are ours to use.
pub fn list() -> Result<Vec<Found>, Error> {
    Ok(list_all()?.into_iter().filter(|f| f.not_ours.is_none()).collect())
}

/// USBTMC instrument numbers, and why they do not move while the daemon runs -- the same rule as
/// board numbers (boards.rs): a number stays with its instrument, one that is unplugged keeps its
/// number marked absent, and a new one takes the next.
#[derive(Default)]
pub struct Slots {
    slots: Vec<(String, bool)>,
}

impl Slots {
    pub fn update(&mut self, keys: &[String]) {
        for s in &mut self.slots {
            s.1 = keys.contains(&s.0);
        }
        for k in keys {
            if !self.slots.iter().any(|s| &s.0 == k) {
                self.slots.push((k.clone(), true));
            }
        }
    }

    /// The instrument a number stands for, if it is attached.
    pub fn get(&self, n: u16) -> Result<String, Error> {
        match self.slots.get(n as usize) {
            Some((k, true)) => Ok(k.clone()),
            Some(_) => Err(Error::Gone),
            None => Err(Error::NoBoard),
        }
    }

    pub fn slots(&self) -> &[(String, bool)] {
        &self.slots
    }
}

/// What GET_CAPABILITIES said.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub usb488: bool,
    pub trigger: bool,
    pub ren: bool,
    /// The device stops a read on a termination character (byte 5, bit 0). USBTMC 3.2.1.2: the
    /// host must not ask one that does not; such a read is cut here instead.
    pub term_char: bool,
}

/// How a transfer on a `Link` failed.
#[derive(Debug)]
pub(crate) enum LinkError {
    /// The device stalled it: for a request, that is the device not doing it (USB 2.0 9.2.7).
    Stall,
    /// It did not finish in the time given, and was cancelled.
    Timeout,
    /// The device is not there any more.
    Gone,
    Other(String),
}

impl LinkError {
    pub(crate) fn into_error(self, what: &str) -> Error {
        match self {
            LinkError::Timeout => Error::Timeout,
            LinkError::Gone => Error::Gone,
            LinkError::Stall => Error::Io(format!("{what}: stalled")),
            LinkError::Other(e) => Error::Io(format!("{what}: {e}")),
        }
    }
}

/// What the protocol needs from below: bytes in and out of the endpoints, control requests, and
/// clearing a halt. Each call waits at most `wait`, and one that does not finish is cancelled
/// before it returns (`LinkError::Timeout`).
pub(crate) trait Link: Send {
    /// Send all of `data` on the bulk-OUT endpoint.
    fn bulk_out(&mut self, data: &[u8], wait: Duration) -> Result<(), LinkError>;
    /// Read from the bulk-IN endpoint up to `max` bytes; a short packet ends it sooner.
    fn bulk_in(&mut self, max: usize, wait: Duration) -> Result<Vec<u8>, LinkError>;
    /// A control request in, by its setup packet's fields; what it answered (`length` at most).
    fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, length: u16, wait: Duration) -> Result<Vec<u8>, LinkError>;
    /// A control request out, carrying `data`.
    fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], wait: Duration) -> Result<(), LinkError>;
    /// Clear a halt on the bulk-OUT (`out`) or bulk-IN endpoint.
    fn clear_halt(&mut self, out: bool);
    /// The interrupt-IN endpoint, for the thread that watches it; None if there is none. Taken once.
    fn interrupts(&mut self) -> Option<Box<dyn Interrupts>>;
    /// The status byte, where the driver reads it itself (Linux's usbtmc owns the interrupt endpoint,
    /// so READ_STATUS_BYTE's answer there never reaches us); None: ask with READ_STATUS_BYTE.
    fn read_stb(&mut self, _wait: Duration) -> Option<Result<u8, LinkError>> {
        None
    }
}

/// The interrupt-IN endpoint, read by one thread while the instrument is open.
pub(crate) trait Interrupts: Send {
    /// One notification, or `LinkError::Timeout` if none came in `wait`.
    fn wait(&mut self, wait: Duration) -> Result<Vec<u8>, LinkError>;
    fn clear_halt(&mut self);
}

/// Request types of the class requests this module makes (bmRequestType: IN, class, recipient).
const CLASS_IN_INTERFACE: u8 = 0xA1;
const CLASS_IN_ENDPOINT: u8 = 0xA2;

/// The endpoints of a USBTMC interface, from its descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Endpoints {
    pub out_addr: u8,
    pub in_addr: u8,
    /// The bulk-IN endpoint's packet size: nusb takes IN buffers only in whole packets.
    pub in_max: usize,
    /// The interrupt-IN endpoint and its packet size, if there is one.
    pub intr: Option<(u8, usize)>,
}

/// The USBTMC interface `iface`'s endpoints, from a whole configuration descriptor (as
/// GET_DESCRIPTOR answers it): the first bulk OUT, bulk IN and interrupt IN of its first USBTMC
/// alternate setting.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))] // read through a USBTMC driver
pub(crate) fn endpoints_in(config: &[u8], iface: u8) -> Option<Endpoints> {
    let (mut out, mut inn, mut intr) = (None, None, None);
    let mut here = false;
    let mut done = false;
    let mut i = 0;
    while i + 2 <= config.len() && !done {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            // An interface descriptor: ours if it is `iface` and USBTMC; a second one of ours ends it.
            4 if len >= 9 => {
                if here {
                    done = true;
                } else {
                    here = d[2] == iface && d[5] == CLASS_APP_SPECIFIC && d[6] == SUBCLASS_USBTMC;
                }
            }
            5 if len >= 7 && here => {
                let (addr, kind, max) = (d[2], d[3] & 3, u16::from_le_bytes([d[4], d[5]]) as usize & 0x7FF);
                match (kind, addr & 0x80 != 0) {
                    (2, false) => out = out.or(Some(addr)),
                    (2, true) => inn = inn.or(Some((addr, max.max(1)))),
                    (3, true) => intr = intr.or(Some((addr, max.max(2)))),
                    _ => {}
                }
            }
            _ => {}
        }
        i += len;
    }
    let (out_addr, (in_addr, in_max)) = (out?, inn?);
    Some(Endpoints { out_addr, in_addr, in_max, intr })
}

/// A device claimed through nusb (WinUSB on Windows).
struct NusbLink {
    iface: nusb::Interface,
    ep_out: nusb::Endpoint<Bulk, Out>,
    ep_in: nusb::Endpoint<Bulk, In>,
    intr: Option<(nusb::Endpoint<Interrupt, In>, usize)>,
}

fn nusb_error(e: TransferError) -> LinkError {
    match e {
        TransferError::Cancelled => LinkError::Timeout,
        TransferError::Disconnected => LinkError::Gone,
        TransferError::Stall => LinkError::Stall,
        e => LinkError::Other(e.to_string()),
    }
}

/// bmRequestType's type and recipient as nusb has them; None for the reserved ones.
fn nusb_setup(request_type: u8) -> Option<(ControlType, Recipient)> {
    let t = match (request_type >> 5) & 3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        _ => return None,
    };
    let r = match request_type & 0x1F {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        3 => Recipient::Other,
        _ => return None,
    };
    Some((t, r))
}

impl Link for NusbLink {
    fn bulk_out(&mut self, data: &[u8], wait: Duration) -> Result<(), LinkError> {
        let mut buf = self.ep_out.allocate(data.len());
        buf.extend_from_slice(data);
        self.ep_out.transfer_blocking(buf, wait).status.map_err(nusb_error)
    }

    fn bulk_in(&mut self, max: usize, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let buf = self.ep_in.allocate(max);
        let c = self.ep_in.transfer_blocking(buf, wait);
        c.status.map_err(nusb_error)?;
        Ok(c.buffer[..c.actual_len].to_vec())
    }

    fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, length: u16, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let (control_type, recipient) = nusb_setup(request_type).ok_or_else(|| LinkError::Other("a reserved request type".into()))?;
        self.iface.control_in(ControlIn { control_type, recipient, request, value, index, length }, wait).wait().map_err(nusb_error)
    }

    fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], wait: Duration) -> Result<(), LinkError> {
        let (control_type, recipient) = nusb_setup(request_type).ok_or_else(|| LinkError::Other("a reserved request type".into()))?;
        self.iface.control_out(ControlOut { control_type, recipient, request, value, index, data }, wait).wait().map_err(nusb_error)
    }

    fn clear_halt(&mut self, out: bool) {
        let _ = if out { self.ep_out.clear_halt().wait() } else { self.ep_in.clear_halt().wait() };
    }

    fn interrupts(&mut self) -> Option<Box<dyn Interrupts>> {
        self.intr.take().map(|(ep, max)| Box::new(NusbInterrupts { ep, max }) as Box<dyn Interrupts>)
    }
}

struct NusbInterrupts {
    ep: nusb::Endpoint<Interrupt, In>,
    max: usize,
}

impl Interrupts for NusbInterrupts {
    fn wait(&mut self, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let buf = self.ep.allocate(self.max);
        let c = self.ep.transfer_blocking(buf, wait);
        c.status.map_err(nusb_error)?;
        Ok(c.buffer[..c.actual_len].to_vec())
    }

    fn clear_halt(&mut self) {
        let _ = self.ep.clear_halt().wait();
    }
}

struct Pipe {
    link: Box<dyn Link>,
    iface_no: u8,
    out_addr: u8,
    in_addr: u8,
    /// Bulk IN buffers are a whole number of these (nusb refuses others before they start).
    in_max: usize,
    /// bTag of the bulk messages: 1..=255, never 0.
    tag: u8,
    /// bTag of READ_STATUS_BYTE: its own space, 2..=127.
    stb_tag: u8,
    /// What an answer brought past the termination character that a read was cut at here, for the
    /// next read; and whether the message ended after it.
    ahead: Vec<u8>,
    ahead_end: bool,
}

/// What the interrupt endpoint has said, for whoever asks.
#[derive(Default)]
struct IntrState {
    /// READ_STATUS_BYTE answers, by bTag.
    stb: HashMap<u8, u8>,
    /// Service requests: the status byte each came with.
    srq: VecDeque<u8>,
    dead: bool,
}

#[derive(Default)]
struct Intr {
    st: Mutex<IntrState>,
    cv: Condvar,
    stop: AtomicBool,
}

/// An open USBTMC instrument.
pub struct Tmc {
    /// The interrupt IN endpoint's packet size, 0 without one (VI_ATTR_USB_MAX_INTR_SIZE's default).
    pub intr_max: u16,
    /// Opened through a vendor's USBTMC driver, which other programs may have open too (IVI-6.2):
    /// the daemon holds it only while a connection that used it is there.
    pub shared: bool,
    pipe: Mutex<Pipe>,
    intr: Option<Arc<Intr>>,
    /// The thread reading the interrupt endpoint. It holds the interface until it ends, so dropping a `Tmc` waits for
    /// it; otherwise reopening at once would find the interface still claimed.
    watcher: Option<std::thread::JoinHandle<()>>,
    pub caps: Caps,
}

impl Tmc {
    /// Open the instrument `key` names: it must be listed as ours.
    pub fn open(key: &str) -> Result<Tmc, Error> {
        let all = list_all()?;
        let f = all.iter().find(|f| f.key() == key).ok_or(Error::Gone)?;
        if let Some(why) = &f.not_ours {
            return Err(Error::Io(format!("{key}: {why}")));
        }
        let (link, eps, shared) = match &f.through {
            #[cfg(windows)]
            Some(path) => {
                let (link, eps) = crate::usbtmc_ivi::open(path, f.iface)?;
                (link, eps, true)
            }
            #[cfg(target_os = "linux")]
            Some(path) => {
                let (link, eps) = crate::usbtmc_linux::open(path, f.iface)?;
                (link, eps, true)
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            Some(_) => return Err(Error::Io("no USBTMC driver to go through here".into())),
            None => {
                let (link, eps) = open_nusb(f)?;
                (Box::new(link) as Box<dyn Link>, eps, false)
            }
        };
        let mut pipe = Pipe {
            link,
            iface_no: f.iface,
            out_addr: eps.out_addr,
            in_addr: eps.in_addr,
            in_max: eps.in_max,
            tag: 0,
            stb_tag: 1,
            ahead: Vec::new(),
            ahead_end: false,
        };
        // A session that was killed may leave an endpoint halted and an unread answer in the IN pipe, which would put
        // later exchanges one behind; both are cleared here. Not on macOS, where clearing a halt resets the host's data
        // toggle while some devices keep theirs, losing the next packet; a pipe that is really halted shows as a stall
        // and is cleared by the exchange that finds it. Nor through a vendor's driver, which other programs may be
        // using; `answer` skips what is not ours anyway.
        if !cfg!(target_os = "macos") && !shared {
            pipe.link.clear_halt(true);
            pipe.link.clear_halt(false);
            pipe.drain();
        }
        let caps = pipe.capabilities(f.usb488);
        let (intr, watcher) = match pipe.link.interrupts() {
            Some(src) => {
                let i = Arc::new(Intr::default());
                let i2 = i.clone();
                (Some(i), Some(std::thread::spawn(move || watch_interrupts(src, i2))))
            }
            None => (None, None),
        };
        crate::server::log(&format!(
            "USBTMC {key}: {}{}, termination character {}, trigger {}, remote/local {}, interrupt endpoint {}",
            if shared { format!("through {} ({}), ", f.driver, f.through.as_deref().unwrap_or("")) } else { String::new() },
            if caps.usb488 { "USB488" } else { "no USB488" },
            if caps.term_char { "yes" } else { "no" },
            if caps.trigger { "yes" } else { "no" },
            if caps.ren { "yes" } else { "no" },
            if intr.is_some() { "yes" } else { "no" },
        ));
        let intr_max = match (&intr, eps.intr) {
            (Some(_), Some((_, max))) => max as u16,
            _ => 0,
        };
        Ok(Tmc { pipe: Mutex::new(pipe), intr, watcher, caps, intr_max, shared })
    }

    /// The instrument, held for one operation.
    pub fn lock(&self) -> Ops<'_> {
        Ops { t: self, p: self.pipe.lock().unwrap_or_else(|e| e.into_inner()) }
    }

    /// Only if nobody has it now: for the service-request watch, which must never make a caller
    /// wait.
    pub fn try_lock(&self) -> Option<Ops<'_>> {
        self.pipe.try_lock().ok().map(|p| Ops { t: self, p })
    }

    /// Does the device report service requests itself (an interrupt endpoint)? Without one they
    /// are found by asking for the status byte.
    pub fn notifies(&self) -> bool {
        self.intr.is_some()
    }

    /// A service request the interrupt endpoint reported: its status byte.
    pub fn take_srq(&self) -> Option<u8> {
        self.intr.as_ref()?.st.lock().unwrap().srq.pop_front()
    }
}

/// Claim the instrument through nusb: its USBTMC interface and endpoints.
fn open_nusb(f: &Found) -> Result<(NusbLink, Endpoints), Error> {
    let info = nusb::list_devices()
        .wait()
        .map_err(|e| Error::Io(format!("cannot enumerate USB devices: {e}")))?
        .find(|d| d.vendor_id() == f.vid && d.product_id() == f.pid && d.serial_number() == Some(f.serial.as_str()))
        .ok_or(Error::Gone)?;
    let device = info.open().wait().map_err(|e| crate::adapter::open_error("open", e))?;
    // The endpoints are read from the descriptors: USBTMC fixes the protocol, not the numbering.
    let config = device.active_configuration().map_err(|e| Error::Io(format!("its configuration: {e}")))?;
    let (mut out_addr, mut in_addr, mut intr, mut in_max) = (None, None, None, 64usize);
    let mut found = false;
    for i in config.interfaces().filter(|i| i.interface_number() == f.iface) {
        let Some(alt) = i.alt_settings().find(|a| a.class() == CLASS_APP_SPECIFIC && a.subclass() == SUBCLASS_USBTMC) else {
            continue;
        };
        found = true;
        for e in alt.endpoints() {
            match (e.transfer_type(), e.direction()) {
                (TransferType::Bulk, Direction::Out) => out_addr = out_addr.or(Some(e.address())),
                (TransferType::Bulk, Direction::In) if in_addr.is_none() => {
                    in_addr = Some(e.address());
                    in_max = e.max_packet_size().max(1);
                }
                (TransferType::Interrupt, Direction::In) if intr.is_none() => intr = Some((e.address(), e.max_packet_size().max(2))),
                _ => {}
            }
        }
        break;
    }
    if !found {
        return Err(Error::Io("its USBTMC interface has no usable setting".into()));
    }
    let (Some(out_addr), Some(in_addr)) = (out_addr, in_addr) else {
        return Err(Error::Io("its USBTMC interface has no bulk endpoint pair".into()));
    };
    crate::adapter::configure(&device);
    let iface = device.claim_interface(f.iface).wait().map_err(|e| crate::adapter::claim_error(&info, f.iface, "claim", e))?;
    let ep_out = iface.endpoint::<Bulk, Out>(out_addr).map_err(|e| Error::Io(format!("bulk OUT: {e}")))?;
    let ep_in = iface.endpoint::<Bulk, In>(in_addr).map_err(|e| Error::Io(format!("bulk IN: {e}")))?;
    let intr_ep = intr.and_then(|(a, max)| iface.endpoint::<Interrupt, In>(a).ok().map(|ep| (ep, max)));
    let eps = Endpoints { out_addr, in_addr, in_max, intr: if intr_ep.is_some() { intr } else { None } };
    Ok((NusbLink { iface, ep_out, ep_in, intr: intr_ep }, eps))
}

impl Drop for Tmc {
    fn drop(&mut self) {
        if let Some(i) = &self.intr {
            i.stop.store(true, Ordering::SeqCst);
        }
        if let Some(w) = self.watcher.take() {
            let _ = w.join(); // within one 200 ms read of the endpoint
        }
    }
}

/// Read the interrupt endpoint until told to stop or the device goes: service requests into the
/// queue, READ_STATUS_BYTE answers by their bTag.
fn watch_interrupts(mut src: Box<dyn Interrupts>, intr: Arc<Intr>) {
    while !intr.stop.load(Ordering::SeqCst) {
        match src.wait(Duration::from_millis(200)) {
            Ok(b) if b.len() >= 2 => {
                let (n1, n2) = (b[0], b[1]);
                let mut st = intr.st.lock().unwrap();
                if n1 == NOTIFY_SRQ {
                    if st.srq.len() >= SRQ_QUEUE_MAX {
                        st.srq.pop_front();
                    }
                    st.srq.push_back(n2);
                } else if n1 & 0x80 != 0 {
                    st.stb.insert(n1 & 0x7F, n2);
                }
                intr.cv.notify_all();
            }
            Ok(_) | Err(LinkError::Timeout) => {}
            Err(LinkError::Stall) => src.clear_halt(),
            Err(LinkError::Gone) => break,
            Err(LinkError::Other(_)) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    intr.st.lock().unwrap().dead = true;
    intr.cv.notify_all();
}

impl Pipe {
    fn next_tag(&mut self) -> u8 {
        self.tag = if self.tag == 255 { 1 } else { self.tag + 1 };
        self.tag
    }

    /// A class request in (`request_type`: to the interface or an endpoint). One the device stalls is one it does not
    /// do (USB 2.0 9.2.7): unsupported, whatever GET_CAPABILITIES claimed.
    fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, Error> {
        self.link.control_in(request_type, request, value, index, length, CONTROL_TIMEOUT).map_err(|e| match e {
            LinkError::Stall => Error::Status(ST_UNSUPPORTED),
            e => e.into_error(&format!("class request {request}")),
        })
    }

    /// A class request to the interface that answers a status byte first.
    fn request(&self, request: u8, value: u16, length: u16) -> Result<Vec<u8>, Error> {
        let r = self.control_in(CLASS_IN_INTERFACE, request, value, self.iface_no as u16, length)?;
        match r.first() {
            Some(&STATUS_SUCCESS) => Ok(r),
            Some(s) => Err(Error::Io(format!("class request {request} answered status {s:#04x}"))),
            None => Err(Error::Io(format!("class request {request} answered nothing"))),
        }
    }

    /// Take whatever is waiting in the IN pipe.
    fn drain(&mut self) {
        for _ in 0..8 {
            match self.link.bulk_in(self.in_max, DRAIN_WAIT) {
                Ok(b) if !b.is_empty() => {}
                _ => break,
            }
        }
    }

    fn capabilities(&self, usb488: bool) -> Caps {
        match self.request(GET_CAPABILITIES, 0, 0x18) {
            // Byte 14: the USB488 interface capabilities (bit 2 488.2, bit 1 REN_CONTROL / GO_TO_LOCAL /
            // LOCAL_LOCKOUT, bit 0 TRIGGER). Only for an interface that is USB488 at all.
            // Byte 5: the USBTMC device capabilities (bit 0 TermChar).
            Ok(r) if r.len() >= 16 && usb488 => {
                Caps { usb488: true, trigger: r[14] & 0x01 != 0, ren: r[14] & 0x02 != 0, term_char: r[5] & 0x01 != 0 }
            }
            Ok(r) if !usb488 => Caps { usb488: false, trigger: false, ren: false, term_char: r.len() > 5 && r[5] & 0x01 != 0 },
            // Required by the class, but an instrument that fumbles it can still carry messages;
            // offered only what cannot be claimed without its word.
            _ => Caps { usb488, trigger: false, ren: false, term_char: false },
        }
    }

    /// The clearing of the bulk halts the class asks for after an abort or a clear. Bulk-OUT is always cleared (the
    /// device halts it, and the host must clear it). Bulk-IN is not cleared on macOS, where clearing resets the host's
    /// data toggle while some devices keep theirs; a pipe that really is halted shows as a stall and is cleared where
    /// it is found.
    fn unhalt(&mut self, out: bool, inn: bool) {
        if out {
            self.link.clear_halt(true);
        }
        if inn && !cfg!(target_os = "macos") {
            self.link.clear_halt(false);
        }
    }

    fn bulk_out(&mut self, msg: Vec<u8>, wait: Duration) -> Result<(), Error> {
        match self.link.bulk_out(&msg, wait) {
            Err(LinkError::Stall) if cfg!(target_os = "macos") => {
                self.link.clear_halt(true);
                Err(LinkError::Stall.into_error("bulk OUT"))
            }
            r => r.map_err(|e| e.into_error("bulk OUT")),
        }
    }

    fn header(msg_id: u8, tag: u8, size: u32, attr: u8, term: u8) -> Vec<u8> {
        let mut h = vec![0u8; HEADER];
        h[0] = msg_id;
        h[1] = tag;
        h[2] = !tag;
        h[4..8].copy_from_slice(&size.to_le_bytes());
        h[8] = attr;
        h[9] = term;
        h
    }

    /// INITIATE_ABORT_BULK_OUT for `tag`, then wait for it to finish, then clear the halt.
    fn abort_out(&mut self, tag: u8) {
        let ok = self
            .control_in(CLASS_IN_ENDPOINT, INITIATE_ABORT_BULK_OUT, tag as u16, self.out_addr as u16, 2)
            .is_ok_and(|r| r.first() == Some(&STATUS_SUCCESS));
        if ok {
            let end = Instant::now() + ABORT_TIMEOUT;
            while Instant::now() < end {
                match self.control_in(CLASS_IN_ENDPOINT, CHECK_ABORT_BULK_OUT_STATUS, 0, self.out_addr as u16, 8) {
                    Ok(r) if r.first() == Some(&STATUS_PENDING) => std::thread::sleep(Duration::from_millis(50)),
                    _ => break,
                }
            }
        }
        self.unhalt(true, false);
    }

    /// INITIATE_ABORT_BULK_IN for `tag`: the device drops the answer it owes, and what it already
    /// sent is read away, so the next read does not take it for its own.
    fn abort_in(&mut self, tag: u8) {
        let ok = self
            .control_in(CLASS_IN_ENDPOINT, INITIATE_ABORT_BULK_IN, tag as u16, self.in_addr as u16, 2)
            .is_ok_and(|r| r.first() == Some(&STATUS_SUCCESS));
        if ok {
            let end = Instant::now() + ABORT_TIMEOUT;
            while Instant::now() < end {
                self.drain();
                match self.control_in(CLASS_IN_ENDPOINT, CHECK_ABORT_BULK_IN_STATUS, 0, self.in_addr as u16, 8) {
                    Ok(r) if r.first() == Some(&STATUS_PENDING) => std::thread::sleep(Duration::from_millis(50)),
                    _ => break,
                }
            }
        }
        self.drain();
    }
}

/// The deadline for an operation given a timeout in ms; `backend::TMO_INFINITE` (VI_TMO_INFINITE,
/// as the request carries it) is none.
fn deadline(tmo_ms: u32) -> Option<Instant> {
    (tmo_ms != crate::backend::TMO_INFINITE).then(|| Instant::now() + Duration::from_millis(tmo_ms.max(1) as u64))
}

fn left(dl: Option<Instant>) -> Duration {
    match dl {
        // WinUSB takes 0 as "no timeout"; the least real wait is a millisecond.
        Some(d) => d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)),
        None => Duration::from_secs(24 * 3600),
    }
}

fn expired(dl: Option<Instant>) -> bool {
    dl.is_some_and(|d| Instant::now() >= d)
}

/// Add one answer's `data` to the read so far, `out`, stopping at the first termination character
/// or at `count`; what is left over is kept in `ahead` for the next read. `end`: the message ended with
/// this answer; `on_term`: the device says it stopped on the character. Gives how the read ends,
/// or None to ask for more.
fn take_into(out: &mut Vec<u8>, mut data: Vec<u8>, end: bool, on_term: bool, count: usize, term: Option<u8>, ahead: &mut impl Ahead) -> Option<ReadEnd> {
    let room = count - out.len();
    let cut = term.and_then(|t| data.iter().take(room).position(|&b| b == t)).map(|i| i + 1);
    let stop = cut.unwrap_or(room).min(data.len());
    let rest = data.split_off(stop);
    out.extend_from_slice(&data);
    let all = rest.is_empty();
    if !all {
        ahead.keep(rest, end);
    }
    // END first (VPP-4.3 RULE 6.1.1), then the termination character: stopping there is what
    // VISA asks, and asking again would wait for an answer a device need not send.
    if end && all {
        Some(ReadEnd::End)
    } else if cut.is_some() || on_term && all {
        Some(ReadEnd::TermChar)
    } else if out.len() >= count {
        Some(ReadEnd::MaxCount)
    } else {
        None
    }
}

/// Where `take_into` keeps what a read did not take.
trait Ahead {
    fn keep(&mut self, data: Vec<u8>, end: bool);
}

impl Ahead for Pipe {
    fn keep(&mut self, data: Vec<u8>, end: bool) {
        self.ahead = data;
        self.ahead_end = end;
    }
}

impl Pipe {
    fn forget_ahead(&mut self) {
        self.ahead.clear();
        self.ahead_end = false;
    }
}

/// One operation on a held instrument.
pub struct Ops<'a> {
    t: &'a Tmc,
    p: MutexGuard<'a, Pipe>,
}

impl Ops<'_> {
    /// One DEV_DEP_MSG_OUT carrying all of `data`, EOM set if `eom`. The class allows a message in several, but some
    /// instruments fail on a command split across messages; the whole write is already here, and the USB transfer is
    /// split into packets below anyway. Bytes sent; on failure none are counted, as the device cannot say how many it
    /// took.
    pub fn write(&mut self, data: &[u8], eom: bool, tmo_ms: u32) -> Result<usize, (usize, Error)> {
        if data.is_empty() && !eom {
            return Ok(0);
        }
        // A new message: the instrument drops what it had not sent of the last (IEEE 488.2), and so
        // does this.
        self.p.forget_ahead();
        let tag = self.p.next_tag();
        let mut msg = Pipe::header(DEV_DEP_MSG_OUT, tag, data.len() as u32, if eom { EOM } else { 0 }, 0);
        msg.extend_from_slice(data);
        msg.resize(msg.len().div_ceil(4) * 4, 0); // every transfer ends on a four-byte boundary
        match self.p.bulk_out(msg, left(deadline(tmo_ms))) {
            Ok(()) => Ok(data.len()),
            Err(e) => {
                if !matches!(e, Error::Gone) {
                    self.p.abort_out(tag);
                }
                Err((0, e))
            }
        }
    }

    /// Read up to `count` bytes: to the end of the message (EOM), the termination character, the
    /// count, or the timeout -- then with what did arrive. A timed-out request is aborted at the
    /// device, so its late answer is not the next read's.
    pub fn read(&mut self, count: usize, term: Option<u8>, tmo_ms: u32) -> Result<ReadResult, Error> {
        let dl = deadline(tmo_ms);
        let mut out = Vec::new();
        // Asked of the device only if it says it can; else its answers are cut here.
        let ask_term = term.filter(|_| self.t.caps.term_char);
        if !self.p.ahead.is_empty() {
            let (data, end) = (std::mem::take(&mut self.p.ahead), self.p.ahead_end);
            if let Some(e) = take_into(&mut out, data, end, false, count, term, &mut *self.p) {
                return Ok(ReadResult { data: out, end: e });
            }
        }
        loop {
            let want = count - out.len();
            if want == 0 {
                return Ok(ReadResult { data: out, end: ReadEnd::MaxCount });
            }
            if expired(dl) {
                return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
            }
            let ask = want.min(CHUNK);
            let tag = self.p.next_tag();
            let req = Pipe::header(REQUEST_DEV_DEP_MSG_IN, tag, ask as u32, if ask_term.is_some() { TERM_CHAR } else { 0 }, ask_term.unwrap_or(0));
            if let Err(e) = self.p.bulk_out(req, left(dl)) {
                if matches!(e, Error::Timeout) {
                    self.p.abort_out(tag);
                    return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
                }
                return Err(e);
            }
            let (data, attr) = match self.answer(tag, ask, dl) {
                Ok(x) => x,
                Err(Error::Timeout) => {
                    self.p.abort_in(tag);
                    return Ok(ReadResult { data: out, end: ReadEnd::Timeout });
                }
                Err(e) => return Err(e),
            };
            let empty = data.is_empty();
            let on_term = attr & TERM_CHAR != 0 && ask_term.is_some();
            if let Some(e) = take_into(&mut out, data, attr & EOM != 0, on_term, count, term, &mut *self.p) {
                return Ok(ReadResult { data: out, end: e });
            }
            if empty {
                // Nothing yet, and no end: ask again until the timeout.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// The DEV_DEP_MSG_IN answering request `tag`: its data and bmTransferAttributes. The first
    /// transfer carries the header; a TransferSize longer than it brought is read on.
    fn answer(&mut self, tag: u8, ask: usize, dl: Option<Instant>) -> Result<(Vec<u8>, u8), Error> {
        let in_max = self.p.in_max;
        let room = (ask + HEADER).div_ceil(in_max) * in_max;
        // What is in the pipe may not be this request's answer: the tail of an earlier one (a session that was killed,
        // a read that gave up), or a fragment. Such a transfer is skipped and the next one read, until the deadline;
        // nothing is cancelled to get there (on macOS a cancelled transfer aborts the whole pipe).
        let got = loop {
            let b = self.p.link.bulk_in(room, left(dl)).map_err(|e| e.into_error("bulk IN"))?;
            if b.len() >= HEADER && b[0] == DEV_DEP_MSG_IN && b[1] == tag && b[2] == !tag {
                break b;
            }
            if expired(dl) {
                return Err(Error::Timeout);
            }
        };
        // No more than was asked for (USBTMC 3.3: TransferSize is at most the request's): a size the
        // instrument made up, up to 4 GiB, is not what the daemon allocates and waits for. What it
        // sends past the request stays in the pipe and is let go as the next request looks for its answer.
        let size = (u32::from_le_bytes(got[4..8].try_into().unwrap()) as usize).min(ask);
        let attr = got[8];
        let mut data = got[HEADER..].to_vec();
        while data.len() < size {
            let more = (size - data.len()).div_ceil(in_max) * in_max;
            let b = self.p.link.bulk_in(more, left(dl)).map_err(|e| e.into_error("bulk IN"))?;
            if b.is_empty() {
                break;
            }
            data.extend_from_slice(&b);
        }
        data.truncate(size); // the rest is padding
        Ok((data, attr))
    }

    /// INITIATE_CLEAR, polled with CHECK_CLEAR_STATUS until done; then both endpoints' halts
    /// cleared and the IN pipe drained, since a clear abandons whatever was in flight.
    pub fn clear(&mut self) -> Result<(), Error> {
        self.p.request(INITIATE_CLEAR, 0, 1)?;
        let end = Instant::now() + ABORT_TIMEOUT;
        loop {
            let r = self.p.control_in(CLASS_IN_INTERFACE, CHECK_CLEAR_STATUS, 0, self.p.iface_no as u16, 2)?;
            match r.first() {
                Some(&STATUS_PENDING) if Instant::now() < end => {
                    // bmClear bit 0: the device wants its IN pipe read to finish the clear.
                    if r.get(1).is_some_and(|b| b & 1 != 0) {
                        self.p.drain();
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Some(&STATUS_SUCCESS) => break,
                Some(&STATUS_PENDING) => return Err(Error::Timeout),
                other => return Err(Error::Io(format!("the clear failed ({other:?})"))),
            }
        }
        self.p.unhalt(true, true);
        self.p.drain();
        self.p.forget_ahead();
        Ok(())
    }

    /// USB488's TRIGGER message: GET, for an instrument that says it takes one.
    pub fn trigger(&mut self) -> Result<(), Error> {
        if !self.t.caps.trigger {
            return Err(Error::Status(ST_UNSUPPORTED));
        }
        let tag = self.p.next_tag();
        self.p.bulk_out(Pipe::header(TRIGGER, tag, 0, 0, 0), CONTROL_TIMEOUT)
    }

    /// READ_STATUS_BYTE. With an interrupt endpoint the byte comes there, as 0x80 | bTag, and the control answer's
    /// third byte is reserved: no answer there is an error, never a status byte of 0.
    pub fn read_stb(&mut self, tmo_ms: u32) -> Result<u8, Error> {
        if !self.t.caps.usb488 {
            return Err(Error::Status(ST_UNSUPPORTED));
        }
        if let Some(r) = self.p.link.read_stb(Duration::from_millis(tmo_ms.clamp(100, 5000) as u64)) {
            return r.map_err(|e| match e {
                LinkError::Stall => Error::Status(ST_UNSUPPORTED),
                e => e.into_error("the status byte"),
            });
        }
        self.p.stb_tag = if self.p.stb_tag >= 127 { 2 } else { self.p.stb_tag + 1 };
        let tag = self.p.stb_tag;
        let r = self.p.request(READ_STATUS_BYTE, tag as u16, 3)?;
        if r.get(1) != Some(&tag) {
            return Err(Error::Io("the status byte answer is for another request".into()));
        }
        let Some(intr) = &self.t.intr else {
            return r.get(2).copied().ok_or_else(|| Error::Io("the status byte answer is short".into()));
        };
        let dl = Instant::now() + Duration::from_millis(tmo_ms.clamp(100, 5000) as u64);
        let mut st = intr.st.lock().unwrap();
        loop {
            if let Some(stb) = st.stb.remove(&tag) {
                return Ok(stb);
            }
            if st.dead {
                return Err(Error::Gone);
            }
            let w = dl.saturating_duration_since(Instant::now());
            if w.is_zero() {
                return Err(Error::Io("it accepted the status request and said nothing on its interrupt endpoint".into()));
            }
            st = intr.cv.wait_timeout(st, w).unwrap().0;
        }
    }

    fn remote_local(&mut self, request: u8, value: u16) -> Result<(), Error> {
        if !self.t.caps.ren {
            return Err(Error::Status(ST_UNSUPPORTED));
        }
        self.p.request(request, value, 1).map(|_| ())
    }

    /// REN_CONTROL: assert or release remote enable.
    pub fn ren(&mut self, on: bool) -> Result<(), Error> {
        self.remote_local(REN_CONTROL, on as u16)
    }

    /// GO_TO_LOCAL.
    pub fn local(&mut self) -> Result<(), Error> {
        self.remote_local(GO_TO_LOCAL, 0)
    }

    /// viUsbControlIn / viUsbControlOut: a control transfer the caller describes whole, by its setup
    /// packet's fields; the direction is bit 7 of `request_type`. What an IN transfer brought.
    pub fn control(&mut self, request_type: u8, request: u8, value: u16, index: u16, length: u16, data: &[u8]) -> Result<Vec<u8>, Error> {
        // The reserved type (3) and recipients (4..31) are a bad request, not the device's to refuse.
        if (request_type >> 5) & 3 == 3 || request_type & 0x1F > 3 {
            return Err(Error::Status(crate::proto::ST_BAD_REQUEST));
        }
        // A stall here is the device refusing what the caller asked for: an I/O error to the caller,
        // not "unsupported" as for the class requests this runtime makes itself.
        let what = format!("control request {request:#04x}");
        if request_type & 0x80 != 0 {
            self.p.link.control_in(request_type, request, value, index, length, CONTROL_TIMEOUT).map_err(|e| e.into_error(&what))
        } else {
            self.p.link.control_out(request_type, request, value, index, data, CONTROL_TIMEOUT).map(|_| Vec::new()).map_err(|e| e.into_error(&what))
        }
    }

    /// LOCAL_LOCKOUT.
    pub fn llo(&mut self) -> Result<(), Error> {
        self.remote_local(LOCAL_LOCKOUT, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_stay_with_their_instruments() {
        let (a, b, c) = ("F4EC:1103:A".to_string(), "0957:2C07:B".to_string(), "0957:2C07:C".to_string());
        let mut s = Slots::default();
        s.update(&[a.clone(), b.clone()]);
        assert_eq!((s.get(0).unwrap(), s.get(1).unwrap()), (a.clone(), b.clone()));
        s.update(std::slice::from_ref(&b));
        assert!(matches!(s.get(0), Err(Error::Gone)), "unplugged: its number says so");
        assert_eq!(s.get(1).unwrap(), b);
        s.update(&[c.clone(), a.clone(), b.clone()]);
        assert_eq!((s.get(0).unwrap(), s.get(2).unwrap()), (a, c), "back with its number; the new one next");
        assert!(matches!(s.get(3), Err(Error::NoBoard)));
    }

    #[derive(Default)]
    struct Kept(Vec<u8>, bool);
    impl Ahead for Kept {
        fn keep(&mut self, data: Vec<u8>, end: bool) {
            *self = Kept(data, end);
        }
    }

    #[test]
    fn a_read_cut_here_at_the_termination_character_keeps_the_rest_for_the_next() {
        let mut k = Kept::default();
        let mut out = Vec::new();
        // A device without TermChar sends the whole message: two lines, then EOM.
        assert_eq!(take_into(&mut out, b"1,2\n3,4\n".to_vec(), true, false, 100, Some(b'\n'), &mut k), Some(ReadEnd::TermChar));
        assert_eq!((out.as_slice(), k.0.as_slice(), k.1), (&b"1,2\n"[..], &b"3,4\n"[..], true));
        // The next read: the kept part, ending the message.
        let mut out = Vec::new();
        let (d, e) = (std::mem::take(&mut k.0), k.1);
        assert_eq!(take_into(&mut out, d, e, false, 100, Some(b'\n'), &mut k), Some(ReadEnd::End));
        assert_eq!((out.as_slice(), k.0.is_empty()), (&b"3,4\n"[..], true));
    }

    #[test]
    fn a_read_ends_on_the_count_the_message_or_neither() {
        let mut k = Kept::default();
        let mut out = Vec::new();
        assert_eq!(take_into(&mut out, b"abcdef".to_vec(), true, false, 4, None, &mut k), Some(ReadEnd::MaxCount));
        assert_eq!((out.as_slice(), k.0.as_slice(), k.1), (&b"abcd"[..], &b"ef"[..], true));
        let mut out = Vec::new();
        let mut k = Kept::default();
        assert_eq!(take_into(&mut out, b"abc".to_vec(), false, false, 10, Some(b'\n'), &mut k), None, "no end yet: ask again");
        assert_eq!(take_into(&mut out, b"de\n".to_vec(), false, true, 10, Some(b'\n'), &mut k), Some(ReadEnd::TermChar));
        assert_eq!(out, b"abcde\n");
        // The character and the end together: END is told (VPP-4.3 RULE 6.1.1).
        let mut out = Vec::new();
        assert_eq!(take_into(&mut out, b"x\n".to_vec(), true, true, 10, Some(b'\n'), &mut k), Some(ReadEnd::End));
        // A character past the count is not the read's.
        let mut out = Vec::new();
        assert_eq!(take_into(&mut out, b"abc\n".to_vec(), false, false, 2, Some(b'\n'), &mut k), Some(ReadEnd::MaxCount));
        assert_eq!((out.as_slice(), k.0.as_slice()), (&b"ab"[..], &b"c\n"[..]));
    }

    #[test]
    fn endpoints_come_from_the_configuration_descriptor() {
        // A USB488 instrument's configuration descriptor, as GET_DESCRIPTOR answers it.
        let sdg = [
            0x09, 0x02, 0x27, 0x00, 0x01, 0x01, 0x00, 0xE0, 0x01, // configuration
            0x09, 0x04, 0x00, 0x00, 0x03, 0xFE, 0x03, 0x01, 0x00, // interface 0: USBTMC, USB488
            0x07, 0x05, 0x81, 0x02, 0x40, 0x00, 0x00, // bulk IN 0x81, 64
            0x07, 0x05, 0x01, 0x02, 0x40, 0x00, 0x00, // bulk OUT 0x01
            0x07, 0x05, 0x82, 0x03, 0x40, 0x00, 0x00, // interrupt IN 0x82, 64
        ];
        assert_eq!(endpoints_in(&sdg, 0), Some(Endpoints { out_addr: 0x01, in_addr: 0x81, in_max: 64, intr: Some((0x82, 64)) }));
        assert_eq!(endpoints_in(&sdg, 1), None, "no such interface");
        assert_eq!(endpoints_in(&sdg[..25], 0), None, "cut short: no bulk OUT");
        // Another interface's endpoints are not this one's.
        let mut two = sdg[..18].to_vec();
        two.extend([0x09, 0x04, 0x01, 0x00, 0x02, 0xFF, 0x00, 0x00, 0x00, 0x07, 0x05, 0x83, 0x02, 0x40, 0x00, 0x00]);
        two.extend(&sdg[18..]);
        assert_eq!(endpoints_in(&two, 0), None, "interface 1's endpoint is not interface 0's");
    }

    #[test]
    fn a_header_is_as_the_class_lays_it_out() {
        let h = Pipe::header(REQUEST_DEV_DEP_MSG_IN, 7, 1000, TERM_CHAR, b'\n');
        assert_eq!(h, [2, 7, !7, 0, 0xE8, 0x03, 0, 0, TERM_CHAR, b'\n', 0, 0]);
        assert_eq!(key(0xF4EC, 0x1103, "SDG1"), "F4EC:1103:SDG1");
    }
}
