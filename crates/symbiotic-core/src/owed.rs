//! Software message termination, for an adapter whose read can bring back more than the caller's read may return: the
//! CONTEC reads a whole message at once, and so does a Prologix or AR488, so a termination character or a count inside
//! it leaves a tail, which is owed to that instrument's next read.
//!
//! What is owed also says whether the message ended there: a tail that EOI closed is marked, or the next read would go
//! back to the bus for a message that is already over, and time out.
//!
//! The NI GPIB-USB-HS and the Keysight 82357B stop a read on the character in the adapter and never read past the
//! count, so the cut finds nothing to keep; their reads still go through it, which keeps one rule for all.
//!
//! What is owed is kept per instrument, so a read of another instrument or a serial poll in between does not drop it.

use crate::adapter::ReadEnd;
use std::collections::HashMap;

#[derive(Default)]
pub struct Owed(HashMap<u8, (Vec<u8>, bool)>, Option<u16>);

impl Owed {
    /// What `pad` is owed, to start its read with, and whether the message ends after it; nothing
    /// is left behind.
    pub fn take(&mut self, pad: u8) -> (Vec<u8>, bool) {
        self.0.remove(&pad).unwrap_or_default()
    }

    /// The instrument has discarded the rest of its message, so nothing it sent before is owed any
    /// more: a device clear, or a new message to it (IEEE 488.2 drops unread output then).
    /// Operations that leave the instrument's output alone -- a serial poll, a trigger, local,
    /// lockout, IFC, reaching another instrument -- must not call this.
    pub fn forget(&mut self, pad: u8) {
        self.0.remove(&pad);
    }

    /// Keep `data` for `pad`'s next read, after whatever is kept already; `ended`: the message is
    /// over after it. For an adapter that goes on receiving a message after a read stopped taking
    /// it (a Prologix: prologix.rs).
    pub fn put(&mut self, pad: u8, data: Vec<u8>, ended: bool) {
        let e = self.0.entry(pad).or_default();
        e.0.extend_from_slice(&data);
        e.1 = ended;
    }

    /// The interface level (`backend::RAW`) has one slot, but more than one talker reaches it: an
    /// INTFC session's own addressing, and every secondary address (backend.rs, `secondary`). `who`
    /// is the one the next RAW read is for; a tail kept for another talker is dropped, since it is
    /// not this one's. `keep: false` drops it anyway: the instrument has discarded its message (a
    /// clear, a new message), or the next read is not its data (a serial poll). True if it was
    /// dropped (or would have been: there may have been nothing).
    pub fn raw_for(&mut self, who: u16, keep: bool) -> bool {
        let drop = self.1 != Some(who) || !keep;
        if drop {
            self.0.remove(&crate::backend::RAW);
        }
        self.1 = Some(who);
        drop
    }

