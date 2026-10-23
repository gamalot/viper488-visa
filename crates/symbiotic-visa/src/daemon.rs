//! The daemon, from a session's side: where it is, the connection each session has to it, and its errors as VISA statuses.

use super::*;

pub(crate) fn trace(msg: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SYMBIOTIC_TRACE").is_some()) {
        eprintln!("[symbiotic] {}", msg());
    }
}

/// A session's connection to the daemon. One per session, so a long operation on one does not
/// hold up another; opened lazily and reopened after a loss (which also restarts the daemon).
pub(crate) type Conn = Arc<Mutex<Option<Client>>>;

/// Where `symbioticd` lives: next to this DLL (or shared object).
pub(crate) fn dll_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::LibraryLoader::{
                GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            };
            let mut h = std::ptr::null_mut();
            let mut buf = [0u16; 1024];
            // SAFETY: the address is a function in this module; the buffer length is passed.
            unsafe {
                if GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    dll_dir as *const u16,
                    &mut h,
                ) == 0
                {
                    return None;
                }
                let n = GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) as usize;
                if n == 0 || n >= buf.len() {
                    return None;
                }
                PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(|p| p.to_path_buf())
            }
        }
        #[cfg(unix)]
        {
            // The shared object this function is in, not the program that loaded it (python3,
            // whose directory has no daemon beside it).
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            // SAFETY: the address is a function in this module; dladdr only fills `info`.
            if unsafe { libc::dladdr(dll_dir as *const libc::c_void, &mut info) } == 0 || info.dli_fname.is_null() {
                return None;
            }
            // SAFETY: dladdr's file name is a NUL-terminated string owned by the loader.
            let name = unsafe { CStr::from_ptr(info.dli_fname) };
            use std::os::unix::ffi::OsStrExt;
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes()));
            // A link is followed: the main program is named as it was started (/usr/bin/symbiotic-explorer,
            // a link into /usr/lib/symbiotic), and the daemon is beside the real file. A name without a
            // directory says nothing.
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            path.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf())
        }
        #[cfg(not(any(windows, unix)))]
        {
            None
        }
    })
    .clone()
}

/// Where to start `symbioticd` from: beside this DLL if it is there; else an install's directory (paths::install_dirs),
/// for a copy of the DLL put in a program's own folder (a 32-bit program's visa32.dll, say). Such a copy could
/// otherwise use only a daemon already running.
pub(crate) fn daemon_dir() -> Option<PathBuf> {
    let own = dll_dir();
    match &own {
        Some(d) => symbiotic_core::paths::daemon_dir_for(d).or(own),
        None => {
            let exe = if cfg!(windows) { "symbioticd.exe" } else { "symbioticd" };
            symbiotic_core::paths::install_dirs().into_iter().find(|d| d.join(exe).is_file())
        }
    }
}

pub(crate) fn connect() -> Result<Client, Error> {
    Client::connect(daemon_dir().as_deref())
}

/// What symbiotic-core starts a daemon from when it connects on its own (an alias's unit checked at viOpen): this
/// library's daemon, not one beside the application.
pub(crate) fn set_daemon_dir_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(d) = daemon_dir() {
            symbiotic_core::client::set_daemon_dir(d);
        }
    });
}

/// The session's connection, made first if there is none. A new one may be to a new daemon, whose
/// board numbers were given out afresh, so it is checked: the session's board number must still
/// mean the adapter the session was opened on. If it does not, the session is lost -- the answer
/// is VI_ERROR_CONN_LOST, not a request sent to another adapter.
pub(crate) fn ensure(s: &Snap, g: &mut Option<Client>) -> Result<(), Error> {
    if g.is_none() {
        // A tunnel to a shared alias that is gone is the session gone (see open_shared).
        if shared_alias(&s.adapter).is_some() {
            return Err(Error::Gone);
        }
        // Its locks were the old connection's, and went with it (a daemon that restarted has none): this is reported
        // once, as the connection lost, rather than a new connection carrying on as if it held them. The call after is
        // a new connection, unlocked.
        if s.locked.swap(false, Ordering::SeqCst) {
            trace(|| format!("{:?}: the connection that held its locks is gone", s.t));
            return Err(Error::Gone);
        }
        let mut c = connect()?;
        if !still_means(&mut c, s.t, &s.adapter)? {
            trace(|| format!("{:?} is no longer {}", s.t, s.adapter));
            return Err(Error::Gone);
        }
        *g = Some(c);
    }
    Ok(())
}

/// Before a name is looked up in the group: have the daemon fetch the directory if it is old
/// (ipc.rs, SHARE_REFRESH). Nothing happens on a machine that is in no group.
pub(crate) fn refresh_group() {
    if symbiotic_core::share::lookup::needs_refresh() {
        if let Ok(mut c) = connect() {
            let _ = c.share_refresh();
        }
    }
}

