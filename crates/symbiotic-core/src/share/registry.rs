//! The registry: who the members are, who owns which alias, and the signed directory of both. `Registry::handle` is the
//! whole protocol as a function from a request and the key the handshake proved to a response; `serve` runs it over a
//! channel. Nothing here touches an instrument, and no instrument data passes through it.
//!
//! A name, once claimed, is its owner's until the owner releases it or the administrator takes it back, locally
//! (`takeover`, `revoke`, `issue_token` are not requests: the network cannot reach them). The one exception: an alias
//! bound to one unit goes to the member that has that unit now, once its owner is offline or says it cannot see it
//! (`may_take_over`).

use super::chan::Channel;
use super::codec::{R, Res, W, hex, unhex};
use crate::pictures;
use super::dir::{Claim, Directory, Kind, Member, Signed};
use super::picstore::GroupPictures;
use ed25519_dalek::SigningKey;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The owner of a group alias: nobody (a LAN instrument every member reaches by the same `TCPIP` name).
pub const GROUP_OWNER: [u8; 32] = [0; 32];
/// How long a pairing code lives, seconds.
pub const TOKEN_TTL: u64 = 900;
/// A member that has not sent a heartbeat for this long is shown offline, seconds.
pub const ONLINE_WITHIN: u64 = 30;
const MAX_MEMBERS: usize = 256;
const MAX_CLAIMS: usize = 4096;
const MAX_ADDRS: usize = 8;
const MAX_ADDR_LEN: usize = 64;
const MAX_NAME_LEN: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    NotMember = 1,
    BadToken = 2,
    BadName = 3,
    AlreadyOwned = 4,
    NotOwner = 5,
    NotFound = 6,
    NameTaken = 7,
    TooMany = 8,
    Bad = 9,
    Internal = 10,
}

impl Code {
    fn from_u8(v: u8) -> Code {
        match v {
            1 => Code::NotMember,
            2 => Code::BadToken,
            3 => Code::BadName,
            4 => Code::AlreadyOwned,
            5 => Code::NotOwner,
            6 => Code::NotFound,
            7 => Code::NameTaken,
            8 => Code::TooMany,
            10 => Code::Internal,
            _ => Code::Bad,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Enroll { token: [u8; 16], name: String },
    /// `identity`: the unit the alias is bound to (identity::Identity::line), or empty.
    Claim { alias: String, kind: Kind, resource: String, identity: String },
    Release { alias: String },
    /// `absent`: the aliases this member claims whose instrument it can see is not there (cheap checks only: a USB descriptor).
    /// `states`: for each alias this member shares, whether its instrument is there, as far as it can tell.
    Heartbeat { addrs: Vec<String>, absent: Vec<String>, states: Vec<AliasState> },
    List,
    /// `List`, and each member's alias states in the answer (`Response::ListedStates`).
    ListStates,
    /// Make `alias` a group alias for the LAN instrument `resource` (a `TCPIP` name), or change it: any member may. `identity`: the unit it is
    /// bound to (`identity::Identity::line`), or empty; with it, a member that finds the unit at another address may move the alias there.
    SetGroup { alias: String, resource: String, identity: String },
    /// Take a group alias away (or a LAN alias a member registered): any member may.
    ClearGroup { alias: String },
    /// The group's pictures: the version, the rules and each picture file's version.
    PicState,
    /// One picture file (a PNG).
    PicGet { file: String },
    /// The original of a picture and how it was edited, if it has them.
    PicOriginal { file: String },
    /// Make a picture of whatever `pattern` matches: any member may. `replaces`, `orig_ext` and `edit` are empty for none; `original` empty for none.
    PicSave { pattern: String, dark_keep: bool, png: Vec<u8>, replaces: String, orig_ext: String, original: Vec<u8>, edit: String },
    PicRemove { pattern: String },
    /// The first request on a connection (from protocol 2 on): the newest protocol the member speaks.
    /// The registry answers with its own, and both use the lower for the rest of the connection. A
    /// registry of protocol 1 refuses the request (`Code::Bad`), and that is how a member knows it.
    Hello { version: u32 },
}

/// The registry protocol. 1: everything before `Hello`, where a field added at the end of a request may be absent (a
/// member of an earlier build sends none). 2: `Hello`; every field of every request is there. A field added from now on
/// comes with a new version, and its decoder asks the connection's version instead of guessing from what is left.
pub const PROTOCOL: u32 = 2;

/// Whether an instrument is there, by its owner's word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Here {
    Unknown = 0,
    Yes = 1,
    No = 2,
}

impl Here {
    fn from_u8(v: u8) -> Here {
        match v {
            1 => Here::Yes,
            2 => Here::No,
            _ => Here::Unknown,
        }
    }
}

/// One shared alias's instrument: `age` is how long ago the owner found it so, seconds (a result kept from an earlier look is older than
/// the heartbeat that carries it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AliasState {
    pub alias: String,
    pub here: Here,
    pub age: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub key: [u8; 32],
    pub online: bool,
    pub addrs: Vec<String>,
    /// Only in the answer to `ListStates`.
    pub states: Vec<AliasState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Done,
    /// The registry's signing key (so the new member can check what it signs from now on) and the
    /// directory with the member in it.
    Enrolled { verify: [u8; 32], dir: Signed },
    Listed { dir: Signed, presence: Vec<Presence> },
    /// `Listed` with each presence's `states`.
    ListedStates { dir: Signed, presence: Vec<Presence> },
    PicState { version: u64, conf: String, files: Vec<(String, u64)> },
    PicBlob { bytes: Vec<u8> },
    PicOriginal { ext: String, bytes: Vec<u8>, edit: String },
    PicSaved { file: String },
    /// The answer to `Hello`: the registry's protocol.
    Hello { version: u32 },
    Err { code: Code, msg: String },
}

fn put_states(w: &mut W, states: &[AliasState]) {
    w.u32(states.len() as u32);
    for s in states {
        w.str(&s.alias).u8(s.here as u8).u32(s.age);
    }
}

