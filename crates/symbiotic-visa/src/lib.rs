//! Viper488 Symbiotic VISA Runtime: the VISA C API (VPP-4.3) for GPIB instruments behind
//! Viper488 WinUSB adapters, the NI GPIB-USB-HS, the Keysight 82357B and the CONTEC GP-IB(USB)FL,
//! for USBTMC instruments bound to WinUSB, and for LAN instruments (`TCPIP`: VXI-11, HiSLIP,
//! SOCKET). Built as `visa64.dll`; the variadic formatted I/O (viPrintf and friends) is fmtio.c.
//!
//! GPIB: the `INSTR` and `INTFC` classes. USB and ASRL (serial ports): `INSTR`. These are owned by
//! the `symbioticd` daemon, which this library starts on demand from its own directory and talks
//! to over a local named pipe, so any number of processes share one adapter, instrument or port.
//! LAN: connected from this process, no daemon (lan.rs). Every `INSTR` session has locks, and all
//! but a serial one the service request (SRQ) as an event.

mod consts;
mod lan;

// The formatted I/O buffers, in fmtio.c.
extern "C" {
    fn symbiotic_fio_flush(vi: u32, mask: u16) -> i32;
    fn symbiotic_fio_setbuf(vi: u32, mask: u16, size: u32) -> i32;
    fn symbiotic_fio_close(vi: u32);
    /// 1: not one of its attributes.
    fn symbiotic_fio_attr(vi: u32, attr: u32, set: i32, value: *mut u64) -> i32;
}
mod transfers;
mod unsupported;

use consts::*;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use symbiotic_core::adapter::{Error, ReadEnd};
use symbiotic_core::backend::NO_SAD;
use symbiotic_core::client::Client;
use symbiotic_core::locks::Target;
use symbiotic_core::proto;
use symbiotic_core::resource;

// The library's parts; `objects` first, for its `on_lan!`.
#[macro_use]
mod objects;
mod attributes;
mod calls;
mod daemon;
mod events;
mod ops;
mod rm;
pub use attributes::*;
pub use calls::*;
use daemon::*;
pub use events::*;
use objects::*;
pub use ops::*;
pub use rm::*;

type ViStatus = i32;
type ViSession = u32;
/// VPP-4.3's ViAttrState: a ViUInt64 in a 64-bit VISA, a ViUInt32 in a 32-bit one.
#[cfg(target_pointer_width = "64")]
type AttrState = u64;
#[cfg(target_pointer_width = "32")]
type AttrState = u32;

// ---------------------------------------------------------------------------------------
// C helpers
// ---------------------------------------------------------------------------------------

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { None } else { CStr::from_ptr(p).to_str().ok() }
}

/// VISA output strings are caller buffers of at least 256 bytes.
pub(crate) unsafe fn put_str(dst: *mut c_char, s: &str) {
    if dst.is_null() {
        return;
    }
    let n = s.len().min(255);
    std::ptr::copy_nonoverlapping(s.as_ptr(), dst as *mut u8, n);
    *dst.add(n) = 0;
}

pub(crate) unsafe fn put<T>(dst: *mut T, v: T) {
    if !dst.is_null() {
        dst.write_unaligned(v);
    }
}

/// Every export runs its body in here. A panic may not unwind out of an `extern` function --
/// Rust aborts the process there -- so without this a bug of ours would kill the host
/// application (LabVIEW, a Python session) instead of failing the one call. The panic itself
/// still prints; the caller gets VI_ERROR_SYSTEM_ERROR.
fn guard(f: impl FnOnce() -> ViStatus) -> ViStatus {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(VI_ERROR_SYSTEM_ERROR)
}

// ---------------------------------------------------------------------------------------
// Beyond VISA
// ---------------------------------------------------------------------------------------

