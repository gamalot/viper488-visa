//! Where an alias is kept, for everything that writes one -- the configuration window, `symbiotic-ctl alias`. A network
//! instrument (a `TCPIP` resource, one behind a LAN/GPIB gateway too) is the same instrument on every computer, so on a
//! computer that has joined a group its alias is the group's (at the registry, any member may change it); everything
//! else, and everything on a computer in no group, is `aliases.conf`'s. The user sees one thing, an alias.

use super::conf::{self, ShareConf};
use super::dir::{Directory, Kind};
use super::keys;
use super::member::Member;
use super::registry::GROUP_OWNER;
use crate::identity::Identity;
use std::time::Duration;

/// A group alias for a network instrument: no computer owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupAlias {
    pub alias: String,
    pub resource: String,
    /// The unit it is bound to, if it is.
    pub identity: Option<Identity>,
}

/// Is `resource` a network instrument's (any spelling)?
pub fn is_lan(resource: &str) -> bool {
    crate::resource::canonical(resource).unwrap_or_else(|| resource.to_string()).to_ascii_uppercase().starts_with("TCPIP")
}

/// A claim that is a network instrument's group alias: one with no owner, or one a member of an earlier build registered for a LAN
/// instrument (`Direct`), which the group treats the same.
pub(crate) fn is_group_lan(c: &super::dir::Claim) -> bool {
    (c.owner == GROUP_OWNER || c.kind == Kind::Direct) && is_lan(&c.resource)
}

/// This computer as a member that only asks (it serves nothing); None when it has joined no group.
pub fn member() -> Option<Result<Member, String>> {
    let mut c = ShareConf::load();
    if !c.joined() {
        return None;
    }
    c.share = false;
    Some((|| {
        let key = crate::paths::user_file("share.key").ok_or("no place for share.key")?;
        let k = keys::load_or_create(&key, false).map_err(|e| e.to_string())?;
        Ok(Member::new(c, k.noise, crate::paths::user_file("share.dir"), crate::alias::load, conf::load_shares))
    })())
}

/// The group's directory, as fresh as `REFRESH_AFTER` allows (the registry is asked when the copy is older). None: no group, or no
/// directory to believe.
fn directory(m: &Member) -> Option<Directory> {
    m.refresh_if_older(super::lookup::REFRESH_AFTER);
    m.directory()
}

/// The group's aliases for network instruments; none on a computer in no group.
pub fn group_lan() -> Vec<GroupAlias> {
    let Some(Ok(m)) = member() else { return Vec::new() };
    let Some(d) = directory(&m) else { return Vec::new() };
    d.claims
        .iter()
        .filter(|c| is_group_lan(c))
        .map(|c| GroupAlias { alias: c.alias.clone(), resource: c.resource.clone(), identity: Identity::parse_line(&c.identity) })
        .collect()
}

/// The group, when `alias` (by name) or `resource` is the group's business here: a computer that has joined a group and a network
/// instrument. Its member and directory, so that nothing is asked twice.
fn group_for(resource: Option<&str>, alias: &str) -> Result<Option<(Member, Directory)>, String> {
    // Another kind of instrument's alias is this computer's whatever the group says.
    if resource.is_some_and(|r| !is_lan(r)) {
        return Ok(None);
    }
    let Some(m) = member() else { return Ok(None) };
    let m = m?;
    let Some(d) = directory(&m) else {
        // An alias kept here is this computer's to change while the registry is away.
        if resource.is_none() && crate::alias::resolve(alias).is_some_and(|(_, r)| !is_lan(&r)) {
            return Ok(None);
        }
        return Err("the group's registry cannot be reached, and the copy of its directory is too old to go by".to_string());
    };
    let mine = d.claim(alias).is_some_and(is_group_lan) || resource.is_some_and(is_lan);
    Ok(mine.then_some((m, d)))
}

/// The alias of `resource`, wherever it is kept: the group's for a network instrument on a computer in a group, else `aliases.conf`'s.
pub fn alias_of(resource: &str) -> Option<String> {
    let c = crate::resource::canonical(resource)?;
    if is_lan(&c) {
        if let Some(a) = group_lan().into_iter().find(|g| g.resource.eq_ignore_ascii_case(&c)) {
            return Some(a.alias);
        }
    }
    crate::alias::alias_of(&c)
}