fn get_states(r: &mut R) -> Res<Vec<AliasState>> {
    let mut v = Vec::new();
    for _ in 0..r.count(9)? {
        v.push(AliasState { alias: r.str()?, here: Here::from_u8(r.u8()?), age: r.u32()? });
    }
    Ok(v)
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        match self {
            Request::Enroll { token, name } => w.u8(1).raw(token).str(name),
            Request::Claim { alias, kind, resource, identity } => w.u8(2).str(alias).u8(*kind as u8).str(resource).str(identity),
            Request::Release { alias } => w.u8(3).str(alias),
            Request::Heartbeat { addrs, absent, states } => {
                w.u8(4).u32(addrs.len() as u32);
                for a in addrs {
                    w.str(a);
                }
                w.u32(absent.len() as u32);
                for a in absent {
                    w.str(a);
                }
                put_states(&mut w, states);
                &mut w
            }
            Request::List => w.u8(5),
            Request::ListStates => w.u8(6),
            Request::SetGroup { alias, resource, identity } => w.u8(7).str(alias).str(resource).str(identity),
            Request::ClearGroup { alias } => w.u8(8).str(alias),
            Request::PicState => w.u8(9),
            Request::PicGet { file } => w.u8(10).str(file),
            Request::PicOriginal { file } => w.u8(11).str(file),
            Request::PicSave { pattern, dark_keep, png, replaces, orig_ext, original, edit } => {
                w.u8(12).str(pattern).u8(*dark_keep as u8).bytes(png).str(replaces).str(orig_ext).bytes(original).str(edit)
            }
            Request::PicRemove { pattern } => w.u8(13).str(pattern),
            Request::Hello { version } => w.u8(14).u32(*version),
        };
        w.0
    }

    /// A request on a connection of protocol 1 (one that has not said `Hello`).
    pub fn decode(b: &[u8]) -> Res<Request> {
        Request::decode_as(b, 1)
    }

    /// A request on a connection of protocol `version`: from 2 on, no field may be left out.
    pub fn decode_as(b: &[u8], version: u32) -> Res<Request> {
        let whole = version >= 2;
        let mut r = R::new(b);
        let q = match r.u8()? {
            1 => Request::Enroll { token: r.array()?, name: r.str()? },
            2 => Request::Claim {
                alias: r.str()?,
                kind: match r.u8()? {
                    0 => Kind::Relay,
                    1 => Kind::Direct,
                    _ => return Err("unknown claim kind"),
                },
                resource: r.str()?,
                // Added at the end: a member of protocol 1 may send none.
                identity: if r.is_empty() && !whole { String::new() } else { r.str()? },
            },
            3 => Request::Release { alias: r.str()? },
            4 => {
                let mut addrs = Vec::new();
                for _ in 0..r.count(4)? {
                    addrs.push(r.str()?);
                }
                let mut absent = Vec::new();
                if !r.is_empty() || whole {
                    for _ in 0..r.count(4)? {
                        absent.push(r.str()?);
                    }
                }
                // Added at the end: a member of protocol 1 may send none.
                let states = if r.is_empty() && !whole { Vec::new() } else { get_states(&mut r)? };
                Request::Heartbeat { addrs, absent, states }
            }
            5 => Request::List,
            6 => Request::ListStates,
            7 => Request::SetGroup {
                alias: r.str()?,
                resource: r.str()?,
                // Added at the end: a member of protocol 1 may send none.
                identity: if r.is_empty() && !whole { String::new() } else { r.str()? },
            },
            8 => Request::ClearGroup { alias: r.str()? },
            9 => Request::PicState,
            10 => Request::PicGet { file: r.str()? },
            11 => Request::PicOriginal { file: r.str()? },
            12 => Request::PicSave {
                pattern: r.str()?,
                dark_keep: r.u8()? != 0,
                png: r.bytes()?.to_vec(),
                replaces: r.str()?,
                orig_ext: r.str()?,
                original: r.bytes()?.to_vec(),
                edit: r.str()?,
            },
            13 => Request::PicRemove { pattern: r.str()? },
            14 => Request::Hello { version: r.u32()? },
            _ => return Err("unknown request"),
        };
        r.done()?;
        Ok(q)
    }
}

impl Response {
    pub fn err(code: Code, msg: impl Into<String>) -> Response {
        Response::Err { code, msg: msg.into() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        match self {
            Response::Done => w.u8(0).u8(0),
            Response::Enrolled { verify, dir } => w.u8(0).u8(1).raw(verify).raw(&dir.encode()),
            Response::Listed { dir, presence } | Response::ListedStates { dir, presence } => {
                let with = matches!(self, Response::ListedStates { .. });
                w.u8(0).u8(if with { 3 } else { 2 }).raw(&dir.encode()).u32(presence.len() as u32);
                for p in presence {
                    w.raw(&p.key).u8(p.online as u8).u32(p.addrs.len() as u32);
                    for a in &p.addrs {
                        w.str(a);
                    }
                    if with {
                        put_states(&mut w, &p.states);
                    }
                }
                &mut w
            }
            Response::PicState { version, conf, files } => {
                w.u8(0).u8(4).u64(*version).str(conf).u32(files.len() as u32);
                for (f, v) in files {
                    w.str(f).u64(*v);
                }
                &mut w
            }
            Response::PicBlob { bytes } => w.u8(0).u8(5).bytes(bytes),
            Response::PicOriginal { ext, bytes, edit } => w.u8(0).u8(6).str(ext).bytes(bytes).str(edit),
            Response::PicSaved { file } => w.u8(0).u8(7).str(file),
            Response::Hello { version } => w.u8(0).u8(8).u32(*version),
            Response::Err { code, msg } => w.u8(1).u8(*code as u8).str(msg),
        };
        w.0
    }

    pub fn decode(b: &[u8]) -> Res<Response> {
        let mut r = R::new(b);
        let x = match (r.u8()?, r.u8()?) {
            (0, 0) => Response::Done,
            (0, 1) => Response::Enrolled { verify: r.array()?, dir: Signed::read(&mut r)? },
            (0, kind @ (2 | 3)) => {
                let dir = Signed::read(&mut r)?;
                let mut presence = Vec::new();
                for _ in 0..r.count(37)? {
                    let (key, online) = (r.array()?, r.u8()? != 0);
                    let mut addrs = Vec::new();
                    for _ in 0..r.count(4)? {
                        addrs.push(r.str()?);
                    }
                    let states = if kind == 3 { get_states(&mut r)? } else { Vec::new() };
                    presence.push(Presence { key, online, addrs, states });
                }
                if kind == 3 { Response::ListedStates { dir, presence } } else { Response::Listed { dir, presence } }
            }
            (0, 4) => {
                let (version, conf) = (r.u64()?, r.str()?);
                let mut files = Vec::new();
                for _ in 0..r.count(12)? {
                    files.push((r.str()?, r.u64()?));
                }
                Response::PicState { version, conf, files }
            }
            (0, 5) => Response::PicBlob { bytes: r.bytes()?.to_vec() },
            (0, 6) => Response::PicOriginal { ext: r.str()?, bytes: r.bytes()?.to_vec(), edit: r.str()? },
            (0, 7) => Response::PicSaved { file: r.str()? },
            (0, 8) => Response::Hello { version: r.u32()? },
            (1, c) => Response::Err { code: Code::from_u8(c), msg: r.str()? },
            _ => return Err("unknown response"),
        };
        r.done()?;
        Ok(x)
    }
}

/// What a new member is given to join: where the registry is, its key (the handshake needs it
/// before anything is said) and a one-time token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingCode {
    pub addr: String,
    pub registry: [u8; 32],
    pub token: [u8; 16],
}

