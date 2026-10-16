//! The registry's administrator commands: issuing a pairing code, listing, revoking a member,
//! taking an alias back. They are not requests of the protocol: the network cannot reach them.
//! They run in the registry's own process, asked over a local channel (a Unix socket in the
//! registry's directory, mode 0600, for the standalone registry; the daemon's own channel for an
//! embedded one). A command is one line of text and the answer is text.

use super::registry::Registry;
use std::sync::{Arc, Mutex};

pub struct Admin {
    pub reg: Arc<Mutex<Registry>>,
    /// The address members are told to dial, for pairing codes.
    pub advertise: Option<String>,
}

fn short(k: &[u8; 32]) -> String {
    super::codec::hex(&k[..4])
}

impl Admin {
    /// `now` in Unix seconds. Err is a message for the administrator.
    pub fn run(&self, line: &str, now: u64) -> Result<String, String> {
        let line = line.trim();
        let (cmd, arg) = line.split_once(char::is_whitespace).map_or((line, ""), |(c, a)| (c, a.trim()));
        let mut reg = self.reg.lock().unwrap_or_else(|p| p.into_inner());
        match cmd {
            "token" => {
                let addr = if arg.is_empty() { self.advertise.as_deref() } else { Some(arg) }
                    .ok_or("this registry was started without --advertise: give the address members dial, `token HOST:PORT`")?;
                let t = reg.issue_token(super::keys::random_token().map_err(|e| e.to_string())?, now);
                Ok(format!("{}\n(good for {} minutes, once)\n", reg.pairing_code(addr, t).to_text(), super::registry::TOKEN_TTL / 60))
            }
            "members" => {
                let pres = reg.presence(now);
                let d = reg.directory();
                let mut out = format!("directory version {}\n", d.version);
                for m in &d.members {
                    let p = pres.iter().find(|p| p.key == m.key);
                    let n = d.claims.iter().filter(|c| c.owner == m.key).count();
                    out += &format!(
                        "{}\t{}\t{}\t{} alias{}\t{}\n",
                        m.name,
                        short(&m.key),
                        if p.is_some_and(|p| p.online) { "online" } else { "offline" },
                        n,
                        if n == 1 { "" } else { "es" },
                        p.map(|p| p.addrs.join(",")).unwrap_or_default()
                    );
                }
                Ok(out)
            }
            "claims" => {
                let d = reg.directory();
                let mut out = String::new();
                for c in &d.claims {
                    let owner = if c.owner == super::registry::GROUP_OWNER { "(group)" } else { d.member(&c.owner).map_or("?", |m| m.name.as_str()) };
                    out += &format!("{}\t{}\t{}\t{}\t{}\n", c.alias, owner, if c.kind == super::dir::Kind::Direct { "direct" } else { "relay" }, c.resource, c.identity);
                }
                Ok(out)
            }
            "revoke" if !arg.is_empty() => match reg.revoke(arg) {
                Ok(true) => Ok(format!("revoked {arg}; its aliases are free\n")),
                Ok(false) => Err(format!("no member {arg:?}")),
                Err(e) => Err(format!("revoked in memory but the state could not be saved: {e}")),
            },
            "takeover" if !arg.is_empty() => match reg.takeover(arg) {
                Ok(true) => Ok(format!("{arg} is free\n")),
                Ok(false) => Err(format!("no alias {arg:?}")),
                Err(e) => Err(format!("taken in memory but the state could not be saved: {e}")),
            },
            _ => Err("commands: token [HOST:PORT] | members | claims | revoke NAME-OR-KEY | takeover ALIAS".to_string()),
        }
    }
}

/// Listen for administrator commands on a Unix socket at `path` (replacing a stale one).
#[cfg(unix)]
pub fn serve_socket(path: &std::path::Path, admin: Arc<Admin>, now: impl Fn() -> u64 + Send + Sync + 'static) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    // sockaddr_un holds about 100 bytes of path; the OS's own message ("path must be shorter than
    // SUN_LEN") does not say which path or what to do.
    if path.as_os_str().len() > 100 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("the admin socket's path is {} bytes and Unix sockets take about 100; use a shorter --dir", path.as_os_str().len()),
        ));
    }
    // A socket nobody answers on is stale; one somebody answers on is another registry.
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another registry is running in this directory"));
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let now = Arc::new(now);
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (admin, now) = (admin.clone(), now.clone());
            std::thread::spawn(move || {
                let _ = c.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new((&c).take(4096)).read_line(&mut line).is_err() {
                    return;
                }
                let text = match admin.run(&line, now()) {
                    Ok(t) => format!("ok\n{t}"),
                    Err(e) => format!("err\n{e}\n"),
                };
                let _ = (&c).write_all(text.as_bytes());
            });
        }
    });
    Ok(())
}

/// Ask a registry on this machine, through its socket.
#[cfg(unix)]
pub fn call_socket(path: &std::path::Path, line: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let mut c = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("no registry answers at {}: {e}", path.display()))?;
    c.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
    c.write_all(format!("{}\n", line.trim()).as_bytes()).map_err(|e| e.to_string())?;
    let mut s = String::new();
    c.read_to_string(&mut s).map_err(|e| e.to_string())?;
    match s.split_once('\n') {
        Some(("ok", t)) => Ok(t.to_string()),
        Some(("err", t)) => Err(t.trim_end().to_string()),
        _ => Err("unreadable answer".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn the_commands() {
        let reg = Arc::new(Mutex::new(Registry::new(SigningKey::from_bytes(&[1; 32]), [2; 32])));
        let a = Admin { reg: reg.clone(), advertise: None };
        assert!(a.run("token", 0).is_err(), "no address to put in the code");
        let code = a.run("token 10.0.0.9:7489", 0).unwrap();
        assert!(code.starts_with("symshare1/10.0.0.9:7489/"), "{code}");
        let t = super::super::registry::PairingCode::parse(code.lines().next().unwrap()).unwrap().token;
        let r = reg.lock().unwrap().handle(&[3; 32], super::super::registry::Request::Enroll { token: t, name: "bench".into() }, 1);
        assert!(matches!(r, super::super::registry::Response::Enrolled { .. }));
        assert!(a.run("members", 2).unwrap().contains("bench\t03030303\toffline\t0 aliases"));
        assert!(a.run("revoke nobody", 2).is_err());
        assert!(a.run("revoke bench", 2).unwrap().contains("revoked"));
        assert!(a.run("claims", 2).unwrap().is_empty());
        assert!(a.run("frobnicate", 2).is_err());
        assert!(a.run("revoke", 2).is_err(), "revoke needs a name");
    }

    #[cfg(unix)]
    #[test]
    fn over_the_socket() {
        let d = std::env::temp_dir().join(format!("sym-adm-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("admin.sock");
        let reg = Arc::new(Mutex::new(Registry::new(SigningKey::from_bytes(&[1; 32]), [2; 32])));
        let a = Arc::new(Admin { reg, advertise: Some("h:1".into()) });
        serve_socket(&p, a.clone(), || 0).unwrap();
        assert!(call_socket(&p, "token").unwrap().starts_with("symshare1/h:1/"));
        assert_eq!(call_socket(&p, "revoke x").unwrap_err(), "no member \"x\"");
        assert!(serve_socket(&p, a, || 0).is_err(), "a second registry in the same directory");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(d).ok();
    }
}
