//! The daemon: owns every WinUSB device -- GPIB adapters and USBTMC instruments -- and the serial ports its clients
//! use, and serves clients over the local channel (local.rs, ipc.rs).
//!
//! Windows gives a WinUSB device to one process at a time, so the hardware has one owner, the daemon, and any number of
//! clients. It exits when nobody has been connected for `idle`, freeing the hardware for other tools; an adapter keeps
//! its settings while powered.
//!
//! Board numbers stay with their adapter for the daemon's life (boards.rs); a USBTMC instrument's number does the same.
//! The device lists are refreshed by LIST and USB_LIST requests and when a request names a device that is not there, so
//! ordinary operations do not enumerate USB each time.

use crate::adapter::Error;
use crate::backend::{self, Backend, Entry, NO_SAD, RAW_INTFC};
use crate::locks::Target;
use crate::usbtmc::Tmc;
use crate::ipc::*;
use std::collections::{HashMap, VecDeque};
use std::io;
use crate::local::{self, Listener};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A lock whose holder panicked still guards usable state, so a panic in one request (caught in `track`) does not fail
/// every later request.
trait Held<T> {
    fn held(&self) -> MutexGuard<'_, T>;
}

impl<T> Held<T> for Mutex<T> {
    fn held(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One client watching one instrument for service requests.
struct Sub {
    /// The connection that made it: only it may wait on it or take it away.
    owner: u64,
    target: Target,
    queue: VecDeque<u8>,
}

const SRQ_QUEUE_MAX: usize = 50;

/// Service requests. The daemon detects them (see `monitor`), because only it can look at the
/// adapters without a client's request; clients subscribe and wait.
#[derive(Default)]
struct Srq {
    subs: Mutex<HashMap<u32, Sub>>,
    cv: Condvar,
    next: AtomicU32,
    /// Status bytes our own serial poll consumed, owed to the next serial poll of that instrument so the client still
    /// sees its RQS bit.
    owed: Mutex<HashMap<Target, u8>>,
}

impl Srq {
    fn subscribe(&self, target: Target, owner: u64) -> u32 {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.subs.held().insert(id, Sub { owner, target, queue: VecDeque::new() });
        id
    }

    /// Who is being watched: GPIB instruments (primary, secondary) per board, and USBTMC instruments.
    fn wanted(&self) -> (HashMap<u16, Vec<(u8, u8)>>, Vec<u16>) {
        let mut m: HashMap<u16, Vec<(u8, u8)>> = HashMap::new();
        let mut usb = Vec::new();
        for s in self.subs.held().values() {
            match s.target {
                Target::Gpib(board, pad, sad) => {
                    let v = m.entry(board).or_default();
                    if !v.contains(&(pad, sad)) {
                        v.push((pad, sad));
                    }
                }
                Target::Usb(n) => {
                    if !usb.contains(&n) {
                        usb.push(n);
                    }
                }
                Target::Asrl(_) => {} // no SRQ on a serial line; nobody subscribes
            }
        }
        (m, usb)
    }

    fn deliver(&self, target: Target, stb: u8) {
        self.owed.held().insert(target, stb);
        for s in self.subs.held().values_mut().filter(|s| s.target == target) {
            if s.queue.len() < SRQ_QUEUE_MAX {
                s.queue.push_back(stb);
            }
        }
        self.cv.notify_all();
    }

    /// Take `owner`'s subscription `id` away; another connection's stays.
    fn unsubscribe(&self, id: u32, owner: u64) {
        let mut subs = self.subs.held();
        if subs.get(&id).is_some_and(|s| s.owner == owner) {
            subs.remove(&id);
        }
    }

    fn wait(&self, id: u32, owner: u64, ms: u32) -> Resp {
        let deadline = Instant::now() + Duration::from_millis(ms.min(1000) as u64);
        let mut subs = self.subs.held();
        loop {
            match subs.get_mut(&id).filter(|s| s.owner == owner) {
                None => return Resp::ok(0, vec![]), // unsubscribed meanwhile, or not this connection's
                Some(s) => {
                    if let Some(stb) = s.queue.pop_front() {
                        return Resp::ok(1, vec![stb]);
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Resp::ok(0, vec![]);
            }
            subs = self.cv.wait_timeout(subs, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

#[derive(Default)]
struct State {
    srq: Srq,
    /// Board numbers: stable for the daemon's life (boards.rs).
    boards: Mutex<crate::boards::Boards>,
    /// The generation of each board number when a connection checked its adapter (LIST with `F_BIND`), by connection:
    /// a request of that connection for a number that has changed hands since is refused (boards.rs).
    bound: Mutex<HashMap<u64, HashMap<u16, u64>>>,
    adapters: Mutex<HashMap<String, Arc<dyn Backend>>>,
    /// What each adapter says it is (Backend::model), by key: asked when it is opened, or by LIST
    /// (`learn_models`), and kept after the handle is let go (idle, unplugged).
    models: Mutex<HashMap<String, String>>,
    /// USBTMC instrument numbers (usbtmc.rs: stable like board numbers, but their own series), and
    /// the ones open, by key.
    usb: Mutex<crate::usbtmc::Slots>,
    tmcs: Mutex<HashMap<String, Arc<Tmc>>>,
    /// The connections that used each instrument open through a vendor's USBTMC driver (`Tmc::shared`):
    /// it is let go when the last of them ends (`release_usb`), as a serial port is.
    tmc_users: Mutex<HashMap<String, std::collections::HashSet<crate::locks::Owner>>>,
    /// Serial ports, by COM number, while a connection uses them (`handle_asrl`).
    serial: Mutex<HashMap<u16, Arc<Mutex<SerialSlot>>>>,
    /// Held while an adapter is being opened, which can take a while; separate from the cache lock, so requests for
    /// adapters already open do not wait behind it.
    opening: Mutex<()>,
    /// viLock's locks, per instrument, held by connections (one per VISA session). `lock_cv` wakes
    /// whoever waits for one when a lock is given back or its holder goes away.
    locks: Mutex<crate::locks::Locks>,
    lock_cv: Condvar,
    /// Requests being served, by connection, for `watchdog`.
    busy: Mutex<HashMap<u64, Busy>>,
    /// Each connection: its process, or the machine of the group it serves, for CLIENTS.
    conns: Mutex<HashMap<u64, Conn>>,
    /// A second handle on each local connection, to notice a client that left while its request waits (a lock without
    /// limit). A peer connection's end is the relay's (share/peer.rs).
    hangup: Mutex<HashMap<u64, local::Stream>>,
    /// What the daemon's scan found of each GPIB board: the addresses that have a listener, and when (BUS_SCAN; the configuration window's tree
    /// and the heartbeat's states read it).
    bus: Mutex<HashMap<u16, (Vec<u8>, Instant)>>,
    /// What each GPIB listener the sharing scan found said it is (`*IDN?`, asked once when it appeared; None: asked, no
    /// answer), by board and address: how a bound alias follows its unit across boards and machines. Forgotten when the
    /// listener or its adapter goes, asked again when it comes back.
    units: Mutex<HashMap<(u16, u8), Option<String>>>,
    /// Which connection used each instrument last, and when (F_IF_QUIET).
    touched: Mutex<HashMap<Target, (u64, Instant)>>,
    /// The boards whose REN is asserted as far as the daemon knows: by the adapter's setting when it was opened, by a bridge that cannot
    /// release it, or by a client's request since. A probe addressing an instrument on such a board would put it in Remote.
    ren_held: Mutex<std::collections::HashSet<u16>>,
    /// The registry embedded in this daemon, if `share.conf` asks for one.
    registry: Mutex<Option<Arc<crate::share::admin::Admin>>>,
    /// Counts every request and every connection that began: the idle watch sees that something happened even when it
    /// began and ended between two of its looks (a short connection that opened an instrument, say).
    activity: AtomicU64,
    /// This machine in its group, once there is one (made at start when it shares, else on the
    /// first use of a shared alias).
    member: Mutex<Option<Arc<crate::share::member::Member>>>,
}

/// A serial port and the connections that use it. The settings outlive the handle: a USB serial
/// adapter that was unplugged is opened again with them when it is back, not at 9600 8N1.
#[derive(Default)]
struct SerialSlot {
    cfg: crate::serial::Config,
    port: Option<crate::serial::Port>,
    users: std::collections::HashSet<crate::locks::Owner>,
}

/// A request in progress: what it is, and when it is overdue (its own GPIB timeout plus a margin).
struct Busy {
    what: String,
    start: Instant,
    overdue: Instant,
    said: bool,
}

/// A connection's id: who holds a lock.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

impl State {
    /// Enumerate the adapters and bring the board numbers up to date. Numbers do not move: an
    /// adapter that went keeps its slot, marked absent (boards.rs).
    fn refresh(&self) -> Result<Vec<crate::boards::Slot>, Error> {
        let list = backend::list()?;
        let mut boards = self.boards.held();
        let before: Vec<(String, bool)> = boards.slots().iter().map(|s| (s.entry.key(), s.present)).collect();
        let same = boards.update(list.clone());
        // Said once per adapter left out, not at every enumeration.
        static SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let mut said = SAID.held();
        said.retain(|k| same.iter().any(|e| &e.key() == k));
        for e in &same {
            if !said.contains(&e.key()) {
                log(&format!("{}: another adapter of that kind has the same serial; only the first is used, and this one has no board number", e.key()));
                said.push(e.key());
            }
        }
        drop(said);
        for (i, s) in boards.slots().iter().enumerate() {
            let was = before.get(i).map(|(k, p)| (k.as_str(), *p));
            if was != Some((s.entry.key().as_str(), s.present)) {
                log(&format!("board {i}: {} {}", s.entry.key(), if s.present { "attached" } else { "not attached" }));
            }
        }
        // Handles to adapters that went are dropped outside the lock: closing one is USB work.
        let gone: Vec<Arc<dyn Backend>> = {
            let mut a = self.adapters.held();
            let keys: Vec<String> = a.keys().filter(|k| !list.iter().any(|d| &d.key() == *k)).cloned().collect();
            keys.iter().filter_map(|k| a.remove(k)).collect()
        };
        let slots = boards.slots().to_vec();
        drop(boards);
        drop(gone);
        Ok(slots)
    }

    /// Ask each attached Viper488 whose model is not known yet what it is. One that is open answers through its handle;
    /// one that is not is opened for IDENTIFY alone and closed again, which does not touch the GPIB bus. One that
    /// cannot be opened now (another program has it) is asked again at the next LIST.
    fn learn_models(&self, slots: &[crate::boards::Slot]) {
        for s in slots.iter().filter(|s| s.present && s.entry.kind == backend::Kind::Viper) {
            let key = s.entry.key();
            if self.models.held().contains_key(&key) {
                continue;
            }
            let _opening = self.opening.held();
            let open = self.adapters.held().get(&key).cloned();
            let model = match open {
                Some(a) => a.model(),
                None => match backend::open(&s.entry) {
                    Ok((a, _)) => a.model(),
                    Err(e) => {
                        log(&format!("{key}: its model could not be asked: {e}"));
                        None
                    }
                },
            };
            if let Some(m) = model {
                self.models.held().insert(key, m);
            }
        }
    }

    /// The adapter a board number stands for, if attached. A board last seen absent is looked for
    /// again first: it may have come back.
    fn entry(&self, board: u16) -> Result<Entry, Error> {
        let known = {
            let b = self.boards.held();
            if b.is_empty() { None } else { Some(b.get(board)) }
        };
        match known {
            Some(Ok(e)) => Ok(e),
            _ => {
                self.refresh()?;
                self.boards.held().get(board)
            }
        }
    }

    fn adapter(&self, board: u16) -> Result<(String, Arc<dyn Backend>), Error> {
        let key = self.entry(board)?.key();
        if let Some(a) = self.adapters.held().get(&key) {
            return Ok((key, a.clone()));
        }
        let _opening = self.opening.held();
        // A blank 82357B woken for this number may come back as another one than the number's (two blank ones cannot be told apart):
        // that one keeps the handle, under its own number, and the next blank one is woken for this number. Bounded by how many
        // there are.
        for _ in 0..8 {
            // Not open (or dropped after a failure): enumerate first, so an adapter that has gone is
            // reported gone instead of an open attempt on a stale entry. Whoever held the lock before
            // us may have opened it meanwhile, and a blank 82357B they woke is listed under a new key.
            self.refresh()?;
            let entry = self.boards.held().get(board)?;
            let key = entry.key();
            if let Some(a) = self.adapters.held().get(&key) {
                return Ok((key, a.clone()));
            }
            let (a, opened) = backend::open(&entry)?;
            let key = opened.key();
            if let Some(m) = a.model() {
                self.models.held().insert(key.clone(), m);
            }
            // A blank 82357B was just woken: it is now a different USB device with a serial, and the number its serial had
            // follows it (boards.rs pairs it with its slot), under the new key.
            let at = if opened.key() != entry.key() {
                self.refresh()?;
                let b = self.boards.held();
                match b.slots().iter().position(|s| s.present && s.entry.key() == key) {
                    Some(i) => i as u16,
                    None => return Err(Error::Gone),
                }
            } else {
                board
            };
            self.set_up(at, &key, &a);
            self.adapters.held().insert(key.clone(), a.clone());
            if at == board {
                return Ok((key, a));
            }
            log(&format!("board {board}: the 82357B woken for it is board {at}'s; waking the next"));
        }
        Err(Error::Gone)
    }

    /// What an adapter is given when the daemon opens it: its remembered T1 level and REN.
    fn set_up(&self, board: u16, key: &str, a: &Arc<dyn Backend>) {
        // The T1 level last set on this adapter, set again (speedconf.rs).
        if let Some(level) = crate::speedconf::remembered(key) {
            let name = crate::proto::speed_name(level);
            match a.lock().speed(level) {
                Ok(_) => log(&format!("board {board} ({key}): T1 level {name}, as last set (speed.conf)")),
                Err(e) => log(&format!("board {board} ({key}): the remembered T1 level {name} could not be set: {e}")),
            }
        }
        // REN as the adapter's setting has it (renconf.rs): released unless the setting asserts it, replacing whatever
        // was left on the line. An instrument addressed while REN is asserted goes to Remote. A bridge that cannot
        // release REN keeps it.
        let hold = crate::renconf::asserted(key);
        let set = a.lock().ren(hold);
        match &set {
            Ok(()) => log(&format!("board {board} ({key}): REN {}", if hold { "asserted (ren.conf)" } else { "released" })),
            Err(e) => log(&format!("board {board} ({key}): REN could not be {}: {e}", if hold { "asserted" } else { "released" })),
        }
        // Not released where the adapter could not say it was: what is on the line is not known, and a probe stays away.
        let mut held = self.ren_held.held();
        if hold || set.is_err() {
            held.insert(board);
        } else {
            held.remove(&board);
        }
    }

    /// The adapter of a board number, for a request of connection `me`: refused (`Gone`) if the number has gone to another adapter
    /// since this connection checked it (boards.rs; LIST with `F_BIND`).
    fn adapter_for(&self, board: u16, me: u64) -> Result<(String, Arc<dyn Backend>), Error> {
        let r = self.adapter(board)?;
        let checked = self.bound.held().get(&me).and_then(|m| m.get(&board).copied());
        if checked.is_some_and(|g| self.boards.held().gen(board) != Some(g)) {
            return Err(Error::Gone);
        }
        Ok(r)
    }

    /// Enumerate the USBTMC instruments and bring their numbers up to date; drop the handles of
    /// ones that went. Lists what is ours to use (usbtmc::list).
    fn usb_refresh(&self) -> Result<Vec<crate::usbtmc::Found>, Error> {
        let list = crate::usbtmc::list()?;
        let keys: Vec<String> = list.iter().map(|f| f.key()).collect();
        let mut slots = self.usb.held();
        let before: Vec<(String, bool)> = slots.slots().to_vec();
        slots.update(&keys);
        for (i, s) in slots.slots().iter().enumerate() {
            if before.get(i) != Some(s) {
                log(&format!("USBTMC {i}: {} {}", s.0, if s.1 { "attached" } else { "not attached" }));
            }
        }
        drop(slots);
        let gone: Vec<Arc<Tmc>> = {
            let mut t = self.tmcs.held();
            let k: Vec<String> = t.keys().filter(|k| !keys.contains(k)).cloned().collect();
            k.iter().filter_map(|k| t.remove(k)).collect()
        };
        drop(gone);
        Ok(list)
    }

    /// The USBTMC instrument a number stands for, opened if it is not yet.
    fn usb(&self, n: u16) -> Result<(String, Arc<Tmc>), Error> {
        let known = {
            let s = self.usb.held();
            if s.slots().is_empty() { None } else { Some(s.get(n)) }
        };
        let key = match known {
            Some(Ok(k)) => k,
            _ => {
                self.usb_refresh()?;
                self.usb.held().get(n)?
            }
        };
        if let Some(t) = self.tmcs.held().get(&key) {
            return Ok((key, t.clone()));
        }
        let _opening = self.opening.held();
        self.usb_refresh()?;
        let key = self.usb.held().get(n)?;
        if let Some(t) = self.tmcs.held().get(&key) {
            return Ok((key, t.clone()));
        }
        let t = Arc::new(Tmc::open(&key)?);
        self.tmcs.held().insert(key.clone(), t.clone());
        Ok((key, t))
    }

    fn forget_usb(&self, key: &str) {
        self.tmcs.held().remove(key);
    }

    /// An adapter that is already open (the monitor never opens or wakes one).
    fn open_adapter(&self, board: u16) -> Option<Arc<dyn Backend>> {
        let entry = self.boards.held().get(board).ok()?;
        self.adapters.held().get(&entry.key()).cloned()
    }

    /// The adapter failed underneath us (unplugged, USB error): drop the handle, so the next request
    /// enumerates and reopens instead of reusing a dead one. The board numbers stay as they are.
    fn forget(&self, key: &str) {
        self.adapters.held().remove(key);
    }
}

fn done<T>(r: Result<T, Error>, f: impl FnOnce(T) -> Resp) -> Resp {
    match r {
        Ok(v) => f(v),
        Err(e) => Resp::err(&e, 0),
    }
}

fn handle(st: &State, q: Req, me: crate::locks::Owner) -> Resp {
    match q.op {
        // The IPC version, and this build's (a client hands an older daemon over by it).
        op::PING => return Resp::ok(VERSION, BUILD.as_bytes().to_vec()),
        op::CLIENTS => {
            // `count` 1: each line also says which machine of the group, for how long, how long idle and what locks,
            // tab-separated after the process.
            let conns = st.conns.held();
            let locks = st.locks.held();
            let now = Instant::now();
            let others: Vec<String> = conns
                .iter()
                .filter(|(c, _)| **c != me)
                .map(|(c, k)| {
                    let pid = k.pid.map_or("?".into(), |p| p.to_string());
                    if q.count != 1 {
                        return pid;
                    }
                    let held: Vec<String> = locks
                        .held_by(*c)
                        .into_iter()
                        .map(|(r, s)| format!("{} {}", crate::locks::describe(r), if s == crate::locks::State::Exclusive { "exclusive" } else { "shared" }))
                        .collect();
                    let peer = k.peer.as_deref().unwrap_or("").replace(['\t', '\n'], " ");
                    format!("{pid}\t{peer}\t{}\t{}\t{}", (now - k.since).as_secs(), (now - k.last).as_secs(), held.join(", "))
                })
                .collect();
            return Resp::ok(others.len() as u32, (others.join("\n") + "\n").into_bytes());
        }
        op::SHARE_REFRESH => {
            if let Ok(m) = member_of(st) {
                // `count` 1: now, whatever the age (the library found its cached information out of date).
                if q.count == 1 {
                    let _ = m.sync();
                } else {
                    m.refresh_if_older(crate::share::lookup::REFRESH_AFTER);
                }
            }
            return Resp::ok(0, vec![]);
        }
        op::BUS_SCAN => return bus_scan(st, &q, me),
        op::SHARE_ADMIN => {
            let admin = st.registry.held().clone();
            return match admin {
                None => Resp { code: C_IO, ext: 0, data: b"this daemon does not run a registry (registry_listen in share.conf)".to_vec() },
                Some(a) => match a.run(&String::from_utf8_lossy(&q.data), crate::share::regserver::unix_now()) {
                    Ok(t) => Resp::ok(0, t.into_bytes()),
                    Err(e) => Resp { code: C_IO, ext: 0, data: e.into_bytes() },
                },
            };
        }
        op::LIST => {
            // One line per board number, absent adapters included (a fourth field says so), so a number always means
            // the same line; a forgotten one has a fifth. Last, `model=` and what the adapter says it is.
            return done(st.refresh(), |l| {
                if q.flags & F_BIND != 0 {
                    let gens = l.iter().enumerate().filter(|(_, s)| s.present).map(|(i, s)| (i as u16, s.gen)).collect();
                    st.bound.held().insert(me, gens);
                }
                st.learn_models(&l);
                let models = st.models.held().clone();
                let text: String = l
                    .iter()
                    .map(|s| {
                        let d = &s.entry;
                        let state = if s.present { "" } else if s.forgotten { "\tabsent\tforgotten" } else { "\tabsent" };
                        let model = models.get(&d.key()).map(|m| format!("\tmodel={}", m.replace(['\t', '\n'], " "))).unwrap_or_default();
                        format!("{}\t{}\t{}{state}{model}\n", d.serial, d.pid, d.kind.name())
                    })
                    .collect();
                Resp::ok(l.len() as u32, text.into_bytes())
            })
        }
        op::FORGET => {
            let r = st.refresh().and_then(|_| st.boards.held().forget(q.board));
            if r.is_ok() {
                log(&format!("board {}: forgotten", q.board));
            }
            return done(r, |_| Resp::ok(0, vec![]));
        }
        op::USB_LIST => {
            // One line per number, absent instruments included, like LIST.
            return done(st.usb_refresh(), |l| {
                let slots = st.usb.held().slots().to_vec();
                let text: String = slots
                    .iter()
                    .map(|(key, present)| {
                        let mut f = key.splitn(3, ':');
                        let (vid, pid, serial) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                        let product = l.iter().find(|x| &x.key() == key).map(|x| x.product.as_str()).unwrap_or("");
                        format!("{vid}\t{pid}\t{serial}\t{product}{}\n", if *present { "" } else { "\tabsent" })
                    })
                    .collect();
                Resp::ok(slots.len() as u32, text.into_bytes())
            });
        }
        op::SHUTDOWN => return Resp::ok(0, vec![]), // the connection loop then exits the process
        op::SRQ_SUB => return Resp::ok(st.srq.subscribe(q.target(), me), vec![]),
        op::SRQ_UNSUB => {
            st.srq.unsubscribe(q.count, me);
            return Resp::ok(0, vec![]);
        }
        op::SRQ_WAIT => return st.srq.wait(q.count, me, q.tmo),
        op::LOCK => return lock(st, &q, me),
        op::UNLOCK => {
            let r = st.locks.held().release(q.target(), me);
            st.lock_cv.notify_all();
            return match r {
                Some(held) => Resp::ok(held as u32, vec![]),
                None => Resp::err(&Error::NotLocked, 0),
            };
        }
        op::LOCK_STATE => return Resp::ok(st.locks.held().state(q.target()) as u32, vec![]),
        _ => {}
    }
    // Who used each instrument last, for a write that wants it quiet (F_IF_QUIET: identity.rs's *IDN?).
    if locked(q.op) {
        let t = q.target();
        let last = st.touched.held().get(&t).copied();
        if let (Some((who, at)), true) = (last, q.op == op::WRITE && q.flags & F_IF_QUIET != 0) {
            // Another program's that is still connected: one that has gone will read no answer, and a
            // connection of the asker's own process (an earlier session of the alias being checked) is not another program's.
            let other = who != me && {
                let conns = st.conns.held();
                let pid = |c: u64| conns.get(&c).and_then(|k| k.pid);
                conns.contains_key(&who) && (pid(who).is_none() || pid(who) != pid(me))
            };
            if at.elapsed() < QUIET_FOR && other {
                return Resp::err(&Error::Busy, 0);
            }
        }
        let mut touched = st.touched.held();
        if touched.len() > 256 {
            touched.retain(|_, (_, at)| at.elapsed() < QUIET_FOR);
        }
        touched.insert(t, (me, Instant::now()));
    }
    if q.flags & F_USB != 0 {
        return handle_usb(st, q, me);
    }
    if q.flags & F_ASRL != 0 {
        return handle_asrl(st, q, me);
    }
    // A GPIB address is 0..=30: the adapters' `0x20 + pad` with anything more is some other interface
    // message (31 is UNL). The library never sends one; the wire is checked anyway.
    if matches!(q.op, op::PROBE | op::WRITE | op::READ | op::CLEAR | op::TRIGGER | op::SPOLL | op::LOCAL) && q.pad > 30 {
        return Resp::err(&Error::Status(crate::proto::ST_BAD_REQUEST), 0);
    }
    let (key, a) = match st.adapter_for(q.board, me) {
        Ok(x) => x,
        Err(e) => {
            // Opening an adapter failed. A listing probes 31 addresses, so say it once per distinct
            // reason, not 31 times.
            static LAST: Mutex<String> = Mutex::new(String::new());
            let msg = format!("board {}: cannot open the adapter: {e}", q.board);
            let mut last = LAST.held();
            if *last != msg {
                log(&msg);
                *last = msg;
            }
            return Resp::err(&e, 0);
        }
    };
    let mut bus = a.lock();
    // Checked with the bus held, so a lock granted from here on waits for this operation, and one
    // held now keeps it out. Operations that name no session's instrument (a probe, the T1 level,
    // the line states) are not the lock's business.
    if locked(q.op) && !st.locks.held().allowed(q.target(), me) {
        return Resp::err(&Error::Locked, 0);
    }
    let resp = if q.sad != NO_SAD && matches!(q.op, op::PROBE | op::WRITE | op::READ | op::CLEAR | op::TRIGGER | op::SPOLL | op::LOCAL) {
        secondary(st, &q, &mut *bus)
    } else {
        match q.op {
            op::PROBE => done(bus.probe(q.pad), |f| Resp::ok(f as u32, vec![])),
            op::WRITE => match bus.write(q.pad, &q.data, q.flags & F_EOI != 0, q.tmo) {
                Ok(n) => Resp::ok(n as u32, vec![]),
                Err((n, e)) => Resp::err(&e, n as u32),
            },
            op::READ => done(
                bus.read_message(q.pad, q.count as usize, (q.flags & F_TERM != 0).then_some(q.term), q.tmo),
                |r| Resp::ok(r.end as u32, r.data),
            ),
            op::CLEAR => done(bus.clear(q.pad), |_| Resp::ok(0, vec![])),
            op::TRIGGER => done(bus.trigger(q.pad), |_| Resp::ok(0, vec![])),
            op::SPOLL => match st.srq.owed.held().remove(&q.target()) {
                Some(stb) => Resp::ok(stb as u32, vec![]),
                None => done(bus.spoll(q.pad, q.tmo), |b| Resp::ok(b as u32, vec![])),
            },
            op::LOCAL => done(bus.local(q.pad), |_| Resp::ok(0, vec![])),
            op::LLO => done(bus.llo(), |_| Resp::ok(0, vec![])),
            op::REN => {
                let on = q.flags & F_ON != 0;
                let r = done(bus.ren(on), |_| Resp::ok(0, vec![]));
                if r.code == C_OK {
                    let mut held = st.ren_held.held();
                    if on { held.insert(q.board); } else { held.remove(&q.board); }
                }
                r
            }
            op::IFC => done(bus.ifc(), |_| Resp::ok(0, vec![])),
            op::SPEED => {
                let level = q.count as u8;
                let r = bus.speed(level);
                // A level that was SET is remembered, so the adapter gets it again whenever it is opened
                // (speedconf.rs); a query changes nothing.
                if let Ok(l) = r {
                    if level != crate::proto::SPEED_QUERY {
                        if let Err(e) = crate::speedconf::remember(&key, l) {
                            log(&format!("board {}: T1 level {} set but not remembered: {e}", q.board, crate::proto::speed_name(l)));
                        }
                    }
                }
                done(r, |l| Resp::ok(l as u32, vec![]))
            }
            // The lines in the low half; in the high half the ones the adapter cannot tell.
            op::LINES => done(bus.lines_known(), |(l, known)| Resp::ok(l as u32 | ((!known as u32) << 16), vec![])),
            op::CMD => done(bus.command(&q.data, q.tmo), |_| Resp::ok(0, vec![])),
            op::RAW_WRITE => {
                bus.raw_for(RAW_INTFC, false);
                match bus.raw_write(&q.data, q.flags & F_EOI != 0, q.tmo) {
                    Ok(n) => Resp::ok(n as u32, vec![]),
                    Err((n, e)) => Resp::err(&e, n as u32),
                }
            }
            op::RAW_READ => {
                bus.raw_for(RAW_INTFC, true);
                done(bus.raw_read(q.count as usize, (q.flags & F_TERM != 0).then_some(q.term), q.tmo), |r| {
                    Resp::ok(r.end as u32, r.data)
                })
            }
            other => Resp::err(&Error::Io(format!("unknown operation {other}")), 0),
        }
    };
    // A serial poll the adapter made on its own (ks.rs: before a write, and to probe) took an
    // instrument's request for service with it; delivered here, or nobody would ever see it.
    for (pad, stb) in bus.take_polled() {
        log(&format!("SRQ: board {} pad {pad} status byte {stb:#04x} (polled by the adapter)", q.board));
        st.srq.deliver(Target::Gpib(q.board, pad, NO_SAD), stb);
    }
    drop(bus);
    if matches!(resp.code, C_GONE | C_IO) {
        st.forget(&key);
    }
    resp
}

/// An instrument operation at a secondary address (backend.rs: made of the interface level).
fn secondary(st: &State, q: &Req, bus: &mut dyn crate::backend::BusOps) -> Resp {
    let (pad, sad) = (q.pad, q.sad);
    if sad > 30 {
        return Resp::err(&Error::Status(crate::proto::ST_BAD_REQUEST), 0);
    }
    match q.op {
        op::PROBE => done(bus.probe_sad(pad, sad), |f| Resp::ok(f as u32, vec![])),
        op::WRITE => match bus.write_sad(pad, sad, &q.data, q.flags & F_EOI != 0, q.tmo) {
            Ok(n) => Resp::ok(n as u32, vec![]),
            Err((n, e)) => Resp::err(&e, n as u32),
        },
        op::READ => done(
            bus.read_sad(pad, sad, q.count as usize, (q.flags & F_TERM != 0).then_some(q.term), q.tmo),
            |r| Resp::ok(r.end as u32, r.data),
        ),
        op::CLEAR => done(bus.clear_sad(pad, sad), |_| Resp::ok(0, vec![])),
        op::TRIGGER => done(bus.trigger_sad(pad, sad), |_| Resp::ok(0, vec![])),
        op::SPOLL => match st.srq.owed.held().remove(&q.target()) {
            Some(stb) => Resp::ok(stb as u32, vec![]),
            None => done(bus.spoll_sad(pad, sad, q.tmo), |b| Resp::ok(b as u32, vec![])),
        },
        _ => done(bus.local_sad(pad, sad), |_| Resp::ok(0, vec![])), // op::LOCAL
    }
}

/// A request for a USBTMC instrument (F_USB): the instrument operations, and nothing that names a
/// bus. The instrument is held for the whole operation, as a GPIB adapter's bus is.
fn handle_usb(st: &State, q: Req, me: crate::locks::Owner) -> Resp {
    let (key, t) = match st.usb(q.board) {
        Ok(x) => x,
        Err(e) => {
            static LAST: Mutex<String> = Mutex::new(String::new());
            let msg = format!("USBTMC {}: cannot open it: {e}", q.board);
            let mut last = LAST.held();
            if *last != msg {
                log(&msg);
                *last = msg;
            }
            return Resp::err(&e, 0);
        }
    };
    if t.shared {
        st.tmc_users.held().entry(key.clone()).or_default().insert(me);
    }
    let mut ops = t.lock();
    if locked(q.op) && !st.locks.held().allowed(q.target(), me) {
        return Resp::err(&Error::Locked, 0);
    }
    let resp = match q.op {
        op::WRITE => match ops.write(&q.data, q.flags & F_EOI != 0, q.tmo) {
            Ok(n) => Resp::ok(n as u32, vec![]),
            Err((n, e)) => Resp::err(&e, n as u32),
        },
        op::READ => done(ops.read(q.count as usize, (q.flags & F_TERM != 0).then_some(q.term), q.tmo), |r| {
            Resp::ok(r.end as u32, r.data)
        }),
        op::CLEAR => done(ops.clear(), |_| Resp::ok(0, vec![])),
        op::TRIGGER => done(ops.trigger(), |_| Resp::ok(0, vec![])),
        op::SPOLL => match st.srq.owed.held().remove(&q.target()) {
            Some(stb) => Resp::ok(stb as u32, vec![]),
            None => done(ops.read_stb(q.tmo), |b| Resp::ok(b as u32, vec![])),
        },
        op::LOCAL => done(ops.local(), |_| Resp::ok(0, vec![])),
        op::LLO => done(ops.llo(), |_| Resp::ok(0, vec![])),
        // Getting here means the instrument is open: viOpen asks this so that an instrument
        // something else holds fails the open, not the first read. The interrupt endpoint's packet
        // size goes with it (VI_ATTR_USB_MAX_INTR_SIZE).
        op::PROBE => Resp::ok(1 | (t.intr_max as u32) << 16, vec![]),
        op::REN => done(ops.ren(q.flags & F_ON != 0), |_| Resp::ok(0, vec![])),
        op::USB_CTRL if q.data.len() >= 4 => {
            let (rt, req, value) = (q.count as u8, (q.count >> 8) as u8, (q.count >> 16) as u16);
            let index = u16::from_le_bytes([q.data[0], q.data[1]]);
            let length = u16::from_le_bytes([q.data[2], q.data[3]]);
            done(ops.control(rt, req, value, index, length, &q.data[4..]), |d| Resp::ok(d.len() as u32, d))
        }
        // A USBTMC instrument has no bus: no probe, IFC, lines, T1 level or interface level.
        _ => Resp::err(&Error::Status(crate::proto::ST_UNSUPPORTED), 0),
    };
    drop(ops);
    // Only a device that has gone is let go of: an instrument refusing a request is an answer, not a broken handle.
    if resp.code == C_GONE {
        drop(t);
        st.forget_usb(&key);
    }
    if resp.code != C_OK && resp.code != C_TIMEOUT {
        log(&format!("USBTMC {} ({key}): op {} failed: {:?}", q.board, q.op, resp.error()));
    }
    resp
}

/// A request for a serial port (F_ASRL). Any request opens the port for the connection that
/// makes it, if it is not open yet; it stays open until the last connection that used it goes
/// (`release_serial`). The port is held for the whole operation, as a GPIB bus is.
fn handle_asrl(st: &State, q: Req, me: crate::locks::Owner) -> Resp {
    use crate::serial::{Port, Status};
    let n = q.board;
    // A port configured as a Prologix/AR488 bridge belongs to that board (prologix.rs).
    if crate::prologix::is_bridge(n) {
        return Resp::err(&Error::NoBoard, 0);
    }
    let slot = st.serial.held().entry(n).or_default().clone();
    let mut s = slot.held();
    s.users.insert(me);
    if s.port.is_none() {
        match Port::open(n, s.cfg) {
            Ok(p) => {
                let c = p.config();
                log(&format!("ASRL{n}: opened, {} baud, {} data bits, parity {}, stop {}, flow {}", c.baud, c.data_bits, c.parity, c.stop_bits, c.flow));
                s.port = Some(p);
            }
            Err(e) => {
                // Not there, or another program has it: this connection does not use it after all.
                s.users.remove(&me);
                let unused = s.users.is_empty();
                drop(s);
                if unused {
                    st.serial.held().remove(&n);
                }
                return Resp::err(&e, 0);
            }
        }
    }
    if locked(q.op) && !st.locks.held().allowed(q.target(), me) {
        return Resp::err(&Error::Locked, 0);
    }
    let p = s.port.as_mut().unwrap();
    let resp = match q.op {
        op::PROBE => Resp::ok(1, vec![]),
        op::WRITE => match p.write(&q.data, q.tmo, (q.flags & F_EOI != 0).then_some(q.count as u16)) {
            Ok(n) => Resp::ok(n as u32, vec![]),
            Err((n, e)) => Resp::err(&e, n as u32),
        },
        op::READ => done(p.read(q.count as usize, (q.flags & F_TERM != 0).then_some(q.term), q.tmo), |r| {
            Resp::ok(r.end as u32, r.data)
        }),
        op::CLEAR => done(p.clear(q.flags & F_ON != 0, q.count as u16, q.tmo), |_| Resp::ok(0, vec![])),
        op::TRIGGER => done(p.trigger(q.tmo), |_| Resp::ok(0, vec![])),
        op::SPOLL => done(p.stb(q.tmo), |b| Resp::ok(b as u32, vec![])),
        op::ASRL_SET => match <[u8; 4]>::try_from(q.data.as_slice()) {
            Ok(v) => done(p.set(q.count, u32::from_le_bytes(v)), |_| Resp::ok(0, vec![])),
            Err(_) => Resp::err(&Error::Io("ASRL_SET wants a u32".into()), 0),
        },
        op::ASRL_GET => done(p.status(), |x: Status| Resp::ok(0, x.encode())),
        op::FLUSH => done(p.flush(q.count, q.tmo), |_| Resp::ok(0, vec![])),
        // No bus, no GPIB messages, no T1 level.
        _ => Resp::err(&Error::Status(crate::proto::ST_UNSUPPORTED), 0),
    };
    let cfg = p.config();
    s.cfg = cfg;
    if resp.code == C_GONE {
        // Unplugged: the handle goes, the settings stay; the next request opens the port again.
        log(&format!("ASRL{n}: gone"));
        s.port = None;
    }
    resp
}

/// A connection went: it no longer uses any serial port, and a port nobody uses is closed.
fn release_serial(st: &State, me: crate::locks::Owner) {
    let slots: Vec<(u16, Arc<Mutex<SerialSlot>>)> = st.serial.held().iter().map(|(n, s)| (*n, s.clone())).collect();
    for (n, slot) in slots {
        let mut s = slot.held();
        if s.users.remove(&me) && s.users.is_empty() {
            if s.port.take().is_some() {
                log(&format!("ASRL{n}: closed"));
            }
            drop(s);
            let mut m = st.serial.held();
            // Unless another connection has taken it up meanwhile.
            if m.get(&n).is_some_and(|x| Arc::ptr_eq(x, &slot) && x.held().users.is_empty()) {
                m.remove(&n);
            }
        }
    }
}

/// A connection ended: an instrument open through a vendor's USBTMC driver that no remaining connection has used is
/// closed, so the daemon does not hold it while idle.
fn release_usb(st: &State, me: crate::locks::Owner) {
    let done: Vec<String> = {
        let mut users = st.tmc_users.held();
        let keys: Vec<String> = users.iter_mut().filter_map(|(k, u)| (u.remove(&me) && u.is_empty()).then(|| k.clone())).collect();
        for k in &keys {
            users.remove(k);
        }
        keys
    };
    for key in done {
        // Taken out under the lock and dropped after it: closing waits for the interrupt thread, and
        // every other USBTMC request would wait on the lock meanwhile.
        let gone = st.tmcs.held().remove(&key);
        if gone.is_some() {
            drop(gone);
            log(&format!("USBTMC {key}: closed (no connection uses it)"));
        }
    }
}

/// viLock: grant it now, or wait for it until the timeout (`count` ms; u32::MAX waits forever).
fn lock(st: &State, q: &Req, me: crate::locks::Owner) -> Resp {
    use crate::locks::{LockType, Refused};
    let kind = match q.flags & 3 {
        1 => LockType::Exclusive,
        2 => LockType::Shared,
        _ => return Resp::err(&Error::Io("bad lock type".into()), 0),
    };
    let key = (!q.data.is_empty()).then(|| String::from_utf8_lossy(&q.data).into_owned());
    let deadline = (q.count != u32::MAX).then(|| Instant::now() + Duration::from_millis(q.count as u64));
    let mut locks = st.locks.held();
    loop {
        match locks.acquire(q.target(), me, kind, key.as_deref()) {
            Ok(g) => return Resp::ok(g.nested as u32, g.key.into_bytes()),
            Err(Refused::BadKey) => return Resp::err(&Error::LockKey, 0),
            Err(Refused::Wait) => {}
        }
        let left = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Resp::err(&Error::Timeout, 0);
                }
                left.min(Duration::from_secs(1))
            }
            None => Duration::from_secs(1),
        };
        locks = st.lock_cv.wait_timeout(locks, left).unwrap_or_else(PoisonError::into_inner).0;
        // Looked at once a second: a client that went is not given the lock (nobody would hear it).
        if st.hangup.held().get(&me).is_some_and(local::hung_up) {
            return Resp::err(&Error::Gone, 0);
        }
    }
}

/// SIGTERM and SIGINT stop the daemon the way `symbiotic-ctl stop` does (`exit_clean`). The signals are blocked in
/// every thread and taken by one that waits for them.
#[cfg(unix)]
fn stop_on_signal(st: Arc<State>, listener: Arc<Listener>) {
    // SAFETY: a sigset_t is plain data, filled by sigemptyset/sigaddset; masks and waits on it are
    // what these calls are for.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return;
        }
        std::thread::spawn(move || {
            let mut sig = 0;
            if libc::sigwait(&set, &mut sig) == 0 {
                log(&format!("signal {sig}: stopping"));
                exit_clean(&st, &listener);
            }
        });
    }
}

/// Leave the adapters the way a clean close does (each one's Drop aborts what is outstanding and resets its chip), then
/// the channel; `process::exit` would skip the destructors.
fn exit_clean(st: &State, listener: &Listener) -> ! {
    release_hardware(st);
    listener.cleanup();
    std::process::exit(0)
}

/// Let go of everything that is open -- adapters, USBTMC instruments, serial ports -- the way a clean close does, so
/// another tool or machine can have the hardware. A daemon that stays (it serves a group) does this when nobody is
/// using it. Returns whether anything was open.
fn release_hardware(st: &State) -> bool {
    // Taken out of the maps first and dropped after: closing an adapter is USB work.
    let adapters = std::mem::take(&mut *st.adapters.held());
    let tmcs = std::mem::take(&mut *st.tmcs.held());
    let serial = std::mem::take(&mut *st.serial.held());
    let any = !(adapters.is_empty() && tmcs.is_empty() && serial.is_empty());
    drop((adapters, tmcs, serial));
    any
}

/// Serve one request for connection `me`: watched by the watchdog while it runs, and the service
/// request subscriptions it makes noted in `mine` (dropped when the connection goes).
fn track(st: &State, me: u64, q: Req, mine: &mut Vec<u32>) -> Resp {
    st.activity.fetch_add(1, Ordering::Relaxed);
    if let Some(c) = st.conns.held().get_mut(&me) {
        c.last = Instant::now();
    }
    let (op, unsub) = (q.op, q.count);
    // Waiting for a service request or a lock takes as long as it takes: not watched.
    let watched = props(op).is_none_or(|p| p.watched);
    if watched {
        let start = Instant::now();
        let what = format!("conn {me}: op {} board {} pad {}", q.op, q.board, q.pad);
        let overdue = start + Duration::from_millis(q.tmo as u64) + Duration::from_secs(5);
        st.busy.held().insert(me, Busy { what, start, overdue, said: false });
    }
    // A bug of ours fails this one request, not the connection and what it holds: the panic hook has
    // logged it, and the client hears an I/O error.
    let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(st, q, me)))
        .unwrap_or_else(|_| Resp::err(&Error::Io("the daemon failed this request (symbioticd.log says why)".into()), 0));
    if watched {
        if let Some(b) = st.busy.held().remove(&me) {
            if b.said {
                log(&format!("{} finished after {:.1} s", b.what, b.start.elapsed().as_secs_f64()));
            }
        }
    }
    if op == op::SRQ_SUB && resp.code == C_OK {
        mine.push(resp.ext);
    } else if op == op::SRQ_UNSUB {
        mine.retain(|i| *i != unsub);
    }
    resp
}

/// A connection ended: it holds no subscriptions, locks or serial port any more.
fn leave(st: &State, me: u64, mine: Vec<u32>) {
    let mut subs = st.srq.subs.held();
    for id in mine {
        subs.remove(&id);
    }
    drop(subs);
    st.locks.held().drop_owner(me);
    release_serial(st, me);
    release_usb(st, me);
    st.conns.held().remove(&me);
    st.hangup.held().remove(&me);
    st.bound.held().remove(&me);
    st.lock_cv.notify_all();
}

/// What a connection holds -- locks, subscriptions, serial ports, its place in `conns` -- given back when it ends,
/// however it ends (a panic included).
struct Gone<'a> {
    st: &'a State,
    me: u64,
    /// The subscriptions this connection made.
    mine: Vec<u32>,
}

