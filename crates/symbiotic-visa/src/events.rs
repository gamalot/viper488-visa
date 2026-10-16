//! Events: the service request (SRQ) through the daemon or a LAN session's own watch, I/O completion and exceptions, handlers and queues.

use super::*;

// The daemon detects SRQ and serial-polls the instruments somebody subscribed to; a session that
// enables the event gets a worker thread on its own connection that waits for the daemon's
// notification, queues it (viWaitOnEvent) and calls the installed handlers.

/// Everything off: the session is closing.
pub(crate) fn stop_events(ev: &Events) {
    let mut st = ev.st.lock().unwrap();
    if let Some(stop) = st.stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    st.srq = (false, false);
    st.io = (false, false);
    st.exc = (false, false);
    st.queue.clear();
    ev.cv.notify_all();
}

/// The service-request worker stops; I/O completion events are not its business.
pub(crate) fn stop_srq(ev: &Events) {
    let mut st = ev.st.lock().unwrap();
    if let Some(stop) = st.stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    st.srq = (false, false);
    st.queue.retain(|e| e.ty != VI_EVENT_SERVICE_REQ);
    ev.cv.notify_all();
}

pub(crate) fn start_worker(vi: u32, ev: &Arc<Events>, t: Target, adapter: String) -> Result<(), Error> {
    let mut client = session_client(&adapter, true)?;
    let id = client.srq_subscribe(t)?;
    let stop = Arc::new(AtomicBool::new(false));
    ev.st.lock().unwrap().stop = Some(stop.clone());
    let ev = ev.clone();
    std::thread::spawn(move || {
        let mut sub = Some((client, id));
        while !stop.load(Ordering::SeqCst) {
            let Some((client, id)) = sub.as_mut() else {
                // The daemon went away and took the subscription with it. Subscribe again once one is running, but
                // never start it from here: the daemon may have been stopped on purpose. The session's next call starts
                // it; notifications in between are lost.
                std::thread::sleep(Duration::from_millis(500));
                sub = resubscribe(t, &adapter);
                if sub.is_some() {
                    trace(|| format!("session {vi}: SRQ subscription renewed"));
                }
                continue;
            };
            match client.srq_wait(*id, 200) {
                Ok(Some(_stb)) => deliver(vi, &ev, Ev::srq()),
                Ok(None) => {}
                Err(e) => {
                    // A wait fails only when the connection is lost: subscribe again rather than end the thread.
                    trace(|| format!("session {vi}: SRQ wait failed ({e}); will subscribe again"));
                    sub = None;
                }
            }
        }
        if let Some((mut client, id)) = sub {
            let _ = client.srq_unsubscribe(id);
        }
    });
    Ok(())
}

/// Wait until no handler of this session is running on another thread: after viUninstallHandler
/// or viClose returns, a handler it took away is not called and is not still running (a caller may
/// free it then: pyvisa frees a ctypes callback). A handler that uninstalls or closes from inside
/// itself is not waited for -- it is this thread. Called holding no lock a handler could need:
/// a handler may call VISA, which takes the registry's lock and its session's.
pub(crate) fn settle(ev: &Events) {
    let me = std::thread::current().id();
    let mut st = ev.st.lock().unwrap();
    while st.calling.iter().any(|t| *t != me) {
        st = ev.returned.wait(st).unwrap();
    }
}

