//! The resource manager: viOpenDefaultRM, viFindRsrc and viFindNext, viParseRsrc, viOpen and viClose.

use super::*;

#[no_mangle]
pub unsafe extern "system" fn viOpenDefaultRM(vi: *mut ViSession) -> ViStatus {
    guard(|| {
        if vi.is_null() {
            return VI_ERROR_INV_PARAMETER;
        }
        set_daemon_dir_once();
        *vi = insert(Object::Rm);
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viClose(vi: ViSession) -> ViStatus {
    if !symbiotic_core::iotrace::on() {
        return close(vi);
    }
    let (began, name) = (std::time::Instant::now(), trace_name(vi));
    let st = close(vi);
    trace_record(vi, "viClose", st, DEPTH.with(|d| d.get()) + 1, Some(began), name);
    st
}

pub(crate) unsafe fn close(vi: ViSession) -> ViStatus {
    guard(|| {
        // What goes is taken out of the registry under its lock and dropped after it: closing a VXI-11
        // link is a call to the instrument (up to a second), and every other VISA call of the process
        // would wait for it on the registry's lock.
        let mut gone: Vec<Arc<Object>> = Vec::new();
        let mut evs: Vec<Arc<Events>> = Vec::new();
        {
            if vi == 0 {
                return VI_WARN_NULL_OBJECT; // VPP-4.3 RULE 3.3.2: closing nothing is a warning
            }
            let mut reg = registry().lock().unwrap();
            let Some(o) = reg.remove(&vi) else { return VI_ERROR_INV_OBJECT };
            transfers::forget(vi);
            // SAFETY: fmtio.c's own table; a session it never saw is not in it.
            unsafe { symbiotic_fio_close(vi) };
            if let Object::Instr(m) = &*o {
                let ev = m.lock().unwrap().ev.clone();
                stop_events(&ev);
                evs.push(ev);
            }
            // A LAN session's worker stops; its interrupt channel closes as the link goes (vxi11.rs).
            if let Object::Lan(l) = &*o {
                stop_events(&l.ev);
                evs.push(l.ev.clone());
            }
            if matches!(*o, Object::Rm) {
                // Closing a resource manager closes what was opened through it.
                let opened: Vec<u32> = reg
                    .iter()
                    .filter(|(_, c)| match &***c {
                        Object::Find { rm, .. } => *rm == vi,
                        Object::Instr(m) => m.lock().unwrap().rm == vi,
                        Object::Lan(l) => l.rm == vi,
                        Object::Rm | Object::Event(_) => false,
                    })
                    .map(|(k, _)| *k)
                    .collect();
                for k in opened {
                    if let Some(c) = reg.remove(&k) {
                        // Its buffers and settings go as its own viClose would take them.
                        transfers::forget(k);
                        // SAFETY: as above.
                        unsafe { symbiotic_fio_close(k) };
                        let ev = match &*c {
                            Object::Instr(m) => Some(m.lock().unwrap().ev.clone()),
                            Object::Lan(l) => Some(l.ev.clone()),
                            _ => None,
                        };
                        if let Some(ev) = ev {
                            stop_events(&ev);
                            evs.push(ev);
                        }
                        gone.push(c);
                    }
                }
            }
            gone.push(o);
        }
        // Its handlers are off; one already running on another thread is waited for, with no lock
        // held that it could need.
        for ev in &evs {
            settle(ev);
        }
        drop(gone);
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viFindRsrc(
    sesn: ViSession, expr: *const c_char, find_list: *mut u32, ret_cnt: *mut u32, desc: *mut c_char,
) -> ViStatus {
    guard(|| {
        if !matches!(get(sesn).as_deref(), Some(Object::Rm)) {
            return VI_ERROR_INV_OBJECT;
        }
        let Some(expr) = cstr(expr) else { return VI_ERROR_INV_EXPR };
        if !resource::valid(expr) {
            return VI_ERROR_INV_EXPR;
        }
        // Configured LAN instruments (lan/conf.rs), checked for an answer while the GPIB side is
        // probed; and, when the configuration turns it on, the ones discovery finds (lan/discover.rs).
        // The setting page's "list absent instruments" (instruments.conf, `list=all`): without it a gateway's GPIB
        // address with no listener is left out.
        let list_all = symbiotic_core::instruments::find_lists_all();
        let lan = {
            use symbiotic_core::lan::{conf, discover, vxi11};
            let mut wanted: Vec<_> = conf::load().into_iter().filter(|t| resource::matches(expr, &t.name())).collect();
            // The group's aliases for network instruments are checked as the configured ones are, and listed only
            // if theirs answers: a group alias is a name, not a sign that its instrument is on.
            if !symbiotic_core::instruments::sees_everything() {
                refresh_group();
                for l in symbiotic_core::share::lookup::listed().into_iter().filter(|l| l.direct && resource::matches(expr, &l.resource)) {
                    if let Some(t) = symbiotic_core::lan::name::parse(&l.resource).filter(|t| !wanted.iter().any(|w| w.name() == t.name())) {
                        wanted.push(t);
                    }
                }
            }
            let find = conf::discover_enabled() && resource::may_match_lan(expr);
            let expr = expr.to_string();
            std::thread::spawn(move || {
                let found = find.then(|| std::thread::spawn(move || discover::discover(discover::WAIT)));
                // SYMBIOTIC_TRACE says how each entry's check went.
                let mut all = Vec::new();
                for r in conf::check_all(wanted, conf::REACH) {
                    trace(|| format!("lan.conf {r}"));
                    if r.result.is_ok() {
                        all.push(r.target);
                    }
                }
                if !list_all {
                    let before = all.len();
                    all = conf::present_behind_gateways(all, std::time::Duration::from_secs(3), &|host, port, iface, within| {
                        vxi11::find_at(host, port, iface, within, |_| {})
                    });
                    trace(|| format!("lan.conf: {} of {before} left after asking the gateways who listens", all.len()));
                }
                for t in found.and_then(|h| h.join().ok()).unwrap_or_default() {
                    if resource::matches(&expr, &t.name()) && !all.iter().any(|o| o.name() == t.name()) {
                        all.push(t);
                    }
                }
                all
            })
        };
        let mut names = Vec::new();
        // An expression no GPIB name can match starts no daemon.
        let gpib = if resource::may_match_gpib(expr) { find_gpib(expr, &mut names) } else { Ok(()) };
        // USBTMC instruments that are ours, from their descriptors: no daemon, nothing opened.
        if resource::may_match_prefix(expr, "USB") {
            for f in symbiotic_core::usbtmc::list().unwrap_or_default() {
                let n = resource::format_usb(&resource::UsbName { board: 0, vid: f.vid, pid: f.pid, serial: f.serial, iface: None });
                if resource::matches(expr, &n) {
                    names.push(n);
                }
            }
        }
        // Serial ports, from the registry: nothing opened (opening one raises DTR).
        if resource::may_match_prefix(expr, "ASRL") {
            // A port configured as a Prologix/AR488 bridge is a GPIB board, not a serial resource.
            let bridges = symbiotic_core::prologix::configured();
            for f in symbiotic_core::serial::list().unwrap_or_default().into_iter().filter(|f| !bridges.iter().any(|(b, _)| *b == f.n)) {
                let n = resource::format_asrl(f.n);
                if resource::matches(expr, &n) {
                    names.push(n);
                }
            }
        }
        names.extend(lan.join().unwrap_or_default().iter().map(|t| t.name()));
        // What each resource is listed as -- its name, its alias, both or nothing (instruments.conf, per resource). The
        // configuration window's tree is made of these results and wants every name.
        if !symbiotic_core::instruments::sees_everything() {
            let found: Vec<String> = names.iter().filter_map(|n| symbiotic_core::resource::canonical(n)).collect();
            // A LAN instrument's settings count only while its address is the instrument they are for (its MAC): instruments.rs.
            names = symbiotic_core::instruments::Exposure::load_for(&found).listed(names);
            // The group's aliases that other machines own are listed by their alias, the only name
            // they have here; a LAN one this machine also found by its resource name is listed by
            // the alias in that name's place, as an alias-only resource is (instruments.rs).
            for l in symbiotic_core::share::lookup::listed() {
                let (alias, res) = (l.alias, l.resource);
                if !resource::matches(expr, &res) || names.iter().any(|n| n.eq_ignore_ascii_case(&alias)) {
                    continue;
                }
                let at = names.iter().position(|n| symbiotic_core::resource::canonical(n).is_some_and(|c| c == res));
                // A network instrument was checked with the configured ones: found, its exposure setting has listed it, by its
                // group alias too (Exposure); not found, it is not listed.
                if l.direct {
                    continue;
                }
                match at {
                    // This machine's own, already listed by its resource name: its exposure setting stands.
                    Some(_) if l.mine => {}
                    Some(i) => names[i] = alias,
                    None => names.push(alias),
                }
            }
        }
        if let Err(e) = gpib {
            // The daemon's failure is the answer only if nothing else was found.
            if names.is_empty() {
                return map_err(&e);
            }
            trace(|| format!("viFindRsrc: GPIB not searched ({e}); LAN instruments listed"));
        }
        // The attribute part, if any (VPP-4.3 4.4.4): each resource the regular expression found is
        // opened and asked. Only an expression that has one opens anything.
        if let Some(f) = resource::attr_filter(expr) {
            names.retain(|n| attr_match(sesn, n, &f));
        }
        if names.is_empty() {
            return VI_ERROR_RSRC_NFOUND;
        }
        put(ret_cnt, names.len() as u32);
        let mut it = names.into_iter();
        put_str(desc, &it.next().unwrap());
        put(find_list, insert(Object::Find { rm: sesn, names: Mutex::new(it) }));
        VI_SUCCESS
    })
}

/// Does the resource `name` satisfy a search's attribute expression? It is opened, in a session of
/// its own that is closed again, and asked for the attributes the expression names. One that cannot
/// be opened (busy, gone, unreachable), or that does not have one of them, is not a match.
pub(crate) unsafe fn attr_match(rm: ViSession, name: &str, f: &resource::AttrExpr) -> bool {
    use symbiotic_core::attrs::Kind;
    use symbiotic_core::resource::AttrValue;
    let Ok(c) = std::ffi::CString::new(name) else { return false };
    let mut vi: ViSession = 0;
    if viOpen(rm, c.as_ptr(), 0, 0, &mut vi) < VI_SUCCESS {
        trace(|| format!("viFindRsrc: {name} could not be opened for its attributes"));
        return false;
    }
    let mut values = Vec::new();
    for (id, kind) in f.attrs() {
        // Room for any attribute: a string is at most 256 bytes with its NUL.
        let mut b = [0u8; 256];
        if viGetAttribute(vi, id, b.as_mut_ptr() as *mut c_void) < VI_SUCCESS {
            viClose(vi);
            return false;
        }
        let n = |k: usize| -> [u8; 8] { std::array::from_fn(|i| if i < k { b[i] } else { 0 }) };
        let v = match kind {
            Kind::U8 => AttrValue::Num(b[0] as i128),
            Kind::I16 => AttrValue::Num(i16::from_ne_bytes([b[0], b[1]]) as i128),
            Kind::U16 | Kind::Bool => AttrValue::Num(u16::from_ne_bytes([b[0], b[1]]) as i128),
            Kind::I32 => AttrValue::Num(i32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as i128),
            Kind::U32 => AttrValue::Num(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as i128),
            Kind::U64 | Kind::Ptr => AttrValue::Num(u64::from_ne_bytes(n(8)) as i128),
            Kind::Str => AttrValue::Str(CStr::from_bytes_until_nul(&b).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
        };
        values.push((id, v));
    }
    viClose(vi);
    f.eval(&|id| values.iter().find(|(i, _)| *i == id).map(|(_, v)| v.clone()))
}

/// The GPIB instruments and boards `expr` matches, through the daemon.
pub(crate) fn find_gpib(expr: &str, names: &mut Vec<String>) -> Result<(), Error> {
    let mut c = connect()?;
    let listed = c.list()?;
    let boards = listed.len();
    // Only the addresses the expression can match are probed: `GPIB1::?*` leaves GPIB0 alone.
    let mut skip: Option<u16> = None;
    for i in resource::candidates(boards as u16, expr) {
        // A number whose adapter is not attached is kept for it, not searched.
        if skip == Some(i.board) || !listed[i.board as usize].present {
            continue;
        }
        match c.probe(i.board, i.pad, i.sad) {
            Ok(true) => names.push(resource::format(i)),
            Ok(false) => {}
            // An adapter something else holds, or that has gone, is skipped: not ours to list.
            Err(_) => skip = Some(i.board),
        }
    }
    // The boards themselves, if the expression asks for them: they need no probing.
    names.extend(
        resource::intfc_candidates(boards as u16, expr)
            .into_iter()
            .filter(|b| listed[*b as usize].present)
            .map(resource::format_intfc),
    );
    Ok(())
}

#[no_mangle]
pub unsafe extern "system" fn viFindNext(find_list: u32, desc: *mut c_char) -> ViStatus {
    guard(|| {
        let o = get(find_list);
        let Some(Object::Find { names, .. }) = o.as_deref() else { return VI_ERROR_INV_OBJECT };
        let next = names.lock().unwrap().next();
        match next {
            Some(n) => {
                put_str(desc, &n);
                VI_SUCCESS
            }
            None => VI_ERROR_RSRC_NFOUND,
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn viParseRsrc(
    sesn: ViSession, name: *const c_char, intf_type: *mut u16, intf_num: *mut u16,
) -> ViStatus {
    guard(|| {
        viParseRsrcEx(sesn, name, intf_type, intf_num, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    })
}

#[no_mangle]
pub unsafe extern "system" fn viParseRsrcEx(
    sesn: ViSession, name: *const c_char, intf_type: *mut u16, intf_num: *mut u16,
    class: *mut c_char, expanded: *mut c_char, alias: *mut c_char,
) -> ViStatus {
    guard(|| {
        if !matches!(get(sesn).as_deref(), Some(Object::Rm)) {
            return VI_ERROR_INV_OBJECT;
        }
        let given = cstr(name).unwrap_or("");
        // A user-defined alias stands for its resource (RULE 4.3.19: the same ones viOpen takes);
        // the group's shared aliases first, as in viOpen. A remote one stands for the owner's name.
        use symbiotic_core::share::lookup::{lookup, Hit};
        refresh_group();
        let defined = match lookup(given) {
            Some(Hit::Stale { .. }) => return VI_ERROR_RSRC_NFOUND,
            Some(Hit::Remote { alias, resource }) | Some(Hit::Direct { alias, resource }) => Some((alias, resource)),
            Some(Hit::Mine { alias }) => symbiotic_core::alias::resolve(&alias),
            None => symbiotic_core::alias::resolve(given),
        };
        let name = defined.as_ref().map_or(given, |(_, r)| r.as_str());
        let (ty, num, cls, full) = if let Some(i) = resource::parse(name) {
            (VI_INTF_GPIB, i.board, "INSTR", resource::format(i))
        } else if let Some(b) = resource::parse_intfc(name) {
            (VI_INTF_GPIB, b, "INTFC", resource::format_intfc(b))
        } else if let Some((n, _)) = resource::parse_asrl(name) {
            (VI_INTF_ASRL, n, "INSTR", resource::format_asrl(n))
        } else if let Some(u) = resource::parse_usb(name) {
            (VI_INTF_USB, u.board, "INSTR", resource::format_usb(&u))
        } else if let Some(t) = symbiotic_core::lan::name::parse(name) {
            (VI_INTF_TCPIP, t.board, t.class(), t.name())
        } else {
            return VI_ERROR_INV_RSRC_NAME;
        };
        // The alias: the one given; COM<n>, which is ASRL<n>'s own; or one the user defined for it.
        let a = match defined {
            Some((a, _)) => a,
            None if resource::parse_asrl(given).is_some_and(|(_, is_com)| is_com) => given.to_string(),
            // A network instrument's alias is the group's first (share::names).
            None => symbiotic_core::alias::alias_of_in(&[symbiotic_core::share::lookup::lan_aliases(), symbiotic_core::alias::load()].concat(), &full).unwrap_or_default(),
        };
        put(intf_type, ty);
        put(intf_num, num);
        put_str(class, cls);
        put_str(expanded, &full);
        put_str(alias, &a);
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viOpen(
    sesn: ViSession, name: *const c_char, access_mode: u32, open_timeout: u32, vi: *mut ViSession,
) -> ViStatus {
    let began = std::time::Instant::now();
    let st = open(sesn, name, access_mode, open_timeout, vi);
    if symbiotic_core::iotrace::on() {
        // Named as the application named it; the session's own name is on its later records.
        let opened = if st >= 0 && !vi.is_null() { *vi } else { 0 };
        trace_record(opened, "viOpen", st, DEPTH.with(|d| d.get()) + 1, Some(began), cstr(name).unwrap_or("").to_string());
    }
    st
}

pub(crate) unsafe fn open(sesn: ViSession, name: *const c_char, access_mode: u32, open_timeout: u32, vi: *mut ViSession) -> ViStatus {
    guard(|| {
        if !matches!(get(sesn).as_deref(), Some(Object::Rm)) {
            return VI_ERROR_INV_OBJECT;
        }
        if vi.is_null() {
            return VI_ERROR_INV_PARAMETER;
        }
        let given = cstr(name).unwrap_or("");
        // The group's shared aliases come first: a name the group shares means the same instrument
        // on every machine, so a private alias of the same name is not looked at (share/lookup.rs).
        use symbiotic_core::share::lookup::{lookup, Hit};
        refresh_group();
        // A shared alias this machine's cache says is its own can have moved to another machine within the last ten seconds:
        // when the unit is not here, the group's directory is fetched at once and the name looked up again.
        let mut refreshed = false;
        'again: loop {
        let (mut resolved, shown, local) = match lookup(given) {
            Some(Hit::Stale { alias }) => {
                trace(|| format!("viOpen({alias}): the group's directory is too old to believe"));
                return VI_ERROR_RSRC_NFOUND;
            }
            Some(Hit::Remote { alias, .. }) => return open_shared(sesn, &alias, access_mode, open_timeout, vi),
            Some(Hit::Direct { alias, resource }) => (Some((alias.clone(), resource)), Some(alias), false),
            Some(Hit::Mine { alias }) => (symbiotic_core::alias::resolve(&alias), Some(alias), true),
            None => (symbiotic_core::alias::resolve(given), None, true),
        };
        // An alias bound to one unit (identity.rs) is that unit or nothing: checked, and looked for where it
        // has gone. It is never opened at a resource that has become another instrument.
        if let (true, Some((alias, res))) = (local, resolved.clone()) {
            if let Some(id) = symbiotic_core::identity::get(&alias) {
                use symbiotic_core::identity::{locate, Located, Real};
                match locate(&alias, &res, &id, &Real::default()) {
                    Located::Here => {}
                    Located::Moved(now) => {
                        trace(|| format!("viOpen({alias}): {res} is not {} {} ({}) any more; it is at {now}", id.vendor, id.model, id.serial));
                        let _ = symbiotic_core::alias::follow(&alias, &now);
                        // What the old place kept of it goes too: settings belong to the instrument (lanunits.rs).
                        if let Err(e) = symbiotic_core::lanunits::settings_follow(&res, &now) {
                            trace(|| format!("viOpen({alias}): its settings did not follow it to {now}: {e}"));
                        }
                        resolved = Some((alias, now));
                    }
                    Located::NotFound(why) => {
                        if shown.is_some() && !refreshed {
                            refreshed = true;
                            if let Ok(mut c) = connect() {
                                if c.share_refresh_now().is_ok() {
                                    continue 'again;
                                }
                            }
                        }
                        trace(|| format!("viOpen({alias}): {why}"));
                        return VI_ERROR_RSRC_NFOUND;
                    }
                    Located::Ambiguous(all) => {
                        trace(|| format!("viOpen({alias}): {} {} ({}) answers at {}: none is chosen", id.vendor, id.model, id.serial, all.join(", ")));
                        return VI_ERROR_RSRC_NFOUND;
                    }
                }
            }
        }
        let name = resolved.as_ref().map_or(given, |(_, r)| r.as_str());
        let st = open_named(sesn, name, access_mode, open_timeout, vi);
        // A shared alias is the session's name on every machine, the owner's included.
        if st >= 0 {
            if let Some(a) = shown {
                match get(*vi).as_deref() {
                    Some(Object::Instr(m)) => m.lock().unwrap().name = a,
                    Some(Object::Lan(l)) => *l.alias.lock().unwrap() = Some(a),
                    _ => {}
                }
            }
        }
        return st;
        }
    })
}

/// viOpen of a resource name that is not an alias.
pub(crate) unsafe fn open_named(sesn: u32, name: &str, access_mode: u32, open_timeout: u32, vi: *mut ViSession) -> ViStatus {
    {
        // An instrument, or the board itself (INTFC): that one is there whenever the board is, and
        // is locked, like an instrument, under its board and backend::RAW.
        let (i, intfc) = if let Some(i) = resource::parse(name) {
            (i, false)
        } else if let Some(board) = resource::parse_intfc(name) {
            (resource::Instr { board, pad: symbiotic_core::backend::RAW, sad: NO_SAD }, true)
        } else if let Some((n, _)) = resource::parse_asrl(name) {
            return open_asrl(sesn, n, access_mode, open_timeout, vi);
        } else if let Some(u) = resource::parse_usb(name) {
            return open_usb(sesn, u, access_mode, open_timeout, vi);
        } else if let Some(t) = symbiotic_core::lan::name::parse(name) {
            // A LAN instrument: connected from here, no daemon (lan.rs).
            if let Err(st) = check_access_mode(access_mode) {
                return st;
            }
            return match lan::open(sesn, t, access_mode, open_timeout) {
                Ok(l) => {
                    *vi = insert(Object::Lan(l));
                    VI_SUCCESS
                }
                Err(e) => e,
            };
        } else {
            return VI_ERROR_INV_RSRC_NAME;
        };
        // VI_EXCLUSIVE_LOCK (1) or VI_SHARED_LOCK (2) takes that lock as the session opens, within
        // `open_timeout`; VI_LOAD_CONFIG (4) has nothing to load.
        if let Err(st) = check_access_mode(access_mode) {
            return st;
        }
        let mut c = match connect() {
            Ok(c) => c,
            Err(e) => return map_err(&e),
        };
        let adapter = match c.list_bound() {
            Ok(d) => match d.get(i.board as usize) {
                Some(x) if x.present => x.entry.key(),
                _ => return VI_ERROR_RSRC_NFOUND, // no such board, or its adapter is not attached
            },
            Err(e) => return map_err(&e),
        };
        // An address nobody listens at is not an instrument to open. A secondary address is not
        // looked for, only whether the adapter can reach one (backend.rs, `probe_sad`).
        if !intfc {
            match c.probe(i.board, i.pad, i.sad) {
                Ok(true) => {}
                Ok(false) => return VI_ERROR_RSRC_NFOUND,
                Err(e) => return map_err(&e),
            }
        }
        let locked = match lock_on_open(&mut c, Target::Gpib(i.board, i.pad, i.sad), access_mode, open_timeout) {
            Ok(l) => l,
            Err(st) => return st,
        };
        let name = if intfc { resource::format_intfc(i.board) } else { resource::format(i) };
        *vi = insert(Object::Instr(Mutex::new(Instr { intfc, ..Instr::new(sesn, c, locked, adapter, name, i.board, i.pad, i.sad) })));
        VI_SUCCESS
    }
}

/// VI_EXCLUSIVE_LOCK (1) or VI_SHARED_LOCK (2), not both, and VI_LOAD_CONFIG (4), which has nothing
/// to load: the access modes viOpen takes.
pub(crate) fn check_access_mode(access_mode: u32) -> Result<(), ViStatus> {
    let locks = access_mode & (VI_EXCLUSIVE_LOCK | VI_SHARED_LOCK);
    if access_mode & !(VI_EXCLUSIVE_LOCK | VI_SHARED_LOCK | VI_LOAD_CONFIG) != 0 || locks == VI_EXCLUSIVE_LOCK | VI_SHARED_LOCK {
        return Err(VI_ERROR_INV_ACC_MODE);
    }
    Ok(())
}

/// The lock an access mode asks for, taken as the session opens, within `open_timeout`. Not free
/// in time: VISA calls that "locked", not "timed out".
/// Whether it took one.
pub(crate) fn lock_on_open(c: &mut Client, t: Target, access_mode: u32, open_timeout: u32) -> Result<bool, ViStatus> {
    if access_mode & (VI_EXCLUSIVE_LOCK | VI_SHARED_LOCK) == 0 {
        return Ok(false);
    }
    let wait = if open_timeout == VI_TMO_INFINITE { u32::MAX } else { open_timeout };
    match c.lock(t, access_mode & VI_SHARED_LOCK != 0, wait, None) {
        Ok(_) => Ok(true),
        Err(Error::Timeout) => Err(VI_ERROR_RSRC_LOCKED),
        Err(e) => Err(map_err(&e)),
    }
}

/// viOpen of a shared alias another machine owns: this connection to the daemon becomes a tunnel to
/// the owner's daemon (ipc.rs, PEER_CONNECT), and from there the session is like any other. What
/// the owner says the instrument is gives the interface type and addresses, so every machine reads
/// the same ones. A tunnel that is lost is the session lost (`ensure`): the owner's locks and
/// subscriptions went with it, and a fresh one would not have them.
pub(crate) unsafe fn open_shared(sesn: u32, alias: &str, access_mode: u32, open_timeout: u32, vi: *mut ViSession) -> ViStatus {
    if let Err(st) = check_access_mode(access_mode) {
        return st;
    }
    let mut c = match connect() {
        Ok(c) => c,
        Err(e) => return map_err(&e),
    };
    let o = match c.peer_connect(alias) {
        Ok(o) => o,
        Err(e) => return map_err(&e),
    };
    let (mut usb, mut asrl, mut usb_intr_max) = (None, None, 0u16);
    let (board, pad, sad) = match o.target {
        Target::Gpib(b, p, s) => {
            match c.probe(b, p, s) {
                Ok(true) => {}
                Ok(false) => return VI_ERROR_RSRC_NFOUND,
                Err(e) => return map_err(&e),
            }
            (b, p, s)
        }
        Target::Usb(n) => {
            usb_intr_max = match c.usb_probe(n) {
                Ok((true, max)) => max,
                Ok((false, _)) => return VI_ERROR_RSRC_NFOUND,
                Err(e) => return map_err(&e),
            };
            // The descriptors only the owner can read come with its answer.
            let Some(u) = resource::parse_usb(&o.resource) else { return VI_ERROR_RSRC_NFOUND };
            let d = o.usb.clone();
            usb = Some(UsbSess {
                iface: d.as_ref().map_or(u.iface.unwrap_or(0), |d| d.iface),
                usb488: d.as_ref().is_some_and(|d| d.usb488),
                manufacturer: d.as_ref().map(|d| d.manufacturer.clone()).unwrap_or_default(),
                product: d.map(|d| d.product).unwrap_or_default(),
                name: u,
            });
            (n, 0, NO_SAD)
        }
        Target::Asrl(n) => {
            if let Err(e) = c.asrl_open(n) {
                return map_err(&e);
            }
            asrl = Some(AsrlSess::default());
            (n, 0, NO_SAD)
        }
    };
    let locked = match lock_on_open(&mut c, o.target, access_mode, open_timeout) {
        Ok(l) => l,
        Err(st) => return st,
    };
    let i = Instr::new(sesn, c, locked, format!("{SHARED}{alias}"), alias.to_string(), board, pad, sad);
    *vi = insert(Object::Instr(Mutex::new(Instr { usb, asrl, usb_intr_max, ..i })));
    VI_SUCCESS
}

/// viOpen of a USBTMC name. The instrument must be one of ours (usbtmc::list: attached, with a
/// serial, on WinUSB), and is opened in the daemon now, so that one another program holds fails
/// here rather than on the first read.
pub(crate) unsafe fn open_usb(sesn: u32, u: resource::UsbName, access_mode: u32, open_timeout: u32, vi: *mut ViSession) -> ViStatus {
    if let Err(st) = check_access_mode(access_mode) {
        return st;
    }
    let found = match symbiotic_core::usbtmc::list() {
        Ok(l) => l.into_iter().find(|f| {
            f.vid == u.vid && f.pid == u.pid && f.serial == u.serial && u.iface.is_none_or(|i| i == f.iface)
        }),
        Err(e) => return map_err(&e),
    };
    let Some(f) = found else { return VI_ERROR_RSRC_NFOUND };
    let key = f.key();
    let mut c = match connect() {
        Ok(c) => c,
        Err(e) => return map_err(&e),
    };
    let n = match c.usb_list() {
        Ok(l) => match l.iter().position(|x| x.present && x.key() == key) {
            Some(n) => n as u16,
            None => return VI_ERROR_RSRC_NFOUND,
        },
        Err(e) => return map_err(&e),
    };
    let usb_intr_max = match c.usb_probe(n) {
        Ok((true, max)) => max,
        Ok((false, _)) => return VI_ERROR_RSRC_NFOUND,
        Err(e) => return map_err(&e),
    };
    let t = Target::Usb(n);
    let locked = match lock_on_open(&mut c, t, access_mode, open_timeout) {
        Ok(l) => l,
        Err(st) => return st,
    };
    let name = resource::format_usb(&u);
    let usb = UsbSess { name: u, iface: f.iface, usb488: f.usb488, manufacturer: f.manufacturer, product: f.product };
    *vi = insert(Object::Instr(Mutex::new(Instr { usb: Some(usb), usb_intr_max, ..Instr::new(sesn, c, locked, key, name, n, 0, NO_SAD) })));
    VI_SUCCESS
}

/// viOpen of a serial port: it must be one the system lists, and is opened in the daemon now, so
/// that one another program has open fails here (VI_ERROR_RSRC_BUSY), not on the first write.
/// Nothing is sent: there is no asking a serial line whether an instrument is on it.
pub(crate) unsafe fn open_asrl(sesn: u32, n: u16, access_mode: u32, open_timeout: u32, vi: *mut ViSession) -> ViStatus {
    if let Err(st) = check_access_mode(access_mode) {
        return st;
    }
    if symbiotic_core::prologix::is_bridge(n) {
        return VI_ERROR_RSRC_NFOUND; // a Prologix/AR488 bridge: its instruments are a GPIB board
    }
    match symbiotic_core::serial::list() {
        Ok(l) if l.iter().any(|f| f.n == n) => {}
        Ok(_) => return VI_ERROR_RSRC_NFOUND,
        Err(e) => return map_err(&e),
    }
    let mut c = match connect() {
        Ok(c) => c,
        Err(e) => return map_err(&e),
    };
    if let Err(e) = c.asrl_open(n) {
        return map_err(&e);
    }
    let t = Target::Asrl(n);
    let locked = match lock_on_open(&mut c, t, access_mode, open_timeout) {
        Ok(l) => l,
        Err(st) => return st,
    };
    let i = Instr::new(sesn, c, locked, symbiotic_core::serial::port_name(n), resource::format_asrl(n), n, 0, NO_SAD);
    *vi = insert(Object::Instr(Mutex::new(Instr { asrl: Some(AsrlSess::default()), ..i })));
    VI_SUCCESS
}