impl Drop for Gone<'_> {
    fn drop(&mut self) {
        leave(self.st, self.me, std::mem::take(&mut self.mine));
    }
}

fn serve(st: Arc<State>, mut s: local::Stream, listener: Arc<Listener>) {
    let me = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    st.conns.held().insert(me, Conn::new(local::peer_pid(&s), None));
    if let Ok(c) = s.try_clone() {
        st.hangup.held().insert(me, c);
    }
    let mut gone = Gone { st: &st, me, mine: Vec::new() };
    while let Ok(q) = Req::read_from(&mut s) {
        if q.op == op::PEER_CONNECT {
            // The rest of this connection belongs to the owner of the alias.
            tunnel(&st, &mut s, q);
            break;
        }
        let stop = q.op == op::SHUTDOWN;
        let resp = track(&st, me, q, &mut gone.mine);
        if resp.write_to(&mut s).is_err() {
            break;
        }
        if stop {
            let who = st.conns.held().get(&me).and_then(|c| c.pid).map(|p| format!(" by process {p}")).unwrap_or_default();
            log(&format!("asked to stop{who}"));
            exit_clean(&st, &listener);
        }
    }
}

/// This machine's part in its group, made when first wanted: a machine that shares has it from the
/// start; one that only uses the group's aliases builds it at the first.
fn member_of(st: &State) -> Result<Arc<crate::share::member::Member>, String> {
    use crate::share::{conf, keys, member::Member};
    let mut g = st.member.held();
    if let Some(m) = g.as_ref() {
        return Ok(m.clone());
    }
    let mut c = conf::ShareConf::load();
    if !c.joined() {
        return Err("this machine has not joined a group (symbiotic-ctl share join)".to_string());
    }
    c.share = false; // it uses the group; serving it is `start_sharing`'s
    let key = crate::paths::user_file("share.key").and_then(|p| keys::load(&p)).ok_or("no share.key")?;
    let m = Arc::new(Member::new(c, key.noise, crate::paths::user_file("share.dir"), crate::alias::load, conf::load_shares));
    *g = Some(m.clone());
    Ok(m)
}

