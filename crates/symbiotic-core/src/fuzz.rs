//! Every decoder of what another program or machine sends, and every parser of a file a user may have
//! edited, given random and damaged input: none may panic, run away or hang. Stable Rust, no cargo-fuzz:
//! a small generator of its own, so `cargo test` (and tools/check.sh) runs it every time, the same
//! inputs each run. Each damaged input starts from a valid encoding, so the decoders get past their
//! first checks; the damage is what a careless or hostile peer does -- a bit flipped, a length made
//! huge, the end cut off, bytes put in.

use std::time::{Duration, Instant};

/// xorshift64*: the same sequence each run, so a failure can be run again.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// `sample`, damaged in one to three ways.
fn damage(r: &mut Rng, sample: &[u8]) -> Vec<u8> {
    let mut v = sample.to_vec();
    for _ in 0..1 + r.below(3) {
        match r.below(7) {
            0 if !v.is_empty() => {
                let i = r.below(v.len());
                v[i] ^= 1 << r.below(8);
            }
            1 if !v.is_empty() => {
                let i = r.below(v.len());
                v[i] = r.next() as u8;
            }
            2 => v.truncate(r.below(v.len() + 1)),
            3 => {
                let i = r.below(v.len() + 1);
                let n = 1 + r.below(16);
                let add = r.bytes(n);
                v.splice(i..i, add);
            }
            // A length or count made huge, small or odd: four bytes, either byte order.
            4 if v.len() >= 4 => {
                let i = r.below(v.len() - 3);
                let x: u32 = [u32::MAX, 0x7FFF_FFFF, 0x8000_0000, 0x0001_0000, 0xFFFF, 0][r.below(6)];
                let b = if r.below(2) == 0 { x.to_le_bytes() } else { x.to_be_bytes() };
                v[i..i + 4].copy_from_slice(&b);
            }
            5 if !v.is_empty() => {
                let a = r.below(v.len());
                let b = a + r.below(v.len() - a);
                let piece = v[a..=b.min(v.len() - 1)].to_vec();
                let at = r.below(v.len() + 1);
                v.splice(at..at, piece);
            }
            _ => {
                let i = r.below(v.len() + 1);
                v.truncate(i);
                let n = r.below(8);
                v.extend(r.bytes(n));
            }
        }
    }
    v
}

/// Random text in the alphabet configuration files and names are made of, and what they must survive:
/// separators, comments, control characters, quotes, braces, non-ASCII.
fn text(r: &mut Rng, max: usize) -> String {
    const PIECES: &[&str] = &[
        "GPIB", "USB", "ASRL", "TCPIP", "INSTR", "INTFC", "SOCKET", "hislip0", "inst0", "::", ":", "0", "1", "15", "30", "65535",
        "0x", "F4EC", "=", " ", "\t", "\n", "\r", "#", ";", ",", "\"", "'", "\\", "{", "}", "[", "]", "(", ")", "*", "?", "+", "|",
        "&&", "||", "!", "==", "!=", ">=", "VI_ATTR_INTF_NUM", "VI_ATTR_MANF_NAME", "VI_ATTR_TMO_VALUE", "-", ".", "/", "%",
        "\0", "\u{7}", "\u{1b}", "\u{e9}", "\u{6d4b}", "\u{202e}", "\u{feff}", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];
    let mut s = String::new();
    for _ in 0..r.below(max) {
        if r.below(4) == 0 {
            s.push(char::from_u32(r.below(0x3000) as u32).unwrap_or('x'));
        } else {
            s.push_str(PIECES[r.below(PIECES.len())]);
        }
    }
    s
}

