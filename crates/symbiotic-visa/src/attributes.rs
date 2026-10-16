//! Attributes (viGetAttribute, viSetAttribute) and status text (viStatusDesc).

use super::*;

/// The GPIB line and controller states. The lines are read from the adapter (LINES); one that
/// cannot report them says VI_STATE_UNKNOWN. The controller attributes belong to INTFC sessions,
/// and REN's to both.
pub(crate) unsafe fn gpib_state(vi: ViSession, attr: u32, out: *mut c_void) -> ViStatus {
    let s = match snap(vi) {
        Ok(s) => s,
        Err(_) => return VI_ERROR_NSUP_ATTR,
    };
    if s.usb || s.asrl.is_some() {
        return VI_ERROR_NSUP_ATTR; // no GPIB lines
    }
    if !s.intfc && attr != VI_ATTR_GPIB_REN_STATE {
        return VI_ERROR_NSUP_ATTR;
    }
    if attr == VI_ATTR_GPIB_CIC_STATE || attr == VI_ATTR_GPIB_SYS_CNTRL_STATE {
        // Every adapter here is the system controller and stays in charge: nothing passes control.
        put(out as *mut u16, 1u16);
        return VI_SUCCESS;
    }
    let bit = match attr {
        VI_ATTR_GPIB_ATN_STATE => proto::BUS_ATN,
        VI_ATTR_GPIB_NDAC_STATE => proto::BUS_NDAC,
        VI_ATTR_GPIB_SRQ_STATE => proto::BUS_SRQ,
        _ => proto::BUS_REN,
    };
    let state: i16 = match with_session(&s, |c| c.lines(s.board)) {
        Ok((_, known)) if known & bit == 0 => -1, // VI_STATE_UNKNOWN: the adapter cannot tell this line
        Ok((l, _)) => (l & bit != 0) as i16,
        Err(Error::Status(proto::ST_UNSUPPORTED)) => -1, // VI_STATE_UNKNOWN
        Err(e) => return map_err(&e),
    };
    put(out as *mut i16, state);
    VI_SUCCESS
}