    /// If `out` already ends the read -- it holds the termination character or reaches `count` --
    /// cut it there, keep what follows for `pad`, and say why the read ended. `ended`: the message
    /// is over after `out` (EOI came with it); the cut-off rest carries that on, and a cut that
    /// leaves nothing over is reported as the end of the message. `None`: read on -- or, if
    /// `ended`, the read is simply over and ends with the message.
    pub fn cut(&mut self, pad: u8, out: &mut Vec<u8>, count: usize, term: Option<u8>, ended: bool) -> Option<ReadEnd> {
        let (at, end) = match term.and_then(|t| out.iter().position(|b| *b == t)) {
            Some(i) if i < count => (i + 1, ReadEnd::TermChar),
            _ if out.len() >= count => (count, ReadEnd::MaxCount),
            _ => return None,
        };
        let rest = out.split_off(at);
        if !rest.is_empty() {
            self.0.insert(pad, (rest, ended));
            Some(end)
        } else if ended {
            Some(ReadEnd::End)
        } else {
            Some(end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_level_tail_is_one_talkers() {
        let raw = crate::backend::RAW;
        let mut o = Owed::default();
        o.raw_for(1, true);
        o.put(raw, b"tail".to_vec(), true);
        o.raw_for(1, true);
        assert_eq!(o.take(raw), (b"tail".to_vec(), true), "the same talker keeps it");
        o.put(raw, b"tail".to_vec(), true);
        o.raw_for(2, true);
        assert_eq!(o.take(raw), (vec![], false), "another talker does not get it");
        o.put(raw, b"tail".to_vec(), true);
        o.raw_for(2, false);
        assert_eq!(o.take(raw), (vec![], false), "a clear drops it");
    }

    #[test]
    fn a_termination_character_ends_the_read_and_the_rest_is_owed() {
        let mut o = Owed::default();
        let mut out = b"1.0\n2.0\n".to_vec();
        assert_eq!(o.cut(5, &mut out, 100, Some(b'\n'), false), Some(ReadEnd::TermChar));
        assert_eq!(out, b"1.0\n");
        let (mut next, _) = o.take(5);
        assert_eq!(o.cut(5, &mut next, 100, Some(b'\n'), false), Some(ReadEnd::TermChar));
        assert_eq!(next, b"2.0\n");
        assert!(o.take(5).0.is_empty());
    }

    #[test]
    fn another_instrument_in_between_does_not_lose_what_is_owed() {
        let mut o = Owed::default();
        let mut a = b"A1\nA2\n".to_vec();
        o.cut(5, &mut a, 100, Some(b'\n'), false);
        // A read of pad 7 in between: it is owed nothing and takes nothing from pad 5.
        assert!(o.take(7).0.is_empty());
        let mut b = b"B1\nB2\n".to_vec();
        o.cut(7, &mut b, 100, Some(b'\n'), false);
        assert_eq!(o.take(5).0, b"A2\n");
        assert_eq!(o.take(7).0, b"B2\n");
    }

    #[test]
    fn forget_touches_only_that_instrument() {
        let mut o = Owed::default();
        o.cut(5, &mut b"x\ny".to_vec(), 100, Some(b'\n'), false);
        o.cut(7, &mut b"p\nq".to_vec(), 100, Some(b'\n'), false);
        o.forget(5);
        assert!(o.take(5).0.is_empty());
        assert_eq!(o.take(7).0, b"q");
    }

    #[test]
    fn the_count_ends_the_read_before_a_later_termination_character() {
        let mut o = Owed::default();
        let mut out = b"abcdef\n".to_vec();
        assert_eq!(o.cut(5, &mut out, 4, Some(b'\n'), false), Some(ReadEnd::MaxCount));
        assert_eq!(out, b"abcd");
        assert_eq!(o.take(5).0, b"ef\n");
    }

    #[test]
    fn a_termination_character_at_the_count_is_reported_as_one() {
        let mut o = Owed::default();
        let mut out = b"abc\nde".to_vec();
        assert_eq!(o.cut(5, &mut out, 4, Some(b'\n'), false), Some(ReadEnd::TermChar));
        assert_eq!(out, b"abc\n");
        assert_eq!(o.take(5).0, b"de");
    }

    #[test]
    fn short_of_both_the_read_goes_on_untouched() {
        let mut o = Owed::default();
        let mut out = b"abc".to_vec();
        assert_eq!(o.cut(5, &mut out, 10, Some(b'\n'), false), None);
        assert_eq!(o.cut(5, &mut out, 10, None, false), None);
        assert_eq!(out, b"abc");
        assert!(o.take(5).0.is_empty());
    }

    #[test]
    fn a_tail_that_ended_the_message_ends_the_next_read() {
        let mut o = Owed::default();
        // A whole message came in at once, EOI and all; the caller asked for 3 bytes.
        let mut out = b"abcdef".to_vec();
        assert_eq!(o.cut(5, &mut out, 3, None, true), Some(ReadEnd::MaxCount));
        let (mut next, ended) = o.take(5);
        assert!(ended, "the rest must remember that EOI came after it");
        // Short of the count and no terminator: not a cut, and the read is over with the message.
        assert_eq!(o.cut(5, &mut next, 100, None, ended), None);
        assert_eq!(next, b"def");
    }

    #[test]
    fn a_cut_that_takes_the_last_byte_of_a_message_is_its_end() {
        let mut o = Owed::default();
        let mut out = b"a,b,".to_vec();
        assert_eq!(o.cut(5, &mut out, 100, Some(b','), true), Some(ReadEnd::TermChar));
        let (mut next, ended) = o.take(5);
        assert_eq!(o.cut(5, &mut next, 100, Some(b','), ended), Some(ReadEnd::End));
        assert_eq!(next, b"b,");
        assert!(o.take(5).0.is_empty());
    }
}