impl PairingCode {
    pub fn to_text(&self) -> String {
        format!("symshare1/{}/{}/{}", self.addr, hex(&self.registry), hex(&self.token))
    }

    pub fn parse(s: &str) -> Option<PairingCode> {
        let mut p = s.trim().split('/');
        if p.next()? != "symshare1" {
            return None;
        }
        let (addr, key, token) = (p.next()?, p.next()?, p.next()?);
        // HOST:PORT, nothing else: the code comes from someone else, and the address goes into share.conf.
        if p.next().is_some() || addr.is_empty() || addr.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        Some(PairingCode { addr: addr.to_string(), registry: unhex(key)?, token: unhex(token)? })
    }
}

pub struct Registry {
    sign: SigningKey,
    noise_pub: [u8; 32],
    dir: Directory,
    tokens: Vec<([u8; 16], u64)>,
    seen: HashMap<[u8; 32], (u64, Vec<String>)>,
    /// By member: the aliases (lower case) it said, in its last heartbeat, it cannot see the instrument of.
    absent: HashMap<[u8; 32], Vec<String>>,
    /// By member: what it said of each alias's instrument in its last heartbeat, and when.
    states: HashMap<[u8; 32], (u64, Vec<AliasState>)>,
    /// The time of the first request this run: before `ONLINE_WITHIN` has passed nobody has had the chance to be seen, and nobody is taken for offline.
    first_seen: Option<u64>,
    path: Option<PathBuf>,
    /// The group's pictures, if this registry has a place to keep them.
    pics: Option<GroupPictures>,
}

fn no_pictures() -> Response {
    Response::err(Code::Internal, "this registry has no place to keep the group's pictures")
}

fn clean_name(n: &str) -> Option<String> {
    let n = n.trim();
    (!n.is_empty() && n.chars().count() <= MAX_NAME_LEN && !n.chars().any(char::is_control)).then(|| n.to_string())
}

impl Registry {
    pub fn new(sign: SigningKey, noise_pub: [u8; 32]) -> Registry {
        Registry { sign, noise_pub, dir: Directory::default(), tokens: Vec::new(), seen: HashMap::new(), absent: HashMap::new(), states: HashMap::new(), first_seen: None, path: None, pics: None }
    }

    /// A registry whose members and claims live in `path` (text, one fact per line, written whole
    /// to a temporary file and renamed). Pairing codes and who is online are not kept.
    pub fn open(path: &Path, sign: SigningKey, noise_pub: [u8; 32]) -> io::Result<Registry> {
        let mut r = Registry::new(sign, noise_pub);
        r.path = Some(path.to_path_buf());
        // The pictures are kept beside the state.
        r.pics = path.parent().map(|d| GroupPictures::open(&d.join("group-pictures"))).transpose()?;
        if let Ok(t) = std::fs::read_to_string(path) {
            r.dir = parse_state(&t).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a registry state file", path.display())))?;
        }
        Ok(r)
    }

    /// A registry that keeps the group's pictures in `dir` (the one that has a state file does, beside it; tests give it a place).
    pub fn with_pictures(mut self, dir: &Path) -> io::Result<Registry> {
        self.pics = Some(GroupPictures::open(dir)?);
        Ok(self)
    }

    pub fn verifying_key(&self) -> [u8; 32] {
        self.sign.verifying_key().to_bytes()
    }

    pub fn directory(&self) -> &Directory {
        &self.dir
    }

    pub fn signed(&self) -> Signed {
        Signed::sign(&self.dir, &self.sign)
    }

    /// Whether `key` belongs to a member (not whether it may complete a handshake: the registry
    /// takes any, because an unenrolled key has to be able to say `Enroll`).
    pub fn is_member(&self, key: &[u8; 32]) -> bool {
        self.dir.is_member(key)
    }