#[no_mangle]
pub unsafe extern "system" fn viGetAttribute(vi: ViSession, attr: u32, out: *mut c_void) -> ViStatus {
    op(vi, "viGetAttribute", || {
        let o = get(vi);
        let Some(o) = o.as_deref() else { return VI_ERROR_INV_OBJECT };
        if out.is_null() {
            return VI_ERROR_INV_PARAMETER;
        }
        if let Object::Event(e) = o {
            let io = e.ty == VI_EVENT_IO_COMPLETION;
            let exc = e.ty == VI_EVENT_EXCEPTION;
            match attr {
                VI_ATTR_EVENT_TYPE => put(out as *mut u32, e.ty),
                VI_ATTR_STATUS if io || exc => put(out as *mut i32, e.status),
                VI_ATTR_JOB_ID if io => put(out as *mut u32, e.job),
                VI_ATTR_RET_COUNT_32 if io => put(out as *mut u32, e.ret as u32),
                VI_ATTR_RET_COUNT_64 if io => put(out as *mut u64, e.ret),
                VI_ATTR_BUFFER if io => put(out as *mut usize, e.buf),
                VI_ATTR_OPER_NAME if io || exc => put_str(out as *mut c_char, &e.oper),
                _ => return VI_ERROR_NSUP_ATTR,
            }
            return VI_SUCCESS;
        }
        match attr {
            VI_ATTR_RSRC_IMPL_VERSION => return put(out as *mut u32, IMPL_VERSION).ok(),
            VI_ATTR_RSRC_SPEC_VERSION => return put(out as *mut u32, SPEC_VERSION).ok(),
            VI_ATTR_RSRC_MANF_NAME => return put_str(out as *mut c_char, "Viper488 Symbiotic").ok(),
            _ => {}
        }
        // The formatted I/O buffers' modes and sizes (fmtio.c), for every instrument session.
        if matches!(o, Object::Instr(_) | Object::Lan(_)) {
            let mut v = 0u64;
            let st = symbiotic_fio_attr(vi, attr, 0, &mut v);
            if st != 1 {
                if st >= 0 {
                    if attr == VI_ATTR_RD_BUF_SIZE || attr == VI_ATTR_WR_BUF_SIZE {
                        put(out as *mut u32, v as u32);
                    } else {
                        put(out as *mut u16, v as u16);
                    }
                }
                return st;
            }
        }
        // Where viReadToFile puts what it reads: every instrument session has this one.
        if attr == VI_ATTR_FILE_APPEND_EN && matches!(o, Object::Instr(_) | Object::Lan(_)) {
            put(out as *mut u16, transfers::file_append(vi) as u16);
            return VI_SUCCESS;
        }
        if let Object::Lan(l) = o {
            return lan::get_attribute(l, attr, out);
        }
        if attr == VI_ATTR_RSRC_LOCK_STATE {
            // The resource's state, whoever holds the lock: only the daemon knows.
            let s = match snap(vi) {
                Ok(s) => s,
                Err(_) => return VI_ERROR_NSUP_ATTR,
            };
            return match with_session(&s, |c| c.lock_state(s.t)) {
                Ok(st) => put(out as *mut u32, st).ok(),
                Err(e) => map_err(&e),
            };
        }
        if matches!(
            attr,
            VI_ATTR_GPIB_ATN_STATE | VI_ATTR_GPIB_NDAC_STATE | VI_ATTR_GPIB_SRQ_STATE | VI_ATTR_GPIB_REN_STATE
                | VI_ATTR_GPIB_CIC_STATE | VI_ATTR_GPIB_SYS_CNTRL_STATE
        ) {
            return gpib_state(vi, attr, out);
        }
        if let Some(r) = asrl_get(vi, attr, out) {
            return r;
        }
        let Object::Instr(m) = o else { return VI_ERROR_NSUP_ATTR };
        let s = m.lock().unwrap();
        // VPP-4.3's generic INSTR and INTFC attributes (section 5) with one answer here.
        let gpib_instr = !s.intfc && s.usb.is_none() && s.asrl.is_none();
        match attr {
            VI_ATTR_DMA_ALLOW_EN => return put(out as *mut u16, s.dma as u16).ok(),
            VI_ATTR_TRIG_ID if !s.intfc => return put(out as *mut i16, VI_TRIG_SW).ok(),
            VI_ATTR_IO_PROT if !s.intfc => return put(out as *mut u16, VI_PROT_NORMAL).ok(),
            VI_ATTR_SUPPRESS_END_EN if !s.intfc => return put(out as *mut u16, s.suppress_end as u16).ok(),
            VI_ATTR_GPIB_UNADDR_EN if gpib_instr => return put(out as *mut u16, 0u16).ok(),
            VI_ATTR_DEV_STATUS_BYTE if s.intfc => return put(out as *mut u8, s.dev_status).ok(),
            VI_ATTR_GPIB_HS488_CBL_LEN if s.intfc => return put(out as *mut i16, VI_GPIB_HS488_NIMPL).ok(),
            VI_ATTR_GPIB_ADDR_STATE if s.intfc => return put(out as *mut i16, s.addr_state).ok(),
            VI_ATTR_USB_MAX_INTR_SIZE if s.usb.is_some() => return put(out as *mut u16, s.usb_intr_max).ok(),
            _ => {}
        }
        if let Some(u) = &s.usb {
            match attr {
                VI_ATTR_INTF_TYPE => put(out as *mut u16, VI_INTF_USB),
                VI_ATTR_INTF_NUM => put(out as *mut u16, u.name.board),
                VI_ATTR_INTF_INST_NAME => put_str(out as *mut c_char, &format!("USB{} (USBTMC)", u.name.board)),
                VI_ATTR_MANF_ID => put(out as *mut u16, u.name.vid),
                VI_ATTR_MODEL_CODE => put(out as *mut u16, u.name.pid),
                VI_ATTR_MANF_NAME => put_str(out as *mut c_char, &u.manufacturer),
                VI_ATTR_MODEL_NAME => put_str(out as *mut c_char, &u.product),
                VI_ATTR_USB_SERIAL_NUM => put_str(out as *mut c_char, &u.name.serial),
                VI_ATTR_USB_INTFC_NUM => put(out as *mut i16, u.iface as i16),
                VI_ATTR_USB_PROTOCOL => put(out as *mut i16, u.usb488 as i16),
                VI_ATTR_4882_COMPLIANT => put(out as *mut u16, u.usb488 as u16),
                VI_ATTR_GPIB_READDR_EN | VI_ATTR_GPIB_PRIMARY_ADDR | VI_ATTR_GPIB_SECONDARY_ADDR => return VI_ERROR_NSUP_ATTR,
                _ => return common_attr(&s, attr, out),
            }
            return VI_SUCCESS;
        }
        match attr {
            VI_ATTR_RSRC_CLASS => put_str(out as *mut c_char, if s.intfc { "INTFC" } else { "INSTR" }),
            VI_ATTR_RSRC_NAME => put_str(out as *mut c_char, &s.name),
            VI_ATTR_INTF_INST_NAME => put_str(out as *mut c_char, &format!("Viper488 adapter (GPIB{})", s.board)),
            VI_ATTR_MAX_QUEUE_LENGTH => put(out as *mut u32, s.max_queue),
            VI_ATTR_TMO_VALUE => put(out as *mut u32, s.tmo_ms),
            VI_ATTR_TERMCHAR => put(out as *mut u8, s.termchar),
            VI_ATTR_TERMCHAR_EN => put(out as *mut u16, s.termchar_en as u16),
            VI_ATTR_SEND_END_EN => put(out as *mut u16, s.send_end as u16),
            VI_ATTR_GPIB_READDR_EN => put(out as *mut u16, s.readdr as u16),
            VI_ATTR_INTF_TYPE => put(out as *mut u16, VI_INTF_GPIB),
            VI_ATTR_INTF_NUM => put(out as *mut u16, s.board),
            // An INTFC session's primary address is the board's own, the controller's: 0.
            VI_ATTR_GPIB_PRIMARY_ADDR => put(out as *mut u16, if s.intfc { 0 } else { s.pad as u16 }),
            VI_ATTR_GPIB_SECONDARY_ADDR => put(out as *mut u16, if s.sad == NO_SAD { VI_NO_SEC_ADDR } else { s.sad as u16 }),
            VI_ATTR_USER_DATA_32 => put(out as *mut u32, s.user_data as u32),
            VI_ATTR_USER_DATA_64 => put(out as *mut u64, s.user_data),
            _ => return VI_ERROR_NSUP_ATTR,
        }
        VI_SUCCESS
    })
}

