//! This machine as a member of a group: the cached directory, keeping the registry told what is
//! shared (`sync`), and serving shared aliases to the others (`listen`).

use super::cache;
use super::chan::{self, Channel, Keypair};
use super::door::{self, Door};
use super::conf::ShareConf;
use super::dir::{Directory, Kind, Signed, accept_newer};
use crate::identity::{Identity, UsbIds};
use super::peer::{self, Opened, PeerHost, UsbInfo};
use super::regclient::{Error, RegistryClient};
use super::registry::{AliasState, Here, Presence};
use super::regserver::{HEARTBEAT, unix_now};
use crate::locks::Target;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_PEERS: usize = 64;
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(5);
/// An unknown key at the door may make us ask the registry again, but not more often than this.
const REFRESH_FOR_STRANGERS: u64 = 5;

/// What `follow_lan` keeps between passes: how many passes in a row an alias's address did not answer, and when it was last looked for.
#[derive(Default)]
pub struct LanFollow {
    misses: std::collections::HashMap<String, u32>,
    searched: std::collections::HashMap<String, std::time::Instant>,
}

/// What one `sync` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub claimed: Vec<String>,
    pub released: Vec<String>,
    /// Aliases taken over because their unit has come here (`follow`).
    pub followed: Vec<String>,
    /// Aliases that could not be shared and why (the name is another member's, or it is not an
    /// instrument the group can reach).
    pub refused: Vec<(String, String)>,
    /// The registry protocol the connection spoke (registry::PROTOCOL, or 1 to a registry of an earlier build).
    pub protocol: u32,
}

pub struct Member {
    pub conf: ShareConf,
    keys: Keypair,
    /// The directory we believe, and when we got it (Unix seconds).
    dir: Mutex<Option<(Directory, u64)>>,
    cache: Option<PathBuf>,
    aliases: Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    shared: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    /// The units aliases are bound to (identities.conf).
    identities: Box<dyn Fn() -> Vec<(String, Identity)> + Send + Sync>,
    /// The USBTMC instruments plugged in here, as descriptors; None where that cannot be told.
    usb: Box<dyn Fn() -> Option<Vec<UsbIds>> + Send + Sync>,
    /// What to write when an alias is taken over: the alias, its resource, its unit.
    adopt: Box<dyn Fn(&str, &str, &Identity) -> io::Result<()> + Send + Sync>,
    /// Whether the instrument of a shared resource (not a USB one: its descriptor says) is there, and how old that finding is, seconds.
    /// The daemon knows (it may not open an adapter for the question); without one, nothing is known.
    check: Box<dyn Fn(&str) -> (Here, u32) + Send + Sync>,
    /// The GPIB instruments here that said who they are: (resource, `*IDN?` answer), from the daemon's scans (it asks each new
    /// listener once, where the user allows `*IDN?`). None where nothing is known.
    gpib: Box<dyn Fn() -> Option<Vec<(String, String)>> + Send + Sync>,
    /// Point this machine's alias at where its unit is now, keeping its binding, with what its old place kept (`lanunits::follow_alias`).
    moved: Box<dyn Fn(&str, &str) -> io::Result<()> + Send + Sync>,
    last_refresh: Mutex<u64>,
    /// When a stranger at the door last made us fetch the directory (`refresh_for_stranger`).
    stranger_refresh: Mutex<u64>,
    peers: AtomicUsize,
    /// Who was online and where, as of the last sync (advisory).
    presence: Mutex<Vec<Presence>>,
    /// Whether `presence` is from a registry answer in this run, and so says who is online.
    presence_fresh: AtomicBool,
    /// The sessions other members have open here: (number, member's key, its connection), so that a
    /// member taken out of the group loses the ones it already had (`cut_revoked`).
    sessions: Mutex<Vec<(u64, [u8; 32], TcpStream)>>,
    next_session: AtomicU64,
}

fn lookup(list: &[(String, String)], alias: &str) -> Option<String> {
    list.iter().find(|(a, _)| a.eq_ignore_ascii_case(alias.trim())).map(|(_, r)| r.clone())
}