/// Feed `n` inputs to `f`: half random bytes, half damaged samples. A panic fails the test where it
/// is; a run that takes far longer than its share is a decoder that loops.
fn bytes_to(name: &str, n: usize, samples: &[Vec<u8>], f: impl Fn(&[u8])) {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ name.len() as u64);
    let t0 = Instant::now();
    for i in 0..n {
        let input = if i % 2 == 0 || samples.is_empty() {
            let len = r.below(200);
            r.bytes(len)
        } else {
            let s = r.below(samples.len());
            damage(&mut r, &samples[s])
        };
        f(&input);
    }
    assert!(t0.elapsed() < Duration::from_secs(120), "{name}: {n} inputs took {:?}", t0.elapsed());
}

fn text_to(name: &str, n: usize, f: impl Fn(&str)) {
    let mut r = Rng(0xD1B5_4A32_D192_ED03 ^ name.len() as u64);
    let t0 = Instant::now();
    for _ in 0..n {
        f(&text(&mut r, 60));
    }
    assert!(t0.elapsed() < Duration::from_secs(120), "{name}: {n} inputs took {:?}", t0.elapsed());
}

const N: usize = 20_000;

#[test]
fn the_daemons_wire() {
    use crate::ipc::{HEADER_U16, HEADER_U32, Req, Resp};
    let mut samples = Vec::new();
    for header in [HEADER_U16, HEADER_U32] {
        let mut b = Vec::new();
        Req { op: 3, board: 1, pad: 15, data: b"*IDN?\n".to_vec(), ..Default::default() }.write_as(&mut b, header).unwrap();
        samples.push(b);
    }
    bytes_to("ipc request", N, &samples, |b| {
        let _ = Req::read_as(&mut &b[..], HEADER_U16);
        let _ = Req::read_as(&mut &b[..], HEADER_U32);
    });
    let mut b = Vec::new();
    Resp::ok(7, b"KEITHLEY,2015\n".to_vec()).write_to(&mut b).unwrap();
    bytes_to("ipc response", N, &[b], |b| {
        if let Ok(r) = Resp::read_from(&mut &b[..]) {
            let _ = r.error();
        }
    });
}

#[test]
fn the_adapters_and_the_serial_status() {
    let mut h = crate::proto::request_header(1, 0, 15, 7, 2000, 3).to_vec();
    h.extend_from_slice(b"abc");
    bytes_to("viper488 response header", N, &[h], |b| {
        let _ = crate::proto::Response::parse_header(b);
    });
    bytes_to("serial status", N, &[], |b| {
        let _ = crate::serial::Status::decode(b);
        let _ = crate::serial::parse_stb(b);
    });
}

#[test]
fn what_the_adapters_answer() {
    // Viper488: a READ response, whole.
    let mut v = vec![crate::proto::OP_READ, 0, 0, 0, 7, 0, 0, 0, 3, 0, 0, 0];
    v.extend_from_slice(b"ab\n");
    bytes_to("viper488 response", N, &[v], |b| {
        let _ = crate::adapter::complete(b, crate::proto::OP_READ, 7);
    });
    // NI: data blocks and a status block; a register read.
    let mut ni = vec![0x36];
    ni.extend_from_slice(&[b'x'; 15]);
    ni.extend_from_slice(&[0x38, 0x21, 0x00, 0x00, 0xFF, 0xFF, 0, 0]);
    let regs = vec![0x34, 1, 2, 3, 0x34, 4, 0, 0, 0x35, 4];
    bytes_to("ni replies", N, &[ni, regs], |b| {
        if let Some(i) = crate::ni::find_read_status(b) {
            let _ = crate::ni::status(&b[i..]);
        }
        let _ = crate::ni::status(b);
        let _ = crate::ni::answers(&[0x0a], b);
        for n in [0, 1, 3, 4, 7] {
            let _ = crate::ni::parse_register_read(b, n);
        }
    });
    // Keysight 82357B: a reply, and the firmware file.
    bytes_to("82357b replies", N, &[vec![!0x0Eu8, 0, 1, 2]], |b| {
        let _ = crate::ks::check(b, 0x0E);
        let _ = crate::ks::abort_accepted(b, true);
        let _ = crate::ks::abort_accepted(b, false);
    });
    text_to("82357b firmware", N, |t| {
        let _ = crate::ks::parse_hex(t);
        let _ = crate::ks::parse_hex(&format!(":{t}\n:00000001FF\n"));
    });
    // CONTEC: a response frame.
    let mut f = 0x7E7E_7E7Eu32.to_le_bytes().to_vec();
    f.extend_from_slice(&[0x30, 0x76, 0, 0, 4, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]);
    bytes_to("contec frames", N, &[f], |b| {
        let mut acc = b.to_vec();
        let _ = crate::contec::frame_done(&mut acc);
        let _ = crate::contec::frame_body(acc, 0x7630);
    });
}