/// How long to wait for the owner's answer to `q`: the time the request itself may take, plus 2 s for the network.
/// None: as long as it takes (a lock without limit).
fn peer_wait(q: &Req) -> Option<Duration> {
    let slack = Duration::from_secs(2);
    match q.op {
        op::LOCK if q.count == u32::MAX => None,
        op::LOCK => Some(Duration::from_millis(q.count as u64) + slack),
        _ if q.tmo == crate::backend::TMO_INFINITE => None,
        _ => Some(Duration::from_millis(q.tmo as u64) + slack),
    }
}

/// PEER_CONNECT: connect to the owner of the alias, and carry this connection's requests there
/// until it ends. Not being able to reach the alias is C_NOBOARD (the VISA library says the
/// resource was not found); the reason is in the log.
fn tunnel(st: &State, s: &mut local::Stream, q: Req) {
    let alias = String::from_utf8_lossy(&q.data).into_owned();
    let no = |s: &mut local::Stream, why: &str| {
        log(&format!("shared alias {alias}: {why}"));
        let _ = Resp { code: C_NOBOARD, ext: 0, data: why.as_bytes().to_vec() }.write_to(s);
    };
    let m = match member_of(st) {
        Ok(m) => m,
        Err(e) => return no(s, &e),
    };
    m.refresh_if_older(10);
    let (mut ch, opened) = match m.connect_peer(&alias) {
        Ok(x) => x,
        Err(e) => return no(s, &e.to_string()),
    };
    if Resp::ok(0, crate::share::peer::encode_opened(&opened)).write_to(s).is_err() {
        return;
    }
    while let Ok(q) = Req::read_from(s) {
        let _ = ch.stream().set_read_timeout(peer_wait(&q));
        let r = match crate::share::peer::call(&mut ch, &q, opened.header) {
            Ok(r) => r,
            Err(e) => {
                // The owner went, or answered too late to trust the framing: the session is lost.
                log(&format!("shared alias {alias}: {e}"));
                let _ = Resp { code: C_GONE, ext: 0, data: vec![] }.write_to(s);
                return;
            }
        };
        if r.write_to(s).is_err() {
            return;
        }
    }
}

