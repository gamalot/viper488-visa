//! Alias sharing between machines: the encrypted channel (`chan`), the keys (`keys`), the registry's signed directory
//! (`dir`), the registry itself and its protocol (`registry`), a client for it (`regclient`), this machine as a member
//! (`member`), the relay of instrument operations (`peer`) and the resolution of names (`lookup`, `names`).

pub mod admin;
pub mod cache;
pub mod chan;
pub mod codec;
pub mod conf;
pub mod dir;
pub mod door;
pub mod keys;
pub mod lookup;
pub mod member;
pub mod names;
pub mod peer;
pub mod picstore;
pub mod regclient;
pub mod regserver;
pub mod registry;
pub mod usage;

#[cfg(test)]
mod tests;

/// TCP keepalive on a connection between two machines of the group: the first probe after `KEEPALIVE_IDLE` of quiet,
/// then one every `KEEPALIVE_EVERY`, the connection gone after `KEEPALIVE_TRIES` unanswered -- about a minute in all. A
/// machine that went without a word (asleep, off the network, powered off) sends no FIN, and without this a connection
/// to it, and the locks its session holds, would wait for ever.
pub const KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_secs(30);
pub const KEEPALIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
pub const KEEPALIVE_TRIES: u32 = 3;

pub fn keep_alive(s: &std::net::TcpStream) -> std::io::Result<()> {
    let k = socket2::TcpKeepalive::new().with_time(KEEPALIVE_IDLE).with_interval(KEEPALIVE_EVERY).with_retries(KEEPALIVE_TRIES);
    socket2::SockRef::from(s).set_tcp_keepalive(&k)
}
