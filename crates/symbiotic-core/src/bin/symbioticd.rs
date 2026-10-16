//! The Symbiotic daemon. Normally started by the VISA DLL or symbiotic-ctl, not by hand.
//!
//!     symbioticd [--idle SECONDS]        (default 10; exits after that long with no client)
//!     symbioticd --version               (its build version, and nothing else: a client asks it before handing over)
//!
//! No console window on Windows: it is started at login (autostart.rs) as well as by programs, and
//! it says nothing on a console either way -- what it has to say goes to symbioticd.log.
//!
//! The channel is a named pipe (Windows) or Unix socket; SYMBIOTIC_PIPE overrides its name.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::time::Duration;

fn main() {
    let mut idle = 10u64;
    let mut a = std::env::args().skip(1);
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{}", symbiotic_core::ipc::BUILD);
        return;
    }
    while let Some(k) = a.next() {
        match (k.as_str(), a.next().and_then(|v| v.parse::<u64>().ok())) {
            ("--idle", Some(v)) => idle = v,
            _ => {
                eprintln!("usage: symbioticd [--idle SECONDS]");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = symbiotic_core::server::run(Duration::from_secs(idle)) {
        symbiotic_core::server::log(&format!("cannot run: {e}"));
        // Only a channel that is taken says another daemon may be on it.
        let hint = if e.kind() == std::io::ErrorKind::AddrInUse { " (already running?)" } else { "" };
        eprintln!("symbioticd: {e}{hint}");
        // 2 is "another daemon has the channel": the one wanted is running, and the systemd unit
        // counts it a success (autostart.rs). Any other failure is 1, and is restarted.
        std::process::exit(if e.kind() == std::io::ErrorKind::AddrInUse { 2 } else { 1 });
    }
}
