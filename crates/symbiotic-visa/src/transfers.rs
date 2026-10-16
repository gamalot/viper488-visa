//! The rest of VPP-4.3's I/O operations: asynchronous reads and writes, reads to and writes from a
//! file, and the USB control transfers.
//!
//! ASYNCHRONOUS OPERATIONS COMPLETE SYNCHRONOUSLY: viReadAsync and viWriteAsync do the transfer
//! before they return, answer VI_SUCCESS_SYNC -- which VPP-4.3 allows -- and post the I/O
//! completion event as an asynchronous one would (NI-VISA does the same when it completes one
//! synchronously). So no job is ever pending, and viTerminate has none to terminate.
//!
//! viBufWrite and viBufRead go through the formatted I/O buffers, so they are in fmtio.c.

use super::*;
use std::collections::HashSet;
use std::io::{Read, Write};

static JOB: AtomicU32 = AtomicU32::new(1);

/// Sessions with VI_ATTR_FILE_APPEND_EN set (default: off, the file is truncated).
static APPEND: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

fn append_set() -> std::sync::MutexGuard<'static, HashSet<u32>> {
    APPEND.get_or_init(Default::default).lock().unwrap()
}

pub(crate) fn file_append(vi: u32) -> bool {
    append_set().contains(&vi)
}

pub(crate) fn set_file_append(vi: u32, on: bool) {
    let mut s = append_set();
    if on {
        s.insert(vi);
    } else {
        s.remove(&vi);
    }
}

/// The session went: its settings here go with it.
pub(crate) fn forget(vi: u32) {
    append_set().remove(&vi);
}

/// A session that I/O can be done on: an instrument (daemon or LAN).
fn io_session(vi: u32) -> Result<(), ViStatus> {
    match get(vi).as_deref() {
        Some(Object::Instr(_) | Object::Lan(_)) => Ok(()),
        Some(_) => Err(VI_ERROR_NSUP_OPER),
        None => Err(VI_ERROR_INV_OBJECT),
    }
}

/// The completion of an asynchronous operation, to the session's queue and handlers.
fn post_io(vi: u32, status: ViStatus, job: u32, ret: u32, buf: usize, oper: &'static str) {
    if let Ok(ev) = events_of(vi) {
        deliver(vi, &ev, Ev { ty: VI_EVENT_IO_COMPLETION, status, job, ret: ret as u64, buf, oper: oper.to_string() });
    }
}

#[no_mangle]
pub unsafe extern "system" fn viReadAsync(vi: ViSession, buf: *mut u8, count: u32, job: *mut u32) -> ViStatus {
    op(vi, "viReadAsync", || {
        if let Err(e) = io_session(vi) {
            return e;
        }
        let id = JOB.fetch_add(1, Ordering::Relaxed);
        put(job, id);
        let mut n = 0u32;
        let status = viRead(vi, buf, count, &mut n);
        post_io(vi, status, id, n, buf as usize, "viReadAsync");
        VI_SUCCESS_SYNC
    })
}

#[no_mangle]
pub unsafe extern "system" fn viWriteAsync(vi: ViSession, buf: *const u8, count: u32, job: *mut u32) -> ViStatus {
    op(vi, "viWriteAsync", || {
        if let Err(e) = io_session(vi) {
            return e;
        }
        let id = JOB.fetch_add(1, Ordering::Relaxed);
        put(job, id);
        let mut n = 0u32;
        let status = viWrite(vi, buf, count, &mut n);
        post_io(vi, status, id, n, buf as usize, "viWriteAsync");
        VI_SUCCESS_SYNC
    })
}

/// Every asynchronous operation here has completed before its call returned: there is never a job
/// to terminate.
#[no_mangle]
pub unsafe extern "system" fn viTerminate(vi: ViSession, degree: u16, _job: u32) -> ViStatus {
    op(vi, "viTerminate", || {
        if get(vi).is_none() {
            return VI_ERROR_INV_OBJECT;
        }
        if degree != 0 {
            return VI_ERROR_INV_DEGREE; // VI_NULL is the only degree VPP-4.3 defines
        }
        VI_ERROR_INV_JOB_ID
    })
}

