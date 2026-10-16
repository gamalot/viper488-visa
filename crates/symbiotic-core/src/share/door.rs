//! The door of a listener that anyone on the network can reach (the registry's, a member's): what
//! keeps strangers from using it up before they have said who they are.
//!
//! A handshake has a deadline for the whole of it, not for each read, so a peer sending a byte
//! every few seconds is cut off all the same. Connections still shaking hands are counted apart
//! from the ones that have finished, and one address may have only a few of them, so a flood of
//! half-open connections fills its own small room and not the one the members use.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Connections shaking hands at once, from everywhere and from one address.
const MAX_PENDING: usize = 32;
const MAX_PENDING_PER_ADDR: usize = 4;

#[derive(Default)]
pub struct Door {
    pending: Mutex<HashMap<IpAddr, usize>>,
    /// The second something was last said about a failed handshake.
    said: AtomicU64,
}

/// A place among the connections shaking hands; given back when dropped.
pub struct Pending {
    door: Arc<Door>,
    from: IpAddr,
}

impl Door {
    /// A place for a connection from `from` to shake hands in, or None: the room is full.
    pub fn enter(self: &Arc<Self>, from: IpAddr) -> Option<Pending> {
        let mut p = self.pending.lock().unwrap();
        if p.values().sum::<usize>() >= MAX_PENDING || p.get(&from).is_some_and(|n| *n >= MAX_PENDING_PER_ADDR) {
            return None;
        }
        *p.entry(from).or_default() += 1;
        Some(Pending { door: self.clone(), from })
    }

    /// Whether a failed handshake may be logged now: one a second at most, so that a flood does not
    /// become a log as large as the disk.
    pub fn may_say(&self, now: u64) -> bool {
        self.said.swap(now, Ordering::Relaxed) != now
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.lock().unwrap().values().sum()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut p = self.door.pending.lock().unwrap();
        if let Some(n) = p.get_mut(&self.from) {
            *n -= 1;
            if *n == 0 {
                p.remove(&self.from);
            }
        }
    }
}

/// Run `f` (a handshake on `c`) with `limit` for the whole of it: when it is up, `c` is shut down
/// and whatever `f` is waiting for fails.
pub fn within<T>(c: &TcpStream, limit: Duration, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let watch = c.try_clone()?;
    let (done, wait) = mpsc::channel::<()>();
    let guard = std::thread::spawn(move || {
        if wait.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            let _ = watch.shutdown(std::net::Shutdown::Both);
            return true;
        }
        false
    });
    let r = f();
    let _ = done.send(());
    match (r, guard.join().unwrap_or(false)) {
        (Err(_), true) => Err(io::Error::new(io::ErrorKind::TimedOut, "the handshake took too long")),
        (r, _) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    #[test]
    fn the_room_is_small_for_one_address_and_for_all() {
        let d = Arc::new(Door::default());
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let held: Vec<_> = (0..MAX_PENDING_PER_ADDR).map(|_| d.enter(a).unwrap()).collect();
        assert!(d.enter(a).is_none(), "one address has a few places");
        let others: Vec<_> = (0..MAX_PENDING - MAX_PENDING_PER_ADDR).map(|i| d.enter(IpAddr::from([10, 0, 1, i as u8])).unwrap()).collect();
        assert!(d.enter("10.0.2.1".parse().unwrap()).is_none(), "and all of them together a few more");
        drop(held);
        drop(others);
        assert_eq!(d.pending(), 0, "places are given back");
        assert!(d.enter(a).is_some());
    }

    #[test]
    fn a_trickle_does_not_outlast_the_deadline() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let slow = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            // One byte every 100 ms: each read is quick, the whole would never end.
            for _ in 0..50 {
                if s.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let (mut c, _) = l.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let start = Instant::now();
        let r: io::Result<()> = within(&c.try_clone().unwrap(), Duration::from_millis(400), || {
            let mut b = [0u8; 1];
            loop {
                c.read_exact(&mut b)?;
            }
        });
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2), "cut off at the deadline, not after the trickle");
        slow.join().unwrap();
        let fine = within(&c, Duration::from_secs(5), || Ok(7));
        assert_eq!(fine.unwrap(), 7, "a handshake that finishes in time is not touched");
    }

    #[test]
    fn one_word_a_second() {
        let d = Door::default();
        assert!(d.may_say(100));
        assert!(!d.may_say(100));
        assert!(d.may_say(101));
    }
}
