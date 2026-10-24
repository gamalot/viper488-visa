//! ONC RPC version 2 (RFC 5531) over TCP, the client half: what VXI-11 and the portmapper need.
//!
//! TCP carries RPC in records: a four-byte header per fragment, the high bit set on the last one, the low 31 bits its
//! length.

use super::xdr::{Decoder, Encoder};
use super::Error;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const CALL: u32 = 0;
const REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;
const MSG_ACCEPTED: u32 = 0;
const SUCCESS: u32 = 0;
const AUTH_NONE: u32 = 0;

pub const PMAP_PORT: u16 = 111;
pub const PMAP_PROG: u32 = 100000;
pub const PMAP_VERS: u32 = 2;
pub const PMAPPROC_GETPORT: u32 = 3;
pub const IPPROTO_TCP: u32 = 6;

/// The largest reply record accepted: a device_read asks for at most `vxi11::READ_CHUNK`.
const MAX_RECORD: usize = 16 << 20;

pub fn build_call(xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u32(xid).u32(CALL).u32(RPC_VERSION).u32(prog).u32(vers).u32(proc_num);
    // Null credential and verifier: a flavor and an empty body each.
    e.u32(AUTH_NONE).u32(0).u32(AUTH_NONE).u32(0);
    let mut v = e.into_vec();
    v.extend_from_slice(args);
    v
}

/// The results of an accepted reply to call `xid`, or why there are none.
///
/// The xid check is what keeps a connection honest: a reply that arrives after its caller gave up is still in the
/// stream, and read as the answer to the next call it would make every later answer belong to the request before it.
pub fn parse_reply(msg: &[u8], xid: u32) -> Result<&[u8], String> {
    let mut d = Decoder::new(msg);
    let got = d.u32().ok_or("not an RPC message")?;
    if got != xid {
        return Err(format!("a reply to call {got}, not {xid}"));
    }
    if d.u32() != Some(REPLY) {
        return Err("not a reply".into());
    }
    if d.u32() != Some(MSG_ACCEPTED) {
        return Err("the call was rejected".into());
    }
    d.u32().ok_or("truncated reply")?; // verifier flavor, never checked
    d.opaque(400).ok_or("truncated reply")?;
    match d.u32().ok_or("truncated reply")? {
        SUCCESS => Ok(d.rest()),
        1 => Err("the server does not have that program".into()),
        2 => Err("the server does not speak that program version".into()),
        3 => Err("the server does not have that procedure".into()),
        4 => Err("the server could not decode the arguments".into()),
        n => Err(format!("the call was not accepted ({n})")),
    }
}

/// A whole record from a stream that blocks (the test servers' side; the client uses `take_record`).
#[cfg(test)]
pub fn read_record(r: &mut impl Read, max: usize) -> std::io::Result<Vec<u8>> {
    let mut msg = Vec::new();
    loop {
        let mut h = [0u8; 4];
        r.read_exact(&mut h)?;
        let h = u32::from_be_bytes(h);
        let len = (h & 0x7FFF_FFFF) as usize;
        if msg.len() + len > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "RPC record too large"));
        }
        let start = msg.len();
        msg.resize(start + len, 0);
        r.read_exact(&mut msg[start..])?;
        if h & 0x8000_0000 != 0 {
            return Ok(msg);
        }
    }
}

/// One record, header and body in one write: two writes with TCP_NODELAY can be two segments.
pub fn write_record(w: &mut impl Write, msg: &[u8]) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(4 + msg.len());
    out.extend_from_slice(&((msg.len() as u32) | 0x8000_0000).to_be_bytes());
    out.extend_from_slice(msg);
    w.write_all(&out)?;
    w.flush()
}

/// One RPC connection, and the transaction id that keeps its replies straight.
pub struct Rpc {
    stream: TcpStream,
    /// What has arrived and is not yet a whole record: a wait that runs out halfway through one
    /// leaves the rest for the next call to finish reading.
    inbox: Vec<u8>,
    xid: u32,
    /// Set once replies can no longer be trusted to line up with calls; nothing is sent after.
    dead: Option<String>,
}

