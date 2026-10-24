//! The instrument operations: read, write, clear, trigger, status byte, REN, interface commands, buffers and locks.

use super::*;

/// What an operation needs from a session, copied out so no session lock is held across I/O.
pub(crate) struct Snap {
    pub(crate) intfc: bool,
    /// The instrument, as the daemon's requests name it.
    pub(crate) t: Target,
    pub(crate) usb: bool,
    pub(crate) asrl: Option<AsrlSess>,
    pub(crate) adapter: String,
    pub(crate) conn: Conn,
    pub(crate) board: u16,
    pub(crate) pad: u8,
    pub(crate) tmo: u32,
    pub(crate) term: Option<u8>,
    /// Only for a serial session's writes: the termination character to append (END_OUT TERMCHAR).
    pub(crate) termchar: u8,
    pub(crate) end: bool,
    pub(crate) tmo_ms: u32,
    pub(crate) suppress_end: bool,
    pub(crate) locked: Arc<AtomicBool>,
}

pub(crate) fn snap(vi: u32) -> Result<Snap, ViStatus> {
    match get(vi).as_deref() {
        Some(Object::Instr(m)) => {
            let s = m.lock().unwrap();
            Ok(Snap {
                intfc: s.intfc,
                t: target_of(&s),
                usb: s.usb.is_some(),
                asrl: s.asrl,
                adapter: s.adapter.clone(),
                conn: s.conn.clone(),
                board: s.board,
                pad: s.pad,
                tmo: fw_timeout(s.tmo_ms),
                // A serial read ends at the character when END_IN says so, whatever TERMCHAR_EN says.
                term: match s.asrl {
                    Some(a) => (a.end_in == VI_ASRL_END_TERMCHAR).then_some(s.termchar),
                    None => s.termchar_en.then_some(s.termchar),
                },
                termchar: s.termchar,
                end: s.send_end,
                tmo_ms: s.tmo_ms,
                suppress_end: s.suppress_end,
                locked: s.locked.clone(),
            })
        }
        Some(_) => Err(VI_ERROR_NSUP_OPER),
        None => Err(VI_ERROR_INV_OBJECT),
    }
}

#[no_mangle]
pub unsafe extern "system" fn viWrite(vi: ViSession, buf: *const u8, count: u32, ret: *mut u32) -> ViStatus {
    op_io(vi, "viWrite", || {
        put(ret, 0u32);
        if buf.is_null() && count > 0 && get(vi).is_some() {
            return VI_ERROR_INV_PARAMETER;
        }
        let data = if count == 0 { &[][..] } else { std::slice::from_raw_parts(buf, count as usize) };
        on_lan!(vi, |l| lan::write(l, data, ret));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let r = {
            let mut g = s.conn.lock().unwrap();
            if let Err(e) = ensure(&s, &mut g) {
                return map_err(&e);
            }
            let send = |c: &mut Client| {
                if let (Some(a), Target::Asrl(n)) = (s.asrl, s.t) {
                    // How a serial message ends is the session's: nothing, the termination
                    // character, or a break -- the last two only with SEND_END_EN.
                    let term = s.end && a.end_out == VI_ASRL_END_TERMCHAR;
                    let brk = (s.end && a.end_out == VI_ASRL_END_BREAK).then_some(a.break_len);
                    if term {
                        let mut d = data.to_vec();
                        d.push(s.termchar);
                        // The character is ours, not the caller's: it is not counted as written.
                        return c.asrl_write(n, &d, brk, s.tmo).map(|k| k.min(data.len())).map_err(|(k, e)| (k.min(data.len()), e));
                    }
                    return c.asrl_write(n, data, brk, s.tmo);
                }
                if s.intfc {
                    c.raw_write(s.board, data, s.end, s.tmo)
                } else {
                    c.write(s.t, data, s.end, s.tmo)
                }
            };
            let mut r = send(g.as_mut().unwrap());
            // Never sent (the daemon was already gone): once more on a new connection, as with_session does.
            if matches!(r, Err((_, Error::NotSent))) {
                *g = None;
                if let Err(e) = ensure(&s, &mut g) {
                    return map_err(&e);
                }
                r = send(g.as_mut().unwrap());
            }
            if matches!(r, Err((_, Error::Gone | Error::NotSent))) {
                drop_conn(&s, &mut g);
            }
            r
        };
        match r {
            Ok(n) => {
                put(ret, n as u32);
                VI_SUCCESS
            }
            Err((n, e)) => {
                put(ret, n as u32);
                map_err(&e)
            }
        }
    }, || {
        // What was asked to be written, and how much of it went (all of it, when the caller did not ask).
        let n = if ret.is_null() { count } else { *ret };
        let shown = if buf.is_null() { &[][..] } else { std::slice::from_raw_parts(buf, count.min(symbiotic_core::iotrace::DATA_MAX as u32) as usize) };
        (n, shown.to_vec())
    })
}