/// Read into a file: a viRead of up to `count` bytes, whose bytes go to `filename` -- appended with
/// VI_ATTR_FILE_APPEND_EN, otherwise the file is written afresh. Read in pieces of at most 1 MiB,
/// so a large count does not need that much memory; the status is the last piece's.
#[no_mangle]
pub unsafe extern "system" fn viReadToFile(vi: ViSession, filename: *const c_char, count: u32, ret: *mut u32) -> ViStatus {
    op(vi, "viReadToFile", || {
        put(ret, 0u32);
        if let Err(e) = io_session(vi) {
            return e;
        }
        let Some(name) = cstr(filename) else { return VI_ERROR_INV_PARAMETER };
        let append = file_append(vi);
        let mut f = match std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(name) {
            Ok(f) => f,
            Err(_) => return VI_ERROR_FILE_ACCESS,
        };
        let mut total: u32 = 0;
        let mut status = VI_SUCCESS;
        let mut buf = vec![0u8; (count as usize).min(1 << 20)];
        while total < count {
            let want = (count - total).min(buf.len() as u32);
            let mut n = 0u32;
            status = viRead(vi, buf.as_mut_ptr(), want, &mut n);
            if f.write_all(&buf[..n as usize]).is_err() {
                put(ret, total);
                return VI_ERROR_FILE_IO;
            }
            total += n;
            // Only a piece that filled its count may have more behind it.
            if status != VI_SUCCESS_MAX_CNT {
                break;
            }
        }
        put(ret, total);
        status
    })
}

/// Write from a file: up to `count` bytes of it (fewer if it is shorter), as one viWrite -- one
/// message, so END goes with its last byte only.
#[no_mangle]
pub unsafe extern "system" fn viWriteFromFile(vi: ViSession, filename: *const c_char, count: u32, ret: *mut u32) -> ViStatus {
    op(vi, "viWriteFromFile", || {
        put(ret, 0u32);
        if let Err(e) = io_session(vi) {
            return e;
        }
        let Some(name) = cstr(filename) else { return VI_ERROR_INV_PARAMETER };
        let Ok(f) = std::fs::File::open(name) else { return VI_ERROR_FILE_ACCESS };
        let mut data = Vec::new();
        if f.take(count as u64).read_to_end(&mut data).is_err() {
            return VI_ERROR_FILE_IO;
        }
        viWrite(vi, data.as_ptr(), data.len() as u32, ret)
    })
}

/// A USB control transfer from a USB INSTR session's device, the setup packet as the caller gives
/// it. The direction bit of `request_type` must say IN.
#[no_mangle]
pub unsafe extern "system" fn viUsbControlIn(
    vi: ViSession, request_type: i16, request: i16, value: u16, index: u16, length: u16, buf: *mut u8, ret: *mut u16,
) -> ViStatus {
    op(vi, "viUsbControlIn", || {
        put(ret, 0u16);
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Target::Usb(n) = s.t else { return VI_ERROR_NSUP_OPER };
        if request_type & 0x80 == 0 || (length > 0 && buf.is_null()) {
            return VI_ERROR_INV_PARAMETER;
        }
        match with_session(&s, |c| c.usb_control(n, request_type as u8, request as u8, value, index, length, &[])) {
            Ok(d) => {
                let k = d.len().min(length as usize);
                if k > 0 {
                    std::ptr::copy_nonoverlapping(d.as_ptr(), buf, k);
                }
                put(ret, k as u16);
                VI_SUCCESS
            }
            Err(e) => map_err(&e),
        }
    })
}

/// A USB control transfer to a USB INSTR session's device. The direction bit must say OUT.
#[no_mangle]
pub unsafe extern "system" fn viUsbControlOut(
    vi: ViSession, request_type: i16, request: i16, value: u16, index: u16, length: u16, buf: *const u8,
) -> ViStatus {
    op(vi, "viUsbControlOut", || {
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Target::Usb(n) = s.t else { return VI_ERROR_NSUP_OPER };
        if request_type & 0x80 != 0 || (length > 0 && buf.is_null()) {
            return VI_ERROR_INV_PARAMETER;
        }
        let data = if length == 0 { &[][..] } else { std::slice::from_raw_parts(buf, length as usize) };
        match with_session(&s, |c| c.usb_control(n, request_type as u8, request as u8, value, index, length, data)) {
            Ok(_) => VI_SUCCESS,
            Err(e) => map_err(&e),
        }
    })
}

/// None of the adapters lets the host drive ATN by itself: ATN goes with the commands
/// (viGpibCommand) and comes down for data. Not supported, rather than pretended.
#[no_mangle]
pub unsafe extern "system" fn viGpibControlATN(vi: ViSession, _mode: u16) -> ViStatus {
    op(vi, "viGpibControlATN", || if get(vi).is_some() { VI_ERROR_NSUP_OPER } else { VI_ERROR_INV_OBJECT })
}

/// Every adapter here is the system controller and stays in charge.
#[no_mangle]
pub unsafe extern "system" fn viGpibPassControl(vi: ViSession, _prim: u16, _sec: u16) -> ViStatus {
    op(vi, "viGpibPassControl", || if get(vi).is_some() { VI_ERROR_NSUP_OPER } else { VI_ERROR_INV_OBJECT })
}