/// One connection, as CLIENTS reports it.
struct Conn {
    /// Its process (None where the system does not say, and for a machine of the group).
    pid: Option<u32>,
    /// The machine of the group it serves, as share/member.rs describes it.
    peer: Option<String>,
    since: Instant,
    /// When its last request came.
    last: Instant,
}

impl Conn {
    fn new(pid: Option<u32>, peer: Option<String>) -> Conn {
        let now = Instant::now();
        Conn { pid, peer, since: now, last: now }
    }
}

/// The daemon as the machines of a sharing group see it (share/peer.rs): each peer connection is
/// a client like a local one, with its own locks and subscriptions.
struct Bridge {
    st: Arc<State>,
    subs: Mutex<HashMap<u64, Vec<u32>>>,
}

impl crate::share::peer::PeerHost for Bridge {
    fn begin(&self, who: &str) -> u64 {
        let me = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
        self.st.conns.held().insert(me, Conn::new(None, Some(who.to_string())));
        self.subs.held().insert(me, Vec::new());
        me
    }

    fn call(&self, me: u64, q: Req) -> Resp {
        let mut mine = self.subs.held().remove(&me).unwrap_or_default();
        let r = track(&self.st, me, q, &mut mine);
        self.subs.held().insert(me, mine);
        r
    }

