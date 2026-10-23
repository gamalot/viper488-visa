//! The sharing page's commands: this computer's place in a group of computers that share aliases, the same
//! `symbiotic-core` functions as `symbiotic-ctl share`. Asking the registry takes a network round trip, so these run
//! off the window's thread, and the page asks only when it is shown or refreshed.

use serde::Serialize;
use symbiotic_core::share::{conf, dir::Kind, keys, member::Member, registry::{GROUP_OWNER, Here, PairingCode}};
use symbiotic_core::{alias, autostart, resource};

use crate::blocking;

#[derive(Serialize)]
pub struct Computer {
    name: String,
    /// The first bytes of its key, to tell two computers of one name apart.
    key: String,
    mine: bool,
    /// None: the registry could not be asked, so nobody knows.
    online: Option<bool>,
    addresses: Vec<String>,
}

#[derive(Serialize)]
pub struct Shared {
    alias: String,
    owner: String,
    mine: bool,
    /// "relay" (through its owner) or "direct" (a LAN instrument everyone opens themselves).
    kind: String,
    resource: String,
    online: Option<bool>,
    /// A group alias: a LAN instrument that belongs to the group, with no owner.
    group: bool,
    /// The unit it is bound to (manufacturer | model | serial), or empty: a bound alias follows its unit to another address.
    identity: String,
    /// What its owner says of the instrument (in its heartbeat): None when it does not know, or says nothing.
    present: Option<bool>,
    /// How old that word is, seconds.
    age: Option<u32>,
}

#[derive(Serialize)]
pub struct MyAlias {
    alias: String,
    resource: String,
    /// Ticked in shares.conf.
    shared: bool,
    /// The group has it as this computer's.
    claimed: bool,
    /// Why a ticked alias is not shared, if it is not.
    problem: Option<String>,
}

#[derive(Serialize)]
pub struct ShareStatus {
    joined: bool,
    sharing: bool,
    name: String,
    registry: String,
    listen: String,
    /// "on", "off" or "unsupported".
    autostart: String,
    autostart_why: Option<String>,
    /// Take an alias over when its USB instrument comes here (share.conf, follow).
    follow: bool,
    /// This computer's daemon runs the registry (registry_listen in share.conf).
    registry_here: bool,
    /// Why the registry could not be asked, if it could not (what is shown then is the last it said).
    error: Option<String>,
    version: Option<u64>,
    computers: Vec<Computer>,
    others: Vec<Shared>,
    mine: Vec<MyAlias>,
    /// Private aliases the group's names hide on this computer.
    hidden_aliases: Vec<String>,
    /// Who used this computer's shared instruments, newest first (symbiotic_core::share::usage).
    used: Vec<Used>,
}

#[derive(Serialize)]
pub struct Used {
    /// When the session began, Unix seconds.
    began: u64,
    secs: f64,
    alias: String,
    member: String,
    key: String,
    from: String,
    requests: u64,
    refused: u64,
}

fn short(k: &[u8; 32]) -> String {
    symbiotic_core::share::codec::hex(&k[..4])
}

fn key_path() -> Result<std::path::PathBuf, String> {
    symbiotic_core::paths::user_file("share.key").ok_or_else(|| "no place for share.key".to_string())
}

/// Why `res` (what an alias stands for) cannot be shared, if it cannot.
fn problem(res: &str) -> Option<String> {
    let c = resource::canonical(res)?;
    if c.to_ascii_uppercase().starts_with("TCPIP") || resource::target_of(&c).is_some() {
        None
    } else {
        Some("interface".to_string())
    }
}