/// One event: queued (if its queue is enabled and not full) and given to its handlers.
pub(crate) fn deliver(vi: u32, ev: &Events, e: Ev) {
    let handlers: Vec<(Handler, usize)> = {
        let mut st = ev.st.lock().unwrap();
        let (q, h) = *st.mech(e.ty);
        if q && st.queue.len() < EVENT_QUEUE_MAX {
            st.queue.push_back(e.clone());
        }
        ev.cv.notify_all();
        // The last installed first (RULE 3.7.8, LIFO).
        if h { st.handlers.iter().rev().filter(|x| x.0 == e.ty).map(|x| (x.1, x.2)).collect() } else { Vec::new() }
    };
    let me = std::thread::current().id();
    for (h, user) in handlers {
        {
            // Still installed and enabled (an earlier handler of this event may have changed that),
            // and counted as running before the lock is let go, so that `settle` sees it.
            let mut st = ev.st.lock().unwrap();
            if !st.mech(e.ty).1 || !st.handlers.iter().any(|x| x.0 == e.ty && x.1 as usize == h as usize && x.2 == user) {
                continue;
            }
            st.calling.push(me);
        }
        struct Returned<'a>(&'a Events, std::thread::ThreadId);
        impl Drop for Returned<'_> {
            fn drop(&mut self) {
                let mut st = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(i) = st.calling.iter().position(|t| *t == self.1) {
                    st.calling.swap_remove(i);
                }
                self.0.returned.notify_all();
            }
        }
        let returned = Returned(ev, me);
        // A handler gets an event context and must not close it; it is released after (RULE 3.7.27).
        let ctx = insert(Object::Event(e.clone()));
        // SAFETY: the client gave us this function pointer and promised it is a handler.
        let r = unsafe { h(vi, e.ty, ctx, user as *const c_void) };
        drop(returned);
        registry().lock().unwrap().remove(&ctx);
        if r == VI_SUCCESS_NCHAIN {
            break; // this handler says the others are not to be called for this event
        }
    }
}


/// The event types a call names: one, or both there are (VI_ALL_ENABLED_EVENTS).
pub(crate) fn types_of(ty: u32) -> Result<&'static [u32], ViStatus> {
    match ty {
        VI_EVENT_SERVICE_REQ => Ok(&[VI_EVENT_SERVICE_REQ]),
        VI_EVENT_IO_COMPLETION => Ok(&[VI_EVENT_IO_COMPLETION]),
        VI_EVENT_EXCEPTION => Ok(&[VI_EVENT_EXCEPTION]),
        VI_ALL_ENABLED_EVENTS => Ok(&[VI_EVENT_SERVICE_REQ, VI_EVENT_IO_COMPLETION, VI_EVENT_EXCEPTION]),
        _ => Err(VI_ERROR_INV_EVENT),
    }
}

/// The worker of a LAN session: waits on the session's own watch (lan.rs), no daemon. It holds
/// neither the session nor its connection, so a handler may use both. It ends when stopped, or
/// when the watch is gone (the connection is lost, or the interrupt channel closed).
pub(crate) fn start_lan_worker(vi: u32, ev: &Arc<Events>, watch: symbiotic_core::lan::SrqWatch) {
    let stop = Arc::new(AtomicBool::new(false));
    ev.st.lock().unwrap().stop = Some(stop.clone());
    let ev = ev.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match watch.wait(Duration::from_millis(200)) {
                Ok(true) => deliver(vi, &ev, Ev::srq()),
                Ok(false) => {}
                Err(e) => {
                    trace(|| format!("session {vi}: no more service requests ({e})"));
                    return;
                }
            }
        }
    });
}

/// A new connection and subscription, to a daemon that is already running -- and only if the board
/// number still means the session's adapter (a new daemon numbers afresh).
pub(crate) fn resubscribe(t: Target, adapter: &str) -> Option<(Client, u32)> {
    let mut c = session_client(adapter, false).ok()?;
    // A shared alias's instrument is the owner's to number; the tunnel is bound to it already.
    if shared_alias(adapter).is_none() && !still_means(&mut c, t, adapter).ok()? {
        return None;
    }
    let id = c.srq_subscribe(t).ok()?;
    Some((c, id))
}

pub(crate) fn t_board(t: Target) -> u16 {
    match t {
        Target::Gpib(b, _, _) | Target::Usb(b) | Target::Asrl(b) => b,
    }
}

/// A session's events, GPIB or LAN.
pub(crate) fn events_of(vi: u32) -> Result<Arc<Events>, ViStatus> {
    match get(vi).as_deref() {
        Some(Object::Instr(m)) => Ok(m.lock().unwrap().ev.clone()),
        Some(Object::Lan(l)) => Ok(l.ev.clone()),
        Some(_) => Err(VI_ERROR_NSUP_OPER),
        None => Err(VI_ERROR_INV_OBJECT),
    }
}