impl Member {
    /// `cache` is where the signed directory is kept between runs; `aliases` is `aliases.conf`'s
    /// content and `shared` is `shares.conf`'s, read afresh each time they are needed.
    pub fn new(
        conf: ShareConf,
        keys: Keypair,
        cache: Option<PathBuf>,
        aliases: impl Fn() -> Vec<(String, String)> + Send + Sync + 'static,
        shared: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Member {
        let m = Member {
            conf,
            keys,
            dir: Mutex::new(None),
            cache,
            aliases: Box::new(aliases),
            shared: Box::new(shared),
            identities: Box::new(crate::identity::load),
            usb: Box::new(|| crate::usbtmc::list().ok().map(|l| l.into_iter().map(|f| UsbIds { vid: f.vid, pid: f.pid, serial: f.serial }).collect())),
            adopt: Box::new(|alias, resource, id| {
                crate::alias::set(alias, Some(resource))?;
                crate::identity::set(alias, Some(id))?;
                super::conf::set_shared(alias, true)
            }),
            check: Box::new(|_| (Here::Unknown, 0)),
            gpib: Box::new(|| None),
            moved: Box::new(crate::lanunits::follow_alias),
            last_refresh: Mutex::new(0),
            stranger_refresh: Mutex::new(0),
            peers: AtomicUsize::new(0),
            presence: Mutex::new(Vec::new()),
            presence_fresh: AtomicBool::new(false),
            sessions: Mutex::new(Vec::new()),
            next_session: AtomicU64::new(1),
        };
        // What was kept from the last run: believed for the directory's age, and the addresses
        // are the last known ones, not a fact about now (`presence_fresh`).
        if let (Some(p), Some(v)) = (&m.cache, &m.conf.registry_verify) {
            if let Some(c) = cache::read(p, v) {
                *m.presence.lock().unwrap() = c.addrs.into_iter().map(|(key, addrs)| Presence { key, online: true, addrs, states: Vec::new() }).collect();
                *m.last_refresh.lock().unwrap() = c.at;
                *m.dir.lock().unwrap() = Some((c.dir, c.at));
            }
        }
        m
    }

    /// For tests: where the units, the USB instruments and the adopted aliases come from.
    pub fn with_world(
        mut self,
        identities: impl Fn() -> Vec<(String, Identity)> + Send + Sync + 'static,
        usb: impl Fn() -> Option<Vec<UsbIds>> + Send + Sync + 'static,
        adopt: impl Fn(&str, &str, &Identity) -> io::Result<()> + Send + Sync + 'static,
    ) -> Member {
        self.identities = Box::new(identities);
        self.usb = Box::new(usb);
        self.adopt = Box::new(adopt);
        self
    }

    /// How the daemon tells whether a non-USB shared instrument is there (GPIB: a probe, only when nobody uses the adapter).
    pub fn with_check(mut self, check: impl Fn(&str) -> (Here, u32) + Send + Sync + 'static) -> Member {
        self.check = Box::new(check);
        self
    }

    /// Where the GPIB instruments here are known from (the daemon's scans), and how an alias is pointed at where its unit went.
    pub fn with_gpib(
        mut self,
        units: impl Fn() -> Option<Vec<(String, String)>> + Send + Sync + 'static,
        moved: impl Fn(&str, &str) -> io::Result<()> + Send + Sync + 'static,
    ) -> Member {
        self.gpib = Box::new(units);
        self.moved = Box::new(moved);
        self
    }

    /// The resource here that is `id`'s unit, by the GPIB instruments known here; None if it is not one of them, or two are.
    fn gpib_unit(&self, units: &[(String, String)], id: &Identity) -> Option<String> {
        let mut at = units.iter().filter(|(_, idn)| id.matches_idn(idn)).map(|(r, _)| r.clone());
        let first = at.next()?;
        at.next().is_none().then_some(first)
    }

    /// This machine's shared GPIB aliases bound to a unit that is here at another address or on another board now (an adapter
    /// that took another number when the daemon started, an instrument given another address): pointed at it, before the
    /// registry is told anything. The aliases moved.
    fn relocate_gpib(&self, rep: &mut SyncReport) {
        let Some(units) = (self.gpib)() else { return };
        let list = (self.aliases)();
        let ids = (self.identities)();
        for a in (self.shared)() {
            let (Some(res), Some((_, id))) = (lookup(&list, &a), ids.iter().find(|(x, _)| x.eq_ignore_ascii_case(&a))) else { continue };
            let Some(cur) = crate::resource::canonical(&res).filter(|c| crate::resource::parse(c).is_some()) else { continue };
            match self.gpib_unit(&units, id) {
                Some(now) if !now.eq_ignore_ascii_case(&cur) => match (self.moved)(&a, &now) {
                    Ok(()) => rep.followed.push(a.clone()),
                    Err(e) => rep.refused.push((a.clone(), format!("its unit is at {now}, but the alias could not be changed: {e}"))),
                },
                _ => {}
            }
        }
    }

    /// Make `alias` a group alias for the LAN instrument `resource`, or change it; `identity` is the unit it is bound
    /// to, or "".
    pub fn group_set(&self, alias: &str, resource: &str, identity: &str) -> Result<(), Error> {
        let (mut c, _) = self.connect_registry()?;
        c.set_group(alias, resource, identity)
    }

    pub fn group_clear(&self, alias: &str) -> Result<(), Error> {
        let (mut c, _) = self.connect_registry()?;
        c.clear_group(alias)
    }

    /// Bind the group alias `alias` to the unit that is at its resource now: ask it `*IDN?` once (that is what binding is) and keep the
    /// manufacturer, model and serial number with the alias at the registry.
    pub fn group_bind(&self, alias: &str, ask: impl Fn(&str) -> Result<String, String>) -> Result<Identity, String> {
        let d = self.directory().ok_or("no directory: the registry has not been reached")?;
        let claim = d.claim(alias).filter(|c| c.owner == super::registry::GROUP_OWNER).ok_or_else(|| format!("{alias:?} is not a group alias"))?;
        let id = Identity::from_idn(&ask(&claim.resource)?)?;
        self.group_set(&claim.alias, &claim.resource, &id.line()).map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Move this member's own aliases for LAN instruments (those in `names`, or all of them) to the group: each is made
    /// a group alias, bound to the unit its local binding names, or else to the unit that answers `*IDN?` now, then
    /// dropped from the local aliases and from the shared list. A name the group has already (whatever it stands for)
    /// is the group's: the local one, which it hid anyway, is dropped. One the registry refuses stays as it is. The
    /// result per alias: what became of it.
    pub fn adopt_lan(
        &self,
        names: &[String],
        ask: impl Fn(&str) -> Result<String, String>,
        drop_local: impl Fn(&str) -> io::Result<()>,
    ) -> Result<Vec<(String, Result<String, String>)>, Error> {
        self.sync()?;
        let d = self.directory().unwrap_or_default();
        let (mut c, _) = self.connect_registry()?;
        let ids = (self.identities)();
        let dropped = |alias: &str, what: String| drop_local(alias).map(|()| what).map_err(|e| format!("could not be taken out of aliases.conf: {e}"));
        let mut out = Vec::new();
        for (alias, res) in (self.aliases)() {
            if !res.to_ascii_uppercase().starts_with("TCPIP") || !(names.is_empty() || names.iter().any(|n| n.eq_ignore_ascii_case(&alias))) {
                continue;
            }
            if let Some(cl) = d.claim(&alias) {
                let what = if cl.resource.eq_ignore_ascii_case(&res) {
                    format!("{res}: the group has it already")
                } else {
                    format!("the group's {} = {} stands; this computer's (= {res}) was dropped", cl.alias, if cl.resource.is_empty() { "a relay" } else { cl.resource.as_str() })
                };
                out.push((alias.clone(), dropped(&alias, what)));
                continue;
            }
            let id = match ids.iter().find(|(a, _)| a.eq_ignore_ascii_case(&alias)) {
                Some((_, i)) => Some(i.clone()),
                None => ask(&res).ok().and_then(|t| Identity::from_idn(&t).ok()),
            };
            let r = match c.set_group(&alias, &res, &id.as_ref().map(Identity::line).unwrap_or_default()) {
                Ok(()) => dropped(&alias, format!("{res}{}", if id.is_some() { ", bound to its unit" } else { ", not bound (it did not answer *IDN?)" })),
                Err(e) => Err(e.to_string()),
            };
            out.push((alias, r));
        }
        Ok(out)
    }

    /// One pass of the group's LAN aliases following their instruments: an alias bound to a unit whose address does not
    /// answer for three passes is looked for among what discovery finds (`discover` gives hosts), once a minute, by
    /// asking each `*IDN?`; where the unit is found, the alias is moved there at the registry. `reach` says whether an
    /// address answers. The aliases moved, with where.
    pub fn follow_lan(&self, st: &mut LanFollow, reach: &dyn Fn(&str) -> bool, discover: &dyn Fn() -> Vec<String>, ask: &dyn Fn(&str) -> Result<String, String>) -> Vec<(String, String)> {
        const MISSES: u32 = 3;
        let mut moved = Vec::new();
        let Some(d) = self.directory() else { return moved };
        let plain = |res: &str| crate::lan::name::parse(res).is_some_and(|t| matches!(t.proto, crate::lan::name::Proto::Vxi11 { .. } | crate::lan::name::Proto::Hislip { .. }) && crate::lan::name::gpib_of(&t.device).is_none());
        for cl in d.claims.iter().filter(|c| c.owner == super::registry::GROUP_OWNER && !c.identity.is_empty() && plain(&c.resource)) {
            if reach(&cl.resource) {
                st.misses.remove(&cl.alias);
                continue;
            }
            let n = st.misses.entry(cl.alias.clone()).or_insert(0);
            *n += 1;
            if *n < MISSES || st.searched.get(&cl.alias).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                continue;
            }
            st.searched.insert(cl.alias.clone(), std::time::Instant::now());
            let Some(id) = Identity::parse_line(&cl.identity) else { continue };
            // By the parsed name, not by splitting at "::": an IPv6 host has colons of its own.
            let Some(target) = crate::lan::name::parse(&cl.resource) else { continue };
            let bare = |h: &str| h.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
            let old_host = bare(&target.host);
            for host in discover().into_iter().filter(|h| bare(h) != old_host).take(24) {
                let mut t = target.clone();
                t.host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
                let candidate = t.name();
                if ask(&candidate).is_ok_and(|t| id.matches_idn(&t)) {
                    if self.group_set(&cl.alias, &candidate, &cl.identity).is_ok() {
                        // This computer's settings for it go along (each computer keeps its own instruments.conf).
                        let _ = crate::lanunits::settings_follow(&cl.resource, &candidate);
                        moved.push((cl.alias.clone(), candidate));
                        st.misses.remove(&cl.alias);
                    }
                    break;
                }
            }
        }
        moved
    }

    // ---- the group's pictures ----

    /// Bring the copy of the group's pictures in `cache` up to date: the rules, and the picture files that are new or changed; those no
    /// rule has any more go. Whether anything changed.
    pub fn pic_sync(&self, cache: &crate::pictures::Store) -> Result<bool, Error> {
        let (mut c, _) = self.connect_registry()?;
        let (version, conf, files) = c.pic_state()?;
        let versions_path = cache.conf.with_file_name("versions");
        let have: std::collections::HashMap<String, u64> = std::fs::read_to_string(&versions_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once(' ').filter(|(f, _)| *f != "version").and_then(|(f, v)| Some((f.to_string(), v.trim().parse().ok()?))))
            .collect();
        let dir = cache.dir();
        std::fs::create_dir_all(&dir)?;
        let mut changed = false;
        for (file, v) in &files {
            // A name, never a path: the registry is trusted, but what lands on this disk is checked here as for a removal.
            if !crate::pictures::valid_file_name(file) || (have.get(file) == Some(v) && dir.join(file).exists()) {
                continue;
            }
            let bytes = c.pic_get(file)?;
            crate::conffile::write(&dir.join(file), bytes)?;
            changed = true;
        }
        for file in have.keys().filter(|f| !files.iter().any(|(x, _)| x == *f)) {
            if crate::pictures::valid_file_name(file) {
                let _ = std::fs::remove_file(dir.join(file));
            }
            changed = true;
        }
        if std::fs::read_to_string(&cache.conf).unwrap_or_default() != conf {
            crate::conffile::write(&cache.conf, &conf)?;
            changed = true;
        }
        let mut text = format!("version {version}\n");
        for (f, v) in &files {
            text += &format!("{f} {v}\n");
        }
        if std::fs::read_to_string(&versions_path).unwrap_or_default() != text {
            crate::conffile::write(&versions_path, text)?;
        }
        Ok(changed)
    }

    /// Move `local` (this computer's own pictures) to the group, the last rule first so that the order is kept. A pattern the group has a
    /// picture for already is skipped and stays in `local`; one moved is taken out of it. `cache` is the copy of the group's, brought up
    /// to date first. Per rule: its pattern, and why not if it was not moved.
    pub fn pic_adopt(&self, local: &crate::pictures::Store, cache: &crate::pictures::Store) -> Result<Vec<(String, Option<String>)>, Error> {
        self.pic_sync(cache)?;
        let theirs = cache.load();
        let mut out = Vec::new();
        for r in local.load().into_iter().rev() {
            if theirs.iter().any(|x| x.pattern == r.pattern) {
                out.push((r.pattern, Some("the group has a picture for that already".to_string())));
                continue;
            }
            let res = (|| -> Result<(), String> {
                let png = local.read_png(&r.file).map_err(|e| e.to_string())?;
                let orig = local.read_original(&r.file).map_err(|e| e.to_string())?;
                self.pic_save(&r.pattern, r.dark_keep, &png, "", orig.as_ref().map(|o| (o.ext.as_str(), o.bytes.as_slice())), orig.as_ref().and_then(|o| o.edit.as_deref()).unwrap_or(""))
                    .map_err(|e| e.to_string())?;
                local.remove(&r.pattern).map_err(|e| format!("moved, but not taken off this computer: {e}"))?;
                Ok(())
            })();
            out.push((r.pattern, res.err()));
        }
        out.reverse();
        let _ = self.pic_sync(cache);
        Ok(out)
    }

    pub fn pic_save(&self, pattern: &str, dark_keep: bool, png: &[u8], replaces: &str, original: Option<(&str, &[u8])>, edit: &str) -> Result<String, Error> {
        let (mut c, _) = self.connect_registry()?;
        c.pic_save(pattern, dark_keep, png, replaces, original, edit)
    }

    pub fn pic_remove(&self, pattern: &str) -> Result<(), Error> {
        let (mut c, _) = self.connect_registry()?;
        c.pic_remove(pattern)
    }

    /// The original of a picture and how it was edited (extension, bytes, edit parameters); None if it has none.
    pub fn pic_original(&self, file: &str) -> Result<Option<(String, Vec<u8>, String)>, Error> {
        let (mut c, _) = self.connect_registry()?;
        match c.pic_original(file) {
            Ok(o) => Ok(Some(o)),
            Err(Error::Refused(super::registry::Code::NotFound, _)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.keys.public
    }

    fn store_cache(&self, s: &Signed, at: u64) {
        let Some(p) = &self.cache else { return };
        let addrs: cache::Addrs = self.presence.lock().unwrap().iter().filter(|p| !p.addrs.is_empty()).map(|p| (p.key, p.addrs.clone())).collect();
        cache::write(p, s, at, &addrs);
    }

    /// Who was online at the last sync, and where (advisory; from the cache if no registry answered since).
    pub fn presence(&self) -> Vec<Presence> {
        self.presence.lock().unwrap().clone()
    }

    /// Whether `presence` is from a registry answer in this run.
    pub fn presence_is_fresh(&self) -> bool {
        self.presence_fresh.load(Ordering::SeqCst)
    }

    /// The directory, unless it is older than `directory_ttl`.
    pub fn directory(&self) -> Option<Directory> {
        let g = self.dir.lock().unwrap();
        let (d, at) = g.as_ref()?;
        (unix_now().saturating_sub(*at) <= self.conf.directory_ttl).then(|| d.clone())
    }

    pub fn allows(&self, key: &[u8; 32]) -> bool {
        self.directory().is_some_and(|d| d.is_member(key))
    }

    /// End the sessions of members a directory we believe no longer has. A directory too old to
    /// believe ends nothing: a registry that is down is not a revocation. How many were ended.
    pub fn cut_revoked(&self) -> usize {
        let Some(d) = self.directory() else { return 0 };
        let mut n = 0;
        for (_, _, s) in self.sessions.lock().unwrap().iter().filter(|(_, k, _)| !d.is_member(k)) {
            let _ = s.shutdown(std::net::Shutdown::Both);
            n += 1;
        }
        n
    }

    /// What this machine means to share: each alias in `shares.conf` that `aliases.conf` has and
    /// that is an instrument the group can reach: each is a `Relay`, one only this machine's daemon can
    /// reach. A `TCPIP` resource is refused: its alias is the group's (`adopt_lan`).
    pub fn desired(&self) -> (Vec<(String, Kind, String, String)>, Vec<(String, String)>) {
        let list = (self.aliases)();
        let units = (self.identities)();
        let unit_of = |a: &str| units.iter().find(|(x, _)| x.eq_ignore_ascii_case(a)).map(|(_, i)| i.line()).unwrap_or_default();
        let (mut ok, mut no) = (Vec::new(), Vec::new());
        for a in (self.shared)() {
            let Some(res) = lookup(&list, &a) else {
                no.push((a, "not in aliases.conf".to_string()));
                continue;
            };
            if res.to_ascii_uppercase().starts_with("TCPIP") {
                // A LAN instrument belongs to the group, not to the member that happens to list it.
                no.push((a, "a LAN instrument's alias belongs to the group: it is moved there (symbiotic-ctl share group adopt)".to_string()));
            } else if let Some(c) = crate::resource::canonical(&res).filter(|c| crate::resource::target_of(c).is_some()) {
                let id = unit_of(&a);
                ok.push((a, Kind::Relay, c, id));
            } else {
                no.push((a, format!("{res} is not an instrument (the interface level cannot be shared)")));
            }
        }
        (ok, no)
    }

    fn registry_addr(&self) -> Result<SocketAddr, Error> {
        self.conf
            .registry
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::NotFound, "the registry's address does not resolve")))
    }

