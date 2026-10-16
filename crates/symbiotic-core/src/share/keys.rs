//! Keys on disk. A member has one X25519 key (`share.key`); the registry has that and an Ed25519
//! signing key (`registry.key`). A file is text, `noise = <hex>` and `sign = <hex>`, readable by
//! the user only where the system lets us say so (Unix mode 0600; on Windows the file sits in the
//! user's profile or in the config directory the installer gave its ACL).

use super::chan::{self, Keypair};
use super::codec::{hex, unhex};
use ed25519_dalek::SigningKey;
use std::io;
use std::path::Path;

pub struct Secrets {
    pub noise: Keypair,
    pub sign: Option<SigningKey>,
}

fn random32() -> io::Result<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b)
}

/// The keys in `path`, if the file is there and readable; nothing is made. A key line that is there
/// but damaged makes the whole file unreadable: a damaged signing key is not a missing one, to be
/// replaced by a new key no member knows.
pub fn load(path: &Path) -> Option<Secrets> {
    let t = std::fs::read_to_string(path).ok()?;
    let (mut noise, mut sign) = (None, None);
    for l in t.lines() {
        match l.split_once('=') {
            Some((k, v)) if k.trim() == "noise" => noise = Some(unhex::<32>(v)?),
            Some((k, v)) if k.trim() == "sign" => sign = Some(unhex::<32>(v)?),
            _ => {}
        }
    }
    let private = noise?;
    Some(Secrets {
        noise: Keypair { private, public: chan::public_of(&private) },
        sign: sign.map(|s| SigningKey::from_bytes(&s)),
    })
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The keys in `path`, made and saved if the file is not there. `with_sign` also makes sure there
/// is a signing key (the registry's). A file that exists but cannot be understood is an error,
/// never silently replaced: replacing a member's key would lock it out of its group.
pub fn load_or_create(path: &Path, with_sign: bool) -> io::Result<Secrets> {
    if path.exists() {
        let s = load(path).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a key file", path.display())))?;
        if !with_sign || s.sign.is_some() {
            return Ok(s);
        }
        let sign = SigningKey::from_bytes(&random32()?);
        write_private(path, &format!("noise = {}\nsign = {}\n", hex(&s.noise.private), hex(&sign.to_bytes())))?;
        return Ok(Secrets { noise: s.noise, sign: Some(sign) });
    }
    let noise = chan::generate()?;
    let sign = if with_sign { Some(SigningKey::from_bytes(&random32()?)) } else { None };
    let mut t = format!("noise = {}\n", hex(&noise.private));
    if let Some(s) = &sign {
        t += &format!("sign = {}\n", hex(&s.to_bytes()));
    }
    write_private(path, &t)?;
    Ok(Secrets { noise, sign })
}

pub fn random_token() -> io::Result<[u8; 16]> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_once_then_the_same() {
        let d = std::env::temp_dir().join(format!("sym-keys-{}", std::process::id()));
        let p = d.join("share.key");
        let a = load_or_create(&p, false).unwrap();
        let b = load_or_create(&p, false).unwrap();
        assert_eq!(a.noise.public, b.noise.public);
        assert!(b.sign.is_none());
        let c = load_or_create(&p, true).unwrap();
        assert_eq!(c.noise.public, a.noise.public, "adding a signing key keeps the noise key");
        let d2 = load_or_create(&p, true).unwrap();
        assert_eq!(c.sign.unwrap().to_bytes(), d2.sign.unwrap().to_bytes());
        std::fs::write(&p, "garbage").unwrap();
        assert!(load_or_create(&p, false).is_err(), "a damaged key file is not replaced");
        let noise = hex(&a.noise.private);
        std::fs::write(&p, format!("noise = {noise}\nsign = 12zz\n")).unwrap();
        assert!(load_or_create(&p, true).is_err(), "nor is a damaged signing key: every member knows the old one");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("noise = {noise}\nsign = 12zz\n"), "the file is left as it was");
        std::fs::remove_dir_all(d).ok();
    }
}
