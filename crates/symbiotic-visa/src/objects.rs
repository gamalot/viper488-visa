//! The objects behind a ViSession: sessions, find lists and events, by number (`insert`, `get`).

use super::*;

/// A service-request handler as VISA gives it to viInstallHandler.
pub(crate) type Handler = unsafe extern "system" fn(u32, u32, u32, *const c_void) -> i32;

pub(crate) const EVENT_QUEUE_MAX: usize = 50;

/// One event, as viWaitOnEvent or a handler gets its context: a service request, the completion of
/// an asynchronous operation (with what it did), or an operation's error (an exception).
#[derive(Clone)]
pub(crate) struct Ev {
    pub(crate) ty: u32,
    pub(crate) status: ViStatus,
    pub(crate) job: u32,
    pub(crate) ret: u64,
    pub(crate) buf: usize,
    pub(crate) oper: String,
}

impl Ev {
    pub(crate) fn srq() -> Ev {
        Ev { ty: VI_EVENT_SERVICE_REQ, status: VI_SUCCESS, job: 0, ret: 0, buf: 0, oper: String::new() }
    }
}

#[derive(Default)]
pub(crate) struct EvState {
    pub(crate) queue: VecDeque<Ev>,
    /// The mechanisms enabled for each event type: (queue, handler).
    pub(crate) srq: (bool, bool),
    pub(crate) io: (bool, bool),
    /// Exceptions: handlers only (VPP-4.3 3.7.2: no queue, no suspended handler).
    pub(crate) exc: (bool, bool),
    pub(crate) handlers: Vec<(u32, Handler, usize)>,
    /// Set while a worker thread is watching for this session's service requests; setting it stops
    /// the worker.
    pub(crate) stop: Option<Arc<AtomicBool>>,
    /// The threads in a handler of this session now, once for each call: viUninstallHandler and
    /// viClose wait for the others' calls to end (events.rs, `settle`).
    pub(crate) calling: Vec<std::thread::ThreadId>,
}

impl EvState {
    pub(crate) fn mech(&mut self, ty: u32) -> &mut (bool, bool) {
        match ty {
            VI_EVENT_IO_COMPLETION => &mut self.io,
            VI_EVENT_EXCEPTION => &mut self.exc,
            _ => &mut self.srq,
        }
    }
}

#[derive(Default)]
pub(crate) struct Events {
    pub(crate) st: Mutex<EvState>,
    pub(crate) cv: Condvar,
    /// Told when a handler's call ends.
    pub(crate) returned: Condvar,
    /// Once viEnableEvent has been called, VI_ATTR_MAX_QUEUE_LENGTH is read-only (VPP-4.3 RULE 3.2.6).
    pub(crate) enabled_once: AtomicBool,
}

/// What a USBTMC session knows of its instrument, from its name and its descriptors.
#[derive(Clone)]
pub(crate) struct UsbSess {
    pub(crate) name: resource::UsbName,
    pub(crate) iface: u8,
    pub(crate) usb488: bool,
    pub(crate) manufacturer: String,
    pub(crate) product: String,
}

/// A serial session's own settings: how its reads and writes end, and whether the instrument
/// takes IEEE 488.2 strings (`*STB?`, `*TRG`, `*CLS`). The port's settings are the daemon's.
#[derive(Clone, Copy)]
pub(crate) struct AsrlSess {
    pub(crate) end_in: u16,
    pub(crate) end_out: u16,
    pub(crate) break_len: u16,
    pub(crate) io_prot: u16,
}

impl Default for AsrlSess {
    fn default() -> AsrlSess {
        AsrlSess { end_in: VI_ASRL_END_TERMCHAR, end_out: VI_ASRL_END_NONE, break_len: 250, io_prot: VI_PROT_NORMAL }
    }
}

