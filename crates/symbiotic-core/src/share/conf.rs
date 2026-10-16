//! `share.conf` (this machine's part in a group) and `shares.conf` (which aliases it shares).
//!
//! `share.conf` is `key = value` lines, `#` comments:
//!
//! ```text
//!     share = on                     serve shared aliases to the group (listen, claim, stay resident)
//!     registry = 192.0.2.10:7489
//!     registry_key = <hex>           the registry's X25519 key (from the pairing code)
//!     registry_verify = <hex>        the registry's signing key (the answer to Enroll)
//!     name = bench-a                 this machine's name in the group
//!     listen = 0.0.0.0:7488          where this machine serves (share = on)
//!     advertise = host:port          what to tell the group when the address it sees is not the one that works
//!     directory_ttl = 604800         seconds a cached directory is believed with no registry
//!     follow = on                    take over an alias whose USB instrument has come to this machine and left its owner
//!     registry_listen = 0.0.0.0:7489 run the registry in this machine's daemon (off when absent)
//!     registry_advertise = host:port the address in the pairing codes it issues
//! ```
//!
//! A line that does not parse is ignored, as in the other `.conf` files. `shares.conf` is one alias
//! per line; the alias must also be in `aliases.conf` for the share to count.

use super::chan::Keypair;
use super::codec::{hex, unhex};
use super::dir::Directory;
use super::regclient::{Error, RegistryClient};
use super::registry::PairingCode;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MEMBER_PORT: u16 = 7488;
pub const REGISTRY_PORT: u16 = 7489;
pub const DEFAULT_TTL: u64 = 7 * 24 * 3600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareConf {
    pub share: bool,
    pub registry: String,
    pub registry_key: Option<[u8; 32]>,
    pub registry_verify: Option<[u8; 32]>,
    pub name: String,
    pub listen: String,
    pub advertise: Option<String>,
    pub directory_ttl: u64,
    /// Take an alias over when the unit it is bound to (a USB instrument) is here and its owner no longer has it.
    pub follow: bool,
    pub registry_listen: Option<String>,
    pub registry_advertise: Option<String>,
}

pub fn default_name() -> String {
    for v in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(n) = std::env::var_os(v).and_then(|n| n.into_string().ok()).filter(|n| !n.trim().is_empty()) {
            return n.trim().to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname").ok().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "member".to_string())
}

impl Default for ShareConf {
    fn default() -> Self {
        ShareConf {
            share: false,
            registry: String::new(),
            registry_key: None,
            registry_verify: None,
            name: default_name(),
            listen: format!("0.0.0.0:{MEMBER_PORT}"),
            advertise: None,
            directory_ttl: DEFAULT_TTL,
            follow: false,
            registry_listen: None,
            registry_advertise: None,
        }
    }
}

impl ShareConf {
    pub fn parse(text: &str) -> ShareConf {
        let mut c = ShareConf::default();
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let Some((k, v)) = l.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "share" => c.share = matches!(v.to_ascii_lowercase().as_str(), "on" | "yes" | "true" | "1"),
                "registry" => c.registry = v.to_string(),
                "registry_key" => c.registry_key = unhex(v),
                "registry_verify" => c.registry_verify = unhex(v),
                "name" if !v.is_empty() => c.name = v.to_string(),
                "listen" if !v.is_empty() => c.listen = v.to_string(),
                "advertise" if !v.is_empty() => c.advertise = Some(v.to_string()),
                "directory_ttl" => c.directory_ttl = v.parse().unwrap_or(DEFAULT_TTL),
                "follow" => c.follow = matches!(v.to_ascii_lowercase().as_str(), "on" | "yes" | "true" | "1"),
                "registry_listen" if !v.is_empty() => c.registry_listen = Some(v.to_string()),
                "registry_advertise" if !v.is_empty() => c.registry_advertise = Some(v.to_string()),
                _ => {}
            }
        }
        c
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# This machine in an alias-sharing group (symbiotic-ctl share ...).\n");
        s += &format!("share = {}\n", if self.share { "on" } else { "off" });
        s += &format!("registry = {}\n", self.registry);
        if let Some(k) = &self.registry_key {
            s += &format!("registry_key = {}\n", hex(k));
        }
        if let Some(k) = &self.registry_verify {
            s += &format!("registry_verify = {}\n", hex(k));
        }
        s += &format!("name = {}\nlisten = {}\n", self.name, self.listen);
        if let Some(a) = &self.advertise {
            s += &format!("advertise = {a}\n");
        }
        s += &format!("directory_ttl = {}\n", self.directory_ttl);
        if self.follow {
            s += "follow = on\n";
        }
        if let Some(a) = &self.registry_listen {
            s += &format!("registry_listen = {a}\n");
        }
        if let Some(a) = &self.registry_advertise {
            s += &format!("registry_advertise = {a}\n");
        }
        s
    }

    pub fn path() -> Option<PathBuf> {
        std::env::var_os("SYMBIOTIC_SHARE_CONF").map(PathBuf::from).or_else(|| crate::paths::user_file("share.conf"))
    }

    pub fn load() -> ShareConf {
        Self::path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| ShareConf::parse(&t)).unwrap_or_default()
    }

    /// Written only if every value is one line (`conffile::Field`): a name or an address that held a
    /// line break would add a setting of its own, `registry_listen` say.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        use crate::conffile::Field;
        Field::new(&self.registry, &[], "the registry's address")?;
        Field::new(&self.name, &[], "this machine's name")?;
        Field::new(&self.listen, &[], "the address to serve on")?;
        for a in [&self.advertise, &self.registry_listen, &self.registry_advertise].into_iter().flatten() {
            Field::new(a, &[], "an address")?;
        }
        crate::conffile::write(path, self.render())
    }

    /// Joined to a group (has the registry's two keys and its address).
    pub fn joined(&self) -> bool {
        !self.registry.is_empty() && self.registry_key.is_some() && self.registry_verify.is_some()
    }
}