#[test]
fn what_lan_instruments_send() {
    use crate::lan::{rpc, xdr};
    // An ONC RPC reply, accepted, with a body.
    let mut reply = xdr::Encoder::new();
    reply.u32(7).u32(1).u32(0).u32(0).u32(0).u32(0).opaque(b"body");
    let reply = reply.into_vec();
    bytes_to("rpc reply", N, std::slice::from_ref(&reply), |b| {
        let _ = rpc::parse_reply(b, 7);
        let _ = rpc::server::parse_call(b);
        let mut d = xdr::Decoder::new(b);
        while d.opaque(1 << 20).is_some() {}
    });
    // mDNS: a PTR answer whose name points back at itself, and an ordinary one.
    let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    m.extend_from_slice(&[5, b'_', b'v', b'x', b'i', b'1', 0, 0, 12, 0, 1, 0, 0, 0, 10, 0, 2, 0xC0, 12]);
    let mut looped = m.clone();
    looped.extend_from_slice(&[0xC0, 12]);
    bytes_to("mdns records", N, &[m, looped], |b| {
        let _ = crate::lan::discover::records(b);
    });
    text_to("lan names", N, |s| {
        let _ = crate::lan::name::parse(s);
        let _ = crate::lan::conf::parse(s);
    });
}

#[test]
fn what_other_machines_send() {
    use crate::share::{dir, peer, registry};
    let key = [7u8; 32];
    let d = dir::Directory {
        version: 3,
        members: vec![dir::Member { key, name: "BENCH-PC".into() }],
        claims: vec![dir::Claim { alias: "K2015".into(), owner: key, kind: dir::Kind::Relay, resource: "GPIB0::15::INSTR".into(), identity: String::new() }],
    };
    let signed = dir::Signed { sig: [1; 64], body: d.encode() };
    bytes_to("directory", N, &[d.encode()], |b| {
        let _ = dir::Directory::decode(b);
    });
    bytes_to("signed directory", N, &[signed.encode()], |b| {
        let _ = dir::Signed::decode(b);
    });
    let requests = [
        registry::Request::Claim { alias: "DMM".into(), kind: dir::Kind::Relay, resource: "GPIB0::15::INSTR".into(), identity: "KEITHLEY\t2015\t0863839".into() },
        registry::Request::Heartbeat {
            addrs: vec!["198.18.68.10:7488".into()],
            absent: vec!["SDG".into()],
            states: vec![registry::AliasState { alias: "K2015".into(), here: registry::Here::Unknown, age: 5 }],
        },
        registry::Request::PicSave { pattern: "K2015".into(), dark_keep: true, png: vec![0x89, b'P', b'N', b'G'], replaces: String::new(), orig_ext: "jpg".into(), original: vec![1, 2], edit: "{}".into() },
        registry::Request::List,
    ];
    let samples: Vec<Vec<u8>> = requests.iter().map(|q| q.encode()).collect();
    bytes_to("registry request", N, &samples, |b| {
        let _ = registry::Request::decode(b);
        let _ = registry::Request::decode_as(b, registry::PROTOCOL);
    });
    let responses = [
        registry::Response::Listed { dir: signed.clone(), presence: Vec::new() },
        registry::Response::PicState { version: 2, conf: "K2015 = k.png\n".into(), files: vec![("k.png".into(), 2)] },
        registry::Response::Err { code: registry::Code::Bad, msg: "no".into() },
    ];
    let samples: Vec<Vec<u8>> = responses.iter().map(|q| q.encode()).collect();
    bytes_to("registry response", N, &samples, |b| {
        let _ = registry::Response::decode(b);
    });
    let opened = peer::Opened { target: crate::locks::Target::Gpib(0, 15, 255), resource: "GPIB0::15::INSTR".into(), usb: None, header: crate::ipc::HEADER_U32 };
    bytes_to("opened", N, &[peer::encode_opened(&opened)], |b| {
        let _ = peer::decode_opened(b);
    });
    text_to("pairing code", N, |s| {
        let _ = registry::PairingCode::parse(s);
    });
}

