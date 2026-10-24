//! `TCPIP::host::port::SOCKET`: a raw TCP byte stream (SCPI raw, usually port 5025).
//!
//! A stream has no END. A read ends at the termination character (when enabled), at the count, or
//! at the timeout; with VI_ATTR_SUPPRESS_END_EN false (it defaults to true, as in NI-VISA and
//! pyvisa-py), "nothing more has arrived for now" counts as END. Bytes received past where a read
//! stopped are kept for the next one.

use super::{Deadline, Error};
use crate::adapter::{ReadEnd, ReadResult};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    closed: Option<String>,
}

impl Conn {
    pub fn open(host: &str, port: u16, within: Duration) -> Result<Conn, Error> {
        Ok(Conn { s: super::connect(host, port, within)?, buf: Vec::new(), closed: None })
    }

    /// The connection, for its TCP options.
    pub(super) fn stream(&self) -> &TcpStream {
        &self.s
    }

    /// Take what the socket has, waiting at most `wait` (`None`: for ever) for the first byte.
    /// Ok(false) is a timeout.
    fn recv(&mut self, wait: Option<Duration>) -> Result<bool, Error> {
        if let Some(why) = &self.closed {
            return Err(Error::Lost(why.clone()));
        }
        self.s.set_read_timeout(wait.map(|w| w.max(Duration::from_millis(1)))).map_err(|e| Error::Lost(e.to_string()))?;
        let mut tmp = [0u8; 64 * 1024];
        match self.s.read(&mut tmp) {
            Ok(0) => {
                let why = "the instrument closed the connection".to_string();
                self.closed = Some(why.clone());
                Err(Error::Lost(why))
            }
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e) => {
                self.closed = Some(e.to_string());
                Err(Error::Lost(e.to_string()))
            }
        }
    }

    fn take(&mut self, n: usize, end: ReadEnd) -> Result<ReadResult, Error> {
        let data: Vec<u8> = self.buf.drain(..n.min(self.buf.len())).collect();
        Ok(ReadResult { data, end })
    }

    pub fn read(&mut self, max: usize, term: Option<u8>, suppress_end: bool, dl: Deadline) -> Result<ReadResult, Error> {
        let mut looked = 0;
        loop {
            // Only the part not looked at before is searched again.
            let upto = self.buf.len().min(max);
            if let Some(i) = term.and_then(|t| self.buf[looked.min(upto)..upto].iter().position(|b| *b == t)) {
                return self.take(looked.min(upto) + i + 1, ReadEnd::TermChar);
            }
            looked = upto;
            if self.buf.len() >= max {
                return self.take(max, ReadEnd::MaxCount);
            }
            // With END not suppressed, a pause after something arrived ends the read.
            let wait = if !suppress_end && !self.buf.is_empty() { Some(Duration::ZERO) } else { dl.left() };
            let got = match self.recv(wait) {
                Ok(g) => g,
                // Bytes that came before the close are still the caller's.
                Err(e) if self.buf.is_empty() => return Err(e),
                Err(_) => return self.take(max, ReadEnd::End),
            };
            if !got {
                if !suppress_end && !self.buf.is_empty() {
                    return self.take(max, ReadEnd::End);
                }
                if dl.expired() {
                    return self.take(max, ReadEnd::Timeout);
                }
            }
        }
    }

    pub fn write(&mut self, data: &[u8], dl: Deadline) -> Result<usize, (usize, Error)> {
        if let Some(why) = &self.closed {
            return Err((0, Error::Lost(why.clone())));
        }
        let mut sent = 0;
        while sent < data.len() {
            let wait = dl.left().map(|l| l.max(Duration::from_millis(1)));
            self.s.set_write_timeout(wait).map_err(|e| (sent, Error::Lost(e.to_string())))?;
            match self.s.write(&data[sent..]) {
                Ok(0) => return Err((sent, Error::Lost("the connection took no bytes".into()))),
                Ok(n) => sent += n,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return Err((sent, Error::Timeout))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.closed = Some(e.to_string());
                    return Err((sent, Error::Lost(e.to_string())));
                }
            }
        }
        Ok(sent)
    }

    /// viClear on a socket: what was received and not read is thrown away, and so is whatever is
    /// already waiting in the socket. There is no message to the instrument (that would be `*CLS`
    /// under VI_PROT_4882_STRS, which is not implemented). An instrument that never stops sending is
    /// read away for `CLEAR_MAX` at most, not for ever.
    pub fn clear(&mut self) -> Result<(), Error> {
        const CLEAR_MAX: Duration = Duration::from_secs(1);
        let start = std::time::Instant::now();
        self.buf.clear();
        while start.elapsed() < CLEAR_MAX && self.recv(Some(Duration::from_millis(10)))? {
            self.buf.clear();
        }
        self.buf.clear();
        Ok(())
    }

    /// Throw away what was received and not read, and what is waiting in the socket now -- not what
    /// is still coming: before a query whose answer must not be taken from earlier output (`*STB?`).
    pub fn drop_unread(&mut self) -> Result<(), Error> {
        let start = std::time::Instant::now();
        self.buf.clear();
        while start.elapsed() < Duration::from_millis(100) && self.recv(Some(Duration::from_millis(1)))? {
            self.buf.clear();
        }
        self.buf.clear();
        Ok(())
    }

    /// Throw away what was received and not read, without touching the socket.
    pub fn discard(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// An instrument that answers each line it gets with `reply`, in `parts` pieces a little apart.
    fn serve(reply: &'static [u8], parts: usize) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut b = [0u8; 256];
            while let Ok(n) = s.read(&mut b) {
                if n == 0 {
                    return;
                }
                for p in reply.chunks(reply.len().div_ceil(parts)) {
                    s.write_all(p).unwrap();
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        });
        port
    }

    fn dl(ms: u64) -> Deadline {
        Deadline::after(Some(Duration::from_millis(ms)))
    }

    #[test]
    fn term_char_count_and_what_is_left_over() {
        let mut c = Conn::open("127.0.0.1", serve(b"A,B\nC\n", 3), Duration::from_secs(1)).unwrap();
        c.write(b"Q?\n", dl(1000)).unwrap();
        let r = c.read(100, Some(b'\n'), true, dl(1000)).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"A,B\n"[..], ReadEnd::TermChar));
        let r = c.read(1, Some(b'\n'), true, dl(1000)).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"C"[..], ReadEnd::MaxCount));
        let r = c.read(100, Some(b'\n'), true, dl(1000)).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"\n"[..], ReadEnd::TermChar));
    }

    #[test]
    fn with_end_suppressed_only_the_timeout_ends_an_unterminated_read() {
        let mut c = Conn::open("127.0.0.1", serve(b"abc", 1), Duration::from_secs(1)).unwrap();
        c.write(b"Q?\n", dl(1000)).unwrap();
        let r = c.read(100, None, true, dl(200)).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"abc"[..], ReadEnd::Timeout));
        // Not suppressed: a pause after the bytes is the end.
        c.write(b"Q?\n", dl(1000)).unwrap();
        let r = c.read(100, None, false, dl(1000)).unwrap();
        assert_eq!((r.data.as_slice(), r.end), (&b"abc"[..], ReadEnd::End));
    }

    #[test]
    fn unread_output_is_dropped_and_the_next_answer_kept() {
        let mut c = Conn::open("127.0.0.1", serve(b"1.5\n2.5\n", 1), Duration::from_secs(1)).unwrap();
        c.write(b"Q?\n", dl(1000)).unwrap();
        let r = c.read(2, None, true, dl(1000)).unwrap();
        assert_eq!(r.data, b"1.");
        std::thread::sleep(Duration::from_millis(50)); // the rest is in the socket by now
        c.drop_unread().unwrap();
        c.write(b"Q?\n", dl(1000)).unwrap();
        let r = c.read(100, Some(b'\n'), true, dl(1000)).unwrap();
        assert_eq!(r.data, b"1.5\n", "the new answer, not the old one's tail");
    }

    #[test]
    fn keepalive_and_nodelay_can_be_set_here_too() {
        use super::super::{Session, TcpOpt};
        let mut c = Session::Socket(Conn::open("127.0.0.1", serve(b"x\n", 1), Duration::from_secs(1)).unwrap());
        assert_eq!((c.tcp_opt(TcpOpt::NoDelay).unwrap(), c.tcp_opt(TcpOpt::KeepAlive).unwrap()), (true, false));
        for on in [true, false] {
            c.set_tcp_opt(TcpOpt::KeepAlive, on).unwrap();
            c.set_tcp_opt(TcpOpt::NoDelay, on).unwrap();
            assert_eq!((c.tcp_opt(TcpOpt::KeepAlive).unwrap(), c.tcp_opt(TcpOpt::NoDelay).unwrap()), (on, on));
        }
    }

    #[test]
    fn a_clear_ends_though_the_instrument_never_stops_talking() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            while s.write_all(&[b'x'; 512]).is_ok() {
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let mut c = Conn::open("127.0.0.1", port, Duration::from_secs(1)).unwrap();
        let t = std::time::Instant::now();
        c.clear().unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[test]
    fn clear_throws_away_what_is_waiting() {
        let mut c = Conn::open("127.0.0.1", serve(b"stale\n", 1), Duration::from_secs(1)).unwrap();
        c.write(b"Q?\n", dl(1000)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        c.clear().unwrap();
        let r = c.read(100, Some(b'\n'), true, dl(100)).unwrap();
        assert_eq!((r.data.len(), r.end), (0, ReadEnd::Timeout));
    }
}
