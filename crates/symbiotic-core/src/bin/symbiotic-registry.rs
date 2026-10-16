//! The registry of shared aliases.
//!
//!     symbiotic-registry run [--dir DIR] [--listen ADDR:PORT] [--advertise HOST:PORT]
//!     symbiotic-registry [--dir DIR] token [HOST:PORT] | members | claims | revoke WHO | takeover ALIAS
//!
//! `run` serves until it is stopped. The other commands ask the running registry on this machine through a socket in
//! DIR (Unix; mode 0600): the administrator is whoever can reach it, and the network cannot. DIR holds the registry's
//! keys and state; it defaults to `registry` under the configuration directory.

#[cfg(not(unix))]
fn main() {
    eprintln!("symbiotic-registry runs on Linux and macOS; on Windows the registry is embedded in symbioticd");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    use std::sync::{Arc, Mutex};
    use symbiotic_core::share::{admin, keys, regserver, registry::Registry};

    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let mut opt = |name: &str| -> Option<String> {
        let i = a.iter().position(|x| x == name)?;
        a.remove(i);
        (i < a.len()).then(|| a.remove(i))
    };
    let dir = opt("--dir")
        .map(std::path::PathBuf::from)
        .or_else(|| symbiotic_core::paths::config_dir().map(|d| d.join("registry")))
        .unwrap_or_else(|| {
            eprintln!("symbiotic-registry: no directory (give --dir)");
            std::process::exit(1)
        });
    let listen = opt("--listen").unwrap_or_else(|| "0.0.0.0:7489".to_string());
    let advertise = opt("--advertise");
    let sock = dir.join("admin.sock");
    let fail = |m: String| -> ! {
        eprintln!("symbiotic-registry: {m}");
        std::process::exit(1)
    };

    if a.first().map(String::as_str) != Some("run") {
        if a.is_empty() {
            fail("usage: symbiotic-registry run [--dir DIR] [--listen ADDR:PORT] [--advertise HOST:PORT] | token | members | claims | revoke WHO | takeover ALIAS".into());
        }
        match admin::call_socket(&sock, &a.join(" ")) {
            Ok(t) => print!("{t}"),
            Err(e) => fail(e),
        }
        return;
    }

    let k = keys::load_or_create(&dir.join("registry.key"), true).unwrap_or_else(|e| fail(format!("keys: {e}")));
    let reg = Registry::open(&dir.join("registry.state"), k.sign.clone().unwrap(), k.noise.public).unwrap_or_else(|e| fail(format!("state: {e}")));
    let reg = Arc::new(Mutex::new(reg));
    let l = std::net::TcpListener::bind(&listen).unwrap_or_else(|e| fail(format!("cannot listen on {listen}: {e}")));
    let adm = Arc::new(admin::Admin { reg: reg.clone(), advertise });
    admin::serve_socket(&sock, adm, regserver::unix_now).unwrap_or_else(|e| fail(format!("admin socket {}: {e}", sock.display())));
    println!("symbiotic-registry: listening on {listen}, directory {}, key {}", dir.display(), symbiotic_core::share::codec::hex(&k.noise.public));
    regserver::run(l, reg, k.noise, |m| eprintln!("symbiotic-registry: {m}"));
}