/// A serial session's attributes, if `vi` is one: the session's own, and the port's, which are
/// asked of the daemon (another process may have changed them). None: not a serial session, or
/// an attribute every session has (common_attr).
pub(crate) unsafe fn asrl_get(vi: ViSession, attr: u32, out: *mut c_void) -> Option<ViStatus> {
    let s = snap(vi).ok()?;
    let (Some(a), Target::Asrl(n)) = (s.asrl, s.t) else { return None };
    let port = |f: &dyn Fn(&symbiotic_core::serial::Status)| match with_session(&s, |c| c.asrl_get(n)) {
        Ok(st) => {
            f(&st);
            VI_SUCCESS
        }
        Err(e) => map_err(&e),
    };
    let state = |b: bool| if b { VI_STATE_ASSERTED } else { VI_STATE_UNASSERTED };
    Some(match attr {
        VI_ATTR_INTF_TYPE => put(out as *mut u16, VI_INTF_ASRL).ok(),
        VI_ATTR_INTF_NUM => put(out as *mut u16, n).ok(),
        VI_ATTR_INTF_INST_NAME => put_str(out as *mut c_char, &symbiotic_core::serial::port_name(n)).ok(),
        VI_ATTR_ASRL_END_IN => put(out as *mut u16, a.end_in).ok(),
        VI_ATTR_ASRL_END_OUT => put(out as *mut u16, a.end_out).ok(),
        VI_ATTR_ASRL_BREAK_LEN => put(out as *mut i16, a.break_len as i16).ok(),
        VI_ATTR_IO_PROT => put(out as *mut u16, a.io_prot).ok(),
        // Fixed here: nulls are kept, no character replaces a bad one, the line may send.
        VI_ATTR_ASRL_DISCARD_NULL => put(out as *mut u16, 0u16).ok(),
        VI_ATTR_ASRL_REPLACE_CHAR => put(out as *mut u8, 0u8).ok(),
        VI_ATTR_ASRL_ALLOW_TRANSMIT => put(out as *mut u16, 1u16).ok(),
        VI_ATTR_ASRL_BAUD => port(&|x| put(out as *mut u32, x.cfg.baud)),
        VI_ATTR_ASRL_DATA_BITS => port(&|x| put(out as *mut u16, x.cfg.data_bits as u16)),
        VI_ATTR_ASRL_PARITY => port(&|x| put(out as *mut u16, x.cfg.parity as u16)),
        VI_ATTR_ASRL_STOP_BITS => port(&|x| put(out as *mut u16, x.cfg.stop_bits as u16)),
        VI_ATTR_ASRL_FLOW_CNTRL => port(&|x| put(out as *mut u16, x.cfg.flow as u16)),
        VI_ATTR_ASRL_XON_CHAR => port(&|x| put(out as *mut u8, x.cfg.xon)),
        VI_ATTR_ASRL_XOFF_CHAR => port(&|x| put(out as *mut u8, x.cfg.xoff)),
        VI_ATTR_ASRL_DTR_STATE => port(&|x| put(out as *mut i16, state(x.cfg.dtr))),
        VI_ATTR_ASRL_RTS_STATE => port(&|x| put(out as *mut i16, state(x.cfg.rts))),
        VI_ATTR_ASRL_AVAIL_NUM => port(&|x| put(out as *mut u32, x.avail)),
        VI_ATTR_ASRL_CTS_STATE => port(&|x| put(out as *mut i16, state(x.cts))),
        VI_ATTR_ASRL_DSR_STATE => port(&|x| put(out as *mut i16, state(x.dsr))),
        VI_ATTR_ASRL_DCD_STATE => port(&|x| put(out as *mut i16, state(x.dcd))),
        VI_ATTR_ASRL_RI_STATE => port(&|x| put(out as *mut i16, state(x.ri))),
        VI_ATTR_ASRL_BREAK_STATE => port(&|x| put(out as *mut i16, state(x.brk))),
        VI_ATTR_GPIB_READDR_EN | VI_ATTR_GPIB_PRIMARY_ADDR | VI_ATTR_GPIB_SECONDARY_ADDR => VI_ERROR_NSUP_ATTR,
        _ => return None,
    })
}