pub(crate) fn event_session(vi: u32) -> Result<(Arc<Events>, Target, String), ViStatus> {
    match get(vi).as_deref() {
        Some(Object::Instr(m)) => {
            let s = m.lock().unwrap();
            Ok((s.ev.clone(), target_of(&s), s.adapter.clone()))
        }
        Some(_) => Err(VI_ERROR_NSUP_OPER),
        None => Err(VI_ERROR_INV_OBJECT),
    }
}

#[no_mangle]
pub unsafe extern "system" fn viEnableEvent(vi: ViSession, ty: u32, mech: u16, _ctx: u32) -> ViStatus {
    op(vi, "viEnableEvent", || {
        let o = get(vi);
        let lan_sess = match o.as_deref() {
            Some(Object::Lan(l)) => Some(l),
            _ => None,
        };
        let (ev, t, adapter) = match lan_sess {
            Some(l) => (l.ev.clone(), Target::Gpib(0, 0, NO_SAD), String::new()),
            None => match event_session(vi) {
                Ok(x) => x,
                Err(e) => return e,
            },
        };
        if ty != VI_EVENT_SERVICE_REQ && ty != VI_EVENT_IO_COMPLETION && ty != VI_EVENT_EXCEPTION {
            return VI_ERROR_INV_EVENT;
        }
        // A suspended handler (events held until the handler is switched on) is not implemented.
        if mech != VI_ALL_MECH && (mech == 0 || mech & !(VI_QUEUE | VI_HNDLR) != 0) {
            return VI_ERROR_INV_MECH;
        }
        // An exception is for handlers only (VPP-4.3 3.7.2); "all mechanisms" means that one.
        let mech = if ty == VI_EVENT_EXCEPTION {
            if mech != VI_ALL_MECH && mech & VI_QUEUE != 0 {
                return VI_ERROR_INV_MECH;
            }
            VI_HNDLR
        } else {
            mech
        };
        let srq = ty == VI_EVENT_SERVICE_REQ;
        if srq && lan_sess.is_none() {
            // The service request is watched per instrument (a serial poll finds who asked); an
            // INTFC session's SRQ event, the line itself, is not implemented, and a serial line has
            // none. Only enabling is refused: disabling and discarding still answer, since pyvisa
            // calls them on every close.
            if t == Target::Gpib(t_board(t), symbiotic_core::backend::RAW, NO_SAD) {
                return VI_ERROR_NSUP_OPER;
            }
            if matches!(t, Target::Asrl(_)) {
                return VI_ERROR_INV_EVENT;
            }
        }
        let (already, need_worker) = {
            let mut st = ev.st.lock().unwrap();
            let (want_q, want_h) = (mech & VI_QUEUE != 0, mech & VI_HNDLR != 0);
            let m = st.mech(ty);
            // "Already enabled" is a warning for exactly this: everything asked for was on before.
            let already = (want_q || want_h) && (!want_q || m.0) && (!want_h || m.1);
            m.0 |= want_q;
            m.1 |= want_h;
            (already, srq && st.stop.is_none())
        };
        // I/O completion needs nothing watching: the operations post it themselves.
        if need_worker {
            if let Some(l) = lan_sess {
                // Watch first, then enable: a VXI-11 session opens its interrupt channel here.
                match lan::watch_srq(l) {
                    Ok(w) => start_lan_worker(vi, &ev, w),
                    Err(e) => {
                        stop_srq(&ev);
                        return e;
                    }
                }
            } else if let Err(e) = start_worker(vi, &ev, t, adapter) {
                stop_srq(&ev);
                return map_err(&e);
            }
        }
        if already { VI_SUCCESS_EVENT_EN } else { VI_SUCCESS }
    })
}

