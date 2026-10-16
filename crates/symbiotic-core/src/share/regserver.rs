//! The registry's network side: a TCP listener, a Noise handshake per connection, and `serve`.
//!
//! The registry lets any key finish the handshake (an unenrolled key has to be able to say
//! `Enroll`), so what keeps a stranger from using it up is the door (`door.rs`: a deadline on the
//! whole handshake, few handshakes at once and fewer from one address), a ceiling on connections
//! that have shaken hands, and a deadline on a silent connection: members send a heartbeat every
//! `HEARTBEAT` seconds, so a connection quiet for `IDLE` is dead.
use super::chan::{self, Channel, Keypair};
use super::door::{self, Door};
use super::registry::{Registry, serve};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const HEARTBEAT: u64 = 10;
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(HEARTBEAT * 6);
const MAX_CONNS: usize = 128;

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Serve `reg` on `l` forever (on this thread). Each connection gets a thread.
pub fn run(l: TcpListener, reg: Arc<Mutex<Registry>>, keys: Keypair, log: impl Fn(&str) + Send + Sync + 'static) {
    let (live, log, door) = (Arc::new(AtomicUsize::new(0)), Arc::new(log), Arc::new(Door::default()));
    for c in l.incoming() {
        let Ok(c) = c else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let Some(place) = c.peer_addr().ok().and_then(|a| door.enter(a.ip())) else {
            continue; // closed without a word
        };
        let (reg, keys, live, log, door) = (reg.clone(), keys.clone(), live.clone(), log.clone(), door.clone());
        std::thread::spawn(move || {
            let peer = c.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let ch = handshake(c, &keys);
            drop(place);
            let mut ch = match ch {
                Ok(ch) => ch,
                Err(e) => {
                    if door.may_say(unix_now()) {
                        log(&format!("{peer}: {e}"));
                    }
                    return;
                }
            };
            if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                live.fetch_sub(1, Ordering::SeqCst);
                return; // closed without a word
            }
            if let Err(e) = serve(&reg, &mut ch, unix_now) {
                log(&format!("{peer}: {e}"));
            }
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn handshake(c: TcpStream, keys: &Keypair) -> std::io::Result<Channel<TcpStream>> {
    c.set_nodelay(true).ok();
    c.set_read_timeout(Some(HANDSHAKE_WITHIN))?;
    c.set_write_timeout(Some(HANDSHAKE_WITHIN))?;
    let watch = c.try_clone()?;
    let mut ch = door::within(&watch, HANDSHAKE_WITHIN, || chan::accept(c, keys, |_| true))?;
    ch.stream().set_read_timeout(Some(IDLE))?;
    Ok(ch)
}