/// Setting a serial session's attribute, if `vi` is one (None: not a serial session, or not a
/// serial attribute). A value the port or the driver will not take is VI_ERROR_NSUP_ATTR_STATE.
pub(crate) fn asrl_set(vi: ViSession, attr: u32, value: u64) -> Option<ViStatus> {
    use symbiotic_core::serial::field;
    let s = snap(vi).ok()?;
    let Target::Asrl(n) = s.t else { return None };
    let local = |f: &dyn Fn(&mut AsrlSess)| {
        if let Some(Object::Instr(m)) = get(vi).as_deref() {
            if let Some(a) = m.lock().unwrap().asrl.as_mut() {
                f(a);
            }
        }
        VI_SUCCESS
    };
    let port = |fld: u32, v: u32| match with_session(&s, |c| c.asrl_set(n, fld, v)) {
        Ok(()) => VI_SUCCESS,
        Err(Error::Status(_)) => VI_ERROR_NSUP_ATTR_STATE,
        Err(e) => map_err(&e),
    };
    // DTR, RTS and break are VI_STATE_ASSERTED (1) or VI_STATE_UNASSERTED (0).
    let line = |fld: u32| match value {
        0 | 1 => port(fld, value as u32),
        _ => VI_ERROR_NSUP_ATTR_STATE,
    };
    Some(match attr {
        VI_ATTR_ASRL_END_IN => match value as u16 {
            v @ (VI_ASRL_END_NONE | VI_ASRL_END_TERMCHAR) => local(&|a| a.end_in = v),
            _ => VI_ERROR_NSUP_ATTR_STATE, // LAST_BIT and BREAK: not done
        },
        VI_ATTR_ASRL_END_OUT => match value as u16 {
            v @ (VI_ASRL_END_NONE | VI_ASRL_END_TERMCHAR | VI_ASRL_END_BREAK) => local(&|a| a.end_out = v),
            _ => VI_ERROR_NSUP_ATTR_STATE, // LAST_BIT: not done
        },
        VI_ATTR_ASRL_BREAK_LEN => match value as i16 {
            v @ 1..=500 => local(&|a| a.break_len = v as u16),
            _ => VI_ERROR_NSUP_ATTR_STATE,
        },
        VI_ATTR_IO_PROT => match value as u16 {
            v @ (VI_PROT_NORMAL | VI_PROT_4882_STRS) => local(&|a| a.io_prot = v),
            _ => VI_ERROR_NSUP_ATTR_STATE,
        },
        VI_ATTR_ASRL_DISCARD_NULL | VI_ATTR_ASRL_REPLACE_CHAR => {
            if value == 0 { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE }
        }
        VI_ATTR_ASRL_ALLOW_TRANSMIT => {
            if value != 0 { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE }
        }
        VI_ATTR_ASRL_BAUD => port(field::BAUD, value as u32),
        VI_ATTR_ASRL_DATA_BITS => port(field::DATA_BITS, value as u16 as u32),
        VI_ATTR_ASRL_PARITY => port(field::PARITY, value as u16 as u32),
        VI_ATTR_ASRL_STOP_BITS => port(field::STOP_BITS, value as u16 as u32),
        VI_ATTR_ASRL_FLOW_CNTRL => port(field::FLOW, value as u16 as u32),
        VI_ATTR_ASRL_XON_CHAR => port(field::XON, value as u8 as u32),
        VI_ATTR_ASRL_XOFF_CHAR => port(field::XOFF, value as u8 as u32),
        VI_ATTR_ASRL_DTR_STATE => line(field::DTR),
        VI_ATTR_ASRL_RTS_STATE => line(field::RTS),
        VI_ATTR_ASRL_BREAK_STATE => line(field::BREAK),
        VI_ATTR_ASRL_AVAIL_NUM | VI_ATTR_ASRL_CTS_STATE | VI_ATTR_ASRL_DSR_STATE | VI_ATTR_ASRL_DCD_STATE
        | VI_ATTR_ASRL_RI_STATE => VI_ERROR_ATTR_READONLY,
        _ => return None,
    })
}