    fn save(&self) -> io::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, render_state(&self.dir))?;
        std::fs::rename(&tmp, p)
    }

    /// The directory changed: a new version, saved.
    fn changed(&mut self) -> Response {
        self.dir.version += 1;
        match self.save() {
            Ok(()) => Response::Done,
            Err(e) => Response::err(Code::Internal, format!("the registry could not save its state: {e}")),
        }
    }

    // ---- administrator, local only ----

    /// A pairing code's token, good for `TOKEN_TTL` seconds and one enrolment.
    pub fn issue_token(&mut self, token: [u8; 16], now: u64) -> [u8; 16] {
        self.tokens.retain(|(_, exp)| *exp > now);
        self.tokens.push((token, now + TOKEN_TTL));
        token
    }

    pub fn pairing_code(&self, addr: &str, token: [u8; 16]) -> PairingCode {
        PairingCode { addr: addr.to_string(), registry: self.noise_pub, token }
    }

    /// Take a member out (by name or hex key), and every alias it owned with it. The next
    /// directory no longer has it, and the others stop admitting it.
    pub fn revoke(&mut self, who: &str) -> io::Result<bool> {
        let who = who.trim();
        let key = unhex::<32>(who);
        let Some(i) = self.dir.members.iter().position(|m| Some(m.key) == key || m.name.eq_ignore_ascii_case(who)) else {
            return Ok(false);
        };
        let m = self.dir.members.remove(i);
        self.dir.claims.retain(|c| c.owner != m.key);
        self.seen.remove(&m.key);
        self.absent.remove(&m.key);
        self.states.remove(&m.key);
        self.dir.version += 1;
        self.save()?;
        Ok(true)
    }

    /// Take an alias back from its owner (who is gone for good, say).
    pub fn takeover(&mut self, alias: &str) -> io::Result<bool> {
        let before = self.dir.claims.len();
        self.dir.claims.retain(|c| !c.alias.eq_ignore_ascii_case(alias.trim()));
        if self.dir.claims.len() == before {
            return Ok(false);
        }
        self.dir.version += 1;
        self.save()?;
        Ok(true)
    }

    // ---- the protocol ----

    pub fn handle(&mut self, peer: &[u8; 32], req: Request, now: u64) -> Response {
        self.first_seen.get_or_insert(now);
        if let Request::Enroll { token, name } = req {
            return self.enroll(peer, token, &name, now);
        }
        // Which protocol: asked before enrolling too, and gives nothing away.
        if let Request::Hello { .. } = req {
            return Response::Hello { version: PROTOCOL };
        }
        if !self.dir.is_member(peer) {
            return Response::err(Code::NotMember, "this key is not a member of the group");
        }
        match req {
            Request::Enroll { .. } | Request::Hello { .. } => unreachable!(),
            Request::Claim { alias, kind, resource, identity } => self.claim(peer, alias, kind, resource, identity, now),
            Request::Release { alias } => {
                match self.dir.claims.iter().position(|c| c.alias.eq_ignore_ascii_case(alias.trim())) {
                    None => Response::err(Code::NotFound, "no such alias"),
                    Some(i) if &self.dir.claims[i].owner != peer => Response::err(Code::NotOwner, "the alias is another member's"),
                    Some(i) => {
                        self.dir.claims.remove(i);
                        self.changed()
                    }
                }
            }
            Request::Heartbeat { addrs, absent, states } => {
                let addrs: Vec<String> = addrs.into_iter().filter(|a| a.len() <= MAX_ADDR_LEN).take(MAX_ADDRS).collect();
                self.seen.insert(*peer, (now, addrs));
                self.states.insert(*peer, (now, states.into_iter().filter(|s| s.alias.len() <= MAX_NAME_LEN).take(MAX_CLAIMS).collect()));
                self.absent.insert(*peer, absent.into_iter().take(MAX_CLAIMS).map(|a| a.trim().to_ascii_lowercase()).collect());
                Response::Done
            }
            Request::List => Response::Listed { dir: self.signed(), presence: self.presence(now) },
            Request::ListStates => Response::ListedStates { dir: self.signed(), presence: self.presence_with_states(now) },
            Request::SetGroup { alias, resource, identity } => self.set_group(alias, resource, identity),
            Request::PicState => match &self.pics {
                None => no_pictures(),
                Some(g) => {
                    let st = g.state();
                    Response::PicState { version: st.version, conf: st.conf, files: st.files }
                }
            },
            Request::PicGet { file } => match &self.pics {
                None => no_pictures(),
                Some(g) => match g.get(&file) {
                    Ok(bytes) => Response::PicBlob { bytes },
                    Err(e) => Response::err(Code::NotFound, e.to_string()),
                },
            },
            Request::PicOriginal { file } => match &self.pics {
                None => no_pictures(),
                Some(g) => match g.original(&file) {
                    Ok(Some(o)) => Response::PicOriginal { ext: o.ext, bytes: o.bytes, edit: o.edit.unwrap_or_default() },
                    Ok(None) => Response::err(Code::NotFound, "that picture has no original"),
                    Err(e) => Response::err(Code::NotFound, e.to_string()),
                },
            },
            Request::PicSave { pattern, dark_keep, png, replaces, orig_ext, original, edit } => match &mut self.pics {
                None => no_pictures(),
                Some(g) => {
                    let s = pictures::Save {
                        pattern: &pattern,
                        dark_keep,
                        png: &png,
                        replaces: Some(replaces.as_str()).filter(|r| !r.is_empty()),
                        original: (!original.is_empty()).then_some((orig_ext.as_str(), original.as_slice())),
                        edit: Some(edit.as_str()).filter(|e| !e.is_empty()),
                    };
                    match g.save(&s) {
                        Ok(file) => Response::PicSaved { file },
                        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Response::err(Code::BadName, e.to_string()),
                        Err(e) => Response::err(Code::Internal, e.to_string()),
                    }
                }
            },
            Request::PicRemove { pattern } => match &mut self.pics {
                None => no_pictures(),
                Some(g) => match g.remove(&pattern) {
                    Ok(true) => Response::Done,
                    Ok(false) => Response::err(Code::NotFound, "no picture for that pattern"),
                    Err(e) => Response::err(Code::Internal, e.to_string()),
                },
            },
            Request::ClearGroup { alias } => match self.dir.claims.iter().position(|c| c.alias.eq_ignore_ascii_case(alias.trim())) {
                None => Response::err(Code::NotFound, "no such alias"),
                Some(i) if self.dir.claims[i].kind == Kind::Relay => Response::err(Code::NotOwner, "that instrument is reached through its computer: only that computer can stop sharing it"),
                Some(i) => {
                    self.dir.claims.remove(i);
                    self.changed()
                }
            },
        }
    }

    /// Who is online, by the heartbeats: advisory, not signed.
    pub fn presence(&self, now: u64) -> Vec<Presence> {
        self.dir
            .members
            .iter()
            .map(|m| {
                let (t, a) = self.seen.get(&m.key).cloned().unwrap_or((0, Vec::new()));
                Presence { key: m.key, online: t != 0 && now.saturating_sub(t) <= ONLINE_WITHIN, addrs: a, states: Vec::new() }
            })
            .collect()
    }

    /// `presence`, with what each member last said of its aliases' instruments, each one as old as the heartbeat that said it plus
    /// the time since. A member that is offline says nothing: its states are not given.
    pub fn presence_with_states(&self, now: u64) -> Vec<Presence> {
        let mut p = self.presence(now);
        for x in p.iter_mut().filter(|x| x.online) {
            if let Some((t, v)) = self.states.get(&x.key) {
                let since = now.saturating_sub(*t).min(u32::MAX as u64) as u32;
                x.states = v.iter().map(|s| AliasState { alias: s.alias.clone(), here: s.here, age: s.age.saturating_add(since) }).collect();
            }
        }
        p
    }

    fn enroll(&mut self, peer: &[u8; 32], token: [u8; 16], name: &str, now: u64) -> Response {
        self.tokens.retain(|(_, exp)| *exp > now);
        let Some(t) = self.tokens.iter().position(|(t, _)| *t == token) else {
            return Response::err(Code::BadToken, "the pairing code is wrong, used or expired");
        };
        let Some(name) = clean_name(name) else {
            return Response::err(Code::BadName, "a member's name is 1 to 64 characters, no control characters");
        };
        if self.dir.members.iter().any(|m| &m.key != peer && m.name.eq_ignore_ascii_case(&name)) {
            return Response::err(Code::NameTaken, "another member has that name");
        }
        let is_new = !self.dir.is_member(peer);
        if is_new && self.dir.members.len() >= MAX_MEMBERS {
            return Response::err(Code::TooMany, "the group is full");
        }
        self.tokens.remove(t);
        match self.dir.members.iter_mut().find(|m| &m.key == peer) {
            Some(m) => m.name = name,
            None => self.dir.members.push(Member { key: *peer, name }),
        }
        match self.changed() {
            Response::Done => Response::Enrolled { verify: self.verifying_key(), dir: self.signed() },
            e => e,
        }
    }

    /// May `peer` have `claim` (another member's) because it has the same unit? The unit is the same by manufacturer, model and serial;
    /// and the owner has to be gone: offline (not heard from for `ONLINE_WITHIN`, once the registry has been up that long, so that a
    /// restart does not make everyone look offline), or has said in its heartbeat that it cannot see the instrument.
    fn may_take_over(&self, claim: &Claim, identity: &str, now: u64) -> bool {
        use crate::identity::Identity;
        let (Some(have), Some(want)) = (Identity::parse_line(&claim.identity), Identity::parse_line(identity)) else { return false };
        if !have.same_unit(&want) {
            return false;
        }
        let told_absent = self.absent.get(&claim.owner).is_some_and(|v| v.iter().any(|a| a.eq_ignore_ascii_case(&claim.alias)));
        let up_long_enough = self.first_seen.is_some_and(|t| now.saturating_sub(t) >= ONLINE_WITHIN);
        let offline = self.seen.get(&claim.owner).is_none_or(|(t, _)| now.saturating_sub(*t) > ONLINE_WITHIN);
        told_absent || (offline && up_long_enough)
    }

    /// A group alias: a LAN instrument with no owner. A member's relayed alias cannot be taken this way; a member's direct one can (it
    /// never needed its owner).
    fn set_group(&mut self, alias: String, resource: String, identity: String) -> Response {
        let alias = alias.trim().to_string();
        if let Err(why) = crate::alias::check_name(&alias) {
            return Response::err(Code::BadName, why);
        }
        let resource = match canonical(&resource) {
            Some(c) if c.starts_with("TCPIP") => c,
            _ => return Response::err(Code::BadName, "a group alias needs a TCPIP resource name"),
        };
        let identity = match identity.trim() {
            "" => String::new(),
            t => match crate::identity::Identity::parse_line(t) {
                Some(i) => i.line(),
                None => return Response::err(Code::BadName, "that is not a unit (manufacturer | model | serial)"),
            },
        };
        let new = Claim { alias: alias.clone(), owner: GROUP_OWNER, kind: Kind::Direct, resource, identity };
        match self.dir.claims.iter().position(|c| c.alias.eq_ignore_ascii_case(&alias)) {
            Some(i) if self.dir.claims[i].kind == Kind::Relay => {
                let owner = self.dir.member(&self.dir.claims[i].owner).map(|m| m.name.clone()).unwrap_or_default();
                Response::err(Code::AlreadyOwned, owner)
            }
            Some(i) if self.dir.claims[i] == new => Response::Done,
            Some(i) => {
                self.dir.claims[i] = new;
                self.changed()
            }
            None if self.dir.claims.len() >= MAX_CLAIMS => Response::err(Code::TooMany, "too many aliases"),
            None => {
                self.dir.claims.push(new);
                self.changed()
            }
        }
    }

    fn claim(&mut self, peer: &[u8; 32], alias: String, kind: Kind, resource: String, identity: String, now: u64) -> Response {
        let alias = alias.trim().to_string();
        if let Err(why) = crate::alias::check_name(&alias) {
            return Response::err(Code::BadName, why);
        }
        // The resource is the owner's canonical name for it. A relay's lets a user match search
        // expressions and read the interface type without connecting; it is not how the user
        // reaches the instrument (only the owner's daemon can).
        let resource = match (kind, canonical(&resource)) {
            (Kind::Direct, Some(c)) if c.starts_with("TCPIP") => c,
            (Kind::Direct, _) => return Response::err(Code::BadName, "a direct alias needs a TCPIP resource name"),
            (Kind::Relay, Some(c)) if crate::resource::target_of(&c).is_some() => c,
            (Kind::Relay, _) => return Response::err(Code::BadName, "a relayed alias needs a GPIB, USB or ASRL instrument"),
        };
        let identity = match identity.trim() {
            "" => String::new(),
            t => match crate::identity::Identity::parse_line(t) {
                Some(i) => i.line(),
                None => return Response::err(Code::BadName, "that is not a unit (manufacturer | model | serial)"),
            },
        };
        match self.dir.claims.iter().position(|c| c.alias.eq_ignore_ascii_case(&alias)) {
            Some(i) if &self.dir.claims[i].owner != peer => {
                if self.may_take_over(&self.dir.claims[i], &identity, now) {
                    // The unit the alias is bound to has come to this member and has left the owner: the alias goes where the unit is.
                    self.dir.claims[i] = Claim { alias, owner: *peer, kind, resource, identity };
                    return self.changed();
                }
                let owner = self.dir.member(&self.dir.claims[i].owner).map(|m| m.name.clone()).unwrap_or_default();
                Response::err(Code::AlreadyOwned, owner)
            }
            Some(i) => {
                let new = Claim { alias, owner: *peer, kind, resource, identity };
                if self.dir.claims[i] == new {
                    return Response::Done;
                }
                self.dir.claims[i] = new;
                self.changed()
            }
            None => {
                if self.dir.claims.len() >= MAX_CLAIMS {
                    return Response::err(Code::TooMany, "too many aliases");
                }
                self.dir.claims.push(Claim { alias, owner: *peer, kind, resource, identity });
                self.changed()
            }
        }
    }
}