    fn connect_registry(&self) -> Result<(RegistryClient<TcpStream>, SocketAddr), Error> {
        let (Some(rk), Some(rv)) = (self.conf.registry_key, self.conf.registry_verify) else {
            return Err(Error::Io(io::Error::new(io::ErrorKind::NotFound, "not joined to a group")));
        };
        let s = TcpStream::connect_timeout(&self.registry_addr()?, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        let local = s.local_addr()?;
        Ok((RegistryClient::connect(s, &self.keys, &rk, &rv)?, local))
    }

    /// Where the others reach us: `advertise`, else the address of the interface that reaches the
    /// registry with the port we serve on.
    fn address(&self, local: SocketAddr) -> String {
        if let Some(a) = &self.conf.advertise {
            return a.clone();
        }
        let port = self.conf.listen.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(super::conf::MEMBER_PORT);
        SocketAddr::new(local.ip(), port).to_string()
    }

    /// Tell the registry what we share and that we are here; take its directory. Without sharing
    /// (`share = off`) it only fetches the directory.
    pub fn sync(&self) -> Result<SyncReport, Error> {
        let (mut c, local) = self.connect_registry()?;
        let mut rep = SyncReport { protocol: c.version(), ..SyncReport::default() };
        let cached = self.dir.lock().unwrap().as_ref().map(|(d, _)| d.clone());
        let (mut d, mut signed, mut presence) = c.list_signed(cached.as_ref())?;
        if self.conf.share {
            // A shared GPIB alias whose unit is at another address here now is pointed at it first, so what is claimed is where it is.
            self.relocate_gpib(&mut rep);
            let (want, mut no) = self.desired();
            rep.refused.append(&mut no);
            let me = self.keys.public;
            let mut changed = false;
            for (alias, kind, res, id) in &want {
                let have = d.claim(alias).filter(|c| c.owner == me).is_some_and(|c| &c.alias == alias && c.kind == *kind && &c.resource == res && &c.identity == id);
                if have {
                    continue;
                }
                match c.claim(alias, *kind, res, id) {
                    Ok(()) => {
                        rep.claimed.push(alias.clone());
                        changed = true;
                    }
                    Err(Error::Refused(_, m)) => rep.refused.push((alias.clone(), format!("refused: {m}"))),
                    Err(e) => return Err(e),
                }
            }
            for cl in d.claims.iter().filter(|c| c.owner == me) {
                if !want.iter().any(|(a, ..)| a.eq_ignore_ascii_case(&cl.alias)) {
                    c.release(&cl.alias)?;
                    rep.released.push(cl.alias.clone());
                    changed = true;
                }
            }
            c.heartbeat(&[self.address(local)], &self.absent(&want), &self.states(&want))?;
            changed |= self.follow(&mut c, &d, &want, &mut rep)?;
            if changed {
                (d, signed, presence) = c.list_signed(Some(&d))?;
            }
        }
        *self.presence.lock().unwrap() = presence;
        self.presence_fresh.store(true, Ordering::SeqCst);
        self.keep(d, &signed);
        Ok(rep)
    }

    /// What this member says of each alias it shares: the instrument is there, is not, or is not known (every ten seconds, in the
    /// heartbeat). A USB one is read from the descriptors, which asks the instrument nothing; the rest are the daemon's to say
    /// (`check`): a GPIB one by its scan. A LAN instrument belongs to the group, and each member looks for itself.
    fn states(&self, want: &[(String, Kind, String, String)]) -> Vec<AliasState> {
        let usb = (self.usb)();
        want.iter()
            .filter(|(_, k, ..)| *k == Kind::Relay)
            .map(|(a, _, res, _)| {
                let (here, age) = match crate::resource::parse_usb(res) {
                    Some(u) => match &usb {
                        Some(present) => (if present.iter().any(|p| p.vid == u.vid && p.pid == u.pid && p.serial.eq_ignore_ascii_case(&u.serial)) { Here::Yes } else { Here::No }, 0),
                        None => (Here::Unknown, 0),
                    },
                    None => (self.check)(res),
                };
                AliasState { alias: a.clone(), here, age }
            })
            .collect()
    }

    /// The aliases of this member's whose USB instrument is not plugged in here: told to the registry in the heartbeat, so that
    /// a member that has the same unit may be given the alias. Only a USB instrument can be told without a command; where the
    /// list of USB instruments cannot be had, nothing is said.
    fn absent(&self, want: &[(String, Kind, String, String)]) -> Vec<String> {
        let usb = (self.usb)();
        let gpib = (self.gpib)();
        want.iter()
            .filter(|(_, k, ..)| *k == Kind::Relay)
            .filter(|(_, _, res, id)| {
                if let Some(u) = crate::resource::parse_usb(res) {
                    return usb.as_ref().is_some_and(|present| !present.iter().any(|p| p.vid == u.vid && p.pid == u.pid && p.serial.eq_ignore_ascii_case(&u.serial)));
                }
                // A bound GPIB instrument: gone only if nothing listens at its address and it is not among the instruments here
                // (both, so that one the user keeps from `*IDN?` is not taken for gone).
                let (Some(_), Some(id), Some(units)) = (crate::resource::parse(res), Identity::parse_line(id), gpib.as_ref()) else { return false };
                (self.check)(res).0 == Here::No && self.gpib_unit(units, &id).is_none()
            })
            .map(|(a, ..)| a.clone())
            .collect()
    }

    /// With `follow` on: an alias of the group that is bound to a unit that is here -- a USB one plugged in, a GPIB one among the
    /// instruments the daemon's scans found -- and is another member's, is asked for, at this machine's resource for it. The
    /// registry gives it only if the owner no longer has the unit (it said so, or is offline); if it does, the alias, its
    /// binding and its sharing are written here too. Whether the registry changed.
    fn follow(&self, c: &mut RegistryClient<TcpStream>, d: &Directory, want: &[(String, Kind, String, String)], rep: &mut SyncReport) -> Result<bool, Error> {
        if !self.conf.follow {
            return Ok(false);
        }
        let (usb, gpib) = ((self.usb)(), (self.gpib)());
        let me = self.keys.public;
        let mut changed = false;
        for cl in &d.claims {
            if cl.owner == me || cl.kind != Kind::Relay || cl.identity.is_empty() || want.iter().any(|(a, ..)| a.eq_ignore_ascii_case(&cl.alias)) {
                continue;
            }
            let Some(id) = Identity::parse_line(&cl.identity) else { continue };
            // Where the unit is here, if it is: a USB name is the same on every machine (its serial is in it); a GPIB one is this
            // machine's board and address, found among the instruments here by who they said they are.
            let here = if crate::resource::parse_usb(&cl.resource).is_some() {
                id.usb.as_ref().filter(|u| usb.as_ref().is_some_and(|p| p.contains(u))).map(|_| cl.resource.clone())
            } else if crate::resource::parse(&cl.resource).is_some() {
                gpib.as_ref().and_then(|units| self.gpib_unit(units, &id))
            } else {
                None
            };
            let Some(here) = here else { continue };
            match c.claim(&cl.alias, Kind::Relay, &here, &cl.identity) {
                Ok(()) => {
                    match (self.adopt)(&cl.alias, &here, &id) {
                        Ok(()) => rep.followed.push(cl.alias.clone()),
                        Err(e) => rep.refused.push((cl.alias.clone(), format!("taken over at the registry, but this computer's configuration could not be written: {e}"))),
                    }
                    changed = true;
                }
                // The owner still has it: not ours yet.
                Err(Error::Refused(super::registry::Code::AlreadyOwned, _)) => {}
                Err(Error::Refused(_, m)) => rep.refused.push((cl.alias.clone(), format!("refused: {m}"))),
                Err(e) => return Err(e),
            }
        }
        Ok(changed)
    }

    fn keep(&self, d: Directory, signed: &Signed) {
        let now = unix_now();
        let mut g = self.dir.lock().unwrap();
        if let Ok(d) = accept_newer(g.as_ref().map(|(d, _)| d), d) {
            *g = Some((d, now));
            self.store_cache(signed, now);
        }
        *self.last_refresh.lock().unwrap() = now;
    }

    /// `sync`, unless the directory is younger than `secs`. A registry that cannot be reached is
    /// not an error here: what is cached is used.
    pub fn refresh_if_older(&self, secs: u64) {
        let now = unix_now();
        {
            let mut l = self.last_refresh.lock().unwrap();
            if now.saturating_sub(*l) < secs {
                return;
            }
            // An attempt counts, so a registry that is down is tried every `secs`, not at every open.
            *l = now;
        }
        let _ = self.sync();
    }

    /// Ask the registry again because a key we do not know came to the door, at most every few
    /// seconds.
    /// Only the directory is fetched: an unauthenticated knock makes us claim, release or report
    /// nothing, and does not put off the sync loop's own work.
    fn refresh_for_stranger(&self) {
        let now = unix_now();
        {
            let mut l = self.stranger_refresh.lock().unwrap();
            if now.saturating_sub(*l) < REFRESH_FOR_STRANGERS {
                return;
            }
            *l = now;
        }
        // A registry that cannot be reached: the cached directory stands.
        let _ = (|| -> Result<(), Error> {
            let (mut c, _) = self.connect_registry()?;
            let cached = self.dir.lock().unwrap().as_ref().map(|(d, _)| d.clone());
            let (d, signed, _) = c.list_signed(cached.as_ref())?;
            let mut g = self.dir.lock().unwrap();
            if let Ok(d) = accept_newer(g.as_ref().map(|(d, _)| d), d) {
                *g = Some((d, now));
                self.store_cache(&signed, now);
            }
            Ok(())
        })();
    }

    /// Whether `alias` is one we serve to the group right now, and what it is: shared in
    /// `shares.conf`, in `aliases.conf`, claimed by us in the directory as a `Relay`.
    pub fn open_for_peer(&self, alias: &str) -> Result<Opened, String> {
        const NOPE: &str = "no such shared alias";
        let d = self.directory().ok_or(NOPE)?;
        let claim = d.claim(alias).filter(|c| c.owner == self.keys.public && c.kind == Kind::Relay).ok_or(NOPE)?;
        let (want, _) = self.desired();
        if !want.iter().any(|(a, k, ..)| a.eq_ignore_ascii_case(&claim.alias) && *k == Kind::Relay) {
            return Err(NOPE.to_string());
        }
        let resource = lookup(&(self.aliases)(), &claim.alias).and_then(|r| crate::resource::canonical(&r)).ok_or(NOPE)?;
        let target: Target = crate::resource::target_of(&resource).ok_or(NOPE)?;
        // A USBTMC instrument's descriptors are read here, where the instrument is.
        let usb = match (target, crate::resource::parse_usb(&resource)) {
            (Target::Usb(_), Some(u)) => crate::usbtmc::list().ok().and_then(|l| {
                l.into_iter().find(|f| f.vid == u.vid && f.pid == u.pid && f.serial == u.serial && u.iface.is_none_or(|i| i == f.iface)).map(|f| UsbInfo {
                    iface: f.iface,
                    usb488: f.usb488,
                    manufacturer: f.manufacturer,
                    product: f.product,
                })
            }),
            _ => None,
        };
        Ok(Opened { target, resource, usb, header: crate::ipc::HEADER_U32 })
    }

    /// Open the shared alias `alias` that another member serves: a channel bound to the instrument,
    /// and what the owner says it is. Errors say what is wrong in words (no such alias, owner
    /// offline, refused).
    pub fn connect_peer(&self, alias: &str) -> io::Result<(chan::Channel<TcpStream>, Opened)> {
        let other = |m: String| io::Error::new(io::ErrorKind::NotFound, m);
        let d = self.directory().ok_or_else(|| other("no directory (the registry has not been reached, or the cached one is too old)".into()))?;
        let claim = d.claim(alias).ok_or_else(|| other(format!("{alias} is not a shared alias")))?;
        if claim.kind != Kind::Relay {
            return Err(other(format!("{alias} is a LAN instrument: open {} directly", claim.resource)));
        }
        let owner = claim.owner;
        let addrs = self.presence.lock().unwrap().iter().find(|p| p.key == owner).map(|p| (p.online, p.addrs.clone()));
        // The registry's word that the owner is offline counts; the addresses kept from an earlier
        // run (no registry reached since) are tried, since they are all there is.
        let fresh = self.presence_fresh.load(Ordering::SeqCst);
        let addrs = match addrs {
            Some((online, a)) if !a.is_empty() && (online || !fresh) => a,
            _ => return Err(other(format!("the machine that shares {alias} is offline"))),
        };
        let mut last = other("no address".into());
        for a in addrs {
            let r = a.to_socket_addrs().and_then(|mut i| i.next().ok_or_else(|| other("no address".into()))).and_then(|sa| {
                let s = TcpStream::connect_timeout(&sa, Duration::from_secs(3))?;
                s.set_nodelay(true).ok();
                // An owner that goes without a word ends the session instead of a read without a timeout.
                super::keep_alive(&s)?;
                s.set_read_timeout(Some(Duration::from_secs(10)))?;
                let mut ch = chan::connect(s, &self.keys, &owner)?;
                let opened = peer::open(&mut ch, &claim.alias)??;
                ch.stream().set_read_timeout(None)?;
                Ok((ch, opened))
            });
            match r {
                Ok(x) => return Ok(x),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Serve the group on `l`, on threads of their own, until the process ends. `host` is the
    /// daemon; returns the address it listens on.
    /// `used`: each session another member had, when it ends, with who it was (share/usage.rs).
    pub fn listen(self: &Arc<Self>, l: TcpListener, host: Arc<dyn PeerHost>, used: Arc<dyn Fn(&super::usage::Record) + Send + Sync>) -> io::Result<SocketAddr> {
        let addr = l.local_addr()?;
        let (me, door) = (self.clone(), Arc::new(Door::default()));
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(c) = c else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                // Shaking hands is in the door's room (door.rs); only a peer that has finished takes one of the `MAX_PEERS`.
                let Some(place) = c.peer_addr().ok().and_then(|a| door.enter(a.ip())) else { continue };
                let (me, host, used) = (me.clone(), host.clone(), used.clone());
                std::thread::spawn(move || {
                    let from = c.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    let ch = me.handshake(c);
                    drop(place);
                    let Ok(mut ch) = ch else { return };
                    if me.peers.fetch_add(1, Ordering::SeqCst) < MAX_PEERS {
                        let who = *ch.peer();
                        let note = |u: &peer::Used| {
                            // The member's name as the directory has it now; its key says who it is either way.
                            let d = me.directory();
                            let member = d.as_ref().and_then(|d| d.member(&who).map(|m| m.name.clone())).unwrap_or_else(|| "?".into());
                            // The alias as the group spells it, not as the user typed it.
                            let alias = d.as_ref().and_then(|d| d.claim(&u.alias).map(|c| c.alias.clone())).unwrap_or_else(|| u.alias.clone());
                            used(&super::usage::Record {
                                began: u.began,
                                secs: u.secs,
                                alias,
                                member,
                                key: super::codec::hex(&who[..4]),
                                from: from.clone(),
                                requests: u.requests,
                                refused: u.refused,
                            });
                        };
                        // Known while it lasts, so that a revocation can end it (`cut_revoked`).
                        let id = me.next_session.fetch_add(1, Ordering::SeqCst);
                        if let Ok(s) = ch.stream().try_clone() {
                            me.sessions.lock().unwrap().push((id, who, s));
                        }
                        let name = me.directory().and_then(|d| d.member(&who).map(|m| m.name.clone())).unwrap_or_else(|| "?".into());
                        let desc = format!("member {name} ({}) from {from}", super::codec::hex(&who[..4]));
                        let _ = peer::serve(&mut ch, host.as_ref(), &desc, &|a| me.open_for_peer(a), &note);
                        me.sessions.lock().unwrap().retain(|(i, _, _)| *i != id);
                    }
                    me.peers.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(addr)
    }

    fn handshake(&self, c: TcpStream) -> io::Result<Channel<TcpStream>> {
        c.set_nodelay(true).ok();
        // A user that goes without a word ends its session, and what it held there (super::keep_alive).
        super::keep_alive(&c)?;
        c.set_read_timeout(Some(HANDSHAKE_WITHIN))?;
        c.set_write_timeout(Some(Duration::from_secs(30)))?;
        let watch = c.try_clone()?;
        let mut ch = door::within(&watch, HANDSHAKE_WITHIN, || {
            chan::accept(c, &self.keys, |k| {
                if self.allows(k) {
                    return true;
                }
                self.refresh_for_stranger();
                self.allows(k)
            })
        })?;
        // A user may sit idle on an open session for as long as it likes, as long as its machine is
        // there: keepalive says when it is not.
        ch.stream().set_read_timeout(None)?;
        Ok(ch)
    }

    /// The background work of a machine that shares: keep the registry told and the directory
    /// fresh, every `HEARTBEAT` seconds. A registry that cannot be reached is not an error: the
    /// cached directory keeps serving.
    pub fn run_sync_loop(self: &Arc<Self>, log: impl Fn(&str) + Send + 'static) {
        let me = self.clone();
        std::thread::spawn(move || {
            let mut failing = false;
            // What was refused last time: said again only when it changes, not every ten seconds.
            let mut said: Vec<(String, String)> = Vec::new();
            loop {
                match me.sync() {
                    Ok(r) => {
                        if failing {
                            log("registry reachable again");
                            failing = false;
                        }
                        let cut = me.cut_revoked();
                        if cut > 0 {
                            log(&format!("ended {cut} session(s) of a machine no longer in the group"));
                        }
                        for a in &r.claimed {
                            log(&format!("sharing {a}"));
                        }
                        for a in &r.released {
                            log(&format!("no longer sharing {a}"));
                        }
                        for a in &r.followed {
                            log(&format!("{a} followed its instrument here"));
                        }
                        for (a, why) in &r.refused {
                            if !said.contains(&(a.clone(), why.clone())) {
                                log(&format!("cannot share {a}: {why}"));
                            }
                        }
                        said = r.refused;
                    }
                    Err(e) => {
                        if !failing {
                            log(&format!("registry: {e}"));
                        }
                        failing = true;
                    }
                }
                std::thread::sleep(Duration::from_secs(HEARTBEAT));
            }
        });
    }
}
