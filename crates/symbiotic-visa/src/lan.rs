//! `TCPIP` sessions: VXI-11, HiSLIP and SOCKET, connected from this process (no daemon; see `symbiotic_core::lan`).
//! Each export in lib.rs hands a LAN session to the function of the same name here.

use crate::consts::*;
use crate::{put, put_str};
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use symbiotic_core::adapter::ReadEnd;
use symbiotic_core::lan::{self, Error, LockState, Ren, Session, Target};

/// How long a connection may take when viOpen's timeout is 0, which is what pyvisa passes by default.
const DEFAULT_CONNECT: Duration = Duration::from_secs(5);

pub struct Attrs {
    tmo_ms: u32,
    termchar: u8,
    termchar_en: bool,
    send_end: bool,
    suppress_end: bool,
    max_queue: u32,
    user_data: u64,
    /// This session's locks on the instrument: how deep the exclusive one is nested, and the
    /// shared one's key and depth. The instrument is locked once, whatever the depth.
    exclusive: u32,
    shared: Option<(String, u32)>,
    /// VI_ATTR_DMA_ALLOW_EN: a hint, kept (VPP-4.3: it affects performance, not function).
    dma: bool,
    /// VI_ATTR_IO_PROT: VI_PROT_NORMAL, or on a SOCKET VI_PROT_4882_STRS -- then viReadSTB,
    /// viAssertTrigger and viClear send *STB?, *TRG and *CLS, as on a serial port.
    io_prot: u16,
}

pub struct LanSess {
    pub rm: u32,
    /// Service requests, as for a GPIB session (lib.rs); fed by the session's own watch.
    pub ev: Arc<crate::Events>,
    target: Target,
    conn: Mutex<Session>,
    attrs: Mutex<Attrs>,
    /// The shared alias the session was opened by, which is then its VI_ATTR_RSRC_NAME.
    pub alias: Mutex<Option<String>>,
}

pub fn map_err(e: &Error) -> i32 {
    crate::trace(|| format!("lan error: {e}"));
    match e {
        Error::NotFound(_) => VI_ERROR_RSRC_NFOUND,
        Error::Timeout => VI_ERROR_TMO,
        Error::Lost(_) => VI_ERROR_CONN_LOST,
        Error::Io(_) => VI_ERROR_IO,
        Error::Locked => VI_ERROR_RSRC_LOCKED,
        Error::NotLocked => VI_ERROR_SESN_NLOCKED,
        Error::Unsupported => VI_ERROR_NSUP_OPER,
        Error::Param => VI_ERROR_INV_PARAMETER,
        Error::Abort => VI_ERROR_ABORT,
    }
}

fn duration(ms: u32) -> Option<Duration> {
    (ms != VI_TMO_INFINITE).then(|| Duration::from_millis(ms as u64))
}

impl LanSess {
    /// Its VI_ATTR_RSRC_NAME: the shared alias it was opened by, else its resource name.
    pub fn name(&self) -> String {
        self.alias.lock().map(|a| a.clone()).ok().flatten().unwrap_or_else(|| self.target.name())
    }

    fn tmo(&self) -> Option<Duration> {
        duration(self.attrs.lock().unwrap().tmo_ms)
    }

    fn with<T>(&self, f: impl FnOnce(&mut Session) -> Result<T, Error>) -> Result<T, i32> {
        f(&mut self.conn.lock().unwrap()).map_err(|e| map_err(&e))
    }
}

fn status(r: Result<(), i32>) -> i32 {
    r.err().unwrap_or(VI_SUCCESS)
}