#[tauri::command]
pub async fn share_status() -> Result<ShareStatus, String> {
    blocking(|| {
        let c = conf::ShareConf::load();
        let (autostart, autostart_why) = match autostart::status() {
            autostart::State::On => ("on", None),
            autostart::State::Off => ("off", None),
            autostart::State::Unsupported(w) => ("unsupported", Some(w)),
        };
        let aliases = alias::load();
        let shares = conf::load_shares();
        let mut s = ShareStatus {
            joined: c.joined(),
            sharing: c.share,
            name: c.name.clone(),
            registry: c.registry.clone(),
            listen: c.listen.clone(),
            autostart: autostart.to_string(),
            autostart_why,
            follow: c.follow,
            registry_here: c.registry_listen.is_some(),
            error: None,
            version: None,
            computers: Vec::new(),
            others: Vec::new(),
            mine: Vec::new(),
            hidden_aliases: Vec::new(),
            used: symbiotic_core::share::usage::read_from(&symbiotic_core::share::usage::path(), 50)
                .into_iter()
                .rev()
                .map(|r| Used { began: r.began, secs: r.secs, alias: r.alias, member: r.member, key: r.key, from: r.from, requests: r.requests, refused: r.refused })
                .collect(),
        };
        let mut me = None;
        let mut dir = None;
        if s.joined {
            // Look at the group without taking part: a Member that shares nothing, which only asks.
            let mut look = c.clone();
            look.share = false;
            let keys = keys::load_or_create(&key_path()?, false).map_err(|e| e.to_string())?;
            let cache = symbiotic_core::paths::user_file("share.dir");
            let m = Member::new(look, keys.noise, cache, alias::load, conf::load_shares);
            me = Some(m.public_key());
            if let Err(e) = m.sync() {
                s.error = Some(e.to_string());
            }
            dir = m.directory();
            let fresh = m.presence_is_fresh();
            let presence = m.presence();
            if let Some(d) = &dir {
                s.version = Some(d.version);
                for mem in &d.members {
                    let p = presence.iter().find(|p| p.key == mem.key);
                    s.computers.push(Computer {
                        name: mem.name.clone(),
                        key: short(&mem.key),
                        mine: Some(mem.key) == me,
                        online: fresh.then(|| p.is_some_and(|p| p.online)),
                        addresses: p.map(|p| p.addrs.clone()).unwrap_or_default(),
                    });
                }
                for cl in &d.claims {
                    let owner = d.member(&cl.owner);
                    let online = presence.iter().find(|p| p.key == cl.owner).map(|p| p.online);
                    let said = presence.iter().find(|p| p.key == cl.owner).and_then(|p| p.states.iter().find(|x| x.alias.eq_ignore_ascii_case(&cl.alias)));
                    s.others.push(Shared {
                        // A network instrument's alias is the group's, owned or not (an earlier build's member registered its own Direct ones).
                        group: cl.owner == GROUP_OWNER || cl.kind == Kind::Direct,
                        identity: cl.identity.clone(),
                        present: said.and_then(|x| match x.here {
                            Here::Yes => Some(true),
                            Here::No => Some(false),
                            Here::Unknown => None,
                        }),
                        age: said.map(|x| x.age),
                        alias: cl.alias.clone(),
                        owner: owner.map(|m| m.name.clone()).unwrap_or_default(),
                        mine: Some(cl.owner) == me,
                        kind: if cl.kind == Kind::Direct { "direct" } else { "relay" }.to_string(),
                        resource: cl.resource.clone(),
                        online: fresh.then(|| online.unwrap_or(false)),
                    });
                }
            } else if s.error.is_none() {
                s.error = Some("no directory yet".to_string());
            }
        }
        for (a, r) in &aliases {
            let shared = shares.iter().any(|x| x.eq_ignore_ascii_case(a));
            let claimed = dir.as_ref().and_then(|d| d.claim(a)).is_some_and(|cl| Some(cl.owner) == me);
            s.mine.push(MyAlias { alias: a.clone(), resource: r.clone(), shared, claimed, problem: if shared { problem(r) } else { None } });
        }
        if let Some(d) = &dir {
            // A private alias a group name hides: any alias of ours the group has, owned by another computer.
            s.hidden_aliases = aliases.iter().filter(|(a, _)| d.claim(a).is_some_and(|cl| Some(cl.owner) != me)).map(|(a, _)| a.clone()).collect();
        }
        Ok(s)
    })
    .await
}

#[derive(Serialize)]
pub struct Probed {
    alias: String,
    /// None: the instrument answers its owner's daemon; else why not.
    error: Option<String>,
}

/// Ask each alias's owner's daemon whether the instrument is there (a PROBE: nothing is written to it), as
/// `symbiotic-ctl share ping` does. The directory says whether a computer is up, not whether what it shares is on.
#[tauri::command]
pub async fn share_probe(aliases: Vec<String>) -> Result<Vec<Probed>, String> {
    blocking(move || {
        let mut look = conf::ShareConf::load();
        look.share = false;
        let keys = keys::load_or_create(&key_path()?, false).map_err(|e| e.to_string())?;
        let cache = symbiotic_core::paths::user_file("share.dir");
        let m = Member::new(look, keys.noise, cache, alias::load, conf::load_shares);
        let _ = m.sync();
        Ok(aliases
            .into_iter()
            .map(|a| {
                let error = (|| {
                    let (mut ch, opened) = m.connect_peer(&a).map_err(|e| e.to_string())?;
                    let q = symbiotic_core::ipc::Req { op: symbiotic_core::ipc::op::PROBE, tmo: 2000, ..Default::default() };
                    let r = symbiotic_core::share::peer::call(&mut ch, &q, opened.header).map_err(|e| e.to_string())?;
                    match r.error() {
                        None => Ok(()),
                        Some(e) => Err(format!("{e:?}")),
                    }
                })()
                .err();
                Probed { alias: a, error }
            })
            .collect())
    })
    .await
}

