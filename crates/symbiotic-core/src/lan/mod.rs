//! LAN instruments: the `TCPIP` resources, spoken to straight from the calling process.
//!
//! Three protocols: VXI-11 (ONC RPC; `vxi11.rs`), HiSLIP (IVI-6.1; `hislip.rs`) and a raw socket (`socket.rs`). None of
//! them goes through the daemon: a TCP connection has no one-process limit, so several processes may talk to one LAN
//! instrument and the instrument arbitrates (links, sessions, locks). Everything here connects out, with one exception:
//! a VXI-11 session with service-request events enabled listens for the instrument's interrupt channel, on the one
//! local address that reaches it, and only while they are enabled (`vxi11.rs`).
//!
//! A timeout here is the whole operation's, as on the GPIB side: an operation that takes several exchanges gives each
//! the time that is left.

pub mod conf;
pub mod discover;
pub mod hislip;
pub mod lxi;
pub mod name;
pub(crate) mod rpc;
pub mod socket;
pub mod vxi11;
pub(crate) mod xdr;

pub use name::{Proto, Target};

use crate::adapter::ReadResult;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Nothing to connect to at open: refused, unreachable, no such name, no such device.
    NotFound(String),
    /// The instrument's own timeout, or ours, expired.
    Timeout,
    /// The connection is gone, or can no longer be trusted to line answers up with requests.
    Lost(String),
    /// The instrument refused or failed the operation.
    Io(String),
    /// Another client holds a lock on the instrument.
    Locked,
    /// An unlock by a client that holds no lock.
    NotLocked,
    /// The protocol, or this instrument, has no such operation.
    Unsupported,
    /// The instrument says a parameter was wrong.
    Param,
    /// The operation was aborted at the instrument.
    Abort,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Timeout => write!(f, "timeout"),
            Error::Lost(s) => write!(f, "connection lost: {s}"),
            Error::Io(s) => write!(f, "I/O error: {s}"),
            Error::Locked => write!(f, "locked by another client"),
            Error::NotLocked => write!(f, "no lock held"),
            Error::Unsupported => write!(f, "not supported"),
            Error::Param => write!(f, "parameter error"),
            Error::Abort => write!(f, "aborted"),
        }
    }
}

/// When an operation must be over; `None` is VI_TMO_INFINITE.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    pub fn after(t: Option<Duration>) -> Deadline {
        Deadline(t.map(|t| Instant::now() + t))
    }

    /// The time that is left (zero once past); `None` if there is no deadline.
    pub fn left(&self) -> Option<Duration> {
        self.0.map(|d| d.saturating_duration_since(Instant::now()))
    }

    pub fn expired(&self) -> bool {
        self.left().is_some_and(|l| l.is_zero())
    }
}

/// A timeout as a u32 millisecond count, the way both VXI-11 and HiSLIP carry one; infinite is
/// the largest.
fn ms(t: Option<Duration>) -> u32 {
    t.map_or(u32::MAX, |t| t.as_millis().min(u32::MAX as u128 - 1) as u32)
}

/// Connect to `host:port` within `within`, trying each address the name resolves to.
fn connect(host: &str, port: u16, within: Duration) -> Result<TcpStream, Error> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare, port)
        .to_socket_addrs()
        .map_err(|e| Error::NotFound(format!("{host}: {e}")))?
        .collect();
    let mut last = format!("{host}: no address");
    for a in addrs {
        match TcpStream::connect_timeout(&a, within) {
            Ok(s) => {
                // Request and answer, small messages: what Nagle's algorithm punishes.
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = format!("{a}: {e}"),
        }
    }
    Err(Error::NotFound(last))
}

/// A TCP option an application turns on and off: VI_ATTR_TCPIP_NODELAY (on from `connect`) and
/// VI_ATTR_TCPIP_KEEPALIVE (off, VISA's default; on, it probes with the system's own timing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpOpt {
    NoDelay,
    KeepAlive,
}

impl TcpOpt {
    fn get(self, s: &TcpStream) -> std::io::Result<bool> {
        match self {
            TcpOpt::NoDelay => s.nodelay(),
            TcpOpt::KeepAlive => socket2::SockRef::from(s).keepalive(),
        }
    }

    fn set(self, s: &TcpStream, on: bool) -> std::io::Result<()> {
        match self {
            TcpOpt::NoDelay => s.set_nodelay(on),
            TcpOpt::KeepAlive => socket2::SockRef::from(s).set_keepalive(on),
        }
    }
}

