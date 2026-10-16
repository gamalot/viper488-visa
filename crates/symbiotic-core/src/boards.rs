//! Board numbers that stay put while the daemon runs.
//!
//! A board number names an adapter in every request, and an open session keeps the number it was opened with. So a
//! number, once given, stays with its adapter for the daemon's life: an adapter that goes keeps its slot, marked absent
//! (requests for it get `Error::Gone`), and gets the same number back when it returns; a new adapter takes the next
//! free number. The first enumeration numbers in order: Viper488, NI, Keysight, CONTEC, each by serial.
//!
//! A Keysight 82357B enumerates blank, without a serial, until its firmware is loaded. An adapter that cannot be
//! matched by its key is paired with an unmatched slot of the same kind, preferring the slot that last saw its serial;
//! blank ones are paired after those with a serial. Two blank ones that return together may swap numbers until their
//! firmware is loaded.
//!
//! Two adapters with the same key (no USB serial, or two CONTECs with the same module ID) cannot be told apart: only
//! the first gets a number, and `update` returns the others.
//!
//! An absent adapter can be forgotten (`symbiotic-ctl --board N forget`, or Remove in the window): its number stays in
//! the list, hidden, and the next new adapter takes it. The forgotten adapter itself, plugged back, counts as new.
//!
//! Each slot has a generation that changes whenever its number goes to another adapter. A connection that bound itself
//! to the numbers (`LIST` with `F_BIND`) is refused for a number that has changed hands since.

use crate::adapter::Error;
use crate::backend::{Entry, Kind};

#[derive(Debug, Clone)]
pub struct Slot {
    /// The adapter as last seen.
    pub entry: Entry,
    /// Attached now.
    pub present: bool,
    /// The last serial it was seen with: an 82357B's, through its blank spells.
    pub serial: String,
    /// Absent and forgotten by the user: a free number, not shown.
    pub forgotten: bool,
    /// New whenever this number goes to another adapter (see the top).
    pub gen: u64,
}

impl Slot {
    fn take(&mut self, e: Entry) {
        if !e.serial.is_empty() {
            self.serial = e.serial.clone();
        }
        self.entry = e;
        self.present = true;
        self.forgotten = false;
    }
}

/// An 82357B with no firmware in it: no serial, so no key of its own.
fn blank(e: &Entry) -> bool {
    e.kind == Kind::Keysight && e.serial.is_empty()
}

#[derive(Default)]
pub struct Boards {
    slots: Vec<Slot>,
    gens: u64,
}

impl Boards {
    fn slot(&mut self, entry: Entry) -> Slot {
        self.gens += 1;
        Slot { serial: entry.serial.clone(), entry, present: true, forgotten: false, gen: self.gens }
    }

    /// Take in a fresh enumeration (in the documented order). Says the adapters left without a number
    /// because another one has the same key (see the top).
    pub fn update(&mut self, list: Vec<Entry>) -> Vec<Entry> {
        let mut seen = std::collections::HashSet::new();
        let (list, same): (Vec<Entry>, Vec<Entry>) = list.into_iter().partition(|e| blank(e) || seen.insert(e.key()));
        for s in &mut self.slots {
            s.present = false;
        }
        let mut rest = Vec::new();
        for e in list {
            match self.slots.iter_mut().find(|s| !s.present && !s.forgotten && !blank(&e) && s.entry.key() == e.key()) {
                Some(s) => s.take(e),
                None => rest.push(e),
            }
        }
        // An 82357B that changed identity (blank <-> running) keeps the number it had: the one its
        // serial last had, else any free one of its kind. The ones with a serial go first, so a
        // blank one does not take a slot that a running one can claim by its serial.
        rest.sort_by_key(|e| e.serial.is_empty());
        let mut new = Vec::new();
        for e in rest {
            let free = |s: &Slot| !s.present && !s.forgotten && e.kind == Kind::Keysight && s.entry.kind == Kind::Keysight;
            let i = self
                .slots
                .iter()
                .position(|s| free(s) && !e.serial.is_empty() && s.serial == e.serial)
                // A slot that remembers another serial is kept for that adapter, unless this one is
                // blank and cannot say who it is.
                .or_else(|| self.slots.iter().position(|s| free(s) && (e.serial.is_empty() || s.serial.is_empty())));
            match i {
                Some(i) => self.slots[i].take(e),
                None => new.push(e),
            }
        }
        // A new adapter takes the lowest forgotten number, else the next one.
        for e in new {
            let s = self.slot(e);
            match self.slots.iter().position(|s| s.forgotten) {
                Some(i) => self.slots[i] = s,
                None => self.slots.push(s),
            }
        }
        same
    }

