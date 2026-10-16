//! VISA resource locks (viLock / viUnlock), as the daemon keeps them for every process.
//!
//! A lock belongs to a resource -- one instrument, a `Target` -- and is held by a session. The
//! daemon's sessions are its connections: the DLL opens one per VISA session, so two sessions on
//! one instrument in one process are two holders, as VISA has it.
//!
//! VPP-4.3's rules, as implemented here:
//! - An exclusive lock lets its holder, and nobody else, use the resource.
//! - A shared lock lets every session that asked for it with the same access key use the resource,
//!   and nobody else. A session that asked for none is locked out of it too.
//! - Both nest: asking again for a lock one already holds counts up, and viUnlock counts down.
//!   One session may hold a shared and an exclusive lock at once; the exclusive goes first.
//! - A lock that cannot be granted now is waited for (by the caller; `acquire` only says so).
//!
//! Pure bookkeeping, no waiting and no I/O, so it can be tested on its own.

use std::collections::HashMap;

/// Who holds a lock: the daemon's id for a connection.
pub type Owner = u64;

/// One instrument, as the daemon names it: a GPIB instrument by board, primary and secondary
/// address (`backend::NO_SAD` for none; two secondaries of one primary are two instruments), a USBTMC
/// instrument by its own number (usbtmc.rs), a serial one by its COM number (serial.rs). They are
/// not the same kind of thing -- a USBTMC device or a serial port is an instrument, not a bus --
/// so none pretends to be another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Gpib(u16, u8, u8),
    Usb(u16),
    Asrl(u16),
}

pub type Resource = Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    Exclusive = 1,
    Shared = 2,
}

/// What a resource's lock state is, as VI_ATTR_RSRC_LOCK_STATE reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    None = 0,
    Exclusive = 1,
    Shared = 2,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Granted {
    /// The session already held this kind of lock; this one nests inside it.
    pub nested: bool,
    /// The shared lock's access key; empty for an exclusive lock.
    pub key: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another session holds an incompatible lock: wait, then ask again.
    Wait,
    /// A nested shared lock asked with a key other than the one the session holds.
    BadKey,
}

#[derive(Default)]
struct Held {
    exclusive: Option<(Owner, u32)>,
    shared: HashMap<Owner, u32>,
    key: String,
}

#[derive(Default)]
pub struct Locks {
    held: HashMap<Resource, Held>,
    next_key: u64,
}

impl Locks {
    /// Grant `kind` to `who` on `res` if VISA's rules allow it now. `key` is the requested shared
    /// key (none: make one up); it is ignored for an exclusive lock.
    pub fn acquire(&mut self, res: Resource, who: Owner, kind: LockType, key: Option<&str>) -> Result<Granted, Refused> {
        let fresh_key = format!("symbiotic-{}", self.next_key);
        let h = self.held.entry(res).or_default();
        let others_shared = h.shared.keys().any(|o| *o != who);
        let other_exclusive = h.exclusive.is_some_and(|(o, _)| o != who);
        match kind {
            LockType::Exclusive => {
                if let Some((o, n)) = h.exclusive.as_mut() {
                    if *o == who {
                        *n += 1;
                        return Ok(Granted { nested: true, key: String::new() });
                    }
                }
                if other_exclusive || others_shared {
                    return Err(Refused::Wait);
                }
                h.exclusive = Some((who, 1));
                Ok(Granted { nested: false, key: String::new() })
            }
            LockType::Shared => {
                if let Some(n) = h.shared.get_mut(&who) {
                    if key.is_some_and(|k| k != h.key) {
                        return Err(Refused::BadKey);
                    }
                    *n += 1;
                    return Ok(Granted { nested: true, key: h.key.clone() });
                }
                if other_exclusive {
                    return Err(Refused::Wait);
                }
                if h.shared.is_empty() {
                    h.key = match key {
                        Some(k) => k.to_string(),
                        None => {
                            self.next_key += 1;
                            fresh_key
                        }
                    };
                } else if key != Some(h.key.as_str()) {
                    // Shared with others under another key (or none given): not for this session.
                    return Err(Refused::Wait);
                }
                h.shared.insert(who, 1);
                Ok(Granted { nested: false, key: h.key.clone() })
            }
        }
    }