#[test]
fn names_and_search_expressions() {
    use crate::resource;
    text_to("resource names", N, |s| {
        let _ = resource::parse(s);
        let _ = resource::parse_intfc(s);
        let _ = resource::parse_usb(s);
        let _ = resource::parse_asrl(s);
        let _ = resource::canonical(s);
    });
    text_to("search expressions", N, |s| {
        if resource::valid(s) {
            let _ = resource::matches(s, "GPIB0::15::INSTR");
            let _ = resource::candidates(2, s);
            if let Some(f) = resource::attr_filter(s) {
                let _ = f.eval(&|_| Some(resource::AttrValue::Num(1)));
            }
        }
        let _ = resource::may_match_gpib(s);
        let _ = resource::may_match_prefix(s, "USB");
    });
    // Deeper and longer than the text above makes them: a run of `!` and `(` (the parser and the
    // evaluation recurse), a long `||` chain (a tree as deep as it is long), groups in the regex.
    let deep = format!("?*{{{}VI_ATTR_INTF_NUM == 0{}}}", "!(".repeat(5000), ")".repeat(5000));
    assert!(!resource::valid(&deep));
    let ok = format!("?*{{{}VI_ATTR_INTF_NUM == 0{}}}", "!(".repeat(20), ")".repeat(20));
    assert!(resource::valid(&ok), "a nesting anyone writes");
    let long = format!("?*{{{}VI_ATTR_INTF_NUM == 0}}", "VI_ATTR_INTF_NUM == 1 || ".repeat(100_000));
    assert!(!resource::valid(&long));
    let groups = format!("{}GPIB0{}", "(".repeat(100_000), ")".repeat(100_000));
    let _ = resource::valid(&groups);
    let _ = resource::matches(&groups, "GPIB0");
    let _ = resource::candidates(2, &groups);
}

#[test]
fn files_a_user_may_have_edited() {
    text_to("configuration files", N, |s| {
        let _ = crate::alias::parse(s);
        let _ = crate::identity::parse(s);
        let _ = crate::identity::Identity::parse_line(s);
        let _ = crate::instruments::parse(s);
        let _ = crate::speedconf::parse(s);
        let _ = crate::renconf::parse(s);
        let _ = crate::daemonconf::parse_release_idle(s);
        let _ = crate::pictures::parse(s);
        let _ = crate::prologix::parse_conf(s);
        let _ = crate::share::conf::ShareConf::parse(s);
        let _ = crate::share::conf::parse_shares(s);
    });
    text_to("trace lines", N, |s| {
        let _ = crate::iotrace::Record::parse(s);
        let _ = crate::iotrace::unescape(s);
    });
}

