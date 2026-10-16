//! What the VISA library asks of the sharing group when it is given a name: is this a shared alias, and whose? It reads
//! files (`share.conf`, `share.key`, the cached directory) and opens no connection, so a machine that has joined no
//! group pays one small read per call.
//!
//! The group's names come first: an alias the group shares means the same instrument on every machine, so a private
//! alias of the same name is ignored.

use super::{cache, conf::ShareConf, keys};
use crate::share::dir::{Claim, Kind};
use std::path::PathBuf;

/// A cached directory older than this (seconds) is fetched again before it is looked in, and the
/// daemon tries at most this often.
pub const REFRESH_AFTER: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// This machine owns it: `aliases.conf` says what it is, and it opens as any local instrument.
    Mine { alias: String },
    /// A LAN instrument: open its `TCPIP` resource name here.
    Direct { alias: String, resource: String },
    /// Another machine's GPIB, USB or serial instrument, reached through its owner's daemon.
    /// `resource` is the owner's name for it.
    Remote { alias: String, resource: String },
    /// The group has it, but the directory is older than `directory_ttl` and is not believed.
    Stale { alias: String },
}

struct Group {
    dir: crate::share::dir::Directory,
    fresh: bool,
    me: [u8; 32],
}

fn dir_path() -> Option<PathBuf> {
    std::env::var_os("SYMBIOTIC_SHARE_DIR").map(PathBuf::from).or_else(|| crate::paths::user_file("share.dir"))
}

fn key_path() -> Option<PathBuf> {
    std::env::var_os("SYMBIOTIC_SHARE_KEY").map(PathBuf::from).or_else(|| crate::paths::user_file("share.key"))
}

fn group() -> Option<Group> {
    let c = ShareConf::load();
    if !c.joined() {
        return None;
    }
    let cached = cache::read(&dir_path()?, &c.registry_verify?)?;
    let me = keys::load(&key_path()?)?.noise.public;
    let fresh = super::regserver::unix_now().saturating_sub(cached.at) <= c.directory_ttl;
    Some(Group { dir: cached.dir, fresh, me })
}

/// Whether this machine is in a group whose directory should be fetched again now: there is
/// none cached, or it is older than `REFRESH_AFTER`. A machine in no group never does.
pub fn needs_refresh() -> bool {
    let c = ShareConf::load();
    if !c.joined() {
        return false;
    }
    let (Some(p), Some(v)) = (dir_path(), c.registry_verify) else { return false };
    match cache::read(&p, &v) {
        Some(d) => super::regserver::unix_now().saturating_sub(d.at) > REFRESH_AFTER,
        None => true,
    }
}

fn kind_of(g: &Group, c: &Claim) -> Hit {
    let alias = c.alias.clone();
    if !g.fresh {
        return Hit::Stale { alias };
    }
    if c.owner == g.me {
        Hit::Mine { alias }
    } else if c.kind == Kind::Direct {
        Hit::Direct { alias, resource: c.resource.clone() }
    } else {
        Hit::Remote { alias, resource: c.resource.clone() }
    }
}

/// `name` as a shared alias, if the group has one by that name (any case).
pub fn lookup(name: &str) -> Option<Hit> {
    let g = group()?;
    let c = g.dir.claim(name)?;
    Some(kind_of(&g, c))
}

/// One of the group's aliases, for `viFindRsrc`.
pub struct Listed {
    pub alias: String,
    /// The owner's resource name for it.
    pub resource: String,
    /// This machine's own. Its GPIB, USB and serial ones are found by the machine itself; a LAN one
    /// may not be (it need not be in `lan.conf`), so it is listed too, for the machine to list what
    /// it shares as every other machine does.
    pub mine: bool,
    pub direct: bool,
}

/// The group's aliases worth listing: the other machines' (LAN ones included, so a program that lists them
/// can open them by their alias here too) and this machine's own LAN ones.
pub fn listed() -> Vec<Listed> {
    let Some(g) = group().filter(|g| g.fresh) else { return Vec::new() };
    g.dir
        .claims
        .iter()
        .filter(|c| c.owner != g.me || c.kind == Kind::Direct)
        .map(|c| Listed { alias: c.alias.clone(), resource: c.resource.clone(), mine: c.owner == g.me, direct: c.kind == Kind::Direct })
        .collect()
}

/// The group's aliases for network instruments, (alias, resource): what this computer calls them, as `aliases.conf` is
/// for the rest. None in no group, or with a directory too old.
pub fn lan_aliases() -> Vec<(String, String)> {
    let Some(g) = group().filter(|g| g.fresh) else { return Vec::new() };
    g.dir
        .claims
        .iter()
        .filter(|c| super::names::is_group_lan(c))
        .map(|c| (c.alias.clone(), c.resource.clone()))
        .collect()
}
