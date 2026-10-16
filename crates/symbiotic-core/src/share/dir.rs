//! The directory the registry signs: who the members are and who owns which alias. Everything
//! here is what a member may rely on; who is online is not (it is advisory, `registry::Presence`).

use super::codec::{R, Res, W};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// v2: a claim carries the identity of the unit (a v1 member fails the signature, plainly, rather than misreading the body).
const DOMAIN: &[u8] = b"symbiotic-directory-v2\0";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The users reach the instrument through its owner.
    Relay = 0,
    /// A LAN instrument: the users open its `TCPIP` resource name themselves.
    Direct = 1,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Member {
    pub key: [u8; 32],
    pub name: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Claim {
    pub alias: String,
    pub owner: [u8; 32],
    pub kind: Kind,
    /// The owner's canonical resource name. A `Direct` claim's is the `TCPIP` name the users open;
    /// a `Relay`'s is a GPIB, USB or ASRL name only the owner's daemon can reach, kept for search
    /// expressions and for the interface type.
    pub resource: String,
    /// The unit the alias is bound to (`identity::Identity::line`), or empty. With it, another member that has the same
    /// unit may be given the alias when the owner no longer has it.
    pub identity: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Directory {
    pub version: u64,
    pub members: Vec<Member>,
    pub claims: Vec<Claim>,
}

impl Directory {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        w.u64(self.version).u32(self.members.len() as u32);
        for m in &self.members {
            w.raw(&m.key).str(&m.name);
        }
        w.u32(self.claims.len() as u32);
        for c in &self.claims {
            w.str(&c.alias).raw(&c.owner).u8(c.kind as u8).str(&c.resource).str(&c.identity);
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> Res<Directory> {
        let mut r = R::new(b);
        let version = r.u64()?;
        let mut members = Vec::new();
        for _ in 0..r.count(36)? {
            members.push(Member { key: r.array()?, name: r.str()? });
        }
        let mut claims = Vec::new();
        for _ in 0..r.count(45)? {
            let alias = r.str()?;
            let owner = r.array()?;
            let kind = match r.u8()? {
                0 => Kind::Relay,
                1 => Kind::Direct,
                _ => return Err("unknown claim kind"),
            };
            claims.push(Claim { alias, owner, kind, resource: r.str()?, identity: r.str()? });
        }
        r.done()?;
        Ok(Directory { version, members, claims })
    }

    pub fn is_member(&self, key: &[u8; 32]) -> bool {
        self.members.iter().any(|m| &m.key == key)
    }

    pub fn member(&self, key: &[u8; 32]) -> Option<&Member> {
        self.members.iter().find(|m| &m.key == key)
    }

    /// The claim on `alias` (VISA names do not care about case).
    pub fn claim(&self, alias: &str) -> Option<&Claim> {
        self.claims.iter().find(|c| c.alias.eq_ignore_ascii_case(alias.trim()))
    }
}

/// A directory and the registry's signature over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub sig: [u8; 64],
    pub body: Vec<u8>,
}

impl Signed {
    pub fn sign(dir: &Directory, key: &SigningKey) -> Signed {
        let body = dir.encode();
        let sig = key.sign(&[DOMAIN, &body].concat()).to_bytes();
        Signed { sig, body }
    }

    /// The directory, if the signature is `registry`'s.
    pub fn verify(&self, registry: &VerifyingKey) -> Res<Directory> {
        registry
            .verify_strict(&[DOMAIN, &self.body].concat(), &Signature::from_bytes(&self.sig))
            .map_err(|_| "the directory's signature is not the registry's")?;
        Directory::decode(&self.body)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        w.raw(&self.sig).bytes(&self.body);
        w.0
    }

    pub fn read(r: &mut R) -> Res<Signed> {
        Ok(Signed { sig: r.array()?, body: r.bytes()?.to_vec() })
    }

    pub fn decode(b: &[u8]) -> Res<Signed> {
        let mut r = R::new(b);
        let s = Signed::read(&mut r)?;
        r.done()?;
        Ok(s)
    }
}

/// Take `new` in place of `cached` only if it is not older: a replayed old directory (one from
/// before a member was revoked, say) must not undo what a newer one said.
pub fn accept_newer(cached: Option<&Directory>, new: Directory) -> Res<Directory> {
    match cached {
        Some(c) if new.version < c.version => Err("the directory is older than the one already held"),
        _ => Ok(new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Directory {
        Directory {
            version: 3,
            members: vec![Member { key: [1; 32], name: "bench-a".into() }],
            claims: vec![Claim { alias: "DMM".into(), owner: [1; 32], kind: Kind::Direct, resource: "TCPIP0::192.0.2.5::inst0::INSTR".into(), identity: String::new() }],
        }
    }

    #[test]
    fn signed_and_checked() {
        let k = SigningKey::from_bytes(&[9; 32]);
        let other = SigningKey::from_bytes(&[8; 32]);
        let s = Signed::sign(&sample(), &k);
        assert_eq!(Signed::decode(&s.encode()).unwrap(), s);
        assert_eq!(s.verify(&k.verifying_key()).unwrap(), sample());
        assert!(s.verify(&other.verifying_key()).is_err(), "another registry's key");
        let mut t = s.clone();
        let n = t.body.len();
        t.body[n - 1] ^= 1;
        assert!(t.verify(&k.verifying_key()).is_err(), "a changed body");
        let mut t = s.clone();
        t.sig[0] ^= 1;
        assert!(t.verify(&k.verifying_key()).is_err(), "a changed signature");
        assert!(sample().claim(" dmm ").is_some(), "case and edge white space do not matter");
    }

    #[test]
    fn no_rolling_back() {
        let (old, new) = (Directory { version: 1, ..Default::default() }, Directory { version: 2, ..Default::default() });
        assert!(accept_newer(Some(&new), old.clone()).is_err());
        assert!(accept_newer(Some(&old), new.clone()).is_ok());
        assert!(accept_newer(Some(&new), new.clone()).is_ok(), "the same version again is not a rollback");
        assert!(accept_newer(None, old).is_ok());
    }

    #[test]
    fn hostile_counts_do_not_allocate() {
        let mut b = sample().encode();
        b[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Directory::decode(&b).is_err());
    }
}