/// Why a call has no reply: the wait ran out (the connection stays), or the connection is gone.
enum Fail {
    Wait,
    Lost(String),
}

impl Rpc {
    pub fn new(stream: TcpStream) -> Result<Rpc, Error> {
        // Any start will do; not 0, which a careless server might use for "no call".
        let xid = std::process::id().wrapping_mul(0x9E37_79B9) | 1;
        Ok(Rpc { stream, inbox: Vec::new(), xid, dead: None })
    }

    /// The connection, for its TCP options.
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// This end's address: where a VXI-11 device can call back to.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.local_addr().ok()
    }

    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.peer_addr().ok()
    }

    pub fn peer_port(&self) -> Option<u16> {
        self.stream.peer_addr().ok().map(|a| a.port())
    }

    /// One call and its reply. `wait` is how long the socket waits (`None`: for ever) and must
    /// be longer than any timeout the call itself carries. The wait running out is
    /// `Error::Timeout` and the connection stays: a reply whose xid is not this call's -- the late
    /// answer to one given up on, or anything else -- is dropped when it comes, as RFC 5531 has
    /// it. A broken stream or a send cut short ends the connection: `Error::Lost`, now and from
    /// then on.
    pub fn call(&mut self, prog: u32, vers: u32, proc_num: u32, args: &[u8], wait: Option<Duration>) -> Result<Vec<u8>, Error> {
        if let Some(why) = &self.dead {
            return Err(Error::Lost(why.clone()));
        }
        let xid = self.xid;
        self.xid = self.xid.wrapping_add(1);
        match self.exchange(xid, prog, vers, proc_num, args, wait) {
            Ok(r) => Ok(r),
            Err(Fail::Wait) => Err(Error::Timeout),
            Err(Fail::Lost(why)) => {
                self.dead = Some(why.clone());
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                Err(Error::Lost(why))
            }
        }
    }

    fn exchange(&mut self, xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8], wait: Option<Duration>) -> Result<Vec<u8>, Fail> {
        let deadline = wait.map(|w| Instant::now() + w);
        // A zero Duration means "no timeout" to the socket; the smallest real one is 1 ms.
        let wait = wait.map(|w| w.max(Duration::from_millis(1)));
        self.stream.set_write_timeout(wait).map_err(|e| Fail::Lost(e.to_string()))?;
        // A send cut short leaves half a record in the device's stream: nothing after it is
        // understood, so that is the end of the connection, timeout or not.
        write_record(&mut self.stream, &build_call(xid, prog, vers, proc_num, args)).map_err(|e| Fail::Lost(format!("sending: {e}")))?;
        loop {
            let reply = self.next_record(deadline)?;
            let got = Decoder::new(&reply).u32().ok_or_else(|| Fail::Lost("an empty RPC record".into()))?;
            // Not this call's: the answer to one given up on, which was always going to arrive.
            if got == xid {
                return parse_reply(&reply, xid).map(|r| r.to_vec()).map_err(Fail::Lost);
            }
        }
    }

    /// The next whole record, reading more until there is one or `deadline` passes.
    fn next_record(&mut self, deadline: Option<Instant>) -> Result<Vec<u8>, Fail> {
        loop {
            match take_record(&mut self.inbox, MAX_RECORD) {
                Ok(Some(r)) => return Ok(r),
                Ok(None) => {}
                Err(e) => return Err(Fail::Lost(e)),
            }
            let left = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(l) if !l.is_zero() => Some(l.max(Duration::from_millis(1))),
                    _ => return Err(Fail::Wait),
                },
                None => None,
            };
            self.stream.set_read_timeout(left).map_err(|e| Fail::Lost(e.to_string()))?;
            let mut buf = [0u8; 64 * 1024];
            match (&self.stream).read(&mut buf) {
                Ok(0) => return Err(Fail::Lost("waiting for the reply: the device closed the connection".into())),
                Ok(n) => self.inbox.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Err(Fail::Wait),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(Fail::Lost(format!("waiting for the reply: {e}"))),
            }
        }
    }
}