/// The aliases `text` (shares.conf) names, first spelling of each.
pub fn parse_shares(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if super::super::alias::check_name(l).is_ok() && !out.iter().any(|o| o.eq_ignore_ascii_case(l)) {
            out.push(l.to_string());
        }
    }
    out
}

pub fn shares_path() -> Option<PathBuf> {
    std::env::var_os("SYMBIOTIC_SHARES").map(PathBuf::from).or_else(|| crate::paths::config_file("shares.conf"))
}

pub fn load_shares() -> Vec<String> {
    shares_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_shares(&t)).unwrap_or_default()
}

/// Share (or stop sharing) `alias`. Other lines and comments stay.
pub fn set_shared(alias: &str, on: bool) -> io::Result<()> {
    super::super::alias::check_name(alias).map_err(|m| io::Error::new(io::ErrorKind::InvalidInput, m))?;
    let p = shares_path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no place for shares.conf"))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() { "# Aliases shared with the group (symbiotic-ctl share add/remove), one per line\n" } else { text };
        let mut lines: Vec<String> = text.lines().filter(|l| !l.trim().eq_ignore_ascii_case(alias)).map(str::to_string).collect();
        if on {
            lines.push(alias.to_string());
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// Join the group a pairing code belongs to: enrol with `keys`, and return the configuration with
/// the registry's keys and this machine's name filled in (the caller saves it) and the directory.
pub fn join(base: &ShareConf, keys: &Keypair, code: &PairingCode, name: &str) -> Result<(ShareConf, Directory), Error> {
    // Refused here, before anything is sent, as the registry and `save` would refuse it.
    crate::conffile::Field::new(name, &[], "this machine's name").map_err(Error::Io)?;
    let addr = code.addr.to_socket_addrs().map_err(Error::Io)?.next().ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::NotFound, "no such address")))?;
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let (_, verify, dir) = RegistryClient::enroll(s, keys, code, name)?;
    let mut c = base.clone();
    c.registry = code.addr.clone();
    c.registry_key = Some(code.registry);
    c.registry_verify = Some(verify);
    c.name = name.to_string();
    Ok((c, dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name or an address that would add a setting of its own (a registry here, say) is not written,
    /// and a name is refused before anything is sent to join.
    #[test]
    fn no_setting_is_added_through_a_value() {
        let dir = std::env::temp_dir().join(format!("symbiotic-share-conf-{}", std::process::id()));
        let file = dir.join("share.conf");
        let evil = "bench\nregistry_listen = 0.0.0.0:7489";
        for c in [
            ShareConf { name: evil.into(), ..Default::default() },
            ShareConf { listen: evil.into(), ..Default::default() },
            ShareConf { advertise: Some(evil.into()), ..Default::default() },
            ShareConf { registry: "h:1\u{1b}".into(), ..Default::default() },
        ] {
            assert!(c.save(&file).is_err());
            assert!(!file.exists(), "nothing written");
        }
        let good = ShareConf { name: "bench".into(), ..Default::default() };
        good.save(&file).unwrap();
        assert_eq!(ShareConf::parse(&std::fs::read_to_string(&file).unwrap()).registry_listen, None);
        std::fs::remove_dir_all(&dir).unwrap();

        let code = PairingCode { addr: "127.0.0.1:9".into(), registry: [0; 32], token: [0; 16] };
        let keys = crate::share::chan::generate().unwrap();
        let t = std::time::Instant::now();
        let e = join(&ShareConf::default(), &keys, &code, evil).err().unwrap();
        assert!(e.to_string().contains("this machine's name"), "{e}");
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "refused before connecting");
    }

    #[test]
    fn the_file_round_trips_and_bad_lines_are_ignored() {
        let c = ShareConf {
            share: true,
            registry: "198.18.68.16:7489".into(),
            registry_key: Some([1; 32]),
            registry_verify: Some([2; 32]),
            name: "bench-a".into(),
            advertise: Some("10.0.0.2:7488".into()),
            registry_listen: Some("0.0.0.0:7489".into()),
            follow: true,
            ..Default::default()
        };
        assert_eq!(ShareConf::parse(&c.render()), c);
        let d = ShareConf::parse("share = maybe\nnonsense\nregistry_key = zz\ndirectory_ttl = soon\nlisten =\n");
        assert_eq!(d, ShareConf { name: d.name.clone(), ..Default::default() });
        assert!(!d.joined());
    }

    #[test]
    fn the_shares_file() {
        let t = "# c\nDMM\n dmm \nbad:name\nCOM3\nscope\n";
        assert_eq!(parse_shares(t), ["DMM", "scope"]);
    }
}
