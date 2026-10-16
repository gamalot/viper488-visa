//! A member's side of the registry protocol.

use super::chan::{self, Channel, Keypair};
use super::dir::{Directory, Kind, Signed, accept_newer};
use super::registry::{AliasState, Code, PairingCode, Presence, Request, Response};
use ed25519_dalek::VerifyingKey;
use std::fmt;
use std::io::{self, Read, Write};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The registry said no.
    Refused(Code, String),
    /// The registry said something it should not have.
    Proto(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Refused(c, m) => write!(f, "the registry refused ({c:?}): {m}"),
            Error::Proto(m) => write!(f, "bad answer from the registry: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

pub struct RegistryClient<S: Read + Write> {
    ch: Channel<S>,
    /// Unknown only during `enroll`, until the registry's answer brings it.
    verify: Option<VerifyingKey>,
    /// The protocol this connection speaks (registry::PROTOCOL; `hello`).
    version: u32,
}

fn key_of(b: &[u8; 32]) -> Result<VerifyingKey, Error> {
    VerifyingKey::from_bytes(b).map_err(|_| Error::Proto("not a signing key"))
}

impl<S: Read + Write> RegistryClient<S> {
    /// Say which protocol this member speaks, first thing on the connection, and take the lower of
    /// it and the registry's. A registry of protocol 1 does not know the request and refuses it as a
    /// bad one: that is its answer.
    fn hello(mut self) -> Result<Self, Error> {
        use super::registry::PROTOCOL;
        self.version = match self.ask(&Request::Hello { version: PROTOCOL }) {
            Ok(Response::Hello { version }) => version.clamp(1, PROTOCOL),
            Err(Error::Refused(Code::Bad, _)) => 1,
            Ok(_) => return Err(Error::Proto("unexpected answer to Hello")),
            Err(e) => return Err(e),
        };
        Ok(self)
    }

    /// The protocol this connection speaks.
    pub fn version(&self) -> u32 {
        self.version
    }

    fn ask(&mut self, q: &Request) -> Result<Response, Error> {
        self.ch.send(&q.encode())?;
        match Response::decode(&self.ch.recv()?).map_err(Error::Proto)? {
            Response::Err { code, msg } => Err(Error::Refused(code, msg)),
            r => Ok(r),
        }
    }

    /// Join the group `code` belongs to. Returns the client, the registry's signing key (to keep
    /// in the configuration) and the directory, which already has this member in it.
    pub fn enroll(stream: S, local: &Keypair, code: &PairingCode, name: &str) -> Result<(Self, [u8; 32], Directory), Error> {
        let ch = chan::connect(stream, local, &code.registry)?;
        // The signing key is not known yet; the answer to this request brings it, over the channel
        // the registry's static key already authenticated.
        let mut c = RegistryClient { ch, verify: None, version: 1 }.hello()?;
        match c.ask(&Request::Enroll { token: code.token, name: name.to_string() })? {
            Response::Enrolled { verify, dir } => {
                let v = key_of(&verify)?;
                let d = dir.verify(&v).map_err(Error::Proto)?;
                c.verify = Some(v);
                if !d.is_member(&local.public) {
                    return Err(Error::Proto("the directory does not have this member"));
                }
                Ok((c, verify, d))
            }
            _ => Err(Error::Proto("unexpected answer to Enroll")),
        }
    }

    /// Talk to the registry as a member already enrolled.
    pub fn connect(stream: S, local: &Keypair, registry: &[u8; 32], verify: &[u8; 32]) -> Result<Self, Error> {
        RegistryClient { ch: chan::connect(stream, local, registry)?, verify: Some(key_of(verify)?), version: 1 }.hello()
    }

    fn done(&mut self, q: Request) -> Result<(), Error> {
        match self.ask(&q)? {
            Response::Done => Ok(()),
            _ => Err(Error::Proto("unexpected answer")),
        }
    }

    /// `identity`: the unit the alias is bound to (identity::Identity::line), or "".
    pub fn claim(&mut self, alias: &str, kind: Kind, resource: &str, identity: &str) -> Result<(), Error> {
        self.done(Request::Claim { alias: alias.into(), kind, resource: resource.into(), identity: identity.into() })
    }

    /// A group alias for a LAN instrument (any member may make or change one).
    pub fn set_group(&mut self, alias: &str, resource: &str, identity: &str) -> Result<(), Error> {
        self.done(Request::SetGroup { alias: alias.into(), resource: resource.into(), identity: identity.into() })
    }

    pub fn clear_group(&mut self, alias: &str) -> Result<(), Error> {
        self.done(Request::ClearGroup { alias: alias.into() })
    }

    /// The group's pictures: the version, the rules and each file's version.
    pub fn pic_state(&mut self) -> Result<(u64, String, Vec<(String, u64)>), Error> {
        match self.ask(&Request::PicState)? {
            Response::PicState { version, conf, files } => Ok((version, conf, files)),
            _ => Err(Error::Proto("unexpected answer to PicState")),
        }
    }

    pub fn pic_get(&mut self, file: &str) -> Result<Vec<u8>, Error> {
        match self.ask(&Request::PicGet { file: file.into() })? {
            Response::PicBlob { bytes } => Ok(bytes),
            _ => Err(Error::Proto("unexpected answer to PicGet")),
        }
    }

    /// The original of a picture (extension, bytes, edit parameters), or `Refused(NotFound)` if it has none.
    pub fn pic_original(&mut self, file: &str) -> Result<(String, Vec<u8>, String), Error> {
        match self.ask(&Request::PicOriginal { file: file.into() })? {
            Response::PicOriginal { ext, bytes, edit } => Ok((ext, bytes, edit)),
            _ => Err(Error::Proto("unexpected answer to PicOriginal")),
        }
    }

    /// Make a picture of whatever `pattern` matches (any member may); the file it is in.
    #[allow(clippy::too_many_arguments)]
    pub fn pic_save(&mut self, pattern: &str, dark_keep: bool, png: &[u8], replaces: &str, original: Option<(&str, &[u8])>, edit: &str) -> Result<String, Error> {
        let (orig_ext, original) = original.map(|(e, b)| (e.to_string(), b.to_vec())).unwrap_or_default();
        match self.ask(&Request::PicSave { pattern: pattern.into(), dark_keep, png: png.to_vec(), replaces: replaces.into(), orig_ext, original, edit: edit.into() })? {
            Response::PicSaved { file } => Ok(file),
            _ => Err(Error::Proto("unexpected answer to PicSave")),
        }
    }

    pub fn pic_remove(&mut self, pattern: &str) -> Result<(), Error> {
        self.done(Request::PicRemove { pattern: pattern.into() })
    }

    pub fn release(&mut self, alias: &str) -> Result<(), Error> {
        self.done(Request::Release { alias: alias.into() })
    }

    /// `absent`: the aliases of this member's whose instrument it can see is not there.
    pub fn heartbeat(&mut self, addrs: &[String], absent: &[String], states: &[AliasState]) -> Result<(), Error> {
        self.done(Request::Heartbeat { addrs: addrs.to_vec(), absent: absent.to_vec(), states: states.to_vec() })
    }

    /// The signed directory (checked, and not older than `cached`) and who is online.
    pub fn list(&mut self, cached: Option<&Directory>) -> Result<(Directory, Vec<Presence>), Error> {
        self.list_signed(cached).map(|(d, _, p)| (d, p))
    }

    /// `list`, and the signed bytes too, for a cache on disk.
    pub fn list_signed(&mut self, cached: Option<&Directory>) -> Result<(Directory, Signed, Vec<Presence>), Error> {
        match self.ask(&Request::ListStates)? {
            Response::Listed { dir, presence } | Response::ListedStates { dir, presence } => {
                let v = self.verify.as_ref().ok_or(Error::Proto("no signing key for the registry"))?;
                let d = accept_newer(cached, dir.verify(v).map_err(Error::Proto)?).map_err(Error::Proto)?;
                Ok((d, dir, presence))
            }
            _ => Err(Error::Proto("unexpected answer to List")),
        }
    }
}