/// The attributes every instrument session has alike (a USBTMC one's own are above).
pub(crate) unsafe fn common_attr(s: &Instr, attr: u32, out: *mut c_void) -> ViStatus {
    match attr {
        VI_ATTR_RSRC_CLASS => put_str(out as *mut c_char, "INSTR"),
        VI_ATTR_RSRC_NAME => put_str(out as *mut c_char, &s.name),
        VI_ATTR_MAX_QUEUE_LENGTH => put(out as *mut u32, s.max_queue),
        VI_ATTR_TMO_VALUE => put(out as *mut u32, s.tmo_ms),
        VI_ATTR_TERMCHAR => put(out as *mut u8, s.termchar),
        VI_ATTR_TERMCHAR_EN => put(out as *mut u16, s.termchar_en as u16),
        VI_ATTR_SEND_END_EN => put(out as *mut u16, s.send_end as u16),
        VI_ATTR_USER_DATA_32 => put(out as *mut u32, s.user_data as u32),
        VI_ATTR_USER_DATA_64 => put(out as *mut u64, s.user_data),
        _ => return VI_ERROR_NSUP_ATTR,
    }
    VI_SUCCESS
}

pub(crate) trait Ok_ { fn ok(self) -> ViStatus; }
impl Ok_ for () { fn ok(self) -> ViStatus { VI_SUCCESS } }