    /// Give back one level of `who`'s lock on `res`, the exclusive one first. Returns what `who`
    /// still holds there, or `None` if it held nothing.
    pub fn release(&mut self, res: Resource, who: Owner) -> Option<State> {
        let h = self.held.get_mut(&res)?;
        match h.exclusive.as_mut() {
            Some((o, n)) if *o == who => {
                *n -= 1;
                if *n == 0 {
                    h.exclusive = None;
                }
            }
            _ => {
                let n = h.shared.get_mut(&who)?;
                *n -= 1;
                if *n == 0 {
                    h.shared.remove(&who);
                }
            }
        }
        if h.exclusive.is_none() && h.shared.is_empty() {
            self.held.remove(&res);
        }
        Some(self.state_of(res, who))
    }

    fn state_of(&self, res: Resource, who: Owner) -> State {
        match self.held.get(&res) {
            Some(h) if h.exclusive.is_some_and(|(o, _)| o == who) => State::Exclusive,
            Some(h) if h.shared.contains_key(&who) => State::Shared,
            _ => State::None,
        }
    }

    /// May `who` use `res` right now?
    pub fn allowed(&self, res: Resource, who: Owner) -> bool {
        match self.held.get(&res) {
            None => true,
            Some(h) => match h.exclusive {
                Some((o, _)) => o == who,
                None => h.shared.is_empty() || h.shared.contains_key(&who),
            },
        }
    }

    /// The resource's lock state, whoever holds it.
    pub fn state(&self, res: Resource) -> State {
        match self.held.get(&res) {
            Some(h) if h.exclusive.is_some() => State::Exclusive,
            Some(h) if !h.shared.is_empty() => State::Shared,
            _ => State::None,
        }
    }

    /// What `who` holds: each resource, and whether exclusively (a session can hold both kinds of
    /// one resource, which lists it twice).
    pub fn held_by(&self, who: Owner) -> Vec<(Resource, State)> {
        let mut v = Vec::new();
        for (r, h) in &self.held {
            if h.exclusive.is_some_and(|(o, _)| o == who) {
                v.push((*r, State::Exclusive));
            }
            if h.shared.contains_key(&who) {
                v.push((*r, State::Shared));
            }
        }
        v
    }

    /// A session went away: everything it held goes with it.
    pub fn drop_owner(&mut self, who: Owner) {
        for h in self.held.values_mut() {
            if h.exclusive.is_some_and(|(o, _)| o == who) {
                h.exclusive = None;
            }
            h.shared.remove(&who);
        }
        self.held.retain(|_, h| h.exclusive.is_some() || !h.shared.is_empty());
    }
}