/// A resource name as the state file can hold it: canonical, and on one line.
fn canonical(resource: &str) -> Option<String> {
    crate::resource::canonical(resource).filter(|c| !c.chars().any(char::is_control))
}

fn render_state(d: &Directory) -> String {
    let mut s = format!("# Registry state: members and aliases (symbiotic-registry). Edit with symbiotic-ctl, not by hand.\nversion {}\n", d.version);
    for m in &d.members {
        s += &format!("member {} {}\n", hex(&m.key), m.name);
    }
    for c in &d.claims {
        let owner = if c.owner == GROUP_OWNER { "group".to_string() } else { hex(&c.owner) };
        s += &format!("claim {} {} {} {}\n", c.alias, owner, if c.kind == Kind::Direct { "direct" } else { "relay" }, c.resource);
    }
    for c in d.claims.iter().filter(|c| !c.identity.is_empty()) {
        s += &format!("identity {} {}\n", c.alias, c.identity);
    }
    s
}

fn parse_state(t: &str) -> Option<Directory> {
    let mut d = Directory::default();
    for l in t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (w, rest) = l.split_once(' ')?;
        match w {
            "version" => d.version = rest.trim().parse().ok()?,
            "member" => {
                let (k, n) = rest.split_once(' ')?;
                d.members.push(Member { key: unhex(k)?, name: n.trim().to_string() });
            }
            "claim" => {
                let mut p = rest.splitn(4, ' ');
                let (alias, owner, kind) = (p.next()?, p.next()?, p.next()?);
                let kind = match kind {
                    "relay" => Kind::Relay,
                    "direct" => Kind::Direct,
                    _ => return None,
                };
                d.claims.push(Claim { alias: alias.to_string(), owner: if owner == "group" { GROUP_OWNER } else { unhex(owner)? }, kind, resource: p.next().unwrap_or("").trim().to_string(), identity: String::new() });
            }
            "identity" => {
                let (alias, line) = rest.split_once(' ')?;
                d.claims.iter_mut().find(|c| c.alias == alias)?.identity = line.trim().to_string();
            }
            _ => return None,
        }
    }
    Some(d)
}