#[no_mangle]
pub unsafe extern "system" fn viSetAttribute(vi: ViSession, attr: u32, value: AttrState) -> ViStatus {
    // ViAttrState is as wide as a pointer: 64 bits in visa64.dll, 32 in visa32.dll.
    #[allow(clippy::unnecessary_cast)]
    let value = value as u64;
    op(vi, "viSetAttribute", || {
        if matches!(get(vi).as_deref(), Some(Object::Instr(_) | Object::Lan(_))) {
            let mut v = value;
            let st = symbiotic_fio_attr(vi, attr, 1, &mut v);
            if st != 1 {
                return st;
            }
            if attr == VI_ATTR_FILE_APPEND_EN {
                transfers::set_file_append(vi, value != 0);
                return VI_SUCCESS;
            }
        }
        on_lan!(vi, |l| lan::set_attribute(l, attr, value));
        if let Some(r) = asrl_set(vi, attr, value) {
            return r;
        }
        let o = get(vi);
        let Some(Object::Instr(m)) = o.as_deref() else {
            return if o.is_some() { VI_ERROR_NSUP_ATTR } else { VI_ERROR_INV_OBJECT };
        };
        let mut s = m.lock().unwrap();
        let gpib_instr = !s.intfc && s.usb.is_none() && s.asrl.is_none();
        // VPP-4.3's generic attributes: what can be taken, and a clear no for what cannot.
        match attr {
            VI_ATTR_DMA_ALLOW_EN => {
                s.dma = value != 0;
                return VI_SUCCESS;
            }
            VI_ATTR_TRIG_ID if !s.intfc => {
                return if value as i16 == VI_TRIG_SW { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE };
            }
            // HS488 and USBTMC's vendor-specific messages are not done.
            VI_ATTR_IO_PROT if !s.intfc => {
                return if value as u16 == VI_PROT_NORMAL { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE };
            }
            VI_ATTR_SUPPRESS_END_EN if !s.intfc => {
                // A serial session's END is its END_IN character; suppressing it is not done.
                if s.asrl.is_some() && value != 0 {
                    return VI_ERROR_NSUP_ATTR_STATE;
                }
                s.suppress_end = value != 0;
                return VI_SUCCESS;
            }
            // Unaddressing after every operation would go behind the adapters' own tracking of who is
            // addressed (a Viper488 keeps a message open until another address is named).
            VI_ATTR_GPIB_UNADDR_EN if gpib_instr => {
                return if value == 0 { VI_SUCCESS } else { VI_ERROR_NSUP_ATTR_STATE };
            }
            VI_ATTR_DEV_STATUS_BYTE if s.intfc => {
                // RQS (bit 6) would ask this board to request service: it is the controller.
                if value & 0x40 != 0 {
                    return VI_ERROR_NSUP_ATTR_STATE;
                }
                s.dev_status = value as u8;
                return VI_SUCCESS;
            }
            VI_ATTR_GPIB_HS488_CBL_LEN if s.intfc => return VI_ERROR_NSUP_ATTR_STATE, // VPP-4.3: no HS488
            VI_ATTR_GPIB_ADDR_STATE if s.intfc => return VI_ERROR_ATTR_READONLY,
            VI_ATTR_USB_MAX_INTR_SIZE if s.usb.is_some() => {
                s.usb_intr_max = value as u16;
                return VI_SUCCESS;
            }
            _ => {}
        }
        match attr {
            VI_ATTR_TMO_VALUE => s.tmo_ms = value as u32,
            VI_ATTR_TERMCHAR => s.termchar = value as u8,
            VI_ATTR_TERMCHAR_EN => s.termchar_en = value != 0,
            VI_ATTR_SEND_END_EN => s.send_end = value != 0,
            VI_ATTR_GPIB_READDR_EN => s.readdr = value != 0,
            VI_ATTR_MAX_QUEUE_LENGTH => s.max_queue = value as u32,
            VI_ATTR_USER_DATA_32 | VI_ATTR_USER_DATA_64 => s.user_data = value,
            VI_ATTR_GPIB_ATN_STATE | VI_ATTR_GPIB_NDAC_STATE | VI_ATTR_GPIB_SRQ_STATE | VI_ATTR_GPIB_CIC_STATE
            | VI_ATTR_GPIB_SYS_CNTRL_STATE
            | VI_ATTR_RSRC_CLASS | VI_ATTR_RSRC_NAME | VI_ATTR_RSRC_IMPL_VERSION | VI_ATTR_RSRC_LOCK_STATE
            | VI_ATTR_RSRC_SPEC_VERSION | VI_ATTR_RSRC_MANF_NAME | VI_ATTR_INTF_TYPE | VI_ATTR_INTF_NUM
            | VI_ATTR_INTF_INST_NAME | VI_ATTR_GPIB_PRIMARY_ADDR | VI_ATTR_GPIB_SECONDARY_ADDR
            | VI_ATTR_GPIB_REN_STATE => return VI_ERROR_ATTR_READONLY,
            _ => return VI_ERROR_NSUP_ATTR,
        }
        VI_SUCCESS
    })
}