/// A server on this machine that answers whatever it is sent with `answer(request)`, damaged three times
/// in four, then closes: what a broken or hostile instrument sends a client that connects to it.
fn hostile(seed: u64, answer: impl Fn(&[u8]) -> Vec<Vec<u8>> + Send + Sync + 'static) -> u16 {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let answer = std::sync::Arc::new(answer);
    std::thread::spawn(move || {
        for (i, s) in l.incoming().enumerate() {
            let Ok(mut s) = s else { continue };
            let answer = answer.clone();
            std::thread::spawn(move || {
                let mut r = Rng(seed ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9));
                let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                // A few turns: the handshake, then what a read asks for.
                for _ in 0..4 {
                    let mut req = [0u8; 4096];
                    let n = match s.read(&mut req) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    for m in answer(&req[..n]) {
                        let out = if r.below(4) == 0 { m } else { damage(&mut r, &m) };
                        if s.write_all(&out).is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    port
}

/// Open and read, against `port`, `n` times: each ends, and in time; and some get to the end (the
/// read's code was reached, not only the handshake's).
fn connect_to(name: &str, n: usize, f: impl Fn() -> Result<(), crate::lan::Error>) {
    let (t0, mut whole) = (Instant::now(), 0);
    for _ in 0..n {
        let t = Instant::now();
        whole += f().is_ok() as usize;
        assert!(t.elapsed() < Duration::from_secs(5), "{name}: one open and read took {:?}", t.elapsed());
    }
    assert!(t0.elapsed() < Duration::from_secs(120), "{name}: {n} took {:?}", t0.elapsed());
    assert!(whole > 0, "{name}: none of {n} got through the handshake and a read");
    eprintln!("{name}: {whole} of {n} opened and read");
}

#[test]
fn what_a_hostile_instrument_sends() {
    use crate::lan::{hislip, vxi11, xdr, Deadline};
    let within = Duration::from_millis(300);
    // HiSLIP: the handshake's answers (sync and async), the maximum message size, data and its end.
    let hs = |ty: u8, control: u8, param: u32, payload: &[u8]| {
        let mut m = b"HS".to_vec();
        m.extend_from_slice(&[ty, control]);
        m.extend_from_slice(&param.to_be_bytes());
        m.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        m.extend_from_slice(payload);
        m
    };
    let port = hostile(1, move |req| {
        match req.get(2) {
            Some(0) => vec![hs(1, 0, 0x0100_0007, &[])],                    // Initialize: InitializeResponse
            Some(17) => vec![hs(18, 0, 0, &[])],                            // AsyncInitialize: its response
            Some(15) => vec![hs(16, 0, 0, &(1u64 << 20).to_be_bytes())],    // AsyncMaximumMessageSize
            _ => vec![hs(6, 0, 0xFFFF_FF00, b"KEITHLEY,"), hs(7, 0, 0xFFFF_FF00, b"2015\n")],
        }
    });
    connect_to("hislip", 300, || {
        let mut c = hislip::Conn::open("127.0.0.1", port, "hislip0", within)?;
        c.write(b"*IDN?\n", true, Deadline::after(Some(within))).map_err(|(_, e)| e)?;
        c.read(1 << 16, Some(b'\n'), false, Deadline::after(Some(within)))?;
        Ok(())
    });

    // VXI-11: replies to the calls, with the call's own xid, so that some get past the first check.
    let port = hostile(2, |req| {
        let xid = req.get(4..8).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0);
        let proc_num = req.get(24..28).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0);
        let mut e = xdr::Encoder::new();
        e.u32(xid).u32(1).u32(0).u32(0).u32(0).u32(0);
        match proc_num {
            10 => e.u32(0).u32(9).u32(0).u32(1024),            // create_link: no error, link 9, max_recv
            11 => e.u32(0).u32(6),                             // device_write: no error, 6 bytes taken
            12 => e.u32(0).u32(4).opaque(b"KEITHLEY,2015\n"), // device_read: no error, END, the data
            _ => e.u32(0),
        };
        let body = e.into_vec();
        let mut rec = ((body.len() as u32) | 0x8000_0000).to_be_bytes().to_vec();
        rec.extend_from_slice(&body);
        vec![rec]
    });
    connect_to("vxi-11", 300, || {
        let mut l = vxi11::Link::open("127.0.0.1", Some(port), "inst0", within, None)?;
        l.write(b"*IDN?\n", true, Deadline::after(Some(within))).map_err(|(_, e)| e)?;
        l.read(1 << 16, Some(b'\n'), false, Deadline::after(Some(within)))?;
        Ok(())
    });
}