/// NOT A VISA FUNCTION. Selects (level 1 low, 2 normal, 3 high) or, with 0, only reads the
/// T1 settling time of the write direction of GPIB board `board`, and stores the level now in
/// force in `*out`. It is a property of the whole adapter: every instrument behind it changes.
/// For a client that has loaded this DLL (ctypes, LoadLibrary) and wants to change it from
/// inside its own process; `symbiotic-ctl` does the same from outside. NSUP_OPER: this
/// adapter has no levels.
#[no_mangle]
pub unsafe extern "system" fn symbioticSpeed(board: u16, level: u32, out: *mut u32) -> ViStatus {
    guard(|| {
        if level > 3 {
            return VI_ERROR_INV_PARAMETER;
        }
        let r = connect().and_then(|mut c| c.speed(board, level as u8));
        match r {
            Ok(l) => {
                put(out, l as u32);
                VI_SUCCESS
            }
            Err(e) => map_err(&e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn viPrintf(vi: ViSession, fmt: *const c_char, ...) -> ViStatus;
    }

    /// The I/O trace end to end: a SOCKET instrument here, real calls, the records read back, in a
    /// trace directory of the test's own (SYMBIOTIC_TRACE_DIR; nothing else in this test binary traces).
    #[test]
    fn what_the_calls_did_is_traced_while_on_and_not_while_off() {
        use std::io::{BufRead, BufReader, Write};
        use symbiotic_core::iotrace::{self, Cursor};
        // The library takes its trace directory once per process, and another test in this binary
        // may call it first: run in a process of our own whose environment names the directory
        // from the start.
        let Some(state) = std::env::var_os("SYMBIOTIC_TRACE_DIR").map(std::path::PathBuf::from) else {
            let state = std::env::temp_dir().join(format!("symbiotic-visa-trace-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&state);
            let me = "tests::what_the_calls_did_is_traced_while_on_and_not_while_off";
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([me, "--exact", "--nocapture"])
                .env("SYMBIOTIC_TRACE_DIR", &state)
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(&state);
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && text.contains("1 passed"), "{text}");
            return;
        };
        let dir = iotrace::dir();
        assert!(dir.starts_with(&state));
        // An instrument that answers *IDN?, to every connection.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let name = std::ffi::CString::new(format!("TCPIP0::127.0.0.1::{}::SOCKET", l.local_addr().unwrap().port())).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut w = s.try_clone().unwrap();
                    for line in BufReader::new(s).lines().map_while(Result::ok) {
                        if line.trim() == "*IDN?" {
                            let _ = w.write_all(b"MAKER,MODEL,SN\x01,1\n");
                        }
                    }
                });
            }
        });
        let talk = || unsafe {
            let (mut rm, mut vi, mut n) = (0, 0, 0u32);
            let mut buf = [0u8; 64];
            assert_eq!(viOpenDefaultRM(&mut rm), VI_SUCCESS);
            assert_eq!(viOpen(rm, name.as_ptr(), 0, 2000, &mut vi), VI_SUCCESS);
            assert_eq!(viSetAttribute(vi, VI_ATTR_TERMCHAR_EN, 1), VI_SUCCESS);
            assert_eq!(viPrintf(vi, c"*IDN?\n".as_ptr()), VI_SUCCESS);
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut n), VI_SUCCESS_TERM_CHAR);
            assert_eq!(viClose(vi), VI_SUCCESS);
            viClose(rm);
            vi
        };
        talk();
        assert!(Cursor::default().next(&dir).is_empty(), "off: nothing is written");

        iotrace::set_on(&dir, true).unwrap();
        // The library looks for the switch once a second; wait for it to see the switch rather than a fixed time.
        let seen = std::time::Instant::now();
        while !iotrace::on() && seen.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(iotrace::on(), "the library never saw the switch");
        let vi = talk();
        // This test's own records: other tests in this binary run at the same time, and the trace is the process's.
        let mine = format!("::{}::SOCKET", name.to_str().unwrap().split("::").nth(2).unwrap());
        let recs: Vec<_> = Cursor::default().next(&dir).into_iter().filter(|r| r.resource.ends_with(&mine)).collect();
        let find = |op: &str| recs.iter().find(|r| r.op == op).unwrap_or_else(|| panic!("no {op} in {recs:?}"));
        assert!(recs.iter().all(|r| r.pid == std::process::id()));
        let open = find("viOpen");
        assert_eq!((open.vi, open.status, open.depth), (vi, VI_SUCCESS, 1));
        assert!(open.resource.ends_with("::SOCKET"));
        let printf = find("viPrintf");
        assert_eq!(printf.depth, 1);
        let write = find("viWrite");
        assert_eq!((write.depth, write.count, write.data.as_slice()), (2, Some(6), &b"*IDN?\n"[..]), "viPrintf's write, a level down");
        assert!(write.at_us >= printf.at_us && write.resource == open.resource);
        let read = find("viRead");
        assert_eq!((read.depth, read.status, read.count, read.data.as_slice()), (1, VI_SUCCESS_TERM_CHAR, Some(18), &b"MAKER,MODEL,SN\x01,1\n"[..]));
        assert!(recs.iter().any(|r| r.op == "viClose" && r.vi == vi && r.resource == open.resource), "closed, named as it was: {recs:?}");
        iotrace::set_on(&dir, false).unwrap();
        let _ = std::fs::remove_dir_all(&state);
    }

    /// viFindRsrc's attribute part end to end: two SOCKET instruments here, configured, and searches
    /// that the attributes decide, in a configuration of the test's own (SYMBIOTIC_CONFIG_DIR and
    /// SYMBIOTIC_LAN_CONF; nothing else in this test binary reads them).
    #[test]
    fn a_search_by_attribute() {
        let conf = std::env::temp_dir().join(format!("symbiotic-visa-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&conf);
        std::fs::create_dir_all(&conf).unwrap();
        let ports: Vec<u16> = (0..2)
            .map(|_| {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                std::thread::spawn(move || for _ in l.incoming() {});
                p
            })
            .collect();
        let lan = conf.join("lan.conf");
        std::fs::write(&lan, ports.iter().map(|p| format!("TCPIP0::127.0.0.1::{p}::SOCKET\n")).collect::<String>()).unwrap();
        std::env::set_var("SYMBIOTIC_CONFIG_DIR", &conf);
        std::env::set_var("SYMBIOTIC_LAN_CONF", &lan);
        let find = |expr: String| unsafe {
            let (mut rm, mut list, mut n) = (0, 0, 0u32);
            let mut first = [0 as c_char; 256];
            assert_eq!(viOpenDefaultRM(&mut rm), VI_SUCCESS);
            let e = std::ffi::CString::new(expr).unwrap();
            let st = viFindRsrc(rm, e.as_ptr(), &mut list, &mut n, first.as_mut_ptr());
            let mut found = Vec::new();
            if st == VI_SUCCESS {
                found.push(CStr::from_ptr(first.as_ptr()).to_string_lossy().into_owned());
                for _ in 1..n {
                    assert_eq!(viFindNext(list, first.as_mut_ptr()), VI_SUCCESS);
                    found.push(CStr::from_ptr(first.as_ptr()).to_string_lossy().into_owned());
                }
                viClose(list);
            }
            viClose(rm);
            (st, found)
        };
        let name = |p: u16| format!("TCPIP0::127.0.0.1::{p}::SOCKET");
        assert_eq!(find("TCPIP?*".into()), (VI_SUCCESS, vec![name(ports[0]), name(ports[1])]));
        assert_eq!(find(format!("TCPIP?*{{VI_ATTR_TCPIP_PORT == {}}}", ports[1])), (VI_SUCCESS, vec![name(ports[1])]));
        assert_eq!(find(format!("TCPIP?*{{!(VI_ATTR_TCPIP_PORT == {}) && VI_ATTR_TCPIP_ADDR == \"127.0.0.1\"}}", ports[1])), (VI_SUCCESS, vec![name(ports[0])]));
        assert_eq!(find("TCPIP?*{VI_ATTR_TCPIP_PORT > 0 && VI_ATTR_RSRC_CLASS == \"SOCKET\"}".into()).1.len(), 2);
        assert_eq!(find("TCPIP?*{VI_ATTR_TCPIP_ADDR == \"10.0.0.1\"}".into()).0, VI_ERROR_RSRC_NFOUND);
        // An attribute these resources do not have: none of them matches, even negated.
        assert_eq!(find("TCPIP?*{!(VI_ATTR_GPIB_PRIMARY_ADDR == 5)}".into()).0, VI_ERROR_RSRC_NFOUND);
        // Not an expression: a local attribute (VPP-4.3 RULE 4.4.7), an unknown one.
        assert_eq!(find("TCPIP?*{VI_ATTR_TMO_VALUE == 2000}".into()).0, VI_ERROR_INV_EXPR);
        assert_eq!(find("TCPIP?*{VI_ATTR_NONESUCH == 1}".into()).0, VI_ERROR_INV_EXPR);
        let _ = std::fs::remove_dir_all(&conf);
    }

    /// A SOCKET instrument on this machine that answers `*IDN?` and says nothing else; its resource name.
    fn idn_socket() -> std::ffi::CString {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let name = std::ffi::CString::new(format!("TCPIP0::127.0.0.1::{}::SOCKET", l.local_addr().unwrap().port())).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut w = s.try_clone().unwrap();
                    for line in BufReader::new(s).lines().map_while(Result::ok) {
                        if line.trim() == "*IDN?" {
                            let _ = w.write_all(b"MAKER,MODEL,SN,1\n");
                        }
                    }
                });
            }
        });
        name
    }

    /// What the handlers below were called with: (which handler, event type, the event's status, its operation).
    static CALLED: Mutex<Vec<(usize, u32, i32, String)>> = Mutex::new(Vec::new());

    unsafe extern "system" fn record(_vi: u32, ty: u32, ctx: u32, user: *const c_void) -> i32 {
        let mut status = 0i32;
        let mut oper = [0 as c_char; 256];
        viGetAttribute(ctx, VI_ATTR_STATUS, &mut status as *mut i32 as *mut c_void);
        viGetAttribute(ctx, VI_ATTR_OPER_NAME, oper.as_mut_ptr() as *mut c_void);
        let oper = CStr::from_ptr(oper.as_ptr()).to_string_lossy().into_owned();
        CALLED.lock().unwrap().push((user as usize, ty, status, oper));
        // Handler 2 says the ones installed before it are not to be called.
        if user as usize == 2 { VI_SUCCESS_NCHAIN } else { VI_SUCCESS }
    }

    /// Set by `slow` as it starts and as it returns; `slow` with user handle 9 uninstalls itself.
    static SLOW: Mutex<(u32, u32)> = Mutex::new((0, 0));

    unsafe extern "system" fn slow(vi: u32, ty: u32, _ctx: u32, user: *const c_void) -> i32 {
        SLOW.lock().unwrap().0 += 1;
        if user as usize == 9 {
            // From inside itself: not waited for, or this would never return.
            assert_eq!(viUninstallHandler(vi, ty, slow as *const c_void, user), VI_SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(300));
        SLOW.lock().unwrap().1 += 1;
        VI_SUCCESS
    }

    /// viUninstallHandler and viClose return only once a handler running on another thread has returned, and a handler
    /// taken away is not called after.
    #[test]
    fn a_handler_taken_away_is_neither_called_nor_running_after() {
        let name = idn_socket();
        unsafe {
            let (mut rm, mut vi) = (0, 0);
            assert_eq!(viOpenDefaultRM(&mut rm), VI_SUCCESS);
            assert_eq!(viOpen(rm, name.as_ptr(), 0, 2000, &mut vi), VI_SUCCESS);
            let srq = VI_EVENT_SERVICE_REQ;
            // `srq` set below as viEnableEvent would, without an instrument's service request.
            let fire = |vi: u32, ev: &Arc<Events>| {
                let ev = ev.clone();
                let t = std::thread::spawn(move || events::deliver(vi, &ev, Ev::srq()));
                while SLOW.lock().unwrap().0 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                t
            };
            for how in ["uninstall", "close"] {
                *SLOW.lock().unwrap() = (0, 0);
                if how == "close" {
                    assert_eq!(viOpen(rm, name.as_ptr(), 0, 2000, &mut vi), VI_SUCCESS);
                }
                let ev = events::events_of(vi).unwrap();
                ev.st.lock().unwrap().srq = (false, true);
                assert_eq!(viInstallHandler(vi, srq, slow as *const c_void, std::ptr::without_provenance(1)), VI_SUCCESS);
                let t = fire(vi, &ev);
                let st = if how == "close" { viClose(vi) } else { viUninstallHandler(vi, srq, slow as *const c_void, std::ptr::without_provenance(1)) };
                assert_eq!(st, VI_SUCCESS);
                assert_eq!(*SLOW.lock().unwrap(), (1, 1), "{how}: back only after the handler returned");
                t.join().unwrap();
                if how == "uninstall" {
                    events::deliver(vi, &ev, Ev::srq());
                    assert_eq!(*SLOW.lock().unwrap(), (1, 1), "not called once uninstalled");
                }
            }
            // A handler that uninstalls itself.
            *SLOW.lock().unwrap() = (0, 0);
            assert_eq!(viOpen(rm, name.as_ptr(), 0, 2000, &mut vi), VI_SUCCESS);
            let ev = events::events_of(vi).unwrap();
            ev.st.lock().unwrap().srq = (false, true);
            assert_eq!(viInstallHandler(vi, srq, slow as *const c_void, std::ptr::without_provenance(9)), VI_SUCCESS);
            fire(vi, &ev).join().unwrap();
            assert_eq!(*SLOW.lock().unwrap(), (1, 1));
            assert_eq!(viUninstallHandler(vi, srq, slow as *const c_void, std::ptr::without_provenance(9)), VI_ERROR_HNDLR_NINSTALLED, "it is gone");
            assert_eq!(viClose(rm), VI_SUCCESS);
        }
    }

    /// The events a session has without hardware: I/O completion queued and handled, exceptions handled,
    /// the warnings for enabling or disabling twice, discarding, handlers' order (VPP-4.3 RULE 3.7.8:
    /// the last installed first) and VI_SUCCESS_NCHAIN.
    #[test]
    fn events_on_a_session_of_this_machine() {
        let name = idn_socket();
        unsafe {
            let (mut rm, mut vi) = (0, 0);
            assert_eq!(viOpenDefaultRM(&mut rm), VI_SUCCESS);
            assert_eq!(viOpen(rm, name.as_ptr(), 0, 2000, &mut vi), VI_SUCCESS);
            assert_eq!(viSetAttribute(vi, VI_ATTR_TERMCHAR_EN, 1), VI_SUCCESS);
            let ioc = VI_EVENT_IO_COMPLETION;

            // Queued.
            assert_eq!(viWaitOnEvent(vi, ioc, 0, std::ptr::null_mut(), std::ptr::null_mut()), VI_ERROR_NOT_ENABLED);
            assert_eq!(viEnableEvent(vi, ioc, VI_QUEUE, 0), VI_SUCCESS);
            assert_eq!(viEnableEvent(vi, ioc, VI_QUEUE, 0), VI_SUCCESS_EVENT_EN, "on already");
            let (mut wjob, mut rjob) = (0u32, 0u32);
            let mut buf = [0u8; 64];
            assert_eq!(transfers::viWriteAsync(vi, b"*IDN?\n".as_ptr(), 6, &mut wjob), VI_SUCCESS_SYNC);
            assert_eq!(transfers::viReadAsync(vi, buf.as_mut_ptr(), buf.len() as u32, &mut rjob), VI_SUCCESS_SYNC);
            for (job, n, st) in [(wjob, 6, VI_SUCCESS), (rjob, 17, VI_SUCCESS_TERM_CHAR)] {
                let (mut ty, mut ctx) = (0u32, 0u32);
                assert_eq!(viWaitOnEvent(vi, ioc, 1000, &mut ty, &mut ctx), VI_SUCCESS);
                let (mut got_job, mut got_n, mut got_st) = (0u32, 0u32, 0i32);
                viGetAttribute(ctx, VI_ATTR_JOB_ID, &mut got_job as *mut u32 as *mut c_void);
                viGetAttribute(ctx, VI_ATTR_RET_COUNT_32, &mut got_n as *mut u32 as *mut c_void);
                viGetAttribute(ctx, VI_ATTR_STATUS, &mut got_st as *mut i32 as *mut c_void);
                assert_eq!((ty, got_job, got_n, got_st), (ioc, job, n, st), "in the order they completed");
                assert_eq!(viClose(ctx), VI_SUCCESS);
            }
            assert_eq!(&buf[..17], b"MAKER,MODEL,SN,1\n");
            assert_eq!(viWaitOnEvent(vi, ioc, 0, std::ptr::null_mut(), std::ptr::null_mut()), VI_ERROR_TMO, "nothing left");
            // Discarded.
            assert_eq!(transfers::viWriteAsync(vi, b"*IDN?\n".as_ptr(), 6, &mut wjob), VI_SUCCESS_SYNC);
            assert_eq!(viDiscardEvents(vi, ioc, VI_QUEUE), VI_SUCCESS);
            assert_eq!(viWaitOnEvent(vi, ioc, 0, std::ptr::null_mut(), std::ptr::null_mut()), VI_ERROR_TMO, "discarded");
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut 0u32), VI_SUCCESS_TERM_CHAR);
            // Off: nothing queued, and waiting is refused.
            assert_eq!(viDisableEvent(vi, ioc, VI_QUEUE), VI_SUCCESS);
            assert_eq!(viDisableEvent(vi, ioc, VI_QUEUE), VI_SUCCESS_EVENT_DIS, "off already");
            assert_eq!(viWaitOnEvent(vi, ioc, 0, std::ptr::null_mut(), std::ptr::null_mut()), VI_ERROR_NOT_ENABLED);

            // Handlers: the last installed is called first; 2 returns VI_SUCCESS_NCHAIN, so 1 is not called.
            let h = record as *const c_void;
            assert_eq!(viInstallHandler(vi, ioc, h, std::ptr::without_provenance(1)), VI_SUCCESS);
            assert_eq!(viInstallHandler(vi, ioc, h, std::ptr::without_provenance(2)), VI_SUCCESS);
            assert_eq!(viInstallHandler(vi, ioc, h, std::ptr::without_provenance(3)), VI_SUCCESS);
            assert_eq!(viEnableEvent(vi, ioc, VI_HNDLR, 0), VI_SUCCESS);
            CALLED.lock().unwrap().clear();
            assert_eq!(transfers::viWriteAsync(vi, b"*IDN?\n".as_ptr(), 6, &mut wjob), VI_SUCCESS_SYNC);
            let calls: Vec<usize> = CALLED.lock().unwrap().iter().map(|c| c.0).collect();
            assert_eq!(calls, [3, 2], "LIFO, and the chain stops at 2");
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut 0u32), VI_SUCCESS_TERM_CHAR);
            assert_eq!(viUninstallHandler(vi, ioc, h, 9 as *const c_void), VI_ERROR_HNDLR_NINSTALLED);
            assert_eq!(viUninstallHandler(vi, ioc, h, 3 as *const c_void), VI_SUCCESS);
            assert_eq!(viUninstallHandler(vi, ioc, std::ptr::null(), std::ptr::null()), VI_SUCCESS, "VI_ANY_HNDLR: the rest");
            CALLED.lock().unwrap().clear();
            assert_eq!(transfers::viWriteAsync(vi, b"*IDN?\n".as_ptr(), 6, &mut wjob), VI_SUCCESS_SYNC);
            assert!(CALLED.lock().unwrap().is_empty(), "none left");
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut 0u32), VI_SUCCESS_TERM_CHAR);

            // Exceptions: handlers only (VPP-4.3 3.7.2), with the failing operation's status and name.
            let exc = VI_EVENT_EXCEPTION;
            assert_eq!(viEnableEvent(vi, exc, VI_QUEUE, 0), VI_ERROR_INV_MECH);
            assert_eq!(viInstallHandler(vi, exc, h, 5 as *const c_void), VI_SUCCESS);
            assert_eq!(viEnableEvent(vi, exc, VI_HNDLR, 0), VI_SUCCESS);
            assert_eq!(viSetAttribute(vi, VI_ATTR_TMO_VALUE, 100), VI_SUCCESS);
            CALLED.lock().unwrap().clear();
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut 0u32), VI_ERROR_TMO, "nothing was asked");
            assert_eq!(*CALLED.lock().unwrap(), [(5, exc, VI_ERROR_TMO, "viRead".to_string())]);
            assert_eq!(viDisableEvent(vi, exc, VI_HNDLR), VI_SUCCESS);
            CALLED.lock().unwrap().clear();
            assert_eq!(viRead(vi, buf.as_mut_ptr(), buf.len() as u32, &mut 0u32), VI_ERROR_TMO);
            assert!(CALLED.lock().unwrap().is_empty(), "off: no handler");

            assert_eq!(viClose(vi), VI_SUCCESS);
            viClose(rm);
        }
    }

    /// viRead's loop on reads that follow a script: (bytes, how the read ended), and what each read was asked.
    fn scripted(count: u32, term: Option<u8>, suppress: bool, tmo_ms: u32, script: &[(&[u8], ReadEnd)]) -> (ViStatus, Vec<u8>, Vec<u32>) {
        let mut next = script.iter();
        let (mut out, mut asked) = (Vec::new(), Vec::new());
        let st = read_loop(count, term, suppress, tmo_ms, fw_timeout(tmo_ms), &mut out, |want, _tmo| {
            asked.push(want);
            match next.next() {
                Some((d, e)) => Ok(symbiotic_core::adapter::ReadResult { data: d.to_vec(), end: *e }),
                None => Ok(symbiotic_core::adapter::ReadResult { data: Vec::new(), end: ReadEnd::Timeout }),
            }
        });
        (st, out, asked)
    }

    #[test]
    fn a_read_with_end_suppressed_goes_on_to_the_character_the_count_or_the_timeout() {
        use ReadEnd::*;
        // END not suppressed: one read, END ends it.
        assert_eq!(scripted(100, None, false, 2000, &[(b"ab", End), (b"cd", End)]), (VI_SUCCESS, b"ab".to_vec(), vec![100]));
        // Suppressed: on past END, each read asking for what the count leaves, until the character.
        assert_eq!(scripted(100, Some(b'\n'), true, 2000, &[(b"ab", End), (b"cd", End), (b"e\n", TermChar)]),
            (VI_SUCCESS_TERM_CHAR, b"abcde\n".to_vec(), vec![100, 98, 96]));
        // The character on the same byte as END ends it, though the adapter called it END.
        assert_eq!(scripted(100, Some(b'\n'), true, 2000, &[(b"x\n", End), (b"more", End)]).0, VI_SUCCESS_TERM_CHAR);
        // No character enabled: a newline is just a byte.
        assert_eq!(scripted(5, None, true, 2000, &[(b"x\n", End), (b"abc", End)]), (VI_SUCCESS_MAX_CNT, b"x\nabc".to_vec(), vec![5, 3]));
        // The count reached: that ended it, so MAX_CNT (VPP-4.3), not SUCCESS, which says END.
        assert_eq!(scripted(4, None, true, 2000, &[(b"ab", End), (b"cd", End)]).0, VI_SUCCESS_MAX_CNT);
        // A timeout after some bytes keeps them.
        assert_eq!(scripted(100, None, true, 2000, &[(b"ab", End), (b"c", Timeout)]), (VI_ERROR_TMO, b"abc".to_vec(), vec![100, 98]));
        // An error is the read's status, with what came before it.
        let mut n = 0;
        let mut out = Vec::new();
        let st = read_loop(100, None, true, 2000, 2000, &mut out, |_, _| {
            n += 1;
            if n == 1 { Ok(symbiotic_core::adapter::ReadResult { data: b"ab".to_vec(), end: End }) } else { Err(Error::Gone) }
        });
        assert_eq!((st, out.as_slice()), (VI_ERROR_CONN_LOST, &b"ab"[..]));
    }

    #[test]
    fn a_read_with_end_suppressed_ends_at_its_timeout_even_when_nothing_comes() {
        // An instrument that answers empty messages (END and no byte) forever: the read is over at its timeout.
        let t = std::time::Instant::now();
        let mut out = Vec::new();
        let mut reads = 0u32;
        let st = read_loop(100, None, true, 200, 200, &mut out, |_, tmo| {
            reads += 1;
            assert!(tmo <= 200, "each read gets what is left: {tmo}");
            std::thread::sleep(Duration::from_millis(5));
            Ok(symbiotic_core::adapter::ReadResult { data: Vec::new(), end: ReadEnd::End })
        });
        assert_eq!(st, VI_ERROR_TMO);
        assert!(t.elapsed() >= Duration::from_millis(200) && t.elapsed() < Duration::from_millis(600), "{:?}", t.elapsed());
        assert!(reads > 1);
    }

    #[test]
    fn a_timeout_on_the_wire() {
        assert_eq!(VI_TMO_INFINITE, symbiotic_core::backend::TMO_INFINITE);
        assert_eq!(fw_timeout(VI_TMO_INFINITE), symbiotic_core::backend::TMO_INFINITE);
        assert_eq!(fw_timeout(VI_TMO_IMMEDIATE), 1);
        assert_eq!(fw_timeout(2000), 2000);
        assert_eq!(fw_timeout(120_000), 120_000, "longer than the u16 the protocol had");
    }
}