    fn end(&self, me: u64) {
        let mine = self.subs.held().remove(&me).unwrap_or_default();
        leave(&self.st, me, mine);
    }
}

/// Start what `share.conf` asks for. Returns whether this daemon serves a group, in which case it
/// stays resident: nobody can reach a daemon that has exited.
fn start_sharing(st: &Arc<State>) -> bool {
    use crate::share::{admin, conf, keys, member::Member, regserver, registry::Registry};
    let c = conf::ShareConf::load();
    let mut resident = false;
    if let (Some(addr), Some(dir)) = (&c.registry_listen, crate::paths::user_file("registry")) {
        let r = (|| -> io::Result<()> {
            let k = keys::load_or_create(&dir.join("registry.key"), true)?;
            let reg = Registry::open(&dir.join("registry.state"), k.sign.clone().unwrap(), k.noise.public)?;
            let reg = Arc::new(Mutex::new(reg));
            let l = std::net::TcpListener::bind(addr)?;
            *st.registry.held() = Some(Arc::new(admin::Admin { reg: reg.clone(), advertise: c.registry_advertise.clone() }));
            std::thread::spawn(move || regserver::run(l, reg, k.noise, |m| log(&format!("registry: {m}"))));
            log(&format!("registry listening on {addr}"));
            Ok(())
        })();
        match r {
            Ok(()) => resident = true,
            Err(e) => log(&format!("registry not started: {e}")),
        }
    }
    if c.share {
        let r = (|| -> io::Result<()> {
            if !c.joined() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "share = on but this machine has not joined a group"));
            }
            let keys = keys::load_or_create(&crate::paths::user_file("share.key").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no configuration directory"))?, false)?;
            let units = st.clone();
            let m = Arc::new(
                Member::new(c.clone(), keys.noise, crate::paths::user_file("share.dir"), crate::alias::load, conf::load_shares)
                    .with_check(gpib_check(st.clone()))
                    .with_gpib(
                        move || {
                            let u = units.units.held();
                            Some(u.iter().filter_map(|((b, p), t)| t.clone().map(|t| (crate::resource::format(crate::resource::Instr::primary(*b, *p)), t))).collect())
                        },
                        // Settings belong to the instrument: they go with the alias (lanunits.rs).
                        crate::lanunits::follow_alias,
                    ),
            );
            let l = std::net::TcpListener::bind(&c.listen)?;
            let host = Arc::new(Bridge { st: st.clone(), subs: Mutex::new(HashMap::new()) });
            // Who used this machine's instruments: a line in the log and one in usage.log (share/usage.rs).
            let used = Arc::new(|r: &crate::share::usage::Record| {
                log(&format!("shared alias {}: used by {} ({}) from {} for {:.1} s, {} request(s)", r.alias, r.member, r.key, r.from, r.secs, r.requests));
                if let Err(e) = crate::share::usage::append_to(&crate::share::usage::path(), r) {
                    log(&format!("usage.log: {e}"));
                }
            });
            m.listen(l, host, used)?;
            *st.member.held() = Some(m.clone());
            m.run_sync_loop(|s| log(&format!("share: {s}")));
            let watch = st.clone();
            std::thread::spawn(move || bus_watch(watch));
            if c.follow {
                let me = m.clone();
                std::thread::spawn(move || lan_follow(me));
            }

            log(&format!("serving aliases to the group on {}", c.listen));
            Ok(())
        })();
        match r {
            Ok(()) => resident = true,
            Err(e) => log(&format!("sharing not started: {e}")),
        }
    }
    resident
}