    /// Forget an absent adapter's number (see the top). Gone if it is attached, NoBoard if there is no such number.
    pub fn forget(&mut self, board: u16) -> Result<(), Error> {
        match self.slots.get_mut(board as usize) {
            Some(s) if s.present => Err(Error::Gone),
            Some(s) => {
                s.forgotten = true;
                Ok(())
            }
            None => Err(Error::NoBoard),
        }
    }

    /// The adapter a board number stands for, if it is attached.
    pub fn get(&self, board: u16) -> Result<Entry, Error> {
        match self.slots.get(board as usize) {
            Some(s) if s.present => Ok(s.entry.clone()),
            Some(_) => Err(Error::Gone),
            None => Err(Error::NoBoard),
        }
    }

    /// The generation of a board number whose adapter is attached (see the top).
    pub fn gen(&self, board: u16) -> Option<u64> {
        self.slots.get(board as usize).filter(|s| s.present).map(|s| s.gen)
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: Kind, serial: &str) -> Entry {
        Entry { kind, serial: serial.into(), pid: 0 }
    }
    fn keys(b: &Boards) -> Vec<(String, bool)> {
        b.slots().iter().map(|s| (s.entry.key(), s.present)).collect()
    }

    #[test]
    fn the_first_enumeration_numbers_in_the_given_order() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N")]);
        assert_eq!(b.get(0).unwrap().key(), "viper:A");
        assert_eq!(b.get(1).unwrap().key(), "ni:N");
        assert!(matches!(b.get(2), Err(Error::NoBoard)));
    }

    #[test]
    fn an_adapter_that_goes_keeps_its_number_and_so_do_the_others() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N"), e(Kind::Keysight, "K")]);
        b.update(vec![e(Kind::Ni, "N"), e(Kind::Keysight, "K")]);           // the Viper488 is unplugged
        assert!(matches!(b.get(0), Err(Error::Gone)), "its number now says it is gone");
        assert_eq!(b.get(1).unwrap().key(), "ni:N", "the NI is still GPIB1");
        assert_eq!(b.get(2).unwrap().key(), "keysight:K");
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N"), e(Kind::Keysight, "K")]);
        assert_eq!(b.get(0).unwrap().key(), "viper:A", "back, with its old number");
    }

    #[test]
    fn a_new_adapter_takes_the_next_number() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Ni, "N")]);
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N")]);              // sorts before the NI
        assert_eq!(b.get(0).unwrap().key(), "ni:N", "the NI does not move");
        assert_eq!(b.get(1).unwrap().key(), "viper:A");
    }

    #[test]
    fn an_82357b_keeps_its_number_through_its_change_of_identity() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Ni, "N"), e(Kind::Keysight, "")]);           // blank, no serial
        b.update(vec![e(Kind::Ni, "N"), e(Kind::Keysight, "MY1")]);        // firmware loaded
        assert_eq!(b.get(1).unwrap().key(), "keysight:MY1");
        b.update(vec![e(Kind::Ni, "N"), e(Kind::Keysight, "")]);           // unplugged and back: blank again
        assert_eq!(b.get(1).unwrap().key(), "keysight:");
        assert_eq!(keys(&b).len(), 2, "no new number for it");
    }

    #[test]
    fn two_82357bs_find_their_own_numbers_once_their_serials_are_back() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Keysight, "K1"), e(Kind::Keysight, "K2")]);
        // Both unplugged and back, blank: no telling which is which.
        b.update(vec![e(Kind::Keysight, ""), e(Kind::Keysight, "")]);
        assert_eq!(keys(&b).len(), 2);
        // Their firmware loaded, in the other order: each goes back to its own number.
        b.update(vec![e(Kind::Keysight, "K2"), e(Kind::Keysight, "K1")]);
        assert_eq!(keys(&b), vec![("keysight:K1".to_string(), true), ("keysight:K2".to_string(), true)]);
        // One back running and one still blank: the blank one does not take the other's number.
        b.update(vec![e(Kind::Keysight, ""), e(Kind::Keysight, "K2")]);
        assert_eq!(b.get(1).unwrap().key(), "keysight:K2");
        assert_eq!(b.get(0).unwrap().key(), "keysight:");
        // A third, new one, while K1 is away: a number of its own, not K1's.
        b.update(vec![e(Kind::Keysight, "K2"), e(Kind::Keysight, "K3")]);
        assert!(matches!(b.get(0), Err(Error::Gone)), "kept for K1");
        assert_eq!(b.get(2).unwrap().key(), "keysight:K3");
    }

    #[test]
    fn a_forgotten_number_goes_to_the_next_new_adapter_and_no_other_moves() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N"), e(Kind::Contec, "C")]);
        assert!(matches!(b.forget(1), Err(Error::Gone)), "an attached adapter is not forgotten");
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Contec, "C")]);
        b.forget(1).unwrap();
        assert!(matches!(b.forget(7), Err(Error::NoBoard)));
        assert!(b.slots()[1].forgotten && matches!(b.get(1), Err(Error::Gone)));
        assert_eq!(b.get(2).unwrap().key(), "contec:C", "the CONTEC stays GPIB2");
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Keysight, "K"), e(Kind::Contec, "C")]);
        assert_eq!(b.get(1).unwrap().key(), "keysight:K", "a new adapter takes the forgotten number");
        assert!(!b.slots()[1].forgotten);
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N"), e(Kind::Keysight, "K"), e(Kind::Contec, "C")]);
        assert_eq!(b.get(3).unwrap().key(), "ni:N", "the forgotten adapter is a new one");
    }

    #[test]
    fn a_forgotten_adapter_plugged_back_is_a_new_one() {
        // Two adapters unplugged and forgotten, then one of them plugged back.
        let mut b = Boards::default();
        let all = |b: &mut Boards, viper: bool, bridge: bool| {
            let mut v = Vec::new();
            if viper {
                v.push(e(Kind::Viper, "D"));
            }
            v.extend(["N1", "N2", "N3", "N4", "N5"].map(|s| e(Kind::Ni, s)));
            if bridge {
                v.push(e(Kind::Prologix, "COM5"));
            }
            b.update(v);
        };
        all(&mut b, true, true);
        assert_eq!(b.get(6).unwrap().key(), "prologix:COM5");
        all(&mut b, false, false);
        b.forget(0).unwrap();
        b.forget(6).unwrap();
        all(&mut b, false, true);
        assert_eq!(b.get(0).unwrap().key(), "prologix:COM5", "the lowest free number, not its old one");
        all(&mut b, true, true);
        assert_eq!(b.get(6).unwrap().key(), "viper:D");
        assert!((1..=5).all(|i| b.get(i).unwrap().kind == Kind::Ni), "no other number moved");
    }

    #[test]
    fn a_number_that_changes_hands_has_a_new_generation() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N")]);
        let (a, n) = (b.gen(0).unwrap(), b.gen(1).unwrap());
        b.update(vec![e(Kind::Ni, "N")]);
        assert_eq!(b.gen(0), None, "absent");
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Ni, "N")]);
        assert_eq!(b.gen(0), Some(a), "the same adapter back: the same generation");
        b.update(vec![e(Kind::Ni, "N")]);
        b.forget(0).unwrap();
        b.update(vec![e(Kind::Contec, "C"), e(Kind::Ni, "N")]);
        assert_eq!(b.get(0).unwrap().key(), "contec:C");
        assert_ne!(b.gen(0), Some(a), "another adapter on number 0");
        assert_eq!(b.gen(1), Some(n));
    }

    #[test]
    fn an_82357b_keeps_its_generation_through_its_change_of_identity() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Keysight, "K1")]);
        let g = b.gen(0).unwrap();
        b.update(vec![e(Kind::Keysight, "")]);
        b.update(vec![e(Kind::Keysight, "K1")]);
        assert_eq!(b.gen(0), Some(g));
    }

    #[test]
    fn of_two_blank_82357bs_the_one_woken_first_keeps_its_number() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Keysight, "K1"), e(Kind::Keysight, "K2")]);
        b.update(vec![e(Kind::Keysight, ""), e(Kind::Keysight, "")]);
        // K1's firmware is loaded; the other is still blank.
        b.update(vec![e(Kind::Keysight, ""), e(Kind::Keysight, "K1")]);
        assert_eq!(b.slots().len(), 2, "no new number");
        assert_eq!(b.get(0).unwrap().key(), "keysight:K1");
        assert_eq!(b.get(1).unwrap().key(), "keysight:", "the blank one is in the other's slot");
        assert_eq!(b.slots()[1].serial, "K2", "which still remembers K2");
    }

    #[test]
    fn two_adapters_with_one_key_get_one_number() {
        let mut b = Boards::default();
        let same = b.update(vec![e(Kind::Contec, "0"), e(Kind::Contec, "0"), e(Kind::Keysight, ""), e(Kind::Keysight, "")]);
        assert_eq!(same.iter().map(|x| x.key()).collect::<Vec<_>>(), vec!["contec:0"]);
        assert_eq!(keys(&b), vec![("contec:0".into(), true), ("keysight:".into(), true), ("keysight:".into(), true)], "blank ones are each their own");
    }

    #[test]
    fn two_of_a_kind_stay_apart() {
        let mut b = Boards::default();
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Viper, "B")]);
        b.update(vec![e(Kind::Viper, "B")]);
        assert!(matches!(b.get(0), Err(Error::Gone)));
        assert_eq!(b.get(1).unwrap().key(), "viper:B");
        b.update(vec![e(Kind::Viper, "A"), e(Kind::Viper, "B")]);
        assert_eq!(keys(&b), vec![("viper:A".to_string(), true), ("viper:B".to_string(), true)]);
    }
}
