//! The directory a member keeps between runs (`share.dir`): the registry's signed bytes, when they
//! were got, and the addresses the members last sent in their heartbeats, so a registry that is
//! down does not stop a user from reaching an owner it knew the address of. The signature is
//! checked again on every load; a file that does not verify is no cache.

use super::codec::{R, W};
use super::dir::{Directory, Signed};
use ed25519_dalek::VerifyingKey;
use std::path::Path;

pub type Addrs = Vec<([u8; 32], Vec<String>)>;

pub struct Cached {
    pub dir: Directory,
    /// Unix seconds when it was got.
    pub at: u64,
    pub addrs: Addrs,
}

pub fn read(path: &Path, verify: &[u8; 32]) -> Option<Cached> {
    let verify = VerifyingKey::from_bytes(verify).ok()?;
    let b = std::fs::read(path).ok()?;
    let mut r = R::new(&b);
    let at = r.u64().ok()?;
    let dir = Signed::read(&mut r).ok()?.verify(&verify).ok()?;
    let mut addrs = Vec::new();
    for _ in 0..r.count(36).ok()? {
        let key = r.array().ok()?;
        let mut a = Vec::new();
        for _ in 0..r.count(4).ok()? {
            a.push(r.str().ok()?);
        }
        addrs.push((key, a));
    }
    r.done().ok()?;
    Some(Cached { dir, at, addrs })
}

pub fn write(path: &Path, signed: &Signed, at: u64, addrs: &Addrs) {
    let mut w = W::default();
    w.u64(at).raw(&signed.encode()).u32(addrs.len() as u32);
    for (k, a) in addrs {
        w.raw(k).u32(a.len() as u32);
        for x in a {
            w.str(x);
        }
    }
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &w.0).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::dir::Member;
    use ed25519_dalek::SigningKey;

    #[test]
    fn round_trip_and_a_forged_file() {
        let k = SigningKey::from_bytes(&[4; 32]);
        let d = Directory { version: 9, members: vec![Member { key: [1; 32], name: "a".into() }], claims: vec![] };
        let p = std::env::temp_dir().join(format!("sym-cache-{}", std::process::id()));
        write(&p, &Signed::sign(&d, &k), 123, &vec![([1; 32], vec!["10.0.0.2:7488".to_string()])]);
        let c = read(&p, &k.verifying_key().to_bytes()).unwrap();
        assert_eq!((c.dir, c.at, c.addrs), (d, 123, vec![([1; 32], vec!["10.0.0.2:7488".to_string()])]));
        let other = SigningKey::from_bytes(&[5; 32]).verifying_key().to_bytes();
        assert!(read(&p, &other).is_none(), "a directory some other key signed");
        let mut b = std::fs::read(&p).unwrap();
        b.truncate(b.len() - 3);
        std::fs::write(&p, b).unwrap();
        assert!(read(&p, &k.verifying_key().to_bytes()).is_none(), "a cut file");
        std::fs::remove_file(&p).ok();
    }
}