/// Serve one member's connection until it closes: each message is one request, each answer one
/// response. Protocol 1 until the member says `Hello`.
pub fn serve<S: Read + Write>(reg: &Mutex<Registry>, ch: &mut Channel<S>, now: impl Fn() -> u64) -> io::Result<()> {
    let mut version = 1;
    loop {
        let m = match ch.recv() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let resp = match Request::decode_as(&m, version) {
            Ok(q) => {
                if let Request::Hello { version: theirs } = q {
                    version = theirs.clamp(1, PROTOCOL);
                }
                let peer = *ch.peer();
                reg.lock().unwrap_or_else(|p| p.into_inner()).handle(&peer, q, now())
            }
            Err(e) => Response::err(Code::Bad, e),
        };
        ch.send(&resp.encode())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::new(SigningKey::from_bytes(&[5; 32]), [6; 32])
    }

    fn enrol(r: &mut Registry, key: u8, name: &str, now: u64) -> Response {
        let t = r.issue_token([key; 16], now);
        r.handle(&[key; 32], Request::Enroll { token: t, name: name.into() }, now)
    }

    fn claim(r: &mut Registry, key: u8, alias: &str) -> Response {
        r.handle(&[key; 32], Request::Claim { alias: alias.into(), kind: Kind::Relay, resource: "GPIB1::6::INSTR".into(), identity: String::new() }, 100)
    }

    #[test]
    fn messages_round_trip() {
        for q in [
            Request::Enroll { token: [3; 16], name: "a".into() },
            Request::Claim { alias: "X".into(), kind: Kind::Direct, resource: "TCPIP0::1.2.3.4::inst0::INSTR".into(), identity: "A | B | C1".into() },
            Request::Release { alias: "X".into() },
            Request::Heartbeat { addrs: vec!["10.0.0.2:7488".into()], absent: vec!["dmm".into()], states: vec![AliasState { alias: "dmm".into(), here: Here::No, age: 12 }] },
            Request::List,
            Request::ListStates,
        ] {
            assert_eq!(Request::decode(&q.encode()).unwrap(), q);
        }
        let mut r = reg();
        enrol(&mut r, 1, "a", 0);
        let l = r.handle(&[1; 32], Request::List, 1);
        assert_eq!(Response::decode(&l.encode()).unwrap(), l);
        let e = Response::err(Code::AlreadyOwned, "bench");
        assert_eq!(Response::decode(&e.encode()).unwrap(), e);
        assert!(Request::decode(&[99]).is_err());
        assert!(Request::decode(&[5, 0]).is_err(), "trailing bytes");
    }

    #[test]
    fn what_a_member_says_of_its_instruments_is_given_with_its_age_and_only_while_it_is_online() {
        let mut r = reg();
        enrol(&mut r, 1, "a", 0);
        enrol(&mut r, 2, "b", 0);
        let said = vec![AliasState { alias: "dmm".into(), here: Here::Yes, age: 0 }, AliasState { alias: "psu".into(), here: Here::No, age: 40 }];
        r.handle(&[1; 32], Request::Heartbeat { addrs: vec![], absent: vec![], states: said }, 100);
        let ask = |r: &mut Registry, now| match r.handle(&[2; 32], Request::ListStates, now) {
            Response::ListedStates { presence, .. } => presence.into_iter().find(|p| p.key == [1; 32]).unwrap(),
            e => panic!("{e:?}"),
        };
        let p = ask(&mut r, 107);
        assert_eq!(p.states.iter().map(|s| (s.alias.as_str(), s.here, s.age)).collect::<Vec<_>>(), [("dmm", Here::Yes, 7), ("psu", Here::No, 47)], "older by the time since");
        assert!(ask(&mut r, 101 + ONLINE_WITHIN).states.is_empty(), "an offline member's word is not given");
        // The plain List stays what it was: no states, and an older member can read it.
        match r.handle(&[2; 32], Request::List, 107) {
            Response::Listed { presence, .. } => assert!(presence.iter().all(|p| p.states.is_empty())),
            e => panic!("{e:?}"),
        }
        let l = r.handle(&[2; 32], Request::ListStates, 107);
        assert_eq!(Response::decode(&l.encode()).unwrap(), l);
        // A heartbeat of the earlier build has no states at the end.
        let mut old = Request::Heartbeat { addrs: vec![], absent: vec![], states: vec![] }.encode();
        old.truncate(old.len() - 4);
        assert_eq!(Request::decode(&old).unwrap(), Request::Heartbeat { addrs: vec![], absent: vec![], states: vec![] });
    }

    #[test]
    fn a_group_alias_has_no_owner_and_any_member_may_change_or_remove_it() {
        let mut r = reg();
        enrol(&mut r, 1, "a", 0);
        enrol(&mut r, 2, "b", 0);
        let set = |r: &mut Registry, key: u8, alias: &str, res: &str| r.handle(&[key; 32], Request::SetGroup { alias: alias.into(), resource: res.into(), identity: String::new() }, 5);
        assert_eq!(set(&mut r, 1, "DMM", "TCPIP0::192.0.2.5::inst0::INSTR"), Response::Done);
        assert_eq!(r.directory().claims[0].owner, GROUP_OWNER);
        assert_eq!(set(&mut r, 2, "dmm", "TCPIP0::192.0.2.6::inst0::INSTR"), Response::Done, "another member changes it");
        assert_eq!(r.directory().claims.len(), 1);
        assert!(r.directory().claims[0].resource.contains("192.0.2.6"));
        assert!(matches!(set(&mut r, 1, "X", "GPIB0::5::INSTR"), Response::Err { code: Code::BadName, .. }), "only a LAN instrument");
        // A relayed alias is its owner's.
        claim(&mut r, 1, "PSU");
        assert!(matches!(set(&mut r, 2, "PSU", "TCPIP0::192.0.2.7::inst0::INSTR"), Response::Err { code: Code::AlreadyOwned, .. }));
        assert!(matches!(r.handle(&[2; 32], Request::ClearGroup { alias: "PSU".into() }, 6), Response::Err { code: Code::NotOwner, .. }));
        // A member's direct alias is taken over without asking it; a revoked member does not take group aliases with it.
        r.handle(&[1; 32], Request::Claim { alias: "OLD".into(), kind: Kind::Direct, resource: "TCPIP0::192.0.2.8::inst0::INSTR".into(), identity: String::new() }, 7);
        assert_eq!(set(&mut r, 2, "OLD", "TCPIP0::192.0.2.8::inst0::INSTR"), Response::Done);
        assert!(r.revoke("a").unwrap());
        assert!(r.directory().claims.iter().any(|c| c.alias.eq_ignore_ascii_case("DMM") && c.owner == GROUP_OWNER));
        assert_eq!(r.handle(&[2; 32], Request::ClearGroup { alias: "DMM".into() }, 8), Response::Done);
        assert!(r.directory().claim("DMM").is_none());
        // On disk and back.
        let back = parse_state(&render_state(r.directory())).unwrap();
        assert_eq!(&back, r.directory());
        for q in [Request::SetGroup { alias: "X".into(), resource: "TCPIP0::1.2.3.4::inst0::INSTR".into(), identity: "A | B | C1".into() }, Request::ClearGroup { alias: "X".into() }] {
            assert_eq!(Request::decode(&q.encode()).unwrap(), q);
        }
    }

    #[test]
    fn a_pairing_code_is_text_and_works_once() {
        let mut r = reg();
        let t = r.issue_token([7; 16], 1000);
        let c = r.pairing_code("10.0.0.9:7489", t);
        assert_eq!(PairingCode::parse(&c.to_text()), Some(c));
        assert_eq!(PairingCode::parse("nonsense"), None);
        // Its address goes into share.conf: HOST:PORT and nothing that would add a line or split one.
        let (hk, ht) = ("00".repeat(32), "00".repeat(16));
        assert!(PairingCode::parse(&format!("symshare1/host:7489/{hk}/{ht}")).is_some());
        for addr in ["h:1\nregistry_listen = 0.0.0.0:7489", "h :1", "h:1\u{1b}", "h\t:1"] {
            assert_eq!(PairingCode::parse(&format!("symshare1/{addr}/{hk}/{ht}")), None, "{addr:?}");
        }
        let ok = r.handle(&[1; 32], Request::Enroll { token: t, name: "bench".into() }, 1001);
        assert!(matches!(ok, Response::Enrolled { .. }));
        let again = r.handle(&[2; 32], Request::Enroll { token: t, name: "other".into() }, 1002);
        assert!(matches!(again, Response::Err { code: Code::BadToken, .. }), "a code is used once");
        let t2 = r.issue_token([8; 16], 1000);
        let late = r.handle(&[2; 32], Request::Enroll { token: t2, name: "other".into() }, 1000 + TOKEN_TTL);
        assert!(matches!(late, Response::Err { code: Code::BadToken, .. }), "and it expires");
    }

    #[test]
    fn only_members_may_ask_and_names_are_first_come() {
        let mut r = reg();
        assert!(matches!(claim(&mut r, 1, "DMM"), Response::Err { code: Code::NotMember, .. }));
        assert!(matches!(r.handle(&[1; 32], Request::List, 0), Response::Err { code: Code::NotMember, .. }));
        enrol(&mut r, 1, "bench-a", 0);
        enrol(&mut r, 2, "bench-b", 0);
        assert_eq!(claim(&mut r, 1, "DMM"), Response::Done);
        assert_eq!(claim(&mut r, 1, "dmm"), Response::Done, "the owner again, any case: it is the same alias");
        assert_eq!(r.directory().claims.len(), 1);
        assert_eq!(claim(&mut r, 2, "Dmm"), Response::err(Code::AlreadyOwned, "bench-a"));
        assert!(matches!(claim(&mut r, 2, "GPIB0::5::INSTR"), Response::Err { code: Code::BadName, .. }));
        assert!(matches!(claim(&mut r, 2, "a:b"), Response::Err { code: Code::BadName, .. }));
        let direct = |res: &str| Request::Claim { alias: "SCOPE".into(), kind: Kind::Direct, resource: res.into(), identity: String::new() };
        assert!(matches!(r.handle(&[2; 32], direct("GPIB0::5::INSTR"), 0), Response::Err { code: Code::BadName, .. }));
        assert_eq!(r.handle(&[2; 32], direct("tcpip0::192.0.2.5::inst0::instr"), 0), Response::Done);
        assert_eq!(r.directory().claim("scope").unwrap().resource, "TCPIP0::192.0.2.5::inst0::INSTR");
        assert!(matches!(r.handle(&[2; 32], Request::Release { alias: "DMM".into() }, 0), Response::Err { code: Code::NotOwner, .. }));
        assert_eq!(r.handle(&[1; 32], Request::Release { alias: "DMM".into() }, 0), Response::Done);
        assert_eq!(claim(&mut r, 2, "DMM"), Response::Done, "free after a release");
    }

    #[test]
    fn offline_keeps_the_name_and_revoking_frees_it() {
        let mut r = reg();
        enrol(&mut r, 1, "bench-a", 0);
        enrol(&mut r, 2, "bench-b", 0);
        claim(&mut r, 1, "DMM");
        r.handle(&[1; 32], Request::Heartbeat { addrs: vec!["10.0.0.2:7488".into()], absent: vec!["dmm".into()], states: vec![] }, 100);
        let online = |r: &mut Registry, now| match r.handle(&[2; 32], Request::List, now) {
            Response::Listed { presence, .. } => presence.iter().find(|p| p.key == [1; 32]).unwrap().online,
            x => panic!("{x:?}"),
        };
        assert!(online(&mut r, 100 + ONLINE_WITHIN));
        assert!(!online(&mut r, 101 + ONLINE_WITHIN));
        assert_eq!(claim(&mut r, 2, "DMM"), Response::err(Code::AlreadyOwned, "bench-a"), "offline does not free a name");
        let v = r.directory().version;
        assert!(r.revoke("BENCH-A").unwrap());
        assert!(r.directory().version > v);
        assert!(!r.is_member(&[1; 32]) && r.directory().claim("DMM").is_none(), "its aliases go with it");
        assert_eq!(claim(&mut r, 2, "DMM"), Response::Done);
        assert!(r.takeover("dmm").unwrap() && !r.takeover("dmm").unwrap());
        assert!(!r.revoke("nobody").unwrap());
    }

    #[test]
    fn the_directory_it_signs_is_the_one_it_holds_and_survives_a_restart() {
        let p = std::env::temp_dir().join(format!("sym-reg-{}.state", std::process::id()));
        std::fs::remove_file(&p).ok();
        let key = SigningKey::from_bytes(&[5; 32]);
        let mut r = Registry::open(&p, key.clone(), [6; 32]).unwrap();
        enrol(&mut r, 1, "bench a", 0);
        r.handle(&[1; 32], Request::Claim { alias: "SCOPE".into(), kind: Kind::Direct, resource: "TCPIP0::192.0.2.5::inst0::INSTR".into(), identity: String::new() }, 0);
        claim(&mut r, 1, "DMM");
        assert_eq!(r.signed().verify(&key.verifying_key()).unwrap(), *r.directory());
        let r2 = Registry::open(&p, key, [6; 32]).unwrap();
        assert_eq!(r2.directory(), r.directory());
        std::fs::write(&p, "bogus line\n").unwrap();
        assert!(Registry::open(&p, SigningKey::from_bytes(&[5; 32]), [6; 32]).is_err(), "damaged state is an error, not an empty group");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn member_names_are_checked() {
        let mut r = reg();
        assert!(matches!(enrol(&mut r, 1, "  ", 0), Response::Err { code: Code::BadName, .. }));
        assert!(matches!(enrol(&mut r, 1, "a\nb", 0), Response::Err { code: Code::BadName, .. }));
        assert!(matches!(enrol(&mut r, 1, "bench", 0), Response::Enrolled { .. }));
        assert!(matches!(enrol(&mut r, 2, "BENCH", 0), Response::Err { code: Code::NameTaken, .. }));
        assert!(matches!(enrol(&mut r, 1, "renamed", 0), Response::Enrolled { .. }), "a member may enrol again with a fresh code and be renamed");
        assert_eq!(r.directory().members.len(), 1);
    }

    fn claim_unit(r: &mut Registry, key: u8, alias: &str, resource: &str, identity: &str, now: u64) -> Response {
        r.handle(&[key; 32], Request::Claim { alias: alias.into(), kind: Kind::Relay, resource: resource.into(), identity: identity.into() }, now)
    }

    const SDG: &str = "Siglent Technologies | SDG1022X Plus | SDG1EXAMPLE001 | usb:F4EC:1103:SDG1EXAMPLE001";

    #[test]
    fn nothing_a_member_sends_becomes_a_line_of_the_state_file() {
        let dir = std::env::temp_dir().join(format!("symbiotic-reg-inject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("registry.state");
        let mut r = Registry::open(&p, SigningKey::from_bytes(&[5; 32]), [6; 32]).unwrap();
        enrol(&mut r, 1, "bench-a", 0);
        let intruder = format!("member {} intruder", hex(&[0xab; 32]));
        let identity = format!("Acme\n{intruder} | M | S1234");
        assert!(matches!(claim_unit(&mut r, 1, "x", SDG_USB, &identity, 1), Response::Err { code: Code::BadName, .. }));
        let resource = format!("USB0::0xF4EC::0x1103::S1\n{intruder}::INSTR");
        assert!(!matches!(claim_unit(&mut r, 1, "y", &resource, "", 1), Response::Done));
        let group = r.handle(&[1; 32], Request::SetGroup { alias: "z".into(), resource: format!("TCPIP0::h\n{intruder}::INSTR"), identity: String::new() }, 1);
        assert!(!matches!(group, Response::Done));
        drop(r);
        assert_eq!(Registry::open(&p, SigningKey::from_bytes(&[5; 32]), [6; 32]).unwrap().directory().members.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
    const SDG_USB: &str = "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR";

    #[test]
    fn an_alias_goes_where_its_unit_is_only_when_the_owner_no_longer_has_it() {
        let mut r = reg();
        enrol(&mut r, 1, "bench-a", 0);
        enrol(&mut r, 2, "bench-b", 0);
        assert_eq!(claim_unit(&mut r, 1, "SDG", SDG_USB, SDG, 1), Response::Done);
        let beat = |r: &mut Registry, key: u8, now: u64, absent: &[&str]| {
            r.handle(&[key; 32], Request::Heartbeat { addrs: vec![], absent: absent.iter().map(|a| a.to_string()).collect(), states: vec![] }, now)
        };
        beat(&mut r, 1, 5, &[]);
        // The owner is online and has it: no.
        assert_eq!(claim_unit(&mut r, 2, "SDG", SDG_USB, SDG, 10), Response::err(Code::AlreadyOwned, "bench-a"));
        // Another unit of the same model, or a claim with no unit at all, is not it.
        let other = "Siglent Technologies | SDG1022X Plus | SDG1EXAMPLE999 | usb:F4EC:1103:SDG1EXAMPLE999";
        assert_eq!(claim_unit(&mut r, 2, "SDG", SDG_USB, other, 10), Response::err(Code::AlreadyOwned, "bench-a"));
        assert_eq!(claim_unit(&mut r, 2, "SDG", SDG_USB, "", 10), Response::err(Code::AlreadyOwned, "bench-a"));
        // The owner says its instrument has gone: yes, though it is online.
        beat(&mut r, 1, 12, &["sdg"]);
        assert_eq!(claim_unit(&mut r, 2, "SDG", SDG_USB, SDG, 13), Response::Done);
        assert_eq!(r.directory().claim("SDG").unwrap().owner, [2; 32]);
        assert_eq!(r.directory().claim("SDG").unwrap().identity, SDG, "the unit stays with the claim");
    }

    #[test]
    fn an_owner_gone_quiet_loses_it_but_not_straight_after_a_restart() {
        let mut r = reg();
        enrol(&mut r, 1, "bench-a", 0);
        enrol(&mut r, 2, "bench-b", 0);
        claim_unit(&mut r, 1, "SDG", SDG_USB, SDG, 1);
        // Nobody has been heard from yet this run (a restart): offline means nothing, for a while.
        assert!(matches!(claim_unit(&mut r, 2, "SDG", SDG_USB, SDG, 2), Response::Err { code: Code::AlreadyOwned, .. }));
        r.handle(&[2; 32], Request::Heartbeat { addrs: vec![], absent: vec![], states: vec![] }, 40);
        // Long enough up, and the owner has not been heard from for longer than a heartbeat can be late: it is offline.
        assert_eq!(claim_unit(&mut r, 2, "SDG", SDG_USB, SDG, 40), Response::Done);
    }

    #[test]
    fn a_unit_is_checked_and_kept_through_the_state_file() {
        let mut r = reg();
        enrol(&mut r, 1, "bench-a", 0);
        assert!(matches!(claim_unit(&mut r, 1, "SDG", SDG_USB, "only | two", 1), Response::Err { code: Code::BadName, .. }));
        assert_eq!(claim_unit(&mut r, 1, "SDG", SDG_USB, "  Siglent Technologies |SDG1022X Plus| SDG1EXAMPLE001", 1), Response::Done);
        assert_eq!(r.directory().claim("SDG").unwrap().identity, "Siglent Technologies | SDG1022X Plus | SDG1EXAMPLE001", "stored in one form");
        let text = render_state(r.directory());
        let back = parse_state(&text).unwrap();
        assert_eq!(back.claims, r.directory().claims);
        // An old state file has no identity lines and still reads.
        assert!(parse_state(&text.lines().filter(|l| !l.starts_with("identity")).collect::<Vec<_>>().join("\n")).is_some());
    }
}