/// viRead's loop, apart from where the bytes come from: `read_once(want, tmo)` is one read through
/// the daemon of at most `want` bytes with the protocol timeout `tmo`; what arrives is appended to
/// `out` (never more than `count`). Without VI_ATTR_SUPPRESS_END_EN it is one read. With it, END does
/// not end the read, which goes on to the termination character, the count or the timeout -- the
/// timeout being the whole read's, each read getting what is left of it.
pub(crate) fn read_loop(
    count: u32,
    term: Option<u8>,
    suppress_end: bool,
    tmo_ms: u32,
    tmo: u32,
    out: &mut Vec<u8>,
    mut read_once: impl FnMut(u32, u32) -> Result<symbiotic_core::adapter::ReadResult, Error>,
) -> ViStatus {
    let deadline = (suppress_end && tmo_ms != VI_TMO_INFINITE).then(|| std::time::Instant::now() + Duration::from_millis(tmo_ms as u64));
    let mut first = true;
    loop {
        let tmo = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(std::time::Instant::now());
                // Over between reads, never before the first (a timeout of 0 still reads once).
                if left.is_zero() && !first {
                    return VI_ERROR_TMO;
                }
                // Rounded up: a read is never given less than is left.
                fw_timeout((left.as_micros().div_ceil(1000) as u32).max(1))
            }
            None => tmo,
        };
        first = false;
        let want = count - out.len() as u32;
        let r = match read_once(want, tmo) {
            Ok(r) => r,
            Err(e) => return map_err(&e),
        };
        let n = r.data.len().min(want as usize);
        out.extend_from_slice(&r.data[..n]);
        return match r.end {
            // END suppressed: the termination character still ends the read, even when it came
            // with END on the same byte (and the adapter called it END).
            ReadEnd::End if suppress_end && n > 0 && term.is_some() && out.last() == term.as_ref() => VI_SUCCESS_TERM_CHAR,
            // END suppressed and the count reached: the count is what ended it (VPP-4.3, viRead).
            ReadEnd::End if suppress_end && out.len() as u32 >= count => VI_SUCCESS_MAX_CNT,
            ReadEnd::End if suppress_end => continue,
            ReadEnd::End => VI_SUCCESS,
            ReadEnd::TermChar => VI_SUCCESS_TERM_CHAR,
            ReadEnd::MaxCount => VI_SUCCESS_MAX_CNT,
            ReadEnd::Timeout => VI_ERROR_TMO, // the bytes that did arrive are returned with it
        };
    }
}

#[no_mangle]
pub unsafe extern "system" fn viRead(vi: ViSession, buf: *mut u8, count: u32, ret: *mut u32) -> ViStatus {
    // Where the count goes when the caller gives nowhere: the trace still says how much came.
    let mut own = 0u32;
    let ret = if ret.is_null() { &mut own as *mut u32 } else { ret };
    op_io(vi, "viRead", || {
        put(ret, 0u32);
        if buf.is_null() && count > 0 && get(vi).is_some() {
            return VI_ERROR_INV_PARAMETER;
        }
        on_lan!(vi, |l| lan::read(l, buf, count, ret));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut out = Vec::new();
        let st = read_loop(count, s.term, s.suppress_end, s.tmo_ms, s.tmo, &mut out, |want, tmo| {
            with_session(&s, |c| if s.intfc { c.raw_read(s.board, want, s.term, tmo) } else { c.read(s.t, want, s.term, tmo) })
        });
        // What arrived is the caller's whatever the status, a timeout's and an error's too.
        if !out.is_empty() {
            std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
        }
        put(ret, out.len() as u32);
        st
    }, || {
        let n = (*ret).min(count);
        let shown = if buf.is_null() { &[][..] } else { std::slice::from_raw_parts(buf, n.min(symbiotic_core::iotrace::DATA_MAX as u32) as usize) };
        (n, shown.to_vec())
    })
}

