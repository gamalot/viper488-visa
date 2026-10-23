//! This runtime's own VISA C API, called in-process (symbiotic-visa built as an rlib too): the
//! interactive I/O goes the way an application's does, whatever the resource.

use std::ffi::{c_char, CStr, CString};

use visa64::{viClear, viClose, viFindNext, viFindRsrc, viOpen, viOpenDefaultRM, viRead, viReadSTB, viSetAttribute, viStatusDesc};

const VI_ATTR_TMO_VALUE: u32 = 0x3FFF_001A;
const VI_ATTR_TERMCHAR: u32 = 0x3FFF_0018;
const VI_ATTR_TERMCHAR_EN: u32 = 0x3FFF_0038;
const VI_ERROR_RSRC_NFOUND: i32 = 0xBFFF_0011_u32 as i32;
const VI_SUCCESS_MAX_CNT: i32 = 0x3FFF_0006;

fn describe(status: i32) -> String {
    let mut buf = [0 as c_char; 256];
    // SAFETY: the buffer is VI_FIND_BUFLEN long, as viStatusDesc needs.
    unsafe { viStatusDesc(0, status, buf.as_mut_ptr()) };
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    if s.is_empty() { format!("VISA status 0x{status:08X}") } else { format!("{s} (0x{status:08X})") }
}

fn check(status: i32) -> Result<i32, String> {
    if status < 0 { Err(describe(status)) } else { Ok(status) }
}

/// A default resource manager for the life of the program.
pub fn rm() -> Result<u32, String> {
    static RM: std::sync::OnceLock<Result<u32, String>> = std::sync::OnceLock::new();
    RM.get_or_init(|| {
        let mut rm = 0u32;
        // SAFETY: a pointer to a u32 the call fills.
        check(unsafe { viOpenDefaultRM(&mut rm) })?;
        Ok(rm)
    })
    .clone()
}

/// viFindRsrc and viFindNext to the end; nothing found is an empty list.
pub fn find(expr: &str) -> Result<Vec<String>, String> {
    let rm = rm()?;
    let e = CString::new(expr).map_err(|e| e.to_string())?;
    let mut list = 0u32;
    let mut count = 0u32;
    let mut desc = [0 as c_char; 256];
    // SAFETY: NUL-terminated expression, VI_FIND_BUFLEN buffer.
    let st = unsafe { viFindRsrc(rm, e.as_ptr(), &mut list, &mut count, desc.as_mut_ptr()) };
    if st == VI_ERROR_RSRC_NFOUND {
        return Ok(Vec::new());
    }
    check(st)?;
    let mut out = vec![unsafe { CStr::from_ptr(desc.as_ptr()) }.to_string_lossy().into_owned()];
    for _ in 1..count {
        // SAFETY: as above.
        if check(unsafe { viFindNext(list, desc.as_mut_ptr()) }).is_err() {
            break;
        }
        out.push(unsafe { CStr::from_ptr(desc.as_ptr()) }.to_string_lossy().into_owned());
    }
    // SAFETY: the find list this call made.
    unsafe { viClose(list) };
    Ok(out)
}

/// An open session (closed when dropped).
pub struct Session(u32);

impl Session {
    pub fn open(name: &str) -> Result<Session, String> {
        let rm = rm()?;
        let n = CString::new(name).map_err(|e| e.to_string())?;
        let mut vi = 0u32;
        // SAFETY: NUL-terminated name, a u32 to fill.
        check(unsafe { viOpen(rm, n.as_ptr(), 0, 2000, &mut vi) })?;
        Ok(Session(vi))
    }

    pub fn setup(&self, timeout_ms: u32, termchar: Option<u8>) -> Result<(), String> {
        // SAFETY: a session this program opened.
        unsafe {
            check(viSetAttribute(self.0, VI_ATTR_TMO_VALUE, timeout_ms as _))?;
            check(viSetAttribute(self.0, VI_ATTR_TERMCHAR_EN, termchar.is_some() as _))?;
            if let Some(c) = termchar {
                check(viSetAttribute(self.0, VI_ATTR_TERMCHAR, c as _))?;
            }
        }
        Ok(())
    }

    pub fn write(&self, data: &[u8]) -> Result<usize, String> {
        let mut n = 0u32;
        // SAFETY: the buffer and its length.
        check(unsafe { visa64::viWrite(self.0, data.as_ptr(), data.len() as u32, &mut n) })?;
        Ok(n as usize)
    }

    /// One message, however many viReads it takes (while each says the count was reached). An
    /// error comes with the bytes that did arrive: a read that times out returns them with
    /// VI_ERROR_TMO (an instrument that sends neither the termination character nor EOI).
    pub fn read(&self) -> Result<Vec<u8>, (Vec<u8>, String)> {
        let mut out = Vec::new();
        loop {
            let mut buf = vec![0u8; 65536];
            let mut n = 0u32;
            // SAFETY: the buffer and its length.
            let st = unsafe { viRead(self.0, buf.as_mut_ptr(), buf.len() as u32, &mut n) };
            out.extend_from_slice(&buf[..n as usize]);
            if let Err(e) = check(st) {
                return Err((out, e));
            }
            if st != VI_SUCCESS_MAX_CNT {
                return Ok(out);
            }
        }
    }

    pub fn read_stb(&self) -> Result<u16, String> {
        let mut stb = 0u16;
        // SAFETY: a u16 to fill.
        check(unsafe { viReadSTB(self.0, &mut stb) })?;
        Ok(stb)
    }

    pub fn clear(&self) -> Result<(), String> {
        // SAFETY: a session this program opened.
        check(unsafe { viClear(self.0) }).map(|_| ())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session this value owns.
        unsafe { viClose(self.0) };
    }
}