/// What viGpibControlREN asks for; the numbers are VISA's (`VI_GPIB_REN_*`), which HiSLIP's
/// AsyncRemoteLocalControl codes happen to equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ren {
    Deassert = 0,
    Assert = 1,
    DeassertGtl = 2,
    AssertAddress = 3,
    AssertLlo = 4,
    AssertAddressLlo = 5,
    AddressGtl = 6,
}

impl Ren {
    pub fn from_u16(v: u16) -> Option<Ren> {
        Some(match v {
            0 => Ren::Deassert,
            1 => Ren::Assert,
            2 => Ren::DeassertGtl,
            3 => Ren::AssertAddress,
            4 => Ren::AssertLlo,
            5 => Ren::AssertAddressLlo,
            6 => Ren::AddressGtl,
            _ => return None,
        })
    }
}

/// Who holds a lock on the instrument, as far as the protocol can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    None,
    Exclusive,
    Shared,
}

/// Where a session's service requests are waited for, apart from the session itself: the thread
/// that waits must not hold up the session's I/O, and the handler it calls may use the session.
#[derive(Clone)]
pub enum SrqWatch {
    Vxi11(vxi11::SrqWatch),
    Hislip(hislip::SrqWatch),
}

impl SrqWatch {
    /// Wait up to `wait` for a service request; Ok(true) is one.
    pub fn wait(&self, wait: Duration) -> Result<bool, Error> {
        match self {
            SrqWatch::Vxi11(w) => w.wait(wait),
            SrqWatch::Hislip(w) => w.wait(wait),
        }
    }
}

/// One open LAN resource.
pub enum Session {
    Vxi11(vxi11::Link),
    Hislip(hislip::Conn),
    Socket(socket::Conn),
}

impl Session {
    /// Connect. `within` bounds the connection itself; `lock` asks for the exclusive lock as the
    /// session opens (VI_EXCLUSIVE_LOCK), waiting at most that long.
    pub fn open(t: &Target, within: Duration, lock: Option<Option<Duration>>) -> Result<Session, Error> {
        match &t.proto {
            Proto::Vxi11 { port, device } => {
                vxi11::Link::open(&t.host, *port, device, within, lock).map(Session::Vxi11)
            }
            Proto::Hislip { port, sub } => {
                let mut c = hislip::Conn::open(&t.host, *port, sub, within)?;
                if let Some(wait) = lock {
                    c.lock(None, wait)?;
                }
                Ok(Session::Hislip(c))
            }
            Proto::Socket { port } => {
                if lock.is_some() {
                    return Err(Error::Unsupported);
                }
                socket::Conn::open(&t.host, *port, within).map(Session::Socket)
            }
        }
    }

    pub fn hislip(&mut self) -> Option<&mut hislip::Conn> {
        match self {
            Session::Hislip(c) => Some(c),
            _ => None,
        }
    }

    pub fn socket(&mut self) -> Option<&mut socket::Conn> {
        match self {
            Session::Socket(c) => Some(c),
            _ => None,
        }
    }