#[no_mangle]
pub unsafe extern "system" fn viClear(vi: ViSession) -> ViStatus {
    op(vi, "viClear", || {
        on_lan!(vi, |l| lan::clear(l));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if s.intfc {
            return VI_ERROR_NSUP_OPER; // addressed to an instrument; an INTFC session has none
        }
        if let (Some(a), Target::Asrl(n)) = (s.asrl, s.t) {
            // Output thrown away, a break (or `*CLS\n` to an IEEE 488.2 instrument), input thrown away.
            let strings = a.io_prot == VI_PROT_4882_STRS;
            return with_session(&s, |c| c.asrl_clear(n, strings, a.break_len, s.tmo)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS);
        }
        with_session(&s, |c| c.clear(s.t)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS)
    })
}

#[no_mangle]
pub unsafe extern "system" fn viAssertTrigger(vi: ViSession, protocol: u16) -> ViStatus {
    op(vi, "viAssertTrigger", || {
        on_lan!(vi, |l| lan::trigger(l, protocol));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if s.intfc {
            return VI_ERROR_NSUP_OPER; // addressed to an instrument; an INTFC session has none
        }
        if protocol != 0 {
            return VI_ERROR_INV_PROT; // only VI_TRIG_PROT_DEFAULT: GET (VPP-4.3 6.1.7's code for the rest)
        }
        if let (Some(a), Target::Asrl(n)) = (s.asrl, s.t) {
            // `*TRG\n`, for an instrument that takes IEEE 488.2 strings; otherwise there is no trigger.
            if a.io_prot != VI_PROT_4882_STRS {
                return VI_ERROR_NSUP_OPER;
            }
            return with_session(&s, |c| c.asrl_trigger(n, s.tmo)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS);
        }
        with_session(&s, |c| c.trigger(s.t)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS)
    })
}