/// `Instr::adapter` of a session opened by a shared alias: the alias after this prefix.
pub(crate) const SHARED: &str = "shared:";

pub(crate) fn shared_alias(adapter: &str) -> Option<&str> {
    adapter.strip_prefix(SHARED)
}

/// A new connection for something a session does on its own (its service request watch): to the
/// daemon, and for a shared alias a tunnel to the owner of the instrument.
pub(crate) fn session_client(adapter: &str, start_daemon: bool) -> Result<Client, Error> {
    let mut c = if start_daemon { connect()? } else { Client::connect_existing()? };
    if let Some(a) = shared_alias(adapter) {
        c.peer_connect(a)?;
    }
    Ok(c)
}

/// The instrument a session names, as the daemon's requests name it.
pub(crate) fn target_of(s: &Instr) -> Target {
    if s.usb.is_some() {
        Target::Usb(s.board)
    } else if s.asrl.is_some() {
        Target::Asrl(s.board)
    } else {
        Target::Gpib(s.board, s.pad, s.sad)
    }
}

/// Does the daemon's number in `t` still mean the adapter (GPIB) or instrument (USBTMC) with key
/// `key`? A new daemon numbers afresh.
pub(crate) fn still_means(c: &mut Client, t: Target, key: &str) -> Result<bool, Error> {
    Ok(match t {
        Target::Gpib(board, _, _) => c.list_bound()?.get(board as usize).is_some_and(|x| x.present && x.entry.key() == key),
        Target::Usb(n) => c.usb_list()?.get(n as usize).is_some_and(|x| x.present && x.key() == key),
        // A COM number is the port's own. A new connection opens it again (for the daemon, a port
        // is open while a connection that used it is there); one that went is the session lost.
        Target::Asrl(n) => match c.asrl_open(n) {
            Ok(()) => true,
            Err(Error::NoBoard) => false,
            Err(e) => return Err(e),
        },
    })
}

/// Run `f` on the session's connection (`ensure`). A request that never went out because the
/// connection was already dead -- the daemon exited or was killed -- is sent once more on a new
/// one: nothing of it ran, so this costs the caller nothing. A connection lost after the request
/// went out may have run it, so that is VI_ERROR_CONN_LOST, and the next call starts afresh.
pub(crate) fn with_session<T>(s: &Snap, mut f: impl FnMut(&mut Client) -> Result<T, Error>) -> Result<T, Error> {
    let mut g = s.conn.lock().unwrap();
    ensure(s, &mut g)?;
    let mut r = f(g.as_mut().unwrap());
    if matches!(r, Err(Error::NotSent)) {
        *g = None;
        ensure(s, &mut g)?;
        r = f(g.as_mut().unwrap());
    }
    if matches!(r, Err(Error::Gone | Error::NotSent)) {
        drop_conn(s, &mut g);
    }
    r
}

/// The session's connection is lost, and this call says so (VI_ERROR_CONN_LOST): its locks went with
/// it, and that has been said now -- the next call connects afresh without saying it again.
pub(crate) fn drop_conn(s: &Snap, g: &mut Option<Client>) {
    *g = None;
    s.locked.store(false, Ordering::SeqCst);
}

pub(crate) fn map_err(e: &Error) -> ViStatus {
    trace(|| format!("error: {e}"));
    match e {
        Error::Timeout => VI_ERROR_TMO,
        Error::NoBoard => VI_ERROR_RSRC_NFOUND,
        Error::NoDevice => VI_ERROR_NLISTENERS,
        Error::Busy => VI_ERROR_RSRC_BUSY,
        Error::Gone | Error::NotSent => VI_ERROR_CONN_LOST,
        // The adapter does not offer the operation: that is the caller's to know, not an I/O failure.
        Error::Status(symbiotic_core::proto::ST_UNSUPPORTED) => VI_ERROR_NSUP_OPER,
        Error::Status(_) | Error::Io(_) => VI_ERROR_IO,
        Error::Locked => VI_ERROR_RSRC_LOCKED,
        Error::NotLocked => VI_ERROR_SESN_NLOCKED,
        Error::LockKey => VI_ERROR_INV_ACCESS_KEY,
    }
}

/// VISA timeout (ms, or the two special values) to the protocol's u32 milliseconds, where 0
/// means "the backend's default", so VI_TMO_IMMEDIATE is 1 ms; VI_TMO_INFINITE is the protocol's
/// infinite too (`backend::TMO_INFINITE`).
pub(crate) fn fw_timeout(tmo: u32) -> u32 {
    match tmo {
        VI_TMO_IMMEDIATE => 1,
        t => t,
    }
}