/// Whether this computer has joined a group.
pub(crate) fn joined() -> bool {
    conf::ShareConf::load().joined()
}

/// This computer's member, for the requests only a registry connection needs.
pub(crate) fn member() -> Result<Member, String> {
    let c = conf::ShareConf::load();
    if !c.joined() {
        return Err("not joined to a group".to_string());
    }
    let mut look = c;
    look.share = false;
    let keys = keys::load_or_create(&key_path()?, false).map_err(|e| e.to_string())?;
    Ok(Member::new(look, keys.noise, symbiotic_core::paths::user_file("share.dir"), alias::load, conf::load_shares))
}

/// Join the group a pairing code belongs to.
#[tauri::command]
pub async fn share_join(code: String, name: String) -> Result<String, String> {
    blocking(move || {
        let code = PairingCode::parse(&code).ok_or("that is not a pairing code (it starts with symshare1/)")?;
        let path = conf::ShareConf::path().ok_or("no configuration directory")?;
        let name = if name.trim().is_empty() { conf::default_name() } else { name.trim().to_string() };
        let k = keys::load_or_create(&key_path()?, false).map_err(|e| e.to_string())?;
        let (c, _) = conf::join(&conf::ShareConf::load(), &k.noise, &code, &name).map_err(|e| e.to_string())?;
        c.save(&path).map_err(|e| e.to_string())?;
        // A network instrument's alias is the group's from now on; one that could not be moved is moved when the window
        // next starts.
        let _ = symbiotic_core::share::names::move_local();
        Ok(code.addr)
    })
    .await
}

/// Serve this computer's shared aliases to the group (or stop): the setting, the start at login, and the daemon
/// told to read it again if no program is using it. The text says what happened to the daemon.
#[tauri::command]
pub async fn share_set(on: bool) -> Result<String, String> {
    blocking(move || {
        let path = conf::ShareConf::path().ok_or("no configuration directory")?;
        let mut c = conf::ShareConf::load();
        if on && !c.joined() {
            return Err("not joined to a group".to_string());
        }
        c.share = on;
        c.save(&path).map_err(|e| e.to_string())?;
        let login = match autostart::set(on || c.registry_listen.is_some()) {
            Ok(m) => m,
            Err(e) => e,
        };
        Ok(format!("{login}. {}", symbiotic_core::client::restart_daemon_if_idle(on || c.registry_listen.is_some())))
    })
    .await
}

/// Share or stop sharing one alias. The daemon sees it within ten seconds, without a restart.
#[tauri::command]
pub fn share_alias(alias: String, on: bool) -> Result<(), String> {
    conf::set_shared(&alias, on).map_err(|e| e.to_string())
}

/// A command for the registry in this computer's daemon (token, revoke NAME, takeover ALIAS, ...).
#[tauri::command]
pub async fn share_registry(line: String) -> Result<String, String> {
    blocking(move || {
        let mut c = symbiotic_core::client::Client::connect(None).map_err(|e| e.to_string())?;
        c.share_admin(&line).map_err(|e| e.to_string())
    })
    .await
}

/// Take the group's aliases over when the USB instrument they are bound to is plugged in here and their owner no longer has it.
#[tauri::command]
pub async fn share_follow(on: bool) -> Result<String, String> {
    blocking(move || {
        let path = conf::ShareConf::path().ok_or("no configuration directory")?;
        let mut c = conf::ShareConf::load();
        c.follow = on;
        c.save(&path).map_err(|e| e.to_string())?;
        Ok(symbiotic_core::client::restart_daemon_if_idle(c.share || c.registry_listen.is_some()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_can_be_shared() {
        for ok in ["GPIB0::15::INSTR", "gpib1::6::instr", "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR", "ASRL6::INSTR", "TCPIP0::198.18.68.57::inst0::INSTR"] {
            assert_eq!(problem(ok), None, "{ok}");
        }
        assert_eq!(problem("GPIB0::INTFC").as_deref(), Some("interface"), "a board is not an instrument");
        assert_eq!(problem("not a resource"), None, "not a resource is not this function's to say");
        assert_eq!(short(&[0xAB; 32]), "abababab");
    }
}
