//! The encrypted channel between two machines: Noise `IK` (`Noise_IK_25519_ChaChaPoly_BLAKE2s`) over
//! any byte stream. The initiator knows the responder's static key beforehand (from the signed
//! directory, or from a pairing code); the responder learns the initiator's during the handshake
//! and decides, before it answers, whether to go on. A refused handshake is answered with nothing:
//! the caller closes the stream.
//!
//! Every Noise message travels with a u16 big endian length. An application message is a u32 total
//! length and its bytes, cut into as many Noise messages as it takes, so one message can be as
//! large as `MAX_MSG` while no single allocation is trusted to the peer before the bytes arrive.

use snow::{Builder, TransportState};
use std::io::{self, Read, Write};

const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// The most one application message may carry.
pub const MAX_MSG: usize = 64 << 20;
const NOISE_MAX: usize = 65535;
const TAG: usize = 16;
const CHUNK: usize = NOISE_MAX - TAG;

#[derive(Clone)]
pub struct Keypair {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub fn generate() -> io::Result<Keypair> {
    let b = Builder::new(PATTERN.parse().map_err(bad)?);
    let k = b.generate_keypair().map_err(bad)?;
    Ok(Keypair {
        private: k.private.try_into().map_err(|_| bad("private key size"))?,
        public: k.public.try_into().map_err(|_| bad("public key size"))?,
    })
}

/// The public key belonging to a private one.
pub fn public_of(private: &[u8; 32]) -> [u8; 32] {
    curve25519_dalek::constants::X25519_BASEPOINT.mul_clamped(*private).to_bytes()
}

pub struct Channel<S: Read + Write> {
    s: S,
    t: TransportState,
    peer: [u8; 32],
}

fn read_frame<S: Read>(s: &mut S) -> io::Result<Vec<u8>> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l)?;
    let mut v = vec![0u8; u16::from_be_bytes(l) as usize];
    s.read_exact(&mut v)?;
    Ok(v)
}

fn write_frame<S: Write>(s: &mut S, b: &[u8]) -> io::Result<()> {
    let mut v = Vec::with_capacity(b.len() + 2);
    v.extend_from_slice(&(b.len() as u16).to_be_bytes());
    v.extend_from_slice(b);
    s.write_all(&v)?;
    s.flush()
}

/// Open a channel to the holder of `remote` (its static public key).
pub fn connect<S: Read + Write>(mut s: S, local: &Keypair, remote: &[u8; 32]) -> io::Result<Channel<S>> {
    let mut hs = Builder::new(PATTERN.parse().map_err(bad)?)
        .local_private_key(&local.private)
        .map_err(bad)?
        .remote_public_key(remote)
        .map_err(bad)?
        .build_initiator()
        .map_err(bad)?;
    let mut buf = vec![0u8; NOISE_MAX];
    let n = hs.write_message(&[], &mut buf).map_err(bad)?;
    write_frame(&mut s, &buf[..n])?;
    let m = read_frame(&mut s)?;
    hs.read_message(&m, &mut buf).map_err(bad)?;
    let t = hs.into_transport_mode().map_err(bad)?;
    Ok(Channel { s, t, peer: *remote })
}

/// Accept a channel. `allow` is given the initiator's static public key and says whether to go on;
/// if not, nothing is written and `PermissionDenied` is returned.
pub fn accept<S: Read + Write>(
    mut s: S,
    local: &Keypair,
    allow: impl FnOnce(&[u8; 32]) -> bool,
) -> io::Result<Channel<S>> {
    let mut hs = Builder::new(PATTERN.parse().map_err(bad)?)
        .local_private_key(&local.private)
        .map_err(bad)?
        .build_responder()
        .map_err(bad)?;
    let mut buf = vec![0u8; NOISE_MAX];
    let m = read_frame(&mut s)?;
    hs.read_message(&m, &mut buf).map_err(bad)?;
    let peer: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| bad("no static key from the initiator"))?;
    if !allow(&peer) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "peer not allowed"));
    }
    let n = hs.write_message(&[], &mut buf).map_err(bad)?;
    write_frame(&mut s, &buf[..n])?;
    let t = hs.into_transport_mode().map_err(bad)?;
    Ok(Channel { s, t, peer })
}