#[no_mangle]
pub unsafe extern "system" fn viDisableEvent(vi: ViSession, ty: u32, mech: u16) -> ViStatus {
    op(vi, "viDisableEvent", || {
        let ev = match events_of(vi) {
            Ok(x) => x,
            Err(e) => return e,
        };
        let types = match types_of(ty) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let (was, srq_off) = {
            let mut st = ev.st.lock().unwrap();
            let mut was = false;
            for t in types {
                let mut m = *st.mech(*t);
                was |= m.0 || m.1;
                if mech & VI_QUEUE != 0 {
                    m.0 = false;
                    st.queue.retain(|e| e.ty != *t);
                }
                if mech & VI_HNDLR != 0 {
                    m.1 = false;
                }
                *st.mech(*t) = m;
            }
            (was, st.srq == (false, false) && st.stop.is_some())
        };
        let r = if was { VI_SUCCESS } else { VI_SUCCESS_EVENT_DIS };
        if srq_off {
            stop_srq(&ev);
            // A VXI-11 session's interrupt channel closes with the last mechanism.
            on_lan!(vi, |l| {
                lan::unwatch_srq(l);
                r
            });
        }
        r
    })
}

#[no_mangle]
pub unsafe extern "system" fn viDiscardEvents(vi: ViSession, ty: u32, _mech: u16) -> ViStatus {
    op(vi, "viDiscardEvents", || {
        let ev = match events_of(vi) {
            Ok(x) => x,
            Err(e) => return e,
        };
        let types = match types_of(ty) {
            Ok(t) => t,
            Err(e) => return e,
        };
        ev.st.lock().unwrap().queue.retain(|e| !types.contains(&e.ty));
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viWaitOnEvent(vi: ViSession, ty: u32, tmo: u32, out_ty: *mut u32, out_ctx: *mut u32) -> ViStatus {
    op(vi, "viWaitOnEvent", || {
        let ev = match events_of(vi) {
            Ok(x) => x,
            Err(e) => return e,
        };
        let types = match types_of(ty) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let deadline = (tmo != VI_TMO_INFINITE).then(|| std::time::Instant::now() + std::time::Duration::from_millis(tmo as u64));
        let mut st = ev.st.lock().unwrap();
        let e = loop {
            // Events are only queued while the queue mechanism is enabled for their type.
            if !types.iter().any(|t| st.mech(*t).0) {
                return VI_ERROR_NOT_ENABLED;
            }
            if let Some(i) = st.queue.iter().position(|e| types.contains(&e.ty)) {
                break st.queue.remove(i).unwrap();
            }
            let slice = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return VI_ERROR_TMO;
                    }
                    left
                }
                None => std::time::Duration::from_secs(1),
            };
            st = ev.cv.wait_timeout(st, slice).unwrap().0;
        };
        drop(st);
        put(out_ty, e.ty);
        // No place for the context: VISA closes it at once, so none is made.
        if !out_ctx.is_null() {
            put(out_ctx, insert(Object::Event(e)));
        }
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viInstallHandler(vi: ViSession, ty: u32, h: *const c_void, user: *const c_void) -> ViStatus {
    op(vi, "viInstallHandler", || {
        let ev = match events_of(vi) {
            Ok(x) => x,
            Err(e) => return e,
        };
        if ty != VI_EVENT_SERVICE_REQ && ty != VI_EVENT_IO_COMPLETION && ty != VI_EVENT_EXCEPTION {
            return VI_ERROR_INV_EVENT;
        }
        if h.is_null() {
            return VI_ERROR_INV_HNDLR_REF;
        }
        // SAFETY: the client says this is a handler; it is only ever called as one.
        let f = std::mem::transmute::<*const c_void, Handler>(h);
        ev.st.lock().unwrap().handlers.push((ty, f, user as usize));
        VI_SUCCESS
    })
}

#[no_mangle]
pub unsafe extern "system" fn viUninstallHandler(vi: ViSession, ty: u32, h: *const c_void, user: *const c_void) -> ViStatus {
    op(vi, "viUninstallHandler", || {
        let ev = match events_of(vi) {
            Ok(x) => x,
            Err(e) => return e,
        };
        let types = match types_of(ty) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let r = {
            let mut st = ev.st.lock().unwrap();
            let before = st.handlers.len();
            // VI_ANY_HNDLR (null) takes every handler of the type out, whatever its user handle.
            st.handlers.retain(|(t, f, u)| !(types.contains(t) && (h.is_null() || (*f as usize == h as usize && *u == user as usize))));
            if st.handlers.len() == before { VI_ERROR_HNDLR_NINSTALLED } else { VI_SUCCESS }
        };
        settle(&ev);
        r
    })
}
