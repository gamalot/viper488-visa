//! A registry and three machines in one process, over loopback TCP.

use super::chan::{self, Keypair};
use super::dir::{Directory, Kind};
use super::keys;
use super::regclient::{Error, RegistryClient};
use super::registry::{Code, PROTOCOL, Registry, Request, Response, serve};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

struct Group {
    reg: Arc<Mutex<Registry>>,
    addr: String,
    registry_noise: [u8; 32],
}

fn start_registry() -> Group {
    start_registry_as(false)
}

/// `earlier`: a registry of protocol 1, as the builds before `Hello` were: it does not know the
/// request, refuses it as a bad one, and lets a field added at the end be left out.
fn start_registry_as(earlier: bool) -> Group {
    let s = keys::Secrets { noise: chan::generate().unwrap(), sign: Some(ed25519_dalek::SigningKey::from_bytes(&[42; 32])) };
    // Each registry its own folder: tests start registries at once, within the same second too.
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pics = std::env::temp_dir().join(format!("symbiotic-grouppics-{}-{}-{n}", std::process::id(), now()));
    let _ = std::fs::remove_dir_all(&pics);
    let reg = Arc::new(Mutex::new(Registry::new(s.sign.clone().unwrap(), s.noise.public).with_pictures(&pics).unwrap()));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (r2, k) = (reg.clone(), s.noise.clone());
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (r3, k2) = (r2.clone(), k.clone());
            std::thread::spawn(move || {
                // The registry lets any key finish the handshake: an unenrolled one has to be able
                // to say Enroll. Whether it may do anything else is `Registry::handle`'s to say.
                if let Ok(mut ch) = chan::accept(c, &k2, |_| true) {
                    if earlier {
                        let _ = serve_protocol_1(&r3, &mut ch);
                    } else {
                        let _ = serve(&r3, &mut ch, now);
                    }
                }
            });
        }
    });
    Group { reg, addr, registry_noise: s.noise.public }
}

impl Group {
    fn join(&self, name: &str) -> (Keypair, RegistryClient<TcpStream>, [u8; 32], Directory) {
        let me = chan::generate().unwrap();
        let t = self.reg.lock().unwrap().issue_token(keys::random_token().unwrap(), now());
        let code = self.reg.lock().unwrap().pairing_code(&self.addr, t);
        let (c, verify, d) = RegistryClient::enroll(TcpStream::connect(&self.addr).unwrap(), &me, &code, name).unwrap();
        (me, c, verify, d)
    }

    fn client(&self, me: &Keypair, verify: &[u8; 32]) -> Result<RegistryClient<TcpStream>, Error> {
        RegistryClient::connect(TcpStream::connect(&self.addr).unwrap(), me, &self.registry_noise, verify)
    }
}