#[no_mangle]
pub unsafe extern "system" fn viReadSTB(vi: ViSession, stb: *mut u16) -> ViStatus {
    op(vi, "viReadSTB", || {
        on_lan!(vi, |l| lan::read_stb(l, stb));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if s.intfc {
            return VI_ERROR_NSUP_OPER; // addressed to an instrument; an INTFC session has none
        }
        // A serial instrument has a status byte only if it takes IEEE 488.2 strings (`*STB?\n`).
        if s.asrl.is_some_and(|a| a.io_prot != VI_PROT_4882_STRS) {
            return VI_ERROR_NSUP_OPER;
        }
        match with_session(&s, |c| c.spoll(s.t, s.tmo)) {
            Ok(b) => {
                put(stb, b as u16);
                VI_SUCCESS
            }
            Err(e) => map_err(&e),
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn viGpibControlREN(vi: ViSession, mode: u16) -> ViStatus {
    op(vi, "viGpibControlREN", || {
        on_lan!(vi, |l| lan::ren(l, mode));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if s.asrl.is_some() {
            return VI_ERROR_NSUP_OPER; // no REN on a serial line
        }
        if s.intfc {
            // The board itself: REN and the bus-wide messages only; the modes that name a device
            // have nobody to address.
            let r = match mode {
                VI_GPIB_REN_DEASSERT | VI_GPIB_REN_DEASSERT_GTL => with_session(&s, |c| c.ren(s.t, false)),
                VI_GPIB_REN_ASSERT => with_session(&s, |c| c.ren(s.t, true)),
                VI_GPIB_REN_ASSERT_LLO => with_session(&s, |c| c.llo(s.t)),
                _ => return VI_ERROR_NSUP_MODE,
            };
            return r.map_or_else(|e| map_err(&e), |_| VI_SUCCESS);
        }
        // VPP-4.3's modes. None of the adapters can address a device without moving data, so
        // ASSERT_ADDRESS and ASSERT_ADDRESS_LLO assert REN (and send LLO) and leave the addressing
        // to the device's next I/O, which makes it remote (in lockout: RWLS) then.
        let r = match mode {
            VI_GPIB_REN_DEASSERT => with_session(&s, |c| c.ren(s.t, false)),
            VI_GPIB_REN_ASSERT | VI_GPIB_REN_ASSERT_ADDRESS => with_session(&s, |c| c.ren(s.t, true)),
            // GTL to this device, then REN deasserted -- sending GTL alone left REN asserted.
            VI_GPIB_REN_DEASSERT_GTL => {
                with_session(&s, |c| c.local(s.t).and_then(|_| c.ren(s.t, false)))
            }
            // LLO to whoever is addressed to listen: REN is not this mode's business.
            VI_GPIB_REN_ASSERT_LLO => with_session(&s, |c| c.llo(s.t)),
            VI_GPIB_REN_ASSERT_ADDRESS_LLO => {
                with_session(&s, |c| c.ren(s.t, true).and_then(|_| c.llo(s.t)))
            }
            VI_GPIB_REN_ADDRESS_GTL => with_session(&s, |c| c.local(s.t)),
            _ => return VI_ERROR_NSUP_MODE,
        };
        match r {
            // A USBTMC instrument that does not do GO_TO_LOCAL or LOCAL_LOCKOUT (it stalls them): the
            // mode is what it cannot do.
            Err(Error::Status(proto::ST_UNSUPPORTED)) if s.usb => VI_ERROR_NSUP_MODE,
            r => r.map_or_else(|e| map_err(&e), |_| VI_SUCCESS),
        }
    })
}

/// VI_ATTR_GPIB_ADDR_STATE, from interface messages an INTFC session sent: its own address (0)
/// named as listener or talker, another talker named (so not this board), UNL, UNT.
pub(crate) fn track_addressing(vi: ViSession, cmds: &[u8]) {
    if let Some(Object::Instr(m)) = get(vi).as_deref() {
        let mut s = m.lock().unwrap();
        for &b in cmds {
            let b = b & 0x7F;
            s.addr_state = match b {
                0x3F if s.addr_state == VI_GPIB_LISTENER => VI_GPIB_UNADDRESSED, // UNL
                0x5F if s.addr_state == VI_GPIB_TALKER => VI_GPIB_UNADDRESSED,   // UNT
                0x20 => VI_GPIB_LISTENER,                                        // MLA
                0x40 => VI_GPIB_TALKER,                                          // MTA
                0x41..=0x5E if s.addr_state == VI_GPIB_TALKER => VI_GPIB_UNADDRESSED, // another talker
                _ => s.addr_state,
            };
        }
    }
}

/// Interface messages, sent with ATN (an INTFC session only); ATN stays asserted afterwards.
#[no_mangle]
pub unsafe extern "system" fn viGpibCommand(vi: ViSession, buf: *const u8, count: u32, ret: *mut u32) -> ViStatus {
    op(vi, "viGpibCommand", || {
        put(ret, 0u32);
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if !s.intfc {
            return VI_ERROR_NSUP_OPER;
        }
        if buf.is_null() || count == 0 {
            return VI_ERROR_INV_PARAMETER;
        }
        let cmds = std::slice::from_raw_parts(buf, count as usize);
        match with_session(&s, |c| c.command(s.board, cmds, s.tmo)) {
            Ok(()) => {
                track_addressing(vi, cmds);
                put(ret, count);
                VI_SUCCESS
            }
            Err(e) => map_err(&e),
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn viGpibSendIFC(vi: ViSession) -> ViStatus {
    op(vi, "viGpibSendIFC", || {
        on_lan!(vi, |l| lan::ifc(l));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        if s.usb || s.asrl.is_some() {
            return VI_ERROR_NSUP_OPER; // a USBTMC or serial instrument has no bus to clear
        }
        if s.intfc {
            if let Some(Object::Instr(m)) = get(vi).as_deref() {
                m.lock().unwrap().addr_state = VI_GPIB_UNADDRESSED; // IFC unaddresses everyone
            }
        }
        with_session(&s, |c| c.ifc(s.board, s.pad)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS)
    })
}

/// Writes are sent as they are made and reads return what the instrument sent, so there is
/// no buffer to flush or size: both succeed and do nothing, honestly.
pub(crate) fn instr_only(vi: ViSession) -> ViStatus {
    match get(vi).as_deref() {
        Some(Object::Instr(_)) => VI_SUCCESS,
        Some(_) => VI_ERROR_NSUP_OPER,
        None => VI_ERROR_INV_OBJECT,
    }
}

#[no_mangle]
pub unsafe extern "system" fn viFlush(vi: ViSession, mask: u16) -> ViStatus {
    op(vi, "viFlush", || {
        if !matches!(get(vi).as_deref(), Some(Object::Instr(_) | Object::Lan(_))) {
            return instr_only(vi);
        }
        // The formatted I/O buffers first (fmtio.c): VI_READ_BUF(_DISCARD), VI_WRITE_BUF(_DISCARD).
        let st = symbiotic_fio_flush(vi, mask);
        if st < 0 {
            return st;
        }
        // A flush of the formatted buffers flushes a serial port's matching one too (VPP-4.3,
        // viFlush): what was read is dropped with what was buffered from it.
        let mask = if matches!(snap(vi).map(|s| s.t), Ok(Target::Asrl(_))) {
            let mut m = mask;
            if mask & (VI_READ_BUF | VI_READ_BUF_DISCARD) != 0 {
                m |= VI_IO_IN_BUF_DISCARD;
            }
            if mask & VI_WRITE_BUF != 0 {
                m |= VI_IO_OUT_BUF;
            }
            m
        } else {
            mask
        };
        on_lan!(vi, |l| lan::flush(l, mask));
        // A serial port has buffers of its own, in the driver (and what the daemon kept of a read).
        if let Ok(s) = snap(vi) {
            if let Target::Asrl(n) = s.t {
                use symbiotic_core::serial::flush;
                let mut what = 0;
                if mask & (VI_IO_IN_BUF | VI_IO_IN_BUF_DISCARD) != 0 {
                    what |= flush::IN_DISCARD;
                }
                if mask & VI_IO_OUT_BUF != 0 {
                    what |= flush::OUT_DRAIN;
                }
                if mask & VI_IO_OUT_BUF_DISCARD != 0 {
                    what |= flush::OUT_DISCARD;
                }
                if what == 0 {
                    return VI_SUCCESS;
                }
                return with_session(&s, |c| c.asrl_flush(n, what, s.tmo)).map_or_else(|e| map_err(&e), |_| VI_SUCCESS);
            }
        }
        instr_only(vi)
    })
}

/// The formatted I/O buffers' sizes are fmtio.c's; the other buffers (a serial port's, the drivers')
/// are not sized from here, and VPP-4.3 (RULE 6.2.3, 6.2.4) lets that succeed.
#[no_mangle]
pub unsafe extern "system" fn viSetBuf(vi: ViSession, mask: u16, size: u32) -> ViStatus {
    op(vi, "viSetBuf", || {
        if !matches!(get(vi).as_deref(), Some(Object::Instr(_) | Object::Lan(_))) {
            return instr_only(vi);
        }
        if mask & (VI_READ_BUF | VI_WRITE_BUF) != 0 {
            return symbiotic_fio_setbuf(vi, mask & (VI_READ_BUF | VI_WRITE_BUF), size);
        }
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viLock(
    vi: ViSession, lock_type: u32, timeout: u32, requested_key: *const c_char, access_key: *mut c_char,
) -> ViStatus {
    op(vi, "viLock", || {
        // A key that would not come back whole in a 256-byte buffer is refused, not cut short
        // (VPP-4.3 RULE 3.6.17): cut, two different keys could end up sharing one lock.
        if lock_type == VI_SHARED_LOCK && !requested_key.is_null() && CStr::from_ptr(requested_key).to_bytes().len() >= 256 {
            return VI_ERROR_INV_ACCESS_KEY;
        }
        on_lan!(vi, |l| lan::lock(l, lock_type, timeout, cstr(requested_key).filter(|k| !k.is_empty()), access_key));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let shared = match lock_type {
            VI_EXCLUSIVE_LOCK => false,
            VI_SHARED_LOCK => true,
            _ => return VI_ERROR_INV_LOCK_TYPE,
        };
        // An exclusive lock has no key; a shared one uses the requested key, or gets one made up.
        let key = if shared { cstr(requested_key) } else { None };
        let wait = if timeout == VI_TMO_INFINITE { u32::MAX } else { timeout };
        match with_session(&s, |c| c.lock(s.t, shared, wait, key)) {
            Ok((nested, key)) => {
                s.locked.store(true, Ordering::SeqCst);
                if shared {
                    put_str(access_key, &key);
                }
                match (nested, shared) {
                    (false, _) => VI_SUCCESS,
                    (true, false) => VI_SUCCESS_NESTED_EXCLUSIVE,
                    (true, true) => VI_SUCCESS_NESTED_SHARED,
                }
            }
            Err(e) => map_err(&e),
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn viUnlock(vi: ViSession) -> ViStatus {
    op(vi, "viUnlock", || {
        on_lan!(vi, |l| lan::unlock(l));
        let s = match snap(vi) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let r = with_session(&s, |c| c.unlock(s.t));
        // One still held (a nested level, or the other kind) keeps the session locked.
        s.locked.store(matches!(r, Ok(1 | 2)), Ordering::SeqCst);
        match r {
            Ok(1) => VI_SUCCESS_NESTED_EXCLUSIVE,
            Ok(2) => VI_SUCCESS_NESTED_SHARED,
            Ok(_) => VI_SUCCESS,
            Err(e) => map_err(&e),
        }
    })
}