/// A whole record from the front of `inbox`, taken out of it; None while it has not all come.
fn take_record(inbox: &mut Vec<u8>, max: usize) -> Result<Option<Vec<u8>>, String> {
    let (mut at, mut len) = (0, 0);
    loop {
        let Some(h) = inbox.get(at..at + 4) else { return Ok(None) };
        let h = u32::from_be_bytes(h.try_into().unwrap());
        let n = (h & 0x7FFF_FFFF) as usize;
        len += n;
        if len > max {
            return Err("RPC record too large".into());
        }
        at += 4 + n;
        if inbox.len() < at {
            return Ok(None);
        }
        if h & 0x8000_0000 != 0 {
            break;
        }
    }
    let mut msg = Vec::with_capacity(len);
    let mut p = 0;
    while p < at {
        let n = (u32::from_be_bytes(inbox[p..p + 4].try_into().unwrap()) & 0x7FFF_FFFF) as usize;
        msg.extend_from_slice(&inbox[p + 4..p + 4 + n]);
        p += 4 + n;
    }
    inbox.drain(..at);
    Ok(Some(msg))
}

/// Ask the portmapper on `host` for the TCP port of a program; 0 means it has none.
///
/// Over TCP, not the customary UDP: one call at open, and TCP says whether the host is there.
pub fn getport(host: &str, prog: u32, vers: u32, within: Duration) -> Result<u16, Error> {
    let mut pm = Rpc::new(super::connect(host, PMAP_PORT, within)?)?;
    let mut a = Encoder::new();
    a.u32(prog).u32(vers).u32(IPPROTO_TCP).u32(0);
    let r = pm
        .call(PMAP_PROG, PMAP_VERS, PMAPPROC_GETPORT, a.as_slice(), Some(within))
        .map_err(|e| Error::NotFound(format!("the portmapper on {host}: {e}")))?;
    let port = Decoder::new(&r).u32().ok_or_else(|| Error::NotFound("a truncated portmapper answer".into()))?;
    u16::try_from(port).map_err(|_| Error::NotFound(format!("the portmapper answered port {port}")))
}

/// A call as a server sees it, and an accepted reply: for the fake servers in tests.
#[cfg(test)]
pub mod server {
    use super::*;

    pub struct Call {
        pub xid: u32,
        pub prog: u32,
        pub proc_num: u32,
        pub args: Vec<u8>,
    }

    pub fn parse_call(msg: &[u8]) -> Option<Call> {
        let mut d = Decoder::new(msg);
        let xid = d.u32()?;
        (d.u32()? == CALL && d.u32()? == RPC_VERSION).then_some(())?;
        let prog = d.u32()?;
        let _vers = d.u32()?;
        let proc_num = d.u32()?;
        d.u32()?;
        d.opaque(400)?;
        d.u32()?;
        d.opaque(400)?;
        Some(Call { xid, prog, proc_num, args: d.rest().to_vec() })
    }

    pub fn reply(xid: u32, results: &[u8]) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u32(xid).u32(REPLY).u32(MSG_ACCEPTED).u32(AUTH_NONE).u32(0).u32(SUCCESS);
        let mut v = e.into_vec();
        v.extend_from_slice(results);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reassemble() {
        let mut buf = Vec::new();
        write_record(&mut buf, b"abcd").unwrap();
        assert_eq!(&buf[..4], &[0x80, 0, 0, 4]);
        assert_eq!(read_record(&mut &buf[..], 64).unwrap(), b"abcd");
        let mut two = Vec::new();
        two.extend_from_slice(&2u32.to_be_bytes());
        two.extend_from_slice(b"ab");
        two.extend_from_slice(&(2u32 | 0x8000_0000).to_be_bytes());
        two.extend_from_slice(b"cd");
        assert_eq!(read_record(&mut &two[..], 64).unwrap(), b"abcd");
        assert!(read_record(&mut &buf[..], 2).is_err(), "longer than allowed");
    }