/// The addresses of one board that have a listener (a probe of each of 1 to 30; the probe writes nothing to an instrument). Stored for
/// `bus_scan` and for the heartbeat.
fn scan_pads<B: backend::BusOps + ?Sized>(bus: &mut B) -> Result<Vec<u8>, Error> {
    let mut found = Vec::new();
    for pad in 1..=30u8 {
        if bus.probe(pad)? {
            found.push(pad);
        }
    }
    Ok(found)
}

fn scan_resp(st: &State, q: &Req, state: u32, pads: &[u8], age: Duration) -> Resp {
    let mut d = (age.as_secs().min(u32::MAX as u64) as u32).to_le_bytes().to_vec();
    if q.flags & F_UNITS == 0 {
        d.extend_from_slice(pads);
        return Resp::ok(state, d);
    }
    let units = st.units.held();
    for &pad in pads {
        d.push(pad);
        match units.get(&(q.board, pad)).cloned().flatten() {
            Some(idn) => {
                let b = &idn.as_bytes()[..idn.len().min(0xFFFE)];
                d.extend_from_slice(&(b.len() as u16).to_le_bytes());
                d.extend_from_slice(b);
            }
            None => d.extend_from_slice(&0xFFFFu16.to_le_bytes()),
        }
    }
    Resp::ok(state | 2, d)
}