impl<S: Read + Write> Channel<S> {
    /// The other end's static public key (proved by the handshake).
    pub fn peer(&self) -> &[u8; 32] {
        &self.peer
    }

    pub fn stream(&mut self) -> &mut S {
        &mut self.s
    }

    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        if data.len() > MAX_MSG {
            return Err(bad("message too large"));
        }
        let mut plain = Vec::with_capacity(data.len() + 4);
        plain.extend_from_slice(&(data.len() as u32).to_le_bytes());
        plain.extend_from_slice(data);
        let mut out = vec![0u8; NOISE_MAX];
        for c in plain.chunks(CHUNK) {
            let n = self.t.write_message(c, &mut out).map_err(bad)?;
            write_frame(&mut self.s, &out[..n])?;
        }
        Ok(())
    }

    pub fn recv(&mut self) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; NOISE_MAX];
        let mut plain: Vec<u8> = Vec::new();
        let mut want: Option<usize> = None;
        loop {
            let f = read_frame(&mut self.s)?;
            let n = self.t.read_message(&f, &mut out).map_err(bad)?;
            plain.extend_from_slice(&out[..n]);
            if want.is_none() && plain.len() >= 4 {
                let w = u32::from_le_bytes(plain[..4].try_into().unwrap()) as usize;
                if w > MAX_MSG {
                    return Err(bad("message too large"));
                }
                want = Some(w);
            }
            if let Some(w) = want {
                if plain.len() >= w + 4 {
                    if plain.len() != w + 4 {
                        return Err(bad("bytes past the end of a message"));
                    }
                    return Ok(plain.split_off(4));
                }
            }
        }
    }
}

#[cfg(test)]
pub fn pipe_pair() -> (impl Read + Write, impl Read + Write) {
    // Two threads' worth of loopback: a connected pair of TCP streams.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (b, _) = l.accept().unwrap();
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_of_any_size_and_the_peers_keys() {
        let (a, b) = pipe_pair();
        let (ka, kb) = (generate().unwrap(), generate().unwrap());
        assert_eq!(public_of(&kb.private), kb.public);
        let (kb2, ka_pub) = (kb.clone(), ka.public);
        let h = std::thread::spawn(move || {
            let mut c = accept(b, &kb2, |k| *k == ka_pub).unwrap();
            assert_eq!(c.peer(), &ka_pub);
            for _ in 0..5 {
                let m = c.recv().unwrap();
                c.send(&m).unwrap();
            }
            let e = c.recv().unwrap();
            assert!(e.is_empty());
            c.send(b"bye").unwrap();
        });
        let mut c = connect(a, &ka, &kb.public).unwrap();
        for n in [1usize, 65_000, 65_519, 65_520, 200_000] {
            let m: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            c.send(&m).unwrap();
            assert_eq!(c.recv().unwrap(), m, "{n} bytes");
        }
        c.send(&[]).unwrap();
        assert_eq!(c.recv().unwrap(), b"bye");
        h.join().unwrap();
    }

    #[test]
    fn a_refused_initiator_gets_nothing_and_a_wrong_key_fails() {
        let (a, b) = pipe_pair();
        let (ka, kb) = (generate().unwrap(), generate().unwrap());
        let kb2 = kb.clone();
        let h = std::thread::spawn(move || accept(b, &kb2, |_| false).err().map(|e| e.kind()));
        let r = connect(a, &ka, &kb.public);
        assert!(r.is_err(), "the initiator must see the stream close, not an answer");
        assert_eq!(h.join().unwrap(), Some(io::ErrorKind::PermissionDenied));

        let (a, b) = pipe_pair();
        let other = generate().unwrap();
        let kb2 = kb.clone();
        let h = std::thread::spawn(move || accept(b, &kb2, |_| true).is_err());
        drop(connect(a, &ka, &other.public));
        assert!(h.join().unwrap(), "a handshake to the wrong static key must not complete");
    }
}