    #[test]
    fn a_record_is_taken_only_once_it_has_all_come() {
        let mut two = Vec::new();
        two.extend_from_slice(&2u32.to_be_bytes());
        two.extend_from_slice(b"ab");
        two.extend_from_slice(&(2u32 | 0x8000_0000).to_be_bytes());
        two.extend_from_slice(b"cd");
        two.extend_from_slice(b"\x80");
        for cut in 0..two.len() - 1 {
            let mut inbox = two[..cut].to_vec();
            assert_eq!(take_record(&mut inbox, 64), Ok(None), "cut at {cut}");
            assert_eq!(inbox.len(), cut, "nothing taken from a part");
        }
        let mut inbox = two.clone();
        assert_eq!(take_record(&mut inbox, 64).unwrap().unwrap(), b"abcd");
        assert_eq!(inbox, b"\x80", "the next record's first byte stays");
        assert!(take_record(&mut two.clone(), 3).is_err(), "longer than allowed");
    }

    /// A server that answers each call it reads by `script`'s (delay, reply bytes) parts, in turn.
    fn serve(script: Vec<Box<dyn Fn(u32) -> Vec<(u64, Vec<u8>)> + Send>>) -> Rpc {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            for answer in script {
                let Ok(call) = read_record(&mut s, 1 << 20) else { return };
                let xid = server::parse_call(&call).unwrap().xid;
                for (ms, bytes) in answer(xid) {
                    std::thread::sleep(Duration::from_millis(ms));
                    if s.write_all(&bytes).is_err() {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        });
        Rpc::new(TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap()
    }

    fn record(xid: u32, results: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        write_record(&mut v, &server::reply(xid, results)).unwrap();
        v
    }

    fn ask(c: &mut Rpc, ms: u64) -> Result<Vec<u8>, Error> {
        c.call(1, 1, 1, &[], Some(Duration::from_millis(ms)))
    }

    #[test]
    fn a_timeout_keeps_the_connection_and_the_late_reply_is_dropped() {
        let mut c = serve(vec![
            Box::new(|x| vec![(300, record(x, b"late"))]),
            Box::new(|x| vec![(0, record(x, b"mine"))]),
        ]);
        assert!(matches!(ask(&mut c, 100), Err(Error::Timeout)));
        assert_eq!(ask(&mut c, 2000).unwrap(), b"mine", "not the answer to the call given up on");
    }

    #[test]
    fn a_reply_cut_by_a_timeout_is_finished_and_dropped() {
        let mut c = serve(vec![
            Box::new(|x| {
                let r = record(x, b"a reply the client stopped waiting for halfway");
                let half = r.len() / 2;
                vec![(0, r[..half].to_vec()), (300, r[half..].to_vec())]
            }),
            Box::new(|x| vec![(0, record(x, b"mine"))]),
        ]);
        assert!(matches!(ask(&mut c, 100), Err(Error::Timeout)));
        assert_eq!(ask(&mut c, 2000).unwrap(), b"mine");
    }

    #[test]
    fn a_reply_to_no_call_of_ours_is_dropped_too() {
        let mut c = serve(vec![Box::new(|x| vec![(0, record(x ^ 0x5A5A_5A5A, b"?")), (0, record(x, b"mine"))])]);
        assert_eq!(ask(&mut c, 2000).unwrap(), b"mine");
    }

    #[test]
    fn a_closed_connection_stays_lost() {
        let mut c = serve(vec![Box::new(|_| vec![])]);
        assert!(matches!(ask(&mut c, 2000), Err(Error::Lost(_))));
        assert!(matches!(ask(&mut c, 2000), Err(Error::Lost(_))));
    }

    #[test]
    fn a_reply_must_answer_this_call() {
        let r = server::reply(7, &[0, 0, 0, 9]);
        assert_eq!(parse_reply(&r, 7).unwrap(), &[0, 0, 0, 9]);
        assert!(parse_reply(&r, 8).unwrap_err().contains("call 7"));
        let c = server::parse_call(&build_call(5, 0x0607AF, 1, 10, &[1, 2, 3, 4])).unwrap();
        assert_eq!((c.xid, c.prog, c.proc_num, c.args), (5, 0x0607AF, 10, vec![1, 2, 3, 4]));
    }
}