pub(crate) struct Instr {
    /// A `GPIBn::INTFC` session: the board itself. Its `pad` is backend::RAW.
    pub(crate) intfc: bool,
    /// A USBTMC instrument: `board` is the daemon's number for it (not a GPIB board), `pad` is
    /// unused, and `adapter` is its key (VID:PID:serial).
    pub(crate) usb: Option<UsbSess>,
    /// A serial port: `board` is its COM number, `pad` is unused, `adapter` is `COM<n>`.
    pub(crate) asrl: Option<AsrlSess>,
    /// The adapter the session was opened on (its key). A board number is only stable while one
    /// daemon runs; a new connection checks the number still means this adapter.
    pub(crate) adapter: String,
    pub(crate) ev: Arc<Events>,
    pub(crate) rm: u32,
    pub(crate) conn: Conn,
    pub(crate) board: u16,
    pub(crate) pad: u8,
    /// A GPIB instrument's secondary address, 0..=30, or backend::NO_SAD.
    pub(crate) sad: u8,
    pub(crate) name: String,
    pub(crate) tmo_ms: u32,
    pub(crate) termchar: u8,
    pub(crate) termchar_en: bool,
    pub(crate) send_end: bool,
    pub(crate) readdr: bool,
    pub(crate) max_queue: u32,
    pub(crate) user_data: u64,
    /// VI_ATTR_DMA_ALLOW_EN: a hint VPP-4.3 lets be ignored (it "affects performance and not
    /// functionality"); kept, not acted on.
    pub(crate) dma: bool,
    /// VI_ATTR_SUPPRESS_END_EN (GPIB and USB): END does not end a read; viRead reads on.
    pub(crate) suppress_end: bool,
    /// VI_ATTR_DEV_STATUS_BYTE (INTFC): the board's own status byte, were it ever polled as a
    /// device; it never is -- every adapter here stays the controller -- so it is only kept.
    pub(crate) dev_status: u8,
    /// VI_ATTR_GPIB_ADDR_STATE (INTFC): what the commands sent through this session addressed the
    /// board as (its own address is 0): VI_GPIB_UNADDRESSED, _TALKER or _LISTENER.
    pub(crate) addr_state: i16,
    /// VI_ATTR_USB_MAX_INTR_SIZE (USB): the interrupt IN packet size unless set.
    pub(crate) usb_intr_max: u16,
    /// The session holds a lock on its connection (viLock, or viOpen's lock): a new connection would
    /// not have it, and `ensure` says so once rather than carry on as if it did.
    pub(crate) locked: Arc<AtomicBool>,
}

impl Instr {
    /// A new session on an instrument, through `c`: VISA's defaults for its attributes (2 s, `\n`
    /// as the termination character but not enabled, END with a write, readdressing). What only
    /// some sessions are -- an interface, a USBTMC instrument, a serial port -- the caller sets.
    pub(crate) fn new(rm: u32, c: Client, locked: bool, adapter: String, name: String, board: u16, pad: u8, sad: u8) -> Instr {
        Instr {
            intfc: false, usb: None, asrl: None, adapter, rm, conn: Arc::new(Mutex::new(Some(c))), board, pad, sad, name,
            tmo_ms: 2000, termchar: b'\n', termchar_en: false, send_end: true, readdr: true,
            max_queue: 50, user_data: 0, ev: Arc::new(Events::default()),
            dma: false, suppress_end: false, dev_status: 0, addr_state: VI_GPIB_UNADDRESSED, usb_intr_max: 0,
            locked: Arc::new(AtomicBool::new(locked)),
        }
    }
}

pub(crate) enum Object {
    Rm,
    /// The context of an event, as viWaitOnEvent or a handler gets it.
    Event(Ev),
    Find { rm: u32, names: Mutex<std::vec::IntoIter<String>> },
    Instr(Mutex<Instr>),
    /// A `TCPIP` resource, connected from this process (lan.rs).
    Lan(lan::LanSess),
}

/// Hand a LAN session to lan.rs: `on_lan!(vi, |l| expr)` returns `expr` when `vi` is one.
macro_rules! on_lan {
    ($vi:expr, |$l:ident| $body:expr) => {
        if let Some(o) = get($vi) {
            if let Object::Lan($l) = &*o {
                return $body;
            }
        }
    };
}

pub(crate) static NEXT: AtomicU32 = AtomicU32::new(1);
pub(crate) static REGISTRY: OnceLock<Mutex<HashMap<u32, Arc<Object>>>> = OnceLock::new();

pub(crate) fn registry() -> &'static Mutex<HashMap<u32, Arc<Object>>> {
    REGISTRY.get_or_init(Default::default)
}

pub(crate) fn insert(o: Object) -> u32 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    registry().lock().unwrap().insert(id, Arc::new(o));
    id
}

pub(crate) fn get(id: u32) -> Option<Arc<Object>> {
    registry().lock().unwrap().get(&id).cloned()
}
