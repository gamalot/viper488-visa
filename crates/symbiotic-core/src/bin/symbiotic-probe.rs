//! First contact, without VISA in the way: list adapters, open the first, IDENTIFY, probe
//! every address, and with a pad argument query *IDN?.
//!
//!     symbiotic-probe [pad]

use symbiotic_core::adapter::{self, Adapter};

fn main() {
    let list = adapter::list().unwrap_or_else(|e| panic!("{e}"));
    println!("adapters: {list:?}");
    let Some(first) = list.first() else { std::process::exit(2) };
    let a = match Adapter::open(&first.serial) {
        Ok(a) => a,
        Err(e) => {
            println!("open {}: {e}", first.serial);
            std::process::exit(3)
        }
    };
    println!("{:?}", a.info);
    let mut bus = a.lock();
    let found: Vec<u8> = (0..=30u8).filter(|&p| bus.probe(p).unwrap_or(false)).collect();
    println!("instruments: {found:?}   lines: {:#06x}", bus.lines().unwrap_or(0xFFFF));
    if let Some(pad) = std::env::args().nth(1).and_then(|s| s.parse::<u8>().ok()) {
        bus.write(pad, b"*IDN?\n", true, 2000).unwrap_or_else(|(n, e)| panic!("write after {n} bytes: {e}"));
        let mut out = Vec::new();
        loop {
            let r = bus.read(pad, 4096, None, 2000).unwrap_or_else(|e| panic!("read: {e}"));
            out.extend(&r.data);
            if r.timed_out || r.reason & symbiotic_core::proto::REASON_END != 0 {
                break;
            }
        }
        println!("*IDN? -> {:?}", String::from_utf8_lossy(&out));
    }
}