/// BUS_SCAN: a kept result that is young enough, else a scan now -- unless another program uses the service or the
/// adapter is busy. A board that holds REN is scanned with REN released and then put back (an instrument addressed
/// under REN goes to Remote); only on request, never by `bus_watch`.
fn bus_scan(st: &State, q: &Req, me: crate::locks::Owner) -> Resp {
    let board = q.board;
    if let Some((pads, t)) = st.bus.held().get(&board) {
        if t.elapsed() <= Duration::from_secs(q.count as u64) {
            return scan_resp(st, q, 1, pads, t.elapsed());
        }
    }
    let pid = q.data.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // Unknown processes count as others: where the system does not say, nothing is probed.
    if st.conns.held().iter().any(|(c, k)| *c != me && k.pid != pid) {
        return Resp::err(&Error::Busy, 0);
    }
    let (_, a) = match st.adapter_for(board, me) {
        Ok(x) => x,
        Err(e) => return Resp::err(&e, 0),
    };
    let Some(mut bus) = a.try_lock() else { return Resp::err(&Error::Busy, 0) };
    let hold = st.ren_held.held().contains(&board);
    if hold && bus.ren(false).is_err() {
        return Resp::err(&Error::Status(crate::proto::ST_UNSUPPORTED), 0);
    }
    let r = scan_pads(&mut *bus);
    if hold {
        let _ = bus.ren(true);
    }
    match r {
        Ok(pads) => {
            st.bus.held().insert(board, (pads.clone(), Instant::now()));
            scan_resp(st, q, 0, &pads, Duration::ZERO)
        }
        Err(e) => Resp::err(&e, 0),
    }
}

/// Every ten seconds, while this daemon shares and nobody is connected: see which adapters are plugged in, scan each
/// board whose REN is released, and learn who each new listener is (`learn_units`), so the heartbeat has a finding to
/// give and a bound alias can follow its unit. An adapter not yet open is opened for it, unless the user has the daemon
/// let go of idle hardware (daemonconf.rs). It never touches REN.
fn bus_watch(st: Arc<State>) {
    loop {
        std::thread::sleep(Duration::from_secs(crate::share::regserver::HEARTBEAT));
        if !st.conns.held().is_empty() {
            continue;
        }
        // What is plugged in now: an adapter that came or went (to another machine, say) is seen without anybody asking.
        let _ = st.refresh();
        let boards: Vec<u16> = st.boards.held().slots().iter().enumerate().filter(|(_, s)| s.present).map(|(n, _)| n as u16).collect();
        for board in boards {
            // An adapter not yet open is opened to be looked at, unless the user has the daemon let go of idle hardware
            // (daemonconf.rs).
            let a = if crate::daemonconf::release_idle() { st.open_adapter(board) } else { st.adapter(board).ok().map(|(_, a)| a) };
            let Some(a) = a else { continue };
            // Opening it may have asserted REN (the adapter's setting): then it is not looked at.
            if st.ren_held.held().contains(&board) {
                continue;
            }
            let Some(mut bus) = a.try_lock() else { continue };
            match scan_pads(&mut *bus) {
                Ok(pads) => {
                    learn_units(&st, board, &mut *bus, &pads);
                    st.bus.held().insert(board, (pads, Instant::now()));
                }
                Err(Error::NoDevice) => {
                    learn_units(&st, board, &mut *bus, &[]);
                    st.bus.held().insert(board, (Vec::new(), Instant::now()));
                }
                Err(_) => {}
            }
        }
        // An adapter that went takes what was known of its instruments with it.
        let present: Vec<u16> = st.boards.held().slots().iter().enumerate().filter(|(_, s)| s.present).map(|(n, _)| n as u16).collect();
        st.units.held().retain(|(b, _), _| present.contains(b));
        st.bus.held().retain(|b, _| present.contains(b));
    }
}

/// Ask each listener on `board` that was not there before who it is (`*IDN?`, once, where the user allows it), and forget the ones
/// that are gone. Only from the sharing scan: the board is idle (nobody is connected) and its REN released.
fn learn_units<B: backend::BusOps + ?Sized>(st: &State, board: u16, bus: &mut B, pads: &[u8]) {
    st.units.held().retain(|(b, p), _| *b != board || pads.contains(p));
    for &pad in pads {
        if st.units.held().contains_key(&(board, pad)) {
            continue;
        }
        let res = crate::resource::format(crate::resource::Instr::primary(board, pad));
        if !crate::instruments::get(&res).idn {
            continue;
        }
        let said = bus
            .write(pad, b"*IDN?\n", true, 2000)
            .ok()
            // The whole answer, so none of it is left in the instrument for the next program's first read.
            .and_then(|_| bus.read_message(pad, 4096, Some(b'\n'), 2000).ok())
            .map(|r| String::from_utf8_lossy(&r.data).trim().to_string())
            .filter(|t| !t.is_empty());
        if let Some(t) = &said {
            log(&format!("{res} is {t}"));
        }
        st.units.held().insert((board, pad), said);
    }
}

/// With `follow` on: the group's LAN aliases that are bound to a unit follow it when it takes another address (Member::follow_lan).
fn lan_follow(m: Arc<crate::share::member::Member>) {
    use crate::lan::{conf as lconf, discover, name};
    let mut st = crate::share::member::LanFollow::default();
    loop {
        std::thread::sleep(Duration::from_secs(crate::share::regserver::HEARTBEAT));
        let moved = m.follow_lan(
            &mut st,
            &|res| name::parse(res).is_some_and(|t| lconf::check(t, lconf::REACH).result.is_ok()),
            &|| discover::discover_full(discover::WAIT).targets.into_iter().map(|t| t.host).collect(),
            &|res| crate::identity::ask_idn(res, Duration::from_millis(2000)),
        );
        for (alias, to) in moved {
            log(&format!("share: {alias} followed its instrument to {to}"));
        }
    }
}

/// Whether a shared GPIB instrument is there, for the heartbeat: what the daemon's scan last found of its board, as old
/// as it is; nothing known before any scan, or for an address with a secondary one.
fn gpib_check(st: Arc<State>) -> impl Fn(&str) -> (crate::share::registry::Here, u32) + Send + Sync + 'static {
    use crate::share::registry::Here;
    move |res: &str| {
        let Some(Target::Gpib(board, pad, sad)) = crate::resource::target_of(res) else { return (Here::Unknown, 0) };
        if sad != NO_SAD {
            return (Here::Unknown, 0);
        }
        // Its adapter is not attached (unplugged, or given to another machine): the instrument is not there, whatever the last scan said.
        if !st.boards.held().slots().get(board as usize).is_some_and(|s| s.present) {
            return (Here::No, 0);
        }
        match st.bus.held().get(&board) {
            Some((pads, t)) => (if pads.contains(&pad) { Here::Yes } else { Here::No }, t.elapsed().as_secs().min(u32::MAX as u64) as u32),
            None => (Here::Unknown, 0),
        }
    }
}

/// Watch for service requests on boards somebody subscribed to. Idle-only and bounded: it uses
/// `try_lock` so it never queues behind or ahead of a caller, looks at the adapter's own SRQ
/// indication (no addressing), and only when that says SRQ does it serial-poll the SUBSCRIBED
/// instruments. If no subscribed instrument is the one asking (something else holds SRQ), it leaves
/// that board alone for a second rather than polling every tick. Adapters it does not find already
/// open are skipped: watching never opens or wakes one.
fn monitor(st: Arc<State>) {
    let mut quiet_until: HashMap<u16, Instant> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let (gpib, usb) = st.srq.wanted();
        // USBTMC instruments: one with an interrupt endpoint says so itself (its notifications are
        // taken whether or not anyone watches, so none is kept for a watcher that comes later);
        // one without is asked for its status byte, only while watched and only when idle.
        let open: Vec<(String, Arc<Tmc>)> = st.tmcs.held().iter().map(|(k, t)| (k.clone(), t.clone())).collect();
        for (key, t) in open {
            let n = {
                let s = st.usb.held();
                s.slots().iter().position(|(k, _)| *k == key)
            };
            let Some(n) = n.map(|n| n as u16) else { continue };
            let watched = usb.contains(&n);
            if t.notifies() {
                while let Some(stb) = t.take_srq() {
                    if watched {
                        log(&format!("SRQ: USBTMC {n} ({key}) status byte {stb:#04x}"));
                        st.srq.deliver(Target::Usb(n), stb);
                    }
                }
            } else if watched {
                let Some(mut ops) = t.try_lock() else { continue };
                if let Ok(stb) = ops.read_stb(100) {
                    if stb & 0x40 != 0 {
                        log(&format!("SRQ: USBTMC {n} ({key}) status byte {stb:#04x}"));
                        st.srq.deliver(Target::Usb(n), stb);
                    }
                }
            }
        }
        for (board, pads) in gpib {
            if quiet_until.get(&board).is_some_and(|t| *t > Instant::now()) {
                continue;
            }
            let Some(a) = st.open_adapter(board) else { continue };
            let Some(mut bus) = a.try_lock() else { continue };
            if !matches!(bus.srq_asserted(), Ok(true)) {
                continue;
            }
            let mut found = false;
            for (pad, sad) in pads {
                let polled = if sad == NO_SAD { bus.spoll(pad, 100) } else { bus.spoll_sad(pad, sad, 100) };
                if let Ok(stb) = polled {
                    if stb & 0x40 != 0 {
                        found = true;
                        let at = if sad == NO_SAD { format!("pad {pad}") } else { format!("pad {pad} sad {sad}") };
                        log(&format!("SRQ: board {board} {at} status byte {stb:#04x}"));
                        st.srq.deliver(Target::Gpib(board, pad, sad), stb);
                    }
                }
            }
            if !found {
                quiet_until.insert(board, Instant::now() + Duration::from_secs(1));
            }
        }
    }
}

/// Say which request is stuck, and where, while it is stuck: a request that never returns leaves
/// its client waiting with nothing in the log otherwise. Each overdue request is named once, and
/// again when (if) it finishes.
fn watchdog(st: Arc<State>) {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = Instant::now();
        for b in st.busy.held().values_mut().filter(|b| !b.said && now > b.overdue) {
            b.said = true;
            log(&format!("{} still running after {:.1} s", b.what, b.start.elapsed().as_secs_f64()));
        }
    }
}