/// Make `alias` stand for `resource`, or with None take it away, where it is kept. A name that is another instrument's alias already is
/// refused, not moved. The aliases of network instruments that are still kept here are moved to the group first (`move_local`).
pub fn set(alias: &str, resource: Option<&str>) -> Result<(), String> {
    let alias = alias.trim();
    crate::alias::check_name(alias).map_err(str::to_string)?;
    let canonical = match resource {
        Some(r) => Some(crate::resource::canonical(r.trim()).ok_or_else(|| format!("{:?} is not a resource name", r.trim()))?),
        None => None,
    };
    // Only a network instrument's alias waits for the move: another's is this computer's, registry or not.
    if let Some(Err(e)) = move_local() {
        if canonical.as_deref().is_none_or(is_lan) {
            return Err(e);
        }
    }
    match group_for(canonical.as_deref(), alias)? {
        Some((m, d)) => {
            let (Some(r), had) = (canonical, d.claim(alias)) else {
                // Taken away: the group's alias, and a stray line of the same name here.
                m.group_clear(alias).map_err(|e| e.to_string())?;
                let _ = crate::alias::set(alias, None);
                let _ = m.sync();
                return Ok(());
            };
            if let Some(cl) = had.filter(|c| !c.resource.eq_ignore_ascii_case(&r)) {
                return Err(format!("{} is already the alias of {}", cl.alias, if cl.resource.is_empty() { "another computer's instrument" } else { cl.resource.as_str() }));
            }
            if let Some((_, other)) = crate::alias::resolve(alias).filter(|(_, o)| !is_lan(o)) {
                return Err(format!("{alias} is already the alias of {other}"));
            }
            // The same instrument at the same address keeps its binding.
            let kept = had.map(|c| c.identity.clone()).unwrap_or_default();
            m.group_set(alias, &r, &kept).map_err(|e| e.to_string())?;
            let _ = m.sync();
            Ok(())
        }
        None => {
            if let (Some(r), Some((_, other))) = (&canonical, crate::alias::resolve(alias)) {
                if !other.eq_ignore_ascii_case(r) {
                    return Err(format!("{alias} is already the alias of {other}"));
                }
            }
            if let Some(Ok(m)) = member() {
                // In a group, a name the group has for another computer's instrument is not taken here as well.
                let me = m.public_key();
                if let (Some(_), Some(cl)) = (&canonical, m.directory().and_then(|d| d.claim(alias).filter(|c| c.owner != me).cloned())) {
                    return Err(format!("{} is already the group's alias of {}", cl.alias, if cl.resource.is_empty() { "another computer's instrument" } else { cl.resource.as_str() }));
                }
            }
            crate::alias::set(alias, canonical.as_deref()).map_err(|e| e.to_string())
        }
    }
}

/// Bind `alias` to the unit at its resource (it is asked `*IDN?` once), wherever the alias is kept.
pub fn bind(alias: &str) -> Result<Identity, String> {
    match group_for(None, alias)? {
        Some((m, _)) => {
            let id = m.group_bind(alias, |r| crate::identity::ask_idn(r, Duration::from_millis(3000)))?;
            let _ = m.sync();
            Ok(id)
        }
        None => crate::identity::bind(alias),
    }
}

/// Point `alias` at `resource` because the unit it is bound to answers there now: the binding stays. The group's alias for a network
/// instrument moves at the registry, with its unit; any other is `aliases.conf`'s (`alias::follow`).
pub fn follow(alias: &str, resource: &str) -> Result<(), String> {
    let r = crate::resource::canonical(resource.trim()).ok_or_else(|| format!("{:?} is not a resource name", resource.trim()))?;
    if is_lan(&r) {
        if let Some(Ok(m)) = member() {
            if let Some(cl) = directory(&m).and_then(|d| d.claim(alias).filter(|c| is_group_lan(c)).cloned()) {
                m.group_set(&cl.alias, &r, &cl.identity).map_err(|e| e.to_string())?;
                let _ = m.sync();
                return Ok(());
            }
        }
    }
    crate::alias::follow(alias, &r).map_err(|e| e.to_string())
}

/// `alias` is a name for its resource again, not for one unit.
pub fn unbind(alias: &str) -> Result<(), String> {
    match group_for(None, alias)? {
        Some((m, d)) => {
            let res = d.claim(alias).map(|c| c.resource.clone()).ok_or("no such alias")?;
            m.group_set(alias, &res, "").map_err(|e| e.to_string())?;
            let _ = m.sync();
            Ok(())
        }
        None => crate::identity::set(alias, None).map_err(|e| e.to_string()),
    }
}

/// Move the aliases of network instruments still kept in `aliases.conf` to the group (`Member::adopt_lan`). None: nothing to do (no
/// group, or no such alias here); else what became of each, or why the group could not be asked.
pub fn move_local() -> Option<Result<Vec<(String, Result<String, String>)>, String>> {
    if !crate::alias::load().iter().any(|(_, r)| is_lan(r)) {
        return None;
    }
    let m = match member()? {
        Ok(m) => m,
        Err(e) => return Some(Err(e)),
    };
    Some(
        m.adopt_lan(&[], |r| crate::identity::ask_idn(r, Duration::from_millis(2000)), |a| {
            crate::alias::set(a, None)?;
            conf::set_shared(a, false)
        })
        .map_err(|e| format!("the aliases of network instruments could not be moved to the group: {e}")),
    )
}