/// A resource as a person reads it: the GPIB and serial ones by their VISA names, a USBTMC
/// instrument by the daemon's number for it (its VISA name needs its USB identity).
pub fn describe(r: Resource) -> String {
    match r {
        Target::Gpib(b, p, s) if s == crate::backend::NO_SAD => format!("GPIB{b}::{p}::INSTR"),
        Target::Gpib(b, p, s) => format!("GPIB{b}::{p}::{s}::INSTR"),
        Target::Usb(n) => format!("USBTMC instrument {n}"),
        Target::Asrl(n) => format!("ASRL{n}::INSTR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Resource = Target::Gpib(0, 20, crate::backend::NO_SAD);
    const OTHER: Resource = Target::Gpib(0, 29, crate::backend::NO_SAD);
    const A: Owner = 1;
    const B: Owner = 2;
    const C: Owner = 3;

    #[test]
    fn what_a_session_holds_is_listed_by_kind() {
        let mut l = Locks::default();
        l.acquire(R, A, LockType::Exclusive, None).unwrap();
        l.acquire(R, A, LockType::Shared, Some("k")).unwrap();
        l.acquire(OTHER, B, LockType::Shared, Some("k")).unwrap();
        let mut a = l.held_by(A);
        a.sort_by_key(|(_, s)| *s as u8);
        assert_eq!(a, [(R, State::Exclusive), (R, State::Shared)]);
        assert_eq!(l.held_by(B), [(OTHER, State::Shared)]);
        assert!(l.held_by(C).is_empty());
        assert_eq!(describe(R), "GPIB0::20::INSTR");
        assert_eq!(describe(Target::Gpib(1, 5, 96)), "GPIB1::5::96::INSTR");
    }

    #[test]
    fn an_exclusive_lock_keeps_everyone_else_out_and_nests() {
        let mut l = Locks::default();
        assert_eq!(l.acquire(R, A, LockType::Exclusive, None), Ok(Granted { nested: false, key: String::new() }));
        assert!(l.allowed(R, A));
        assert!(!l.allowed(R, B));
        assert!(l.allowed(OTHER, B), "a lock is on one instrument, not the board");
        assert!(l.allowed(Target::Usb(0), B), "nor on a USBTMC instrument numbered like it");
        assert_eq!(l.acquire(R, B, LockType::Exclusive, None), Err(Refused::Wait));
        assert_eq!(l.acquire(R, B, LockType::Shared, None), Err(Refused::Wait));
        assert!(l.acquire(R, A, LockType::Exclusive, None).unwrap().nested);
        assert_eq!(l.release(R, A), Some(State::Exclusive), "still held once");
        assert!(!l.allowed(R, B));
        assert_eq!(l.release(R, A), Some(State::None));
        assert!(l.allowed(R, B));
        assert_eq!(l.state(R), State::None);
        assert_eq!(l.release(R, A), None, "not locked any more");
    }

    #[test]
    fn a_shared_lock_admits_the_sessions_that_hold_its_key_and_nobody_else() {
        let mut l = Locks::default();
        let key = l.acquire(R, A, LockType::Shared, Some("k1")).unwrap().key;
        assert_eq!(key, "k1");
        assert_eq!(l.acquire(R, B, LockType::Shared, Some("k1")), Ok(Granted { nested: false, key: "k1".into() }));
        assert!(l.allowed(R, A) && l.allowed(R, B));
        assert!(!l.allowed(R, C), "a session that asked for no lock is locked out");
        assert_eq!(l.acquire(R, C, LockType::Shared, Some("other")), Err(Refused::Wait));
        assert_eq!(l.acquire(R, C, LockType::Shared, None), Err(Refused::Wait));
        assert_eq!(l.acquire(R, A, LockType::Exclusive, None), Err(Refused::Wait), "B shares it");
        assert_eq!(l.acquire(R, A, LockType::Shared, Some("wrong")), Err(Refused::BadKey));
        assert_eq!(l.state(R), State::Shared);
        l.release(R, A);
        l.release(R, B);
        assert_eq!(l.state(R), State::None);
        assert!(l.allowed(R, C));
    }

    #[test]
    fn a_shared_lock_with_no_key_asked_gets_one_made_up() {
        let mut l = Locks::default();
        let k = l.acquire(R, A, LockType::Shared, None).unwrap().key;
        assert!(!k.is_empty());
        assert_eq!(l.acquire(R, B, LockType::Shared, Some(&k)).unwrap().key, k);
        let k2 = l.acquire(OTHER, A, LockType::Shared, None).unwrap().key;
        assert_ne!(k, k2, "each new shared lock gets its own key");
    }

    #[test]
    fn one_session_may_hold_both_and_gives_back_the_exclusive_first() {
        let mut l = Locks::default();
        l.acquire(R, A, LockType::Shared, Some("k")).unwrap();
        assert!(!l.acquire(R, A, LockType::Exclusive, None).unwrap().nested);
        assert_eq!(l.state(R), State::Exclusive);
        assert_eq!(l.release(R, A), Some(State::Shared));
        assert_eq!(l.state(R), State::Shared);
        assert_eq!(l.release(R, A), Some(State::None));
    }

    #[test]
    fn a_session_that_goes_away_takes_its_locks_with_it() {
        let mut l = Locks::default();
        l.acquire(R, A, LockType::Exclusive, None).unwrap();
        l.acquire(OTHER, A, LockType::Shared, Some("k")).unwrap();
        l.acquire(OTHER, B, LockType::Shared, Some("k")).unwrap();
        l.drop_owner(A);
        assert_eq!(l.state(R), State::None);
        assert!(l.allowed(R, C));
        assert_eq!(l.state(OTHER), State::Shared, "B still shares it");
        assert!(l.allowed(OTHER, B) && !l.allowed(OTHER, C));
    }
}