/// The daemon has no console, so its messages go to `symbioticd.log` (`paths::log_file`).
pub fn log(msg: &str) {
    use std::io::Write;
    // One writer at a time in this process: two threads turning the file over at once could move the
    // fresh one aside too.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.held();
    let p = crate::paths::log_file();
    // Kept to two files of `LOG_MAX` at most: past it the log becomes symbioticd.log.1 (the one before goes).
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::rename(&p, p.with_extension("log.1"));
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600); // the user's alone: it names instruments and machines
    if let Ok(mut f) = o.open(&p) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{t} pid {} {msg}", std::process::id());
    }
}

const LOG_MAX: u64 = 4 << 20;

fn now_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

/// Serve until asked to stop or idle for `idle`. Fails at once if another daemon owns the
/// channel.
pub fn run(idle: Duration) -> io::Result<()> {
    std::panic::set_hook(Box::new(|i| log(&format!("PANIC: {i}"))));
    crate::paths::move_earlier_logs();
    let listener = Arc::new(Listener::bind()?);
    log(&format!("started, version {BUILD}, idle exit after {} s", idle.as_secs()));
    let st = Arc::new(State::default());
    // Before any thread is started, which inherits the signal mask.
    #[cfg(unix)]
    stop_on_signal(st.clone(), listener.clone());
    let active = Arc::new(AtomicUsize::new(0));
    // When a client last connected or left. Idleness is measured from this, not from whether
    // a poll happens to catch a connection open: a short-lived one falls between polls.
    let t0 = Instant::now();
    let last = Arc::new(AtomicU64::new(0));

    let resident = start_sharing(&st);
    let sm = st.clone();
    std::thread::spawn(move || monitor(sm));
    let sw = st.clone();
    std::thread::spawn(move || watchdog(sw));

    // Accepting blocks, so idleness is watched from beside it.
    let (l, a, la, sw) = (listener.clone(), active.clone(), last.clone(), st.clone());
    std::thread::spawn(move || {
        // A daemon that stays does not exit when idle but lets go of the hardware (`release_hardware`), once nobody
        // -- no program here, no machine of the group -- has been connected for `idle`.
        let (mut empty_since, mut released, mut seen) = (None::<Instant>, false, 0u64);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let quiet = now_ms(t0).saturating_sub(la.load(Ordering::SeqCst));
            if !resident && a.load(Ordering::SeqCst) == 0 && quiet > idle.as_millis() as u64 {
                log("idle, exiting");
                exit_clean(&sw, &l);
            }
            if resident {
                let act = sw.activity.load(Ordering::Relaxed);
                if act != seen {
                    // Something was asked since the last look: the hardware may be open again, and the quiet begins afresh.
                    (seen, empty_since, released) = (act, None, false);
                }
                if a.load(Ordering::SeqCst) != 0 || !sw.conns.held().is_empty() {
                    (empty_since, released) = (None, false);
                } else if !released && empty_since.get_or_insert_with(Instant::now).elapsed() > idle {
                    released = true;
                    // Off by default (daemonconf.rs).
                    if crate::daemonconf::release_idle() && release_hardware(&sw) {
                        log("idle: let go of the hardware");
                    }
                }
            }
        }
    });

    loop {
        let s = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                // One connection that failed to establish is not a reason to stop serving.
                log(&format!("accept failed: {e}"));
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        active.fetch_add(1, Ordering::SeqCst);
        last.store(now_ms(t0), Ordering::SeqCst);
        let (st, active, listener, last) = (st.clone(), active.clone(), listener.clone(), last.clone());
        std::thread::spawn(move || {
            // A guard, so a panic in `serve` still counts the connection out: otherwise the
            // daemon would never find itself idle and would keep the adapters for good.
            struct Leave(Arc<AtomicUsize>, Arc<AtomicU64>, Instant);
            impl Drop for Leave {
                fn drop(&mut self) {
                    self.1.store(now_ms(self.2), Ordering::SeqCst);
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _leave = Leave(active, last, t0);
            serve(st, s, listener);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscription_is_its_connections_alone() {
        let srq = Srq::default();
        let t = Target::Gpib(0, 6, 0);
        let id = srq.subscribe(t, 1);
        srq.deliver(t, 0x41);
        assert_eq!(srq.wait(id, 2, 0).ext, 0, "another connection reads nothing from it");
        srq.unsubscribe(id, 2);
        assert!(srq.subs.held().contains_key(&id), "nor takes it away");
        let r = srq.wait(id, 1, 0);
        assert_eq!((r.ext, r.data), (1, vec![0x41]), "its own connection still has the status byte");
        srq.unsubscribe(id, 1);
        assert!(srq.subs.held().is_empty());
    }

    #[test]
    fn a_quiet_write_is_refused_while_another_connection_uses_the_instrument() {
        let st = State::default();
        let (me, other) = (51, 52);
        let at = |op, flags| Req { op, board: 0, pad: 9, flags, data: b"*IDN?\n".to_vec(), ..Default::default() };
        // The other program, connected, wrote a query to GPIB0::9 (whether the board is there does not matter).
        st.conns.held().insert(other, Conn::new(Some(8), None));
        st.conns.held().insert(me, Conn::new(Some(9), None));
        let _ = handle(&st, at(op::WRITE, F_EOI), other);
        assert_eq!(handle(&st, at(op::WRITE, F_EOI | F_IF_QUIET), me).code, C_BUSY, "its answer is not ours to lose");
        assert_ne!(handle(&st, at(op::WRITE, F_EOI), me).code, C_BUSY, "an ordinary write is not held back");
        // Now this connection was the last to use it: quiet enough for itself.
        assert_ne!(handle(&st, at(op::WRITE, F_EOI | F_IF_QUIET), me).code, C_BUSY);
        // A connection of the same process is not another program.
        st.conns.held().insert(53, Conn::new(Some(7), None));
        st.conns.held().insert(54, Conn::new(Some(7), None));
        let _ = handle(&st, at(op::WRITE, F_EOI), 53);
        assert_ne!(handle(&st, at(op::WRITE, F_EOI | F_IF_QUIET), 54).code, C_BUSY);
        // A program that has gone reads no answer: not in the way.
        let _ = handle(&st, at(op::WRITE, F_EOI), other);
        st.conns.held().remove(&other);
        assert_ne!(handle(&st, at(op::WRITE, F_EOI | F_IF_QUIET), me).code, C_BUSY, "its connection has gone");
        st.conns.held().insert(other, Conn::new(Some(8), None));
        // Another instrument is not affected.
        let mut q = at(op::WRITE, F_EOI | F_IF_QUIET);
        q.pad = 10;
        let _ = handle(&st, at(op::READ, 0), other);
        assert_ne!(handle(&st, q, me).code, C_BUSY);
    }

    #[test]
    fn a_connection_that_ends_in_a_panic_gives_back_what_it_held() {
        use crate::locks::LockType;
        let st = State::default();
        let (me, other, t) = (41, 42, Target::Gpib(0, 6, NO_SAD));
        st.conns.held().insert(me, Conn::new(Some(1), None));
        st.locks.held().acquire(t, me, LockType::Exclusive, None).unwrap();
        let id = st.srq.subscribe(t, me);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _gone = Gone { st: &st, me, mine: vec![id] };
            // A lock held while the bug strikes is poisoned; the state behind it is still usable.
            let _held = st.locks.lock().unwrap();
            panic!("a bug in a request");
        }));
        assert!(r.is_err());
        assert!(st.locks.held().allowed(t, other), "its lock went with it");
        assert!(st.conns.held().is_empty() && st.srq.subs.held().is_empty(), "and its place and its subscription");
    }

    #[test]
    fn a_connection_is_not_handed_to_the_adapter_that_took_over_its_number() {
        struct Fake;
        impl Backend for Fake {
            fn lock(&self) -> Box<dyn crate::backend::BusOps + '_> { unreachable!() }
            fn try_lock(&self) -> Option<Box<dyn crate::backend::BusOps + '_>> { unreachable!() }
        }
        let e = |kind, serial: &str| Entry { kind, serial: serial.into(), pid: 0 };
        let st = State::default();
        let (session, other) = (61, 62);
        st.boards.held().update(vec![e(backend::Kind::Viper, "A"), e(backend::Kind::Ni, "N")]);
        for k in ["viper:A", "ni:N", "contec:C"] {
            st.adapters.held().insert(k.into(), Arc::new(Fake));
        }
        // The session checked the numbers (LIST with F_BIND) while the NI was GPIB1.
        let gens = (0..2).map(|b| (b, st.boards.held().gen(b).unwrap())).collect();
        st.bound.held().insert(session, gens);
        assert!(st.adapter_for(1, session).is_ok());
        // The NI is unplugged and removed while the session makes no request; a CONTEC plugged in takes GPIB1.
        st.boards.held().update(vec![e(backend::Kind::Viper, "A")]);
        st.boards.held().forget(1).unwrap();
        st.boards.held().update(vec![e(backend::Kind::Viper, "A"), e(backend::Kind::Contec, "C")]);
        assert_eq!(st.boards.held().get(1).unwrap().key(), "contec:C");
        assert!(matches!(st.adapter_for(1, session), Err(Error::Gone)), "not sent to the CONTEC");
        assert!(st.adapter_for(0, session).is_ok(), "its other number is still its adapter's");
        assert!(st.adapter_for(1, other).is_ok(), "a connection that checked nothing is as before");
    }
}
