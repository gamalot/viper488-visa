//! ONC RPC version 2 (RFC 5531) over TCP, the client half: what VXI-11 and the portmapper need.
//!
//! TCP carries RPC in records: a four-byte header per fragment, the high bit set on the last one, the low 31 bits its
//! length.

use super::xdr::{Decoder, Encoder};
use super::Error;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

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
    reader: BufReader<TcpStream>,
    xid: u32,
    /// Set once replies can no longer be trusted to line up with calls; nothing is sent after.
    dead: Option<String>,
}

impl Rpc {
    pub fn new(stream: TcpStream) -> Result<Rpc, Error> {
        let copy = stream.try_clone().map_err(|e| Error::NotFound(e.to_string()))?;
        // Any start will do; not 0, which a careless server might use for "no call".
        let xid = std::process::id().wrapping_mul(0x9E37_79B9) | 1;
        Ok(Rpc { stream, reader: BufReader::with_capacity(64 * 1024, copy), xid, dead: None })
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
    /// be longer than any timeout the call itself carries. A socket timeout, a broken stream or a
    /// reply to another call ends the connection: `Error::Lost`, now and from then on.
    pub fn call(&mut self, prog: u32, vers: u32, proc_num: u32, args: &[u8], wait: Option<Duration>) -> Result<Vec<u8>, Error> {
        if let Some(why) = &self.dead {
            return Err(Error::Lost(why.clone()));
        }
        let xid = self.xid;
        self.xid = self.xid.wrapping_add(1);
        let r = self.exchange(xid, prog, vers, proc_num, args, wait);
        if let Err(why) = &r {
            self.dead = Some(why.clone());
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
        r.map_err(Error::Lost)
    }

    fn exchange(&mut self, xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8], wait: Option<Duration>) -> Result<Vec<u8>, String> {
        // A zero Duration means "no timeout" to the socket; the smallest real one is 1 ms.
        let wait = wait.map(|w| w.max(Duration::from_millis(1)));
        self.stream.set_write_timeout(wait).map_err(|e| e.to_string())?;
        self.stream.set_read_timeout(wait).map_err(|e| e.to_string())?;
        write_record(&mut self.stream, &build_call(xid, prog, vers, proc_num, args)).map_err(|e| format!("sending: {e}"))?;
        let reply = read_record(&mut self.reader, MAX_RECORD).map_err(|e| format!("waiting for the reply: {e}"))?;
        parse_reply(&reply, xid).map(|r| r.to_vec())
    }
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
    fn a_reply_must_answer_this_call() {
        let r = server::reply(7, &[0, 0, 0, 9]);
        assert_eq!(parse_reply(&r, 7).unwrap(), &[0, 0, 0, 9]);
        assert!(parse_reply(&r, 8).unwrap_err().contains("call 7"));
        let c = server::parse_call(&build_call(5, 0x0607AF, 1, 10, &[1, 2, 3, 4])).unwrap();
        assert_eq!((c.xid, c.prog, c.proc_num, c.args), (5, 0x0607AF, 10, vec![1, 2, 3, 4]));
    }
}
