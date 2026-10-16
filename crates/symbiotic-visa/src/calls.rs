//! Every VISA call's way in and out: how deep this thread is, the status it ends with (an exception event for an error), and its record in the I/O trace.

use super::*;

thread_local! {
    /// How deep this thread is in VISA operations: an operation that another one calls (viPrintf's
    /// viWrite, viReadAsync's viRead) is not the one that failed as far as the application knows.
    pub(crate) static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// When each operation this thread is in began, innermost last: for the I/O trace.
    pub(crate) static STARTED: std::cell::RefCell<Vec<std::time::Instant>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The bytes the operation now ending moved (count, the first few), for the I/O trace.
    pub(crate) static MOVED: std::cell::RefCell<Option<(u32, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn enter() {
    DEPTH.with(|d| d.set(d.get() + 1));
    STARTED.with(|s| s.borrow_mut().push(std::time::Instant::now()));
}

/// The name an I/O trace record gives session `vi` (its VI_ATTR_RSRC_NAME); "" if it has none.
pub(crate) fn trace_name(vi: ViSession) -> String {
    match get(vi).as_deref() {
        Some(Object::Instr(m)) => m.try_lock().map(|s| s.name.clone()).unwrap_or_default(),
        Some(Object::Lan(l)) => l.name(),
        _ => String::new(),
    }
}

/// Write an I/O trace record (symbiotic_core::iotrace) for a call that began at `began` and is ending.
pub(crate) fn trace_record(vi: ViSession, name: &str, st: ViStatus, depth: u32, began: Option<std::time::Instant>, resource: String) {
    use symbiotic_core::iotrace::{self, Record};
    let dur = began.map(|b| b.elapsed()).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (count, data) = match MOVED.with(|m| m.borrow_mut().take()) {
        Some((c, d)) => (Some(c), d),
        None => (None, Vec::new()),
    };
    iotrace::write(&Record {
        at_us: now.saturating_sub(dur).as_micros() as u64,
        pid: std::process::id(),
        depth,
        vi,
        op: name.to_string(),
        status: st,
        dur_us: dur.as_micros() as u64,
        resource,
        count,
        data,
    });
}

/// A session operation: its body, and, if it fails, the exception event (VPP-4.3 3.7.2) -- raised
/// only by the operation the application called, in its own thread (RULE 3.7.11), before the
/// operation returns its error. An operation a handler calls raises none (it would recurse).
pub(crate) fn op(vi: ViSession, name: &'static str, f: impl FnOnce() -> ViStatus) -> ViStatus {
    enter();
    let st = guard(f);
    leave(vi, st, name)
}

/// `op` for a call that moves bytes: `moved` says how many and gives the first of them, for the I/O
/// trace, once the call is over (and only while tracing).
pub(crate) fn op_io(vi: ViSession, name: &'static str, f: impl FnOnce() -> ViStatus, moved: impl FnOnce() -> (u32, Vec<u8>)) -> ViStatus {
    enter();
    let st = guard(f);
    if symbiotic_core::iotrace::on() {
        let m = guard_value(moved);
        MOVED.with(|x| *x.borrow_mut() = m);
    }
    leave(vi, st, name)
}

/// `moved`, unless it panics (a bad pointer the application gave): the trace is not worth a crash.
pub(crate) fn guard_value<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

pub(crate) fn leave(vi: ViSession, st: ViStatus, name: &str) -> ViStatus {
    let began = STARTED.with(|s| s.borrow_mut().pop());
    let depth = DEPTH.with(|d| d.get());
    if symbiotic_core::iotrace::on() {
        trace_record(vi, name, st, depth, began, trace_name(vi));
    } else {
        MOVED.with(|m| m.borrow_mut().take());
    }
    let outermost = DEPTH.with(|d| {
        d.set(d.get().saturating_sub(1));
        d.get() == 0
    });
    if outermost && st < 0 {
        raise(vi, st, name);
    }
    st
}

pub(crate) fn raise(vi: ViSession, st: ViStatus, name: &str) {
    let Ok(ev) = events_of(vi) else { return };
    if !ev.st.lock().unwrap().exc.1 {
        return;
    }
    DEPTH.with(|d| d.set(d.get() + 1));
    deliver(vi, &ev, Ev { ty: VI_EVENT_EXCEPTION, status: st, job: 0, ret: 0, buf: 0, oper: name.to_string() });
    DEPTH.with(|d| d.set(d.get() - 1));
}

/// fmtio.c's side of `op`: its exported functions call these around their bodies.
#[no_mangle]
pub extern "C" fn symbiotic_op_enter() {
    enter();
}

/// # Safety
/// `name` is a NUL-terminated string (fmtio.c passes a literal).
#[no_mangle]
pub unsafe extern "C" fn symbiotic_op_leave(vi: ViSession, st: ViStatus, name: *const c_char) -> ViStatus {
    let n = cstr(name).unwrap_or("");
    guard(|| leave(vi, st, n))
}