pub(crate) fn status_text(s: ViStatus) -> String {
    let t = match s {
        VI_SUCCESS => "Operation completed successfully.",
        VI_SUCCESS_TERM_CHAR => "The specified termination character was read.",
        VI_SUCCESS_MAX_CNT => "The number of bytes read is equal to the input count.",
        VI_ERROR_SYSTEM_ERROR => "Unknown system error.",
        VI_ERROR_INV_OBJECT => "The given session or object reference is invalid.",
        VI_ERROR_INV_EXPR => "Invalid expression specified for search.",
        VI_ERROR_RSRC_NFOUND => "Insufficient location information or the requested device or resource is not present in the system.",
        VI_ERROR_INV_RSRC_NAME => "Invalid resource reference specified. Parsing error.",
        VI_ERROR_INV_ACC_MODE => "Invalid access mode.",
        VI_ERROR_TMO => "Timeout expired before operation completed.",
        VI_ERROR_NSUP_ATTR => "The specified attribute is not defined or supported by the referenced session.",
        VI_ERROR_NSUP_ATTR_STATE => "The specified state of the attribute is not valid, or is not supported as defined by the session.",
        VI_ERROR_ATTR_READONLY => "The specified attribute is read-only.",
        VI_ERROR_IO => "Could not perform operation because of I/O error.",
        VI_ERROR_NSUP_MODE => "The given mode is not supported by this resource.",
        VI_ERROR_NLISTENERS => "No Listeners condition is detected (both NRFD and NDAC are deasserted).",
        VI_ERROR_NSUP_OPER => "The given operation is not supported by this resource.",
        VI_ERROR_RSRC_BUSY => "The resource is valid, but VISA cannot currently access it (it may be open in another process).",
        VI_ERROR_INV_PARAMETER => "The value of some parameter is invalid.",
        VI_ERROR_CONN_LOST => "The connection for the given session has been lost.",
        VI_SUCCESS_SYNC => "Asynchronous operation request was performed synchronously.",
        VI_SUCCESS_EVENT_EN => "Specified event is already enabled for at least one of the specified mechanisms.",
        VI_SUCCESS_EVENT_DIS => "Specified event is already disabled for at least one of the specified mechanisms.",
        VI_ERROR_INV_JOB_ID => "Specified job identifier is invalid.",
        VI_ERROR_INV_DEGREE => "Specified degree is invalid.",
        VI_ERROR_FILE_ACCESS => "An error occurred while trying to open the specified file.",
        VI_ERROR_FILE_IO => "An error occurred while accessing the specified file.",
        VI_ERROR_INV_EVENT => "Specified event type is not supported by the resource.",
        VI_ERROR_INV_MECH => "Invalid mechanism specified.",
        VI_ERROR_NOT_ENABLED => "The session must be enabled for events of the specified type to receive them.",
        VI_ERROR_RSRC_LOCKED => "Specified operation could not be performed because the resource is locked by another session.",
        _ => return format!("Unknown status code 0x{:08X}.", s as u32),
    };
    t.to_string()
}

#[no_mangle]
pub unsafe extern "system" fn viStatusDesc(_vi: ViSession, status: ViStatus, desc: *mut c_char) -> ViStatus {
    guard(|| {
        put_str(desc, &status_text(status));
        VI_SUCCESS
    })
}