/// A member's listener: admits whoever its current directory lists, and echoes one message.
fn node_listener(me: Keypair, dir: Arc<Mutex<Directory>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let d = dir.clone();
            let me = me.clone();
            std::thread::spawn(move || {
                if let Ok(mut ch) = chan::accept(c, &me, |k| d.lock().unwrap().is_member(k)) {
                    if let Ok(m) = ch.recv() {
                        let _ = ch.send(&m);
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn three_machines_and_a_registry() {
    let g = start_registry();
    let (a, mut ca, va, _) = g.join("bench-a");
    let (b, mut cb, vb, _) = g.join("bench-b");

    // A shares two aliases; B sees them, signed, and cannot take either.
    ca.claim("DMM", Kind::Relay, "GPIB1::6::INSTR", "").unwrap();
    ca.claim("SCOPE", Kind::Direct, "TCPIP0::192.0.2.5::inst0::INSTR", "").unwrap();
    ca.heartbeat(&["127.0.0.1:1".to_string()], &[], &[]).unwrap();
    let (dir_b, presence) = cb.list(None).unwrap();
    assert_eq!(dir_b.claim("dmm").unwrap().owner, a.public);
    assert_eq!(dir_b.claim("SCOPE").unwrap().resource, "TCPIP0::192.0.2.5::inst0::INSTR");
    assert!(presence.iter().any(|p| p.key == a.public && p.online && p.addrs == ["127.0.0.1:1"]));
    assert!(presence.iter().any(|p| p.key == b.public && !p.online), "B has not sent a heartbeat");
    match cb.claim("Dmm", Kind::Relay, "GPIB1::6::INSTR", "") {
        Err(Error::Refused(Code::AlreadyOwned, who)) => assert_eq!(who, "bench-a"),
        x => panic!("{x:?}"),
    }

    // A directory older than the one B holds is refused.
    let mut newer = dir_b.clone();
    newer.version += 100;
    assert!(matches!(cb.list(Some(&newer)), Err(Error::Proto(_))));

    // B opens a channel to A with A's key from the directory; A admits it by its own copy.
    let a_dir = Arc::new(Mutex::new(ca.list(None).unwrap().0));
    let a_addr = node_listener(a.clone(), a_dir.clone());
    let to_a = || chan::connect(TcpStream::connect(&a_addr).unwrap(), &b, &a.public);
    let mut ch = to_a().unwrap();
    ch.send(b"*IDN?").unwrap();
    assert_eq!(ch.recv().unwrap(), b"*IDN?");
    assert_eq!(ch.peer(), &a.public);

    // A machine nobody enrolled is turned away by A, and cannot do anything at the registry.
    let c = chan::generate().unwrap();
    let mut ch = chan::connect(TcpStream::connect(&a_addr).unwrap(), &c, &a.public).ok();
    assert!(ch.as_mut().is_none_or(|ch| ch.send(b"x").is_err() || ch.recv().is_err()), "an outsider got an answer from a member");
    let mut outsider = RegistryClient::connect(TcpStream::connect(&g.addr).unwrap(), &c, &g.registry_noise, &va).unwrap();
    assert!(matches!(outsider.list(None), Err(Error::Refused(Code::NotMember, _))));

    // Revoked: the registry drops B and its aliases; A refreshes and turns B away.
    cb.claim("PSU", Kind::Relay, "GPIB1::7::INSTR", "").unwrap();
    assert!(g.reg.lock().unwrap().revoke("bench-b").unwrap());
    let held = a_dir.lock().unwrap().clone();
    let fresh = ca.list(Some(&held)).unwrap().0;
    *a_dir.lock().unwrap() = fresh;
    assert!(a_dir.lock().unwrap().claim("PSU").is_none());
    let mut ch = to_a().ok();
    assert!(ch.as_mut().is_none_or(|ch| ch.send(b"x").is_err() || ch.recv().is_err()), "a revoked member got an answer");
    let mut cb2 = g.client(&b, &vb).unwrap();
    assert!(matches!(cb2.list(None), Err(Error::Refused(Code::NotMember, _))));

    // A name that A releases is free for the others.
    let (_, mut cd, _, _) = g.join("bench-d");
    assert!(matches!(cd.claim("DMM", Kind::Relay, "GPIB1::6::INSTR", ""), Err(Error::Refused(Code::AlreadyOwned, _))));
    ca.release("DMM").unwrap();
    cd.claim("DMM", Kind::Relay, "GPIB1::6::INSTR", "").unwrap();
    let _ = va;
}

#[test]
fn a_wrong_registry_key_or_forged_signature_is_not_believed() {
    let g = start_registry();
    let (a, _, va, _) = g.join("bench-a");
    // Dialling the registry's address with some other static key: the handshake cannot complete.
    let other = chan::generate().unwrap();
    let r = RegistryClient::connect(TcpStream::connect(&g.addr).unwrap(), &a, &other.public, &va).and_then(|mut c| c.list(None).map(|_| ()));
    assert!(r.is_err());
    // The right registry, but a signing key that is not its own: what it signs does not verify.
    let forged = ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes();
    let mut c = g.client(&a, &forged).unwrap();
    assert!(matches!(c.list(None), Err(Error::Proto(_))));
}

// ---- members: sync, the listener, and a shared instrument over the channel ----

use super::conf::ShareConf;
use super::member::Member;
use super::peer::{self, PeerHost};
use crate::ipc::{C_IO, Req, Resp, op};
use crate::locks::Target;

/// A daemon that knows no hardware: it answers PING and the lock operations from a real
/// `Locks`-less script, enough to see which instrument a request was bound to and who ended.
#[derive(Default)]
struct FakeHost {
    /// What `Member::listen` said of each session that ended (share/usage.rs).
    used: Mutex<Vec<super::usage::Record>>,
    next: std::sync::atomic::AtomicU64,
    seen: Mutex<Vec<(u64, Target, u8)>>,
    ended: Mutex<Vec<u64>>,
}

impl PeerHost for FakeHost {
    fn begin(&self, _: &str) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }
    fn call(&self, me: u64, q: Req) -> Resp {
        self.seen.lock().unwrap().push((me, q.target(), q.op));
        Resp::ok(q.board as u32, q.data.clone())
    }
    fn end(&self, me: u64) {
        self.ended.lock().unwrap().push(me);
    }
}

type Shared = Arc<Mutex<Vec<String>>>;

fn member(g: &Group, name: &str, aliases: &[(&str, &str)], shared: &[&str], share: bool) -> (Arc<Member>, Arc<FakeHost>, Shared, String) {
    let keys = chan::generate().unwrap();
    let t = g.reg.lock().unwrap().issue_token(keys::random_token().unwrap(), now());
    let code = g.reg.lock().unwrap().pairing_code(&g.addr, t);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = ShareConf { share, listen: l.local_addr().unwrap().to_string(), ..Default::default() };
    let (conf, _) = super::conf::join(&base, &keys, &code, name).unwrap();
    let al: Vec<(String, String)> = aliases.iter().map(|(a, r)| (a.to_string(), r.to_string())).collect();
    let sh: Shared = Arc::new(Mutex::new(shared.iter().map(|s| s.to_string()).collect()));
    let sh2 = sh.clone();
    let m = Arc::new(Member::new(conf, keys, None, move || al.clone(), move || sh2.lock().unwrap().clone()));
    let host = Arc::new(FakeHost::default());
    let h2 = host.clone();
    let addr = m.listen(l, host.clone(), Arc::new(move |r: &super::usage::Record| h2.used.lock().unwrap().push(r.clone()))).unwrap().to_string();
    (m, host, sh, addr)
}

#[test]
fn a_member_shares_by_its_own_say_and_serves_only_that() {
    let g = start_registry();
    let (a, host_a, shared_a, addr_a) = member(&g, "bench-a", &[("DMM", "GPIB1::6::INSTR"), ("PRIVATE", "GPIB1::7::INSTR"), ("SCOPE", "TCPIP0::192.0.2.5::inst0::INSTR"), ("BOARD", "GPIB1::INTFC")], &["DMM", "SCOPE", "BOARD", "GHOST"], true);
    let (b, _, _, _) = member(&g, "bench-b", &[], &[], false);

    // A's sync claims what it can and says why not for the rest.
    let rep = a.sync().unwrap();
    assert_eq!(rep.claimed, ["DMM"]);
    let why: std::collections::HashMap<_, _> = rep.refused.iter().cloned().collect();
    assert!(why["BOARD"].contains("interface level") && why["GHOST"].contains("not in aliases.conf"), "{rep:?}");
    assert!(why["SCOPE"].contains("belongs to the group"), "a LAN alias is not claimed by a member: {rep:?}");
    // It is moved to the group instead, and then anyone has it, owner-less.
    let moved = a.adopt_lan(&[], |_| Err("not answering".to_string()), |_| Ok(())).unwrap();
    assert_eq!(moved.len(), 1);
    assert!(moved[0].1.is_ok(), "{moved:?}");
    let again = a.sync().unwrap();
    assert!(again.claimed.is_empty() && again.released.is_empty(), "nothing new the second time: {again:?}");

    // B, which shares nothing, still sees the directory.
    b.sync().unwrap();
    let d = b.directory().unwrap();
    assert_eq!(d.claim("dmm").unwrap().kind, Kind::Relay);
    assert_eq!(d.claim("dmm").unwrap().resource, "GPIB1::6::INSTR", "a relay's claim carries the owner's name for it");
    assert_eq!(d.claim("SCOPE").unwrap().resource, "TCPIP0::192.0.2.5::inst0::INSTR");
    assert_eq!(d.claim("SCOPE").unwrap().owner, crate::share::registry::GROUP_OWNER);
    assert!(d.claim("PRIVATE").is_none(), "an alias not ticked is not shared");

    // B asks A for DMM: every request is bound to GPIB1::6, whatever it says.
    a.sync().unwrap();
    b.sync().unwrap();
    let (mut ch, opened) = b.connect_peer("dmm").unwrap();
    assert_eq!(opened.target, Target::Gpib(1, 6, crate::backend::NO_SAD));
    assert_eq!(opened.resource, "GPIB1::6::INSTR");
    let q = Req { op: op::WRITE, board: 9, pad: 30, sad: 3, flags: crate::ipc::F_USB, data: b"*IDN?\n".to_vec(), ..Default::default() };
    let r = peer::call(&mut ch, &q, opened.header).unwrap();
    assert_eq!((r.code, r.data), (0, b"*IDN?\n".to_vec()));
    assert_eq!(r.ext, 1, "the board the daemon saw is DMM's, not the 9 the request said");
    {
        let seen = host_a.seen.lock().unwrap();
        assert_eq!(seen.last().unwrap().1, Target::Gpib(1, 6, crate::backend::NO_SAD));
        assert_eq!(seen.last().unwrap().2, op::WRITE);
    }

    // Interface-level operations are not offered.
    for o in [op::CMD, op::IFC, op::SHUTDOWN, op::CLIENTS, op::LIST, op::SPEED] {
        let r = peer::call(&mut ch, &Req { op: o, ..Default::default() }, opened.header).unwrap();
        assert_eq!(r.code, C_IO, "op {o}");
    }
    assert_eq!(host_a.seen.lock().unwrap().len(), 1, "none of them reached the daemon");

    // The connection's end is the daemon's cue to drop its locks.
    drop(ch);
    for _ in 0..100 {
        if !host_a.ended.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(host_a.ended.lock().unwrap().len(), 1);
    // A's record of it: who, which alias (as the group spells it), how many requests and refusals.
    for _ in 0..100 {
        if !host_a.used.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    {
        let used = host_a.used.lock().unwrap();
        assert_eq!(used.len(), 1, "{used:?}");
        let u = &used[0];
        assert_eq!((u.alias.as_str(), u.member.as_str(), u.requests, u.refused), ("DMM", "bench-b", 7, 6));
        assert_eq!(u.key, crate::share::codec::hex(&b.public_key()[..4]));
        assert!(u.from.starts_with("127.0.0.1:") && u.secs >= 0.0);
    }

    // Not served: not ticked, a LAN alias (open it yourself), an interface, unknown names.
    for name in ["PRIVATE", "BOARD", "GHOST"] {
        assert!(b.connect_peer(name).is_err(), "{name}");
    }
    let why = b.connect_peer("SCOPE").err().unwrap().to_string();
    assert!(why.contains("open TCPIP0::192.0.2.5::inst0::INSTR directly"), "{why}");
    // A stops sharing DMM. Its listener stops serving it at once, before any registry round trip;
    // the next sync releases the name, and then B no longer sees it.
    shared_a.lock().unwrap().retain(|n| n != "DMM");
    let why = b.connect_peer("DMM").err().unwrap().to_string();
    assert!(why.contains("no such shared alias"), "{why}");
    assert_eq!(a.sync().unwrap().released, ["DMM"]);
    b.sync().unwrap();
    assert!(b.directory().unwrap().claim("DMM").is_none());
    assert!(b.connect_peer("DMM").err().unwrap().to_string().contains("is not a shared alias"));

    // A key the group does not know gets nothing from A's listener, however it asks.
    let stranger = chan::generate().unwrap();
    let r = chan::connect(TcpStream::connect(&addr_a).unwrap(), &stranger, &a.public_key());
    assert!(r.is_err(), "a stranger completed a handshake with a member");
}

#[test]
fn a_session_notices_a_machine_that_went_and_a_revocation_ends_the_ones_open() {
    let g = start_registry();
    let (a, host_a, _, _) = member(&g, "bench-a", &[("DMM", "GPIB1::6::INSTR")], &["DMM"], true);
    let (b, _, _, _) = member(&g, "bench-b", &[], &[], false);
    a.sync().unwrap();
    b.sync().unwrap();
    let (mut ch, opened) = b.connect_peer("DMM").unwrap();
    // The user's end probes a silent owner; the owner's end is made the same way.
    let k = socket2::SockRef::from(&*ch.stream());
    assert!(k.keepalive().unwrap(), "keepalive on the user's connection");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(k.tcp_keepalive_time().unwrap(), super::KEEPALIVE_IDLE);
    peer::call(&mut ch, &Req { op: op::WRITE, data: b"x".to_vec(), ..Default::default() }, opened.header).unwrap();
    assert_eq!(a.cut_revoked(), 0, "a member in good standing keeps its session");

    // B is taken out of the group. Once A has the new directory, B's open session ends -- and with it
    // what it held at A's daemon -- without B sending anything more.
    assert!(g.reg.lock().unwrap().revoke("bench-b").unwrap());
    a.sync().unwrap();
    assert_eq!(a.cut_revoked(), 1);
    for _ in 0..100 {
        if !host_a.ended.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(host_a.ended.lock().unwrap().len(), 1, "the owner's daemon was told the session ended");
    assert!(peer::call(&mut ch, &Req { op: op::WRITE, data: b"x".to_vec(), ..Default::default() }, opened.header).is_err());
}

// ---- an alias follows its USB instrument from one member to another ----

use crate::identity::{Identity, UsbIds};

#[test]
fn an_alias_follows_its_usb_instrument_to_the_member_it_is_plugged_into_and_back() {
    let g = start_registry();
    let unit = UsbIds { vid: 0xF4EC, pid: 0x1103, serial: "SDG1EXAMPLE001".into() };
    let mut id = Identity::from_idn("Siglent Technologies,SDG1022X Plus,SDG1EXAMPLE001,V1").unwrap();
    id.usb = Some(unit.clone());
    let res = "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR".to_string();

    type Shared<T> = Arc<Mutex<T>>;
    struct Setup {
        aliases: Shared<Vec<(String, String)>>,
        shared: Shared<Vec<String>>,
        ids: Shared<Vec<(String, Identity)>>,
        usb: Shared<Option<Vec<UsbIds>>>,
        adopted: Shared<Vec<String>>,
        m: Member,
    }
    let make = |name: &str, follow: bool, has_alias: bool, plugged: bool| -> Setup {
        let keys = chan::generate().unwrap();
        let t = g.reg.lock().unwrap().issue_token(keys::random_token().unwrap(), now());
        let code = g.reg.lock().unwrap().pairing_code(&g.addr, t);
        let base = ShareConf { share: true, follow, listen: "127.0.0.1:0".into(), ..Default::default() };
        let (conf, _) = super::conf::join(&base, &keys, &code, name).unwrap();
        let aliases: Shared<Vec<(String, String)>> = Arc::new(Mutex::new(if has_alias { vec![("SDG".to_string(), res.clone())] } else { vec![] }));
        let shared: Shared<Vec<String>> = Arc::new(Mutex::new(if has_alias { vec!["SDG".to_string()] } else { vec![] }));
        let ids: Shared<Vec<(String, Identity)>> = Arc::new(Mutex::new(if has_alias { vec![("SDG".to_string(), id.clone())] } else { vec![] }));
        let usb: Shared<Option<Vec<UsbIds>>> = Arc::new(Mutex::new(Some(if plugged { vec![unit.clone()] } else { vec![] })));
        let adopted: Shared<Vec<String>> = Arc::new(Mutex::new(vec![]));
        let (a2, s2, i2, u2, ad2, (a3, s3, i3)) = (aliases.clone(), shared.clone(), ids.clone(), usb.clone(), adopted.clone(), (aliases.clone(), shared.clone(), ids.clone()));
        let m = Member::new(conf, keys, None, move || a2.lock().unwrap().clone(), move || s2.lock().unwrap().clone()).with_world(
            move || i2.lock().unwrap().clone(),
            move || u2.lock().unwrap().clone(),
            // What `follow` writes to the configuration files, written to the pretend ones.
            move |alias, resource, unit_id| {
                ad2.lock().unwrap().push(alias.to_string());
                a3.lock().unwrap().push((alias.to_string(), resource.to_string()));
                s3.lock().unwrap().push(alias.to_string());
                i3.lock().unwrap().push((alias.to_string(), unit_id.clone()));
                Ok(())
            },
        );
        Setup { aliases, shared, ids, usb, adopted, m }
    };
    let (a, b) = (make("bench-a", false, true, true), make("bench-b", true, false, false));
    let owner = |m: &Member| m.directory().and_then(|d| d.claim("SDG").map(|c| c.owner));
    let (pa, pb) = (a.m.public_key(), b.m.public_key());

    // A has the instrument and the alias; the group learns whose it is, and the unit with it.
    assert_eq!(a.m.sync().unwrap().claimed, ["SDG"]);
    assert_eq!(owner(&a.m), Some(pa));
    assert_eq!(a.m.directory().unwrap().claim("SDG").unwrap().identity, id.line());
    // B does not have it plugged in: nothing to follow.
    let r = b.m.sync().unwrap();
    assert!(r.followed.is_empty() && b.adopted.lock().unwrap().is_empty(), "{r:?}");

    // Moved to B while A is still up. A's heartbeat now says it cannot see the instrument; B's next sync is given the alias.
    *a.usb.lock().unwrap() = Some(vec![]);
    *b.usb.lock().unwrap() = Some(vec![unit.clone()]);
    a.m.sync().unwrap();
    let r = b.m.sync().unwrap();
    assert_eq!(r.followed, ["SDG"], "{r:?}");
    assert_eq!(*b.adopted.lock().unwrap(), ["SDG"], "its configuration was written");
    assert_eq!(owner(&b.m), Some(pb));

    // A still has the alias in its own configuration and asks for it again: refused, said in the report, not taken back.
    let r = a.m.sync().unwrap();
    assert!(r.claimed.is_empty() && r.refused.iter().any(|(n, _)| n == "SDG"), "{r:?}");
    assert_eq!(owner(&a.m), Some(pb));
    // B now serves it as any member serves its own alias, and is not asked to follow it again.
    assert!(b.m.sync().unwrap().followed.is_empty());

    // Back to A: B says the instrument is gone, and A's claim, which it has always made, is accepted.
    *b.usb.lock().unwrap() = Some(vec![]);
    *a.usb.lock().unwrap() = Some(vec![unit.clone()]);
    b.m.sync().unwrap();
    let r = a.m.sync().unwrap();
    assert_eq!(r.claimed, ["SDG"], "{r:?}");
    assert_eq!(owner(&a.m), Some(pa));

    // A member that cannot tell what is plugged in (the list failed) says nothing is absent and follows nothing.
    *b.usb.lock().unwrap() = None;
    let r = b.m.sync().unwrap();
    assert!(r.followed.is_empty());
    let _ = (&a.aliases, &a.shared, &a.ids, &b.aliases, &b.shared, &b.ids);
}

#[test]
fn a_network_alias_moved_to_the_group_gives_way_to_the_groups_own() {
    let g = start_registry();
    let (a, _, _, _) = member(&g, "bench-a", &[("SCOPE", "TCPIP0::192.0.2.5::inst0::INSTR")], &[], false);
    let (b, _, _, _) = member(&g, "bench-b", &[("SCOPE", "TCPIP0::192.0.2.6::inst0::INSTR"), ("SAME", "TCPIP0::192.0.2.5::inst0::INSTR"), ("DMM", "GPIB0::6::INSTR")], &[], false);
    let dropped = std::sync::Mutex::new(Vec::new());
    let moved = a.adopt_lan(&[], |_| Err("no answer".into()), |x| Ok(dropped.lock().unwrap().push(x.to_string()))).unwrap();
    assert!(moved.iter().all(|(_, r)| r.is_ok()), "{moved:?}");
    // B's SCOPE names another instrument than the group's: the group's stands, B's is dropped; SAME is new to the group; DMM is not a
    // network instrument's and stays B's own.
    let moved = b.adopt_lan(&[], |_| Err("no answer".into()), |x| Ok(dropped.lock().unwrap().push(x.to_string()))).unwrap();
    let said: std::collections::HashMap<_, _> = moved.into_iter().collect();
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(said["SCOPE"].as_ref().unwrap().contains("stands"), "{said:?}");
    assert!(said["SAME"].is_ok(), "{said:?}");
    assert_eq!(*dropped.lock().unwrap(), ["SCOPE", "SCOPE", "SAME"]);
    b.sync().unwrap();
    let d = b.directory().unwrap();
    assert_eq!(d.claim("SCOPE").unwrap().resource, "TCPIP0::192.0.2.5::inst0::INSTR", "the group's first");
    assert_eq!(d.claim("SAME").unwrap().owner, crate::share::registry::GROUP_OWNER);
    assert!(d.claim("DMM").is_none());
}

#[test]
fn a_group_lan_alias_follows_its_instrument_to_a_new_address() {
    use crate::share::member::LanFollow;
    let g = start_registry();
    let (a, _, _, _) = member(&g, "bench-a", &[("DMM", "TCPIP0::192.0.2.5::inst0::INSTR"), ("GW", "TCPIP0::192.0.2.9::gpib0,6::INSTR")], &[], false);
    let (b, _, _, _) = member(&g, "bench-b", &[], &[], false);
    let idn = "Keysight Technologies,34465A,MY20000001,A.03.03";
    // A moves its LAN aliases to the group, binding the one that answers; the other (behind a gateway, unanswering) is not bound.
    let moved = a.adopt_lan(&[], |r| if r.contains("192.0.2.5") { Ok(idn.to_string()) } else { Err("no answer".into()) }, |_| Ok(())).unwrap();
    assert!(moved.iter().all(|(_, r)| r.is_ok()), "{moved:?}");
    b.sync().unwrap();
    let d = b.directory().unwrap();
    assert!(d.claim("DMM").unwrap().identity.starts_with("Keysight Technologies | 34465A | MY20000001"));
    assert!(d.claim("GW").unwrap().identity.is_empty());

    // The instrument takes another address. B notices three passes in a row, then looks for it among what discovery finds.
    let mut st = LanFollow::default();
    let reach = |r: &str| !r.contains("192.0.2.5");
    let discover = || vec!["192.0.2.5".to_string(), "192.0.2.77".to_string(), "192.0.2.78".to_string()];
    let ask = |r: &str| if r.contains("192.0.2.78") { Ok(idn.to_string()) } else if r.contains("192.0.2.77") { Ok("Other,Model,123,1".to_string()) } else { Err("no answer".to_string()) };
    assert!(b.follow_lan(&mut st, &reach, &discover, &ask).is_empty(), "one miss is not enough");
    assert!(b.follow_lan(&mut st, &reach, &discover, &ask).is_empty());
    let moved = b.follow_lan(&mut st, &reach, &discover, &ask);
    assert_eq!(moved, [("DMM".to_string(), "TCPIP0::192.0.2.78::inst0::INSTR".to_string())], "found by its serial number, not by being the only one");
    a.sync().unwrap();
    assert_eq!(a.directory().unwrap().claim("DMM").unwrap().resource, "TCPIP0::192.0.2.78::inst0::INSTR");
    // An address that answers again is left alone, and the unbound alias is never looked for.
    assert!(b.follow_lan(&mut st, &|_| true, &discover, &ask).is_empty());
}

#[test]
fn a_group_lan_alias_at_an_ipv6_address_follows_its_instrument_too() {
    use crate::share::member::LanFollow;
    let g = start_registry();
    let (a, _, _, _) = member(&g, "bench-a", &[("SCOPE", "TCPIP0::[2001:db8::5]::hislip0::INSTR")], &[], false);
    let idn = "Keysight Technologies,MSOX3054A,MY12345678,1.0";
    let moved = a.adopt_lan(&[], |_| Ok(idn.to_string()), |_| Ok(())).unwrap();
    assert!(moved.iter().all(|(_, r)| r.is_ok()), "{moved:?}");
    a.sync().unwrap();
    // The colons in the host are not taken for the name's separators; a bare address found is put in brackets.
    let mut st = LanFollow::default();
    let reach = |r: &str| !r.contains("2001:db8::5]");
    let discover = || vec!["[2001:db8::5]".to_string(), "2001:db8::78".to_string()];
    let ask = |r: &str| if r.contains("[2001:db8::78]") { Ok(idn.to_string()) } else { Err("no answer".to_string()) };
    for _ in 0..2 {
        assert!(a.follow_lan(&mut st, &reach, &discover, &ask).is_empty());
    }
    let moved = a.follow_lan(&mut st, &reach, &discover, &ask);
    assert_eq!(moved, [("SCOPE".to_string(), "TCPIP0::[2001:db8::78]::hislip0::INSTR".to_string())]);
}

#[test]
fn the_groups_pictures_are_kept_by_the_registry_and_copied_by_the_members() {
    use crate::pictures::Store;
    let g = start_registry();
    let (a, _, _, _) = member(&g, "bench-a", &[], &[], false);
    let (b, _, _, _) = member(&g, "bench-b", &[], &[], false);
    let png = |tag: &str| {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(tag.as_bytes());
        v
    };
    let cache = std::env::temp_dir().join(format!("symbiotic-piccache-{}-{}", std::process::id(), now()));
    let _ = std::fs::remove_dir_all(&cache);
    let store = Store::at(cache.join("pictures.conf"));

    assert!(!b.pic_sync(&store).unwrap(), "nothing there yet");
    // A makes two pictures, one with its original; any member may.
    let f1 = a.pic_save("33220A", false, &png("one"), "", Some(("jpg", b"the whole photograph")), "{\"zoom\":100}").unwrap();
    let f2 = a.pic_save("MODEL 2015|K2015", true, &png("two"), "", None, "").unwrap();
    assert_eq!((f1.as_str(), f2.as_str()), ("33220A.png", "MODEL_2015_K2015.png"));
    assert!(b.pic_sync(&store).unwrap());
    let rules = store.load();
    assert_eq!(rules.iter().map(|r| (r.pattern.as_str(), r.dark_keep)).collect::<Vec<_>>(), [("MODEL 2015|K2015", true), ("33220A", false)], "the newest first, as on one computer");
    assert_eq!(store.read_png("33220A.png").unwrap(), png("one"));
    assert!(!b.pic_sync(&store).unwrap(), "a second look finds nothing new");
    // The original is not in the copy: it is asked for when wanted.
    assert!(store.read_original("33220A.png").unwrap().is_none());
    let (ext, bytes, edit) = b.pic_original("33220A.png").unwrap().unwrap();
    assert_eq!((ext.as_str(), bytes.as_slice(), edit.as_str()), ("jpg", &b"the whole photograph"[..], "{\"zoom\":100}"));
    assert!(b.pic_original("MODEL_2015_K2015.png").unwrap().is_none());

    // B changes A's picture (the same pattern keeps its file), and removes the other; A's copy follows.
    assert_eq!(b.pic_save("33220A", false, &png("one, better"), "", None, "").unwrap(), "33220A.png");
    b.pic_remove("MODEL 2015|K2015").unwrap();
    assert!(a.pic_sync(&Store::at(cache.join("a").join("pictures.conf"))).unwrap());
    assert!(b.pic_sync(&store).unwrap());
    assert_eq!(store.load().len(), 1);
    assert_eq!(store.read_png("33220A.png").unwrap(), png("one, better"));
    assert!(store.read_png("MODEL_2015_K2015.png").is_err(), "a picture no rule has is gone from the copy");
    assert!(matches!(b.pic_remove("nothing"), Err(Error::Refused(Code::NotFound, _))));
    assert!(matches!(b.pic_save("X", false, b"not a png", "", None, ""), Err(Error::Refused(Code::BadName, _))));
    let _ = std::fs::remove_dir_all(&cache);
}

/// What a registry of protocol 1 does with each request: `Hello` is one it does not know.
fn serve_protocol_1(reg: &Mutex<Registry>, ch: &mut chan::Channel<TcpStream>) -> std::io::Result<()> {
    loop {
        let m = ch.recv()?;
        let resp = match Request::decode(&m) {
            Ok(Request::Hello { .. }) | Err(_) => Response::err(Code::Bad, "unknown request"),
            Ok(q) => {
                let peer = *ch.peer();
                reg.lock().unwrap().handle(&peer, q, now())
            }
        };
        ch.send(&resp.encode())?;
    }
}

#[test]
fn a_member_and_a_registry_agree_on_the_protocol() {
    let g = start_registry();
    let (_, c, _, _) = g.join("bench");
    assert_eq!(c.version(), PROTOCOL, "both of this build");
    // A registry of an earlier build refuses Hello: the member speaks protocol 1 to it, and all works.
    let old = start_registry_as(true);
    let (_, mut c, _, d) = old.join("bench");
    assert_eq!(c.version(), 1);
    assert_eq!(d.members.len(), 1);
    c.claim("DMM", Kind::Relay, "GPIB0::6::INSTR", "").unwrap();
    assert!(c.list(None).unwrap().0.claim("DMM").is_some());
}

#[test]
fn a_member_of_protocol_1_may_leave_out_what_it_did_not_have_and_one_of_2_may_not() {
    let g = start_registry();
    let (me, _, _, _) = g.join("bench");
    // A claim as a member of an earlier build sends it: no identity at the end (its length, 4 bytes, gone).
    let short = |alias: &str| {
        let mut b = Request::Claim { alias: alias.into(), kind: Kind::Relay, resource: "GPIB0::6::INSTR".into(), identity: String::new() }.encode();
        b.truncate(b.len() - 4);
        b
    };
    let mut ch = chan::connect(TcpStream::connect(&g.addr).unwrap(), &me, &g.registry_noise).unwrap();
    let mut ask = |b: &[u8]| {
        ch.send(b).unwrap();
        Response::decode(&ch.recv().unwrap()).unwrap()
    };
    assert_eq!(ask(&short("OLD")), Response::Done, "no Hello: protocol 1");
    assert_eq!(ask(&Request::Hello { version: 99 }.encode()), Response::Hello { version: PROTOCOL }, "a newer member is told this registry's");
    assert!(matches!(ask(&short("NEW")), Response::Err { code: Code::Bad, .. }), "after Hello every field is there");
    let whole = Request::Claim { alias: "NEW".into(), kind: Kind::Relay, resource: "GPIB0::7::INSTR".into(), identity: String::new() }.encode();
    assert_eq!(ask(&whole), Response::Done);
}

#[test]
fn an_alias_follows_its_gpib_instrument_across_boards_and_members() {
    use super::registry::Here;
    let g = start_registry();
    const IDN: &str = "KEITHLEY INSTRUMENTS INC.,MODEL 2015,0863839,B11  /A02";
    let id = Identity::from_idn(IDN).unwrap();

    type Shared<T> = Arc<Mutex<T>>;
    struct Setup {
        aliases: Shared<Vec<(String, String)>>,
        units: Shared<Vec<(String, String)>>,
        here: Shared<Here>,
        adopted: Shared<Vec<(String, String)>>,
        m: Member,
    }
    let make = |name: &str, follow: bool, at: Option<&str>| -> Setup {
        let keys = chan::generate().unwrap();
        let t = g.reg.lock().unwrap().issue_token(keys::random_token().unwrap(), now());
        let code = g.reg.lock().unwrap().pairing_code(&g.addr, t);
        let base = ShareConf { share: true, follow, listen: "127.0.0.1:0".into(), ..Default::default() };
        let (conf, _) = super::conf::join(&base, &keys, &code, name).unwrap();
        let aliases: Shared<Vec<(String, String)>> = Arc::new(Mutex::new(at.map(|r| vec![("K2015".to_string(), r.to_string())]).unwrap_or_default()));
        let shared: Shared<Vec<String>> = Arc::new(Mutex::new(if at.is_some() { vec!["K2015".to_string()] } else { vec![] }));
        let ids: Shared<Vec<(String, Identity)>> = Arc::new(Mutex::new(if at.is_some() { vec![("K2015".to_string(), id.clone())] } else { vec![] }));
        let units: Shared<Vec<(String, String)>> = Arc::new(Mutex::new(at.map(|r| vec![(r.to_string(), IDN.to_string())]).unwrap_or_default()));
        let here: Shared<Here> = Arc::new(Mutex::new(if at.is_some() { Here::Yes } else { Here::Unknown }));
        let adopted: Shared<Vec<(String, String)>> = Arc::new(Mutex::new(vec![]));
        let (a2, s2, i2, u2, h2, ad2) = (aliases.clone(), shared.clone(), ids.clone(), units.clone(), here.clone(), adopted.clone());
        let (a3, a4, s3, i3) = (aliases.clone(), aliases.clone(), shared.clone(), ids.clone());
        let m = Member::new(conf, keys, None, move || a2.lock().unwrap().clone(), move || s2.lock().unwrap().clone())
            .with_world(
                move || i2.lock().unwrap().clone(),
                || Some(vec![]),
                move |alias, resource, unit_id| {
                    ad2.lock().unwrap().push((alias.to_string(), resource.to_string()));
                    a3.lock().unwrap().retain(|(a, _)| a != alias);
                    a3.lock().unwrap().push((alias.to_string(), resource.to_string()));
                    s3.lock().unwrap().push(alias.to_string());
                    i3.lock().unwrap().push((alias.to_string(), unit_id.clone()));
                    Ok(())
                },
            )
            .with_check(move |_| (*h2.lock().unwrap(), 0))
            .with_gpib(move || Some(u2.lock().unwrap().clone()), move |alias, resource| {
                let mut a = a4.lock().unwrap();
                a.retain(|(x, _)| x != alias);
                a.push((alias.to_string(), resource.to_string()));
                Ok(())
            });
        Setup { aliases, units, here, adopted, m }
    };
    let (a, b) = (make("bench-a", false, Some("GPIB0::15::INSTR")), make("bench-b", true, None));
    let claim = |m: &Member| m.directory().and_then(|d| d.claim("K2015").map(|c| (c.owner, c.resource.clone())));
    let (pa, pb) = (a.m.public_key(), b.m.public_key());

    // A has it and shares it; B does not have it.
    assert_eq!(a.m.sync().unwrap().claimed, ["K2015"]);
    assert_eq!(claim(&a.m), Some((pa, "GPIB0::15::INSTR".to_string())));
    assert!(b.m.sync().unwrap().followed.is_empty());

    // A's adapter took another number (the daemon restarted with another set plugged in): A points its alias at where the unit
    // is now, and the group's directory says so.
    *a.units.lock().unwrap() = vec![("GPIB1::15::INSTR".into(), IDN.into())];
    *a.here.lock().unwrap() = Here::No;
    assert_eq!(a.m.sync().unwrap().followed, ["K2015"]);
    assert_eq!(a.aliases.lock().unwrap().as_slice(), [("K2015".to_string(), "GPIB1::15::INSTR".to_string())]);
    assert_eq!(claim(&a.m), Some((pa, "GPIB1::15::INSTR".to_string())));
    *a.here.lock().unwrap() = Here::Yes;

    // Not known at A whether anything listens there (no scan): A does not say it is gone, so B, which sees the unit, may not take it.
    *a.units.lock().unwrap() = vec![];
    *a.here.lock().unwrap() = Here::Unknown;
    *b.units.lock().unwrap() = vec![("GPIB2::7::INSTR".into(), IDN.into())];
    a.m.sync().unwrap();
    assert!(b.m.sync().unwrap().followed.is_empty());
    assert_eq!(claim(&b.m).map(|c| c.0), Some(pa));

    // Moved to B: nothing listens at A's address and A has the unit nowhere; B is given the alias, at its own board and address.
    *a.here.lock().unwrap() = Here::No;
    a.m.sync().unwrap();
    let r = b.m.sync().unwrap();
    assert_eq!(r.followed, ["K2015"], "{r:?}");
    assert_eq!(*b.adopted.lock().unwrap(), [("K2015".to_string(), "GPIB2::7::INSTR".to_string())]);
    assert_eq!(claim(&b.m), Some((pb, "GPIB2::7::INSTR".to_string())));

    // And back to A, at yet another address: B says it is gone, A points its alias at it and claims it again.
    *b.units.lock().unwrap() = vec![];
    *b.here.lock().unwrap() = Here::No;
    b.m.sync().unwrap();
    *a.units.lock().unwrap() = vec![("GPIB0::9::INSTR".into(), IDN.into())];
    let r = a.m.sync().unwrap();
    assert!(r.followed.contains(&"K2015".to_string()), "{r:?}");
    assert_eq!(claim(&a.m), Some((pa, "GPIB0::9::INSTR".to_string())));
}