/// A key for a shared lock the caller did not name one for. Unique in this process; another
/// process that wants to share it is given it by the caller, as VISA intends.
fn make_key() -> String {
    static N: AtomicU32 = AtomicU32::new(1);
    format!("symbiotic-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// viOpen of a `TCPIP` name. `access_mode` has been checked by the caller.
pub fn open(rm: u32, t: Target, access_mode: u32, open_timeout: u32) -> Result<LanSess, i32> {
    let within = match open_timeout {
        0 | VI_TMO_INFINITE => DEFAULT_CONNECT,
        t => Duration::from_millis(t as u64),
    };
    let lock_wait = duration(open_timeout);
    let exclusive = access_mode & VI_EXCLUSIVE_LOCK != 0;
    let mut conn = Session::open(&t, within, exclusive.then_some(lock_wait)).map_err(|e| map_err(&e))?;
    let mut shared = None;
    if access_mode & VI_SHARED_LOCK != 0 {
        let key = make_key();
        conn.lock(Some(&key), lock_wait).map_err(|e| map_err(&e))?;
        shared = Some((key, 1));
    }
    let socket = t.class() == "SOCKET";
    let attrs = Attrs {
        tmo_ms: 2000,
        termchar: b'\n',
        termchar_en: false,
        send_end: true,
        // A stream has no END; NI-VISA (and pyvisa-py) default to waiting for a term char or the count.
        suppress_end: socket,
        max_queue: 50,
        user_data: 0,
        exclusive: exclusive as u32,
        shared,
        dma: false,
        io_prot: VI_PROT_NORMAL,
    };
    Ok(LanSess { rm, ev: Arc::default(), target: t, conn: Mutex::new(conn), attrs: Mutex::new(attrs), alias: Mutex::new(None) })
}

pub unsafe fn write(s: &LanSess, data: &[u8], ret: *mut u32) -> i32 {
    let (end, tmo) = {
        let a = s.attrs.lock().unwrap();
        (a.send_end, duration(a.tmo_ms))
    };
    match s.conn.lock().unwrap().write(data, end, tmo) {
        Ok(n) => {
            put(ret, n as u32);
            VI_SUCCESS
        }
        Err((n, e)) => {
            put(ret, n as u32);
            map_err(&e)
        }
    }
}

pub unsafe fn read(s: &LanSess, buf: *mut u8, count: u32, ret: *mut u32) -> i32 {
    let (term, suppress, tmo) = {
        let a = s.attrs.lock().unwrap();
        (a.termchar_en.then_some(a.termchar), a.suppress_end, duration(a.tmo_ms))
    };
    match s.with(|c| c.read(count as usize, term, suppress, tmo)) {
        Ok(r) => {
            let n = r.data.len().min(count as usize);
            if n > 0 {
                std::ptr::copy_nonoverlapping(r.data.as_ptr(), buf, n);
            }
            put(ret, n as u32);
            match r.end {
                ReadEnd::End => VI_SUCCESS,
                ReadEnd::TermChar => VI_SUCCESS_TERM_CHAR,
                ReadEnd::MaxCount => VI_SUCCESS_MAX_CNT,
                ReadEnd::Timeout => VI_ERROR_TMO,
            }
        }
        Err(e) => e,
    }
}

/// A SOCKET session in VI_PROT_4882_STRS: IEEE 488.2 strings stand in for the operations a raw
/// stream does not have.
fn strings(s: &LanSess) -> bool {
    matches!(s.target.proto, lan::Proto::Socket { .. }) && s.attrs.lock().unwrap().io_prot == VI_PROT_4882_STRS
}

pub fn clear(s: &LanSess) -> i32 {
    let t = s.tmo();
    if strings(s) {
        // What came in is dropped, and the instrument is told to clear its status.
        return status(s.with(|c| {
            c.clear(t)?;
            c.write(b"*CLS\n", true, t).map(|_| ()).map_err(|(_, e)| e)
        }));
    }
    status(s.with(|c| c.clear(t)))
}

pub fn trigger(s: &LanSess, protocol: u16) -> i32 {
    if protocol != 0 {
        return VI_ERROR_INV_PROT;
    }
    let t = s.tmo();
    if strings(s) {
        return status(s.with(|c| c.write(b"*TRG\n", true, t).map(|_| ()).map_err(|(_, e)| e)));
    }
    status(s.with(|c| c.trigger(t)))
}

pub unsafe fn read_stb(s: &LanSess, stb: *mut u16) -> i32 {
    let t = s.tmo();
    if strings(s) {
        let r = s.with(|c| {
            // Output nobody read would be taken for the answer; the instrument drops it on a new
            // query anyway (IEEE 488.2), as on a serial port (serial.rs, `stb`).
            if let Some(k) = c.socket() {
                k.drop_unread()?;
            }
            c.write(b"*STB?\n", true, t).map_err(|(_, e)| e)?;
            let r = c.read(64, Some(b'\n'), true, t)?;
            if r.end == ReadEnd::Timeout {
                return Err(lan::Error::Timeout);
            }
            symbiotic_core::serial::parse_stb(&r.data)
                .ok_or_else(|| lan::Error::Io(format!("*STB? answered {:?}", String::from_utf8_lossy(&r.data))))
        });
        return match r {
            Ok(b) => {
                put(stb, b as u16);
                VI_SUCCESS
            }
            Err(e) => e,
        };
    }
    match s.with(|c| c.read_stb(t)) {
        Ok(b) => {
            put(stb, b as u16);
            VI_SUCCESS
        }
        Err(e) => e,
    }
}

pub fn ren(s: &LanSess, mode: u16) -> i32 {
    let Some(mode) = Ren::from_u16(mode) else { return VI_ERROR_NSUP_MODE };
    let t = s.tmo();
    match s.with(|c| c.ren(mode, t)) {
        // The protocol has no such mode: the mode is the problem, not the operation.
        Err(VI_ERROR_NSUP_OPER) if !matches!(s.target.proto, lan::Proto::Socket { .. }) => VI_ERROR_NSUP_MODE,
        r => status(r),
    }
}

/// The instrument's lock, taken once however deeply this session nests it.
pub unsafe fn lock(s: &LanSess, lock_type: u32, timeout: u32, requested: Option<&str>, access_key: *mut c_char) -> i32 {
    let wait = duration(timeout);
    match lock_type {
        VI_EXCLUSIVE_LOCK => {
            if s.attrs.lock().unwrap().exclusive > 0 {
                s.attrs.lock().unwrap().exclusive += 1;
                return VI_SUCCESS_NESTED_EXCLUSIVE;
            }
            if let Err(e) = s.with(|c| c.lock(None, wait)) {
                return e;
            }
            s.attrs.lock().unwrap().exclusive = 1;
            VI_SUCCESS
        }
        VI_SHARED_LOCK => {
            if let Some((key, depth)) = &mut s.attrs.lock().unwrap().shared {
                if requested.is_some_and(|r| r != key) {
                    return VI_ERROR_INV_ACCESS_KEY;
                }
                *depth += 1;
                put_str(access_key, key);
                return VI_SUCCESS_NESTED_SHARED;
            }
            let key = requested.map_or_else(make_key, str::to_string);
            if let Err(e) = s.with(|c| c.lock(Some(&key), wait)) {
                return e;
            }
            put_str(access_key, &key);
            s.attrs.lock().unwrap().shared = Some((key, 1));
            VI_SUCCESS
        }
        _ => VI_ERROR_INV_LOCK_TYPE,
    }
}

pub fn unlock(s: &LanSess) -> i32 {
    let mut a = s.attrs.lock().unwrap();
    let last = if a.exclusive > 0 {
        a.exclusive -= 1;
        a.exclusive == 0
    } else if let Some((_, depth)) = &mut a.shared {
        *depth -= 1;
        let last = *depth == 0;
        if last {
            a.shared = None;
        }
        last
    } else {
        return VI_ERROR_SESN_NLOCKED;
    };
    let still = if a.exclusive > 0 {
        VI_SUCCESS_NESTED_EXCLUSIVE
    } else if a.shared.is_some() {
        VI_SUCCESS_NESTED_SHARED
    } else {
        VI_SUCCESS
    };
    drop(a);
    if last {
        if let Err(e) = s.with(|c| c.unlock()) {
            return e;
        }
    }
    still
}

/// Interface clear on a LAN/GPIB gateway's bus.
pub fn ifc(s: &LanSess) -> i32 {
    let t = s.tmo();
    status(s.with(|c| c.ifc(t)))
}

/// Start watching the instrument's service requests (a VXI-11 session opens its interrupt channel).
pub fn watch_srq(s: &LanSess) -> Result<lan::SrqWatch, i32> {
    s.with(|c| c.watch_srq())
}

pub fn unwatch_srq(s: &LanSess) {
    s.conn.lock().unwrap().unwatch_srq();
}

/// viFlush: only a socket keeps received bytes, and only the discarding masks touch them.
pub fn flush(s: &LanSess, mask: u16) -> i32 {
    const VI_READ_BUF_DISCARD: u16 = 0x04;
    const VI_IO_IN_BUF_DISCARD: u16 = 0x40;
    if mask & (VI_READ_BUF_DISCARD | VI_IO_IN_BUF_DISCARD) != 0 {
        s.conn.lock().unwrap().discard_input();
    }
    VI_SUCCESS
}

pub unsafe fn get_attribute(s: &LanSess, attr: u32, out: *mut c_void) -> i32 {
    let t = &s.target;
    let instr = t.class() == "INSTR";
    if attr == VI_ATTR_RSRC_LOCK_STATE {
        // The instrument's state if the protocol can ask (HiSLIP, RULE 3.6.6), else this session's.
        let tmo = s.tmo();
        let asked = s.conn.lock().unwrap().lock_state(tmo).ok().flatten();
        let state = asked.unwrap_or_else(|| {
            let a = s.attrs.lock().unwrap();
            if a.exclusive > 0 {
                LockState::Exclusive
            } else if a.shared.is_some() {
                LockState::Shared
            } else {
                LockState::None
            }
        });
        let v = match state {
            LockState::None => 0u32,
            LockState::Exclusive => VI_EXCLUSIVE_LOCK,
            LockState::Shared => VI_SHARED_LOCK,
        };
        put(out as *mut u32, v);
        return VI_SUCCESS;
    }
    // An instrument behind a LAN/GPIB gateway has a GPIB address, and a bus whose REN can be asked.
    let gpib = t.gpib();
    match (attr, &gpib) {
        (VI_ATTR_GPIB_PRIMARY_ADDR, Some(g)) => return put(out as *mut u16, g.pad as u16).ok_(),
        (VI_ATTR_GPIB_SECONDARY_ADDR, Some(g)) => return put(out as *mut u16, g.sad.map_or(VI_NO_SEC_ADDR, u16::from)).ok_(),
        (VI_ATTR_GPIB_REN_STATE, Some(_)) => {
            let tmo = s.tmo();
            return match s.with(|c| c.ren_state(tmo)) {
                Ok(Some(on)) => put(out as *mut i16, on as i16).ok_(),
                Ok(None) => VI_ERROR_NSUP_ATTR,
                Err(e) => e,
            };
        }
        _ => {}
    }
    // HiSLIP's own, and the TCP options.
    match attr {
        VI_ATTR_TCPIP_HISLIP_OVERLAP_EN | VI_ATTR_TCPIP_HISLIP_VERSION | VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB if t.is_hislip() => {
            let mut g = s.conn.lock().unwrap();
            let Some(h) = g.hislip() else { return VI_ERROR_NSUP_ATTR };
            match attr {
                // The mode in use: the instrument's choice at Initialize, then the last device clear's.
                VI_ATTR_TCPIP_HISLIP_OVERLAP_EN => put(out as *mut u16, h.overlap as u16),
                // ViVersion: major in the top 12 bits, minor in the next 12 (0x00100000 is 1.0).
                VI_ATTR_TCPIP_HISLIP_VERSION => put(out as *mut u32, ((h.version as u32 >> 8) << 20) | ((h.version as u32 & 0xFF) << 8)),
                _ => put(out as *mut u32, (h.our_max / 1024) as u32),
            }
            return VI_SUCCESS;
        }
        // RULE 5.1.17 asks them of a HiSLIP INSTR; a VXI-11 one has them too.
        VI_ATTR_TCPIP_NODELAY | VI_ATTR_TCPIP_KEEPALIVE => {
            return match s.conn.lock().unwrap().tcp_opt(tcp_opt(attr)) {
                Ok(on) => put(out as *mut u16, on as u16).ok_(),
                Err(e) => map_err(&e),
            };
        }
        _ => {}
    }
    let a = s.attrs.lock().unwrap();
    match attr {
        VI_ATTR_DMA_ALLOW_EN => put(out as *mut u16, a.dma as u16),
        VI_ATTR_TRIG_ID if instr => put(out as *mut i16, VI_TRIG_SW),
        VI_ATTR_IO_PROT => put(out as *mut u16, a.io_prot),
        VI_ATTR_RSRC_CLASS => put_str(out as *mut c_char, t.class()),
        VI_ATTR_RSRC_NAME => put_str(out as *mut c_char, &s.alias.lock().unwrap().clone().unwrap_or_else(|| t.name())),
        VI_ATTR_INTF_TYPE => put(out as *mut u16, VI_INTF_TCPIP),
        VI_ATTR_INTF_NUM => put(out as *mut u16, t.board),
        VI_ATTR_INTF_INST_NAME => {
            let proto = match t.proto {
                lan::Proto::Vxi11 { .. } => "VXI-11",
                lan::Proto::Hislip { .. } => "HiSLIP",
                lan::Proto::Socket { .. } => "SOCKET",
            };
            put_str(out as *mut c_char, &format!("TCPIP{} ({proto})", t.board))
        }
        VI_ATTR_TCPIP_ADDR | VI_ATTR_TCPIP_HOSTNAME => {
            put_str(out as *mut c_char, t.host.trim_start_matches('[').trim_end_matches(']'))
        }
        VI_ATTR_TCPIP_PORT => {
            drop(a);
            let port = s.conn.lock().unwrap().port().or(t.port()).unwrap_or(0);
            put(out as *mut u16, port);
            return VI_SUCCESS;
        }
        VI_ATTR_TCPIP_DEVICE_NAME if instr => put_str(out as *mut c_char, &t.device),
        VI_ATTR_TCPIP_IS_HISLIP if instr => put(out as *mut u16, t.is_hislip() as u16),
        VI_ATTR_MAX_QUEUE_LENGTH => put(out as *mut u32, a.max_queue),
        VI_ATTR_TMO_VALUE => put(out as *mut u32, a.tmo_ms),
        VI_ATTR_TERMCHAR => put(out as *mut u8, a.termchar),
        VI_ATTR_TERMCHAR_EN => put(out as *mut u16, a.termchar_en as u16),
        VI_ATTR_SEND_END_EN => put(out as *mut u16, a.send_end as u16),
        VI_ATTR_SUPPRESS_END_EN => put(out as *mut u16, a.suppress_end as u16),
        VI_ATTR_USER_DATA_32 => put(out as *mut u32, a.user_data as u32),
        VI_ATTR_USER_DATA_64 => put(out as *mut u64, a.user_data),
        _ => return VI_ERROR_NSUP_ATTR,
    }
    VI_SUCCESS
}

fn tcp_opt(attr: u32) -> lan::TcpOpt {
    if attr == VI_ATTR_TCPIP_NODELAY { lan::TcpOpt::NoDelay } else { lan::TcpOpt::KeepAlive }
}

pub fn set_attribute(s: &LanSess, attr: u32, value: u64) -> i32 {
    let t = &s.target;
    let socket = t.class() == "SOCKET";
    match attr {
        // A change is a device clear that asks for the mode (VPP-4.3 5.1.2); an instrument that
        // will not go leaves the session as it was, and says so as NSUP_ATTR_STATE.
        VI_ATTR_TCPIP_HISLIP_OVERLAP_EN if t.is_hislip() => {
            let want = value != 0;
            let tmo = s.tmo();
            let mut g = s.conn.lock().unwrap();
            let Some(h) = g.hislip() else { return VI_ERROR_NSUP_ATTR };
            if h.overlap == want {
                return VI_SUCCESS;
            }
            return match h.set_overlap(want, lan::Deadline::after(tmo)) {
                Ok(()) if h.overlap == want => VI_SUCCESS,
                Ok(()) => VI_ERROR_NSUP_ATTR_STATE,
                Err(e) => map_err(&e),
            };
        }
        VI_ATTR_TCPIP_HISLIP_VERSION if t.is_hislip() => return VI_ERROR_ATTR_READONLY,
        VI_ATTR_TCPIP_HISLIP_MAX_MESSAGE_KB if t.is_hislip() => {
            let kb = (value as u32).max(1) as u64;
            let tmo = s.tmo();
            let mut g = s.conn.lock().unwrap();
            let Some(h) = g.hislip() else { return VI_ERROR_NSUP_ATTR };
            return match h.set_max_message(kb * 1024, lan::Deadline::after(tmo)) {
                Ok(()) => VI_SUCCESS,
                Err(e) => map_err(&e),
            };
        }
        VI_ATTR_TCPIP_NODELAY | VI_ATTR_TCPIP_KEEPALIVE => {
            return match s.conn.lock().unwrap().set_tcp_opt(tcp_opt(attr), value != 0) {
                Ok(()) => VI_SUCCESS,
                Err(e) => map_err(&e),
            };
        }
        VI_ATTR_TRIG_ID if t.class() == "INSTR" => {
            return if value as i16 == VI_TRIG_SW { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE };
        }
        VI_ATTR_IO_PROT => {
            let v = value as u16;
            if v != VI_PROT_NORMAL && !(socket && v == VI_PROT_4882_STRS) {
                return VI_ERROR_NSUP_ATTR_STATE;
            }
            s.attrs.lock().unwrap().io_prot = v;
            return VI_SUCCESS;
        }
        _ => {}
    }
    let mut a = s.attrs.lock().unwrap();
    match attr {
        VI_ATTR_DMA_ALLOW_EN => a.dma = value != 0,
        VI_ATTR_TMO_VALUE => a.tmo_ms = value as u32,
        VI_ATTR_TERMCHAR => a.termchar = value as u8,
        VI_ATTR_TERMCHAR_EN => a.termchar_en = value != 0,
        VI_ATTR_SEND_END_EN => a.send_end = value != 0,
        VI_ATTR_SUPPRESS_END_EN => a.suppress_end = value != 0,
        VI_ATTR_MAX_QUEUE_LENGTH if s.ev.enabled_once.load(Ordering::SeqCst) => return VI_ERROR_ATTR_READONLY,
        VI_ATTR_MAX_QUEUE_LENGTH => a.max_queue = value as u32,
        VI_ATTR_USER_DATA_32 | VI_ATTR_USER_DATA_64 => a.user_data = value,
        VI_ATTR_RSRC_CLASS | VI_ATTR_RSRC_NAME | VI_ATTR_RSRC_IMPL_VERSION | VI_ATTR_RSRC_LOCK_STATE
        | VI_ATTR_RSRC_SPEC_VERSION | VI_ATTR_RSRC_MANF_NAME | VI_ATTR_INTF_TYPE | VI_ATTR_INTF_NUM
        | VI_ATTR_INTF_INST_NAME | VI_ATTR_TCPIP_ADDR | VI_ATTR_TCPIP_HOSTNAME | VI_ATTR_TCPIP_PORT
        | VI_ATTR_TCPIP_DEVICE_NAME | VI_ATTR_TCPIP_IS_HISLIP => return VI_ERROR_ATTR_READONLY,
        VI_ATTR_GPIB_PRIMARY_ADDR | VI_ATTR_GPIB_SECONDARY_ADDR | VI_ATTR_GPIB_REN_STATE if s.target.gpib().is_some() => {
            return VI_ERROR_ATTR_READONLY
        }
        _ => return VI_ERROR_NSUP_ATTR,
    }
    VI_SUCCESS
}

trait Ok_ {
    fn ok_(self) -> i32;
}

impl Ok_ for () {
    fn ok_(self) -> i32 {
        VI_SUCCESS
    }
}