    /// Send `data`; with `end`, it is a whole message. On failure, how much went out.
    pub fn write(&mut self, data: &[u8], end: bool, tmo: Option<Duration>) -> Result<usize, (usize, Error)> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.write(data, end, dl),
            Session::Hislip(c) => c.write(data, end, dl),
            Session::Socket(c) => c.write(data, dl),
        }
    }

    /// Read at most `max` bytes: until the end of the message (unless `suppress_end`), the
    /// termination character if there is one, the count, or the timeout -- in which case what did
    /// arrive comes back with `ReadEnd::Timeout`.
    pub fn read(&mut self, max: usize, term: Option<u8>, suppress_end: bool, tmo: Option<Duration>) -> Result<ReadResult, Error> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.read(max, term, suppress_end, dl),
            Session::Hislip(c) => c.read(max, term, suppress_end, dl),
            Session::Socket(c) => c.read(max, term, suppress_end, dl),
        }
    }

    pub fn clear(&mut self, tmo: Option<Duration>) -> Result<(), Error> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.clear(dl),
            Session::Hislip(c) => c.clear(dl),
            Session::Socket(c) => c.clear(),
        }
    }

    pub fn trigger(&mut self, tmo: Option<Duration>) -> Result<(), Error> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.trigger(dl),
            Session::Hislip(c) => c.trigger(dl),
            Session::Socket(_) => Err(Error::Unsupported),
        }
    }

    pub fn read_stb(&mut self, tmo: Option<Duration>) -> Result<u8, Error> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.read_stb(dl),
            Session::Hislip(c) => c.read_stb(dl),
            Session::Socket(_) => Err(Error::Unsupported),
        }
    }

    pub fn ren(&mut self, mode: Ren, tmo: Option<Duration>) -> Result<(), Error> {
        let dl = Deadline::after(tmo);
        match self {
            Session::Vxi11(l) => l.ren(mode, dl),
            Session::Hislip(c) => c.ren(mode, dl),
            Session::Socket(_) => Err(Error::Unsupported),
        }
    }

    /// Interface clear: only on an instrument behind a LAN/GPIB gateway (its bus).
    pub fn ifc(&mut self, tmo: Option<Duration>) -> Result<(), Error> {
        match self {
            Session::Vxi11(l) => l.ifc(Deadline::after(tmo)),
            _ => Err(Error::Unsupported),
        }
    }

    /// The REN line of a LAN/GPIB gateway's bus; None where there is no GPIB bus to ask.
    pub fn ren_state(&mut self, tmo: Option<Duration>) -> Result<Option<bool>, Error> {
        match self {
            Session::Vxi11(l) if l.gpib().is_some() => l.ren_state(Deadline::after(tmo)).map(Some),
            _ => Ok(None),
        }
    }

    /// Take the instrument's lock: exclusive, or shared under `key`. Waits at most `wait`.
    pub fn lock(&mut self, key: Option<&str>, wait: Option<Duration>) -> Result<(), Error> {
        match self {
            Session::Vxi11(l) if key.is_none() => l.lock(wait),
            Session::Hislip(c) => c.lock(key, wait),
            _ => Err(Error::Unsupported),
        }
    }

    pub fn unlock(&mut self) -> Result<(), Error> {
        match self {
            Session::Vxi11(l) => l.unlock(),
            Session::Hislip(c) => c.unlock(),
            Session::Socket(_) => Err(Error::Unsupported),
        }
    }

    /// The instrument's lock state, if the protocol can ask for it (only HiSLIP can).
    pub fn lock_state(&mut self, tmo: Option<Duration>) -> Result<Option<LockState>, Error> {
        match self {
            Session::Hislip(c) => c.lock_info(Deadline::after(tmo)).map(Some),
            _ => Ok(None),
        }
    }

    /// Start watching for the instrument's service requests (VXI-11: opens the interrupt channel).
    pub fn watch_srq(&mut self) -> Result<SrqWatch, Error> {
        match self {
            Session::Vxi11(l) => l.watch_srq().map(SrqWatch::Vxi11),
            Session::Hislip(c) => Ok(SrqWatch::Hislip(c.watch_srq())),
            Session::Socket(_) => Err(Error::Unsupported),
        }
    }

    /// Stop watching (VXI-11: closes the interrupt channel). A HiSLIP watch just stops being read.
    pub fn unwatch_srq(&mut self) {
        if let Session::Vxi11(l) = self {
            l.unwatch_srq();
        }
    }

    /// The port connected to, where the name did not give it (VXI-11 through the portmapper).
    pub fn port(&self) -> Option<u16> {
        match self {
            Session::Vxi11(l) => Some(l.port),
            _ => None,
        }
    }

    /// A TCP option as the session's connection has it (HiSLIP's two channels are set together).
    pub fn tcp_opt(&self, o: TcpOpt) -> Result<bool, Error> {
        let s = match self {
            Session::Vxi11(l) => l.stream(),
            Session::Hislip(c) => c.stream(),
            Session::Socket(c) => c.stream(),
        };
        o.get(s).map_err(|e| Error::Io(format!("{o:?}: {e}")))
    }

    /// Set a TCP option on every connection the session has: both of HiSLIP's channels (VXI-11's
    /// interrupt channel is the instrument's connection to us, not ours).
    pub fn set_tcp_opt(&mut self, o: TcpOpt, on: bool) -> Result<(), Error> {
        let r = match self {
            Session::Vxi11(l) => o.set(l.stream(), on),
            Session::Hislip(c) => c.each_stream(|s| o.set(s, on)),
            Session::Socket(c) => o.set(c.stream(), on),
        };
        r.map_err(|e| Error::Io(format!("{o:?}: {e}")))
    }

    /// Throw away whatever has been received and not read (viFlush's discard-read-buffer).
    pub fn discard_input(&mut self) {
        if let Session::Socket(c) = self {
            c.discard();
        }
    }
}
