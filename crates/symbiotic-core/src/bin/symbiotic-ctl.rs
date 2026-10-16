//! What is not VISA, from outside it: adapter settings through the daemon (started on demand),
//! the configured and discovered LAN instruments, the USBTMC instruments the daemon can see, and the
//! serial ports.
//!
//!     symbiotic-ctl list
//!     symbiotic-ctl [--board N] speed [low|normal|high]     (no level: only report it)
//!     symbiotic-ctl [--board N] ren [on|off]
//!     symbiotic-ctl stop                                    (ask the daemon to exit)
//!     symbiotic-ctl clients                                 (the processes and group machines connected to the daemon now)
//!     symbiotic-ctl locks                                   (the VISA locks held now, and by whom)
//!     symbiotic-ctl lan [list | add NAME | remove NAME]     (the LAN instruments viFindRsrc lists)
//!     symbiotic-ctl lan discover [on | off]                 (viFindRsrc also finds LAN instruments)
//!     symbiotic-ctl lan scan [GATEWAY-IP]                   (find LAN instruments now; with an IP, the instruments behind that gateway)
//!     symbiotic-ctl [--board N] ren [on|off]               (whether the adapter holds REN asserted; released by default)
//!     symbiotic-ctl [--board N] bus [SECONDS]               (the addresses with a listener, by the daemon's scan)
//!     symbiotic-ctl [--board N] forget                      (an adapter not attached: no longer listed, its number free)
//!     symbiotic-ctl usb                                     (the USBTMC instruments, and which are ours)
//!     symbiotic-ctl serial                                  (the serial ports, as ASRL resources)
//!     symbiotic-ctl alias [list | add NAME RESOURCE | remove NAME | bind NAME | unbind NAME]   (aliases for resource names)
//!     symbiotic-ctl bridge [list | add PORT [prologix|ar488] | remove PORT]   (PORT: COMn, or /dev/tty... on Linux)
//!                                                           (serial ports that are Prologix/AR488 GPIB boards)
//!     symbiotic-ctl share [status | join CODE [NAME] | on | off | add ALIAS | remove ALIAS | ping ALIAS | usage [-n N] | follow [on|off] | autostart [on|off] | group [list | set ALIAS RESOURCE [--keep-unit] | bind ALIAS | remove ALIAS | adopt [ALIAS...]] | pictures [sync | adopt]]
//!     symbiotic-ctl share registry COMMAND...               (the registry in this machine's daemon: token, members, claims, revoke, takeover)
//!     symbiotic-ctl trace [status | on | off | clear | show [-f]]   (I/O trace: what every program's VISA calls did)
//!     symbiotic-ctl log [-n LINES] [-f]                     (the end of the daemon's log; -f follows it)
//!     symbiotic-ctl diag [FOLDER]                           (a diagnostics bundle, a zip, in Downloads unless a folder is given)
//!
//! `speed` is the write direction's T1 settling time, a property of the whole adapter. It is not
//! a VISA feature and is not exposed as one. A level that is set is remembered (speedconf.rs) and
//! set again whenever the daemon opens that adapter; `speed normal` forgets it. `list` shows the
//! remembered levels. Because the daemon owns the adapter, this works
//! while applications are using it. Exit code: 0 ok, 1 usage, 2 no adapter, 3 busy (something
//! other than the daemon holds it), 4 refused or failed.
//!
//! `bridge` edits bridges.conf (prologix.rs): a port listed there is a GPIB board, its instruments
//! `GPIBn::<address>::INSTR`, and is no longer an ASRL resource. Nothing is sent to a port that is
//! not listed. A daemon that is running sees the change at its next listing.
//!
//! `lan` edits the list of configured LAN instruments and the discovery switch (lan/conf.rs) and
//! needs no daemon; `lan list` also says which of them answer now, and `lan scan` runs a discovery
//! whether or not it is switched on. `usb` lists every USBTMC instrument plugged in, and says which
//! are bound to WinUSB (and so Symbiotic's) and which are still on another driver.

use symbiotic_core::adapter::Error;
use symbiotic_core::client::Client;
use symbiotic_core::proto::*;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut board = 0u16;
    if args.first().map(String::as_str) == Some("--board") {
        board = args.get(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
        args.drain(..2);
    }
    let cmd = args.first().cloned().unwrap_or_else(|| usage());
    if cmd == "stop" {
        // Do not start a daemon just to stop it.
        if let Ok(mut c) = Client::connect_existing() {
            let _ = c.shutdown();
        }
        return;
    }
    if cmd == "clients" || cmd == "locks" {
        // Who holds the adapters, if a daemon runs; this program's own connection is not counted.
        // `locks`: only the connections that hold a lock, and what.
        let Ok(mut c) = Client::connect_existing() else {
            println!("no daemon is running");
            return;
        };
        let mut l = c.clients().unwrap_or_else(|e| fail(4, &e.to_string()));
        if cmd == "locks" {
            l.retain(|c| !c.locks.is_empty());
            if l.is_empty() {
                println!("no lock is held");
            }
            for c in l {
                for k in &c.locks {
                    println!("{k}, held by {}", c.who());
                }
            }
            return;
        }
        if l.is_empty() {
            println!("no other process is connected");
        }
        for c in l {
            let mut line = c.who();
            if let (Some(s), Some(i)) = (c.secs, c.idle) {
                line += &format!(", connected {}, idle {}", span(s), span(i));
            }
            if !c.locks.is_empty() {
                line += &format!(", holds {}", c.locks.join(", "));
            }
            println!("{line}");
        }
        return;
    }
    if cmd == "usb" {
        usb();
        return;
    }
    if cmd == "trace" {
        trace(&args[1..]);
        return;
    }
    if cmd == "log" {
        log(&args[1..]);
        return;
    }
    if cmd == "diag" {
        // Starts no daemon and opens nothing (diag.rs): what a user sends when something is wrong. No private key is in it.
        match symbiotic_core::diag::write_bundle(args.get(1).map(std::path::Path::new)) {
            Ok(p) => println!("{}\n(it holds serial numbers, addresses and aliases from this computer, and no private key: look before you send it)", p.display()),
            Err(e) => fail(4, &e.to_string()),
        }
        return;
    }
    if cmd == "serial" {
        serial();
        return;
    }
    if cmd == "bridge" {
        bridge(&args[1..]);
        return;
    }
    if cmd == "alias" {
        alias(&args[1..]);
        return;
    }
    if cmd == "share" {
        share(&args[1..]);
        return;
    }
    if cmd == "lan" {
        lan(&args[1..]);
        return;
    }
    let mut c = Client::connect(None).unwrap_or_else(|e| fail(4, &e.to_string()));
    match cmd.as_str() {
        "list" => {
            let l = c.list().unwrap_or_else(|e| fail(4, &e.to_string()));
            // A level other than normal is shown: it is set again every time the adapter is opened.
            let levels = symbiotic_core::speedconf::load();
            for (i, x) in l.iter().enumerate().filter(|(_, x)| !x.forgotten) {
                let d = &x.entry;
                let t1 = levels.get(&d.key()).map(|lv| format!("  T1 {} (remembered)", speed_name(*lv))).unwrap_or_default();
                let model = x.model.as_ref().map(|m| format!(" ({m})")).unwrap_or_default();
                println!("board {i}: {}{model} serial {} pid {:#06x}{}{t1}", d.kind.name(), d.serial, d.pid,
                         if x.present { "" } else { "  (not attached)" });
            }
            if l.iter().all(|x| x.forgotten) {
                std::process::exit(2);
            }
        }
        "speed" => {
            let level = match args.get(1) {
                None => SPEED_QUERY,
                Some(n) => speed_from_name(n).unwrap_or_else(|| usage()),
            };
            match c.speed(board, level) {
                Ok(l) => println!("board {board}: T1 level {}", speed_name(l)),
                Err(Error::NoBoard) => fail(2, &format!("no adapter for board {board}")),
                Err(Error::Busy) => fail(3, "the adapter is in use by a program other than the daemon"),
                Err(Error::Status(ST_UNSUPPORTED)) => fail(4, "this adapter has no selectable T1 level"),
                Err(e) => fail(4, &e.to_string()),
            }
        }
        "bus" => {
            // The daemon's scan of the board's addresses (what the window's tree and the heartbeat read); AGE is the oldest result taken, seconds.
            let age = args.get(1).map(|a| a.parse::<u32>().unwrap_or_else(|_| usage())).unwrap_or(12);
            match c.bus_scan(board, age) {
                Ok(r) => println!("board {board}: listeners at {:?} (found {} s ago)", r.pads, r.age),
                Err(Error::Busy) => fail(3, "another program is using the service: nothing was probed"),
                Err(Error::NoBoard) => fail(2, &format!("no adapter for board {board}")),
                Err(e) => fail(4, &e.to_string()),
            }
        }
        "forget" => match c.forget(board) {
            Ok(()) => println!("board {board}: forgotten"),
            Err(Error::Gone) => fail(4, &format!("board {board}'s adapter is attached: unplug it first")),
            Err(Error::NoBoard) => fail(2, &format!("no adapter for board {board}")),
            Err(e) => fail(4, &e.to_string()),
        },
        "ren" => {
            // Whether the adapter holds REN asserted (ren.conf): released unless it is set on. Setting it applies at once and is remembered.
            let key = c.list().unwrap_or_else(|e| fail(4, &e.to_string())).get(board as usize).map(|x| x.entry.key()).unwrap_or_else(|| fail(2, &format!("no adapter for board {board}")));
            match args.get(1).map(String::as_str) {
                None => {}
                Some(v @ ("on" | "off")) => {
                    let t = symbiotic_core::locks::Target::Gpib(board, 0, symbiotic_core::backend::NO_SAD);
                    if let Err(e) = c.ren(t, v == "on") {
                        fail(4, &format!("the adapter did not take it: {e}"));
                    }
                    symbiotic_core::renconf::set(&key, v == "on").unwrap_or_else(|e| fail(4, &e.to_string()));
                }
                Some(_) => usage(),
            }
            println!("board {board}: REN {}", if symbiotic_core::renconf::asserted(&key) { "asserted when opened (ren.conf)" } else { "released" });
        }
        _ => usage(),
    }
}

/// Every USBTMC device the OS lists, from its cached descriptors (nothing is opened): the ones
/// that are ours by their VISA name, the others with why not.
fn usb() {
    let all = symbiotic_core::usbtmc::list_all().unwrap_or_else(|e| fail(4, &e.to_string()));
    if all.is_empty() {
        println!("no USBTMC devices");
    }
    for f in all {
        let name = format!("USB0::0x{:04X}::0x{:04X}::{}::INSTR", f.vid, f.pid, f.serial);
        let what = format!("{}{}", f.product, if f.usb488 { ", USB488" } else { "" });
        match &f.not_ours {
            None => println!("{name}  ({what}; {})", if f.driver.is_empty() { "ours" } else { &f.driver }),
            Some(why) => println!("{name}  ({what}) not used: {why}"),
        }
    }
}

/// The serial ports, from the registry (on Linux, sysfs): nothing is opened, so this says nothing about what is on
/// them, or whether another program has one.
fn serial() {
    let l = symbiotic_core::serial::list().unwrap_or_else(|e| fail(4, &e.to_string()));
    if l.is_empty() {
        println!("no serial ports");
    }
    let bridges = symbiotic_core::prologix::configured();
    for f in l {
        if bridges.iter().any(|(b, _)| *b == f.n) {
            let (port, asrl) = (symbiotic_core::serial::port_name(f.n), symbiotic_core::resource::format_asrl(f.n));
            println!("{port}  ({}; a Prologix/AR488 bridge, so a GPIB board, not {asrl})", f.device);
        } else {
            println!("{}  ({}, {})", symbiotic_core::resource::format_asrl(f.n), symbiotic_core::serial::port_name(f.n), f.device);
        }
    }
}

/// aliases.conf (alias.rs): `DMM` opens `GPIB1::6::INSTR`, and viParseRsrcEx names it back.
fn alias(args: &[String]) {
    use symbiotic_core::alias;
    use symbiotic_core::share::names;
    // On a computer in a group, a network instrument's alias is the group's: one still kept here is moved there first.
    let moved = |report: Option<Result<Vec<(String, Result<String, String>)>, String>>| match report {
        None => {}
        Some(Err(e)) => println!("{e}"),
        Some(Ok(done)) => {
            for (a, r) in done {
                match r {
                    Ok(what) => println!("{a}: moved to the group ({what})"),
                    Err(e) => println!("{a}: not moved to the group ({e})"),
                }
            }
        }
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            moved(names::move_local());
            println!("{}", alias::path().map(|p| p.display().to_string()).unwrap_or_default());
            for (a, r) in alias::load() {
                match symbiotic_core::identity::get(&a) {
                    Some(i) => println!("  {a} = {r}   [{} {} serial {}]", i.vendor, i.model, i.serial),
                    None => println!("  {a} = {r}"),
                }
            }
            let group = names::group_lan();
            if !group.is_empty() {
                println!("the group's (network instruments; any computer of the group may change them)");
            }
            for g in group {
                match g.identity {
                    Some(i) => println!("  {} = {}   [{} {} serial {}]", g.alias, g.resource, i.vendor, i.model, i.serial),
                    None => println!("  {} = {}", g.alias, g.resource),
                }
            }
        }
        Some("add") => {
            let (Some(a), Some(r)) = (args.get(1), args.get(2)) else { usage() };
            names::set(a, Some(r)).unwrap_or_else(|e| fail(1, &e));
            let r = symbiotic_core::resource::canonical(r).unwrap_or_default();
            println!("{a} = {r}{}", if names::member().is_some() && names::is_lan(&r) { " (the group's)" } else { "" });
        }
        Some("remove") => {
            let Some(a) = args.get(1) else { usage() };
            names::set(a, None).unwrap_or_else(|e| fail(1, &e));
            println!("{a} removed");
        }
        Some("bind") => {
            // The alias is that one unit from now on: it follows the instrument when its address changes
            // and is never opened at a resource that has become another instrument (identity.rs).
            let Some(a) = args.get(1) else { usage() };
            println!("asking {a}'s instrument *IDN? once...");
            let id = names::bind(a).unwrap_or_else(|e| fail(1, &e));
            println!("{a} is {} {} serial {}", id.vendor, id.model, id.serial);
        }
        Some("unbind") => {
            let Some(a) = args.get(1) else { usage() };
            names::unbind(a).unwrap_or_else(|e| fail(4, &e));
            println!("{a} is a name for a resource again, not for one unit");
        }
        _ => usage(),
    }
}

fn share(args: &[String]) {
    use symbiotic_core::share::{conf, keys, member::Member, registry::PairingCode};
    let path = conf::ShareConf::path().unwrap_or_else(|| fail(4, "no configuration directory"));
    let load = || conf::ShareConf::load();
    let restart = "a running daemon reads this when it starts: `symbiotic-ctl stop`, and the next use starts it again";
    match args.first().map(String::as_str) {
        None | Some("status") => {
            let c = load();
            println!("{}", path.display());
            println!("  sharing:   {}", if c.share { "on" } else { "off" });
            println!("  group:     {}", if c.joined() { c.registry.as_str() } else { "(not joined: symbiotic-ctl share join CODE)" });
            println!("  name:      {}", c.name);
            println!("  serves on: {}", c.listen);
            println!("  follow:    {}", if c.follow { "on (takes over aliases whose USB instrument comes here)" } else { "off" });
            if let Some(r) = &c.registry_listen {
                println!("  registry in this daemon, listening on {r}");
            }
            let shares = conf::load_shares();
            let aliases = symbiotic_core::alias::load();
            for a in &shares {
                let note = match aliases.iter().find(|(x, _)| x.eq_ignore_ascii_case(a)) {
                    Some((_, r)) => r.clone(),
                    None => "(not in aliases.conf: not shared)".to_string(),
                };
                println!("  shares {a} = {note}");
            }
            if c.joined() {
                let keys = keys::load_or_create(&path.with_file_name("share.key"), false).unwrap_or_else(|e| fail(4, &e.to_string()));
                let m = Member::new(without_sharing(c), keys.noise, path.with_file_name("share.dir").into(), symbiotic_core::alias::load, conf::load_shares);
                match m.sync() {
                    Ok(r) => println!("  registry:  protocol {}", r.protocol),
                    Err(e) => println!("  registry: {e} (the cached directory below, if any)"),
                }
                if let Some(d) = m.directory() {
                    println!("  directory version {}:", d.version);
                    for cl in &d.claims {
                        let owner = if cl.owner == symbiotic_core::share::registry::GROUP_OWNER { "(group)" } else { d.member(&cl.owner).map_or("?", |m| m.name.as_str()) };
                        let mine = if cl.owner == m.public_key() { " (this machine)" } else { "" };
                        // What its owner last said of the instrument in its heartbeat.
                        let said = m.presence().into_iter().find(|p| p.key == cl.owner).and_then(|p| p.states.into_iter().find(|x| x.alias.eq_ignore_ascii_case(&cl.alias)));
                        let here = match said {
                            Some(x) => format!("  [{}, {} s ago]", match x.here { symbiotic_core::share::registry::Here::Yes => "there", symbiotic_core::share::registry::Here::No => "not there", _ => "not known" }, x.age),
                            None => String::new(),
                        };
                        println!("    {} -> {owner}{mine}  {}{here}", cl.alias, if cl.resource.is_empty() { "relay" } else { cl.resource.as_str() });
                    }
                }
            }
        }
        Some("join") => {
            let code = args.get(1).and_then(|c| PairingCode::parse(c)).unwrap_or_else(|| fail(1, "give the pairing code the registry printed (symshare1/...)"));
            let name = args.get(2).cloned().unwrap_or_else(|| load().name);
            let k = keys::load_or_create(&path.with_file_name("share.key"), false).unwrap_or_else(|e| fail(4, &e.to_string()));
            let (c, dir) = conf::join(&load(), &k.noise, &code, &name).unwrap_or_else(|e| fail(4, &e.to_string()));
            c.save(&path).unwrap_or_else(|e| fail(4, &e.to_string()));
            println!("joined {} as {name}: {} members, {} shared aliases", code.addr, dir.members.len(), dir.claims.len());
            // A network instrument's alias is the group's from now on.
            if let Some(r) = symbiotic_core::share::names::move_local() {
                match r {
                    Err(e) => println!("{e}"),
                    Ok(done) => {
                        for (a, r) in done {
                            println!("{a}: {}", r.map(|w| format!("moved to the group ({w})")).unwrap_or_else(|e| format!("not moved to the group ({e})")));
                        }
                    }
                }
            }
            println!("sharing is {}; `symbiotic-ctl share on` to serve aliases to the group ({restart})", if c.share { "on" } else { "off" });
        }
        Some(w @ ("on" | "off")) => {
            let mut c = load();
            if w == "on" && !c.joined() {
                fail(1, "not joined to a group: symbiotic-ctl share join CODE");
            }
            c.share = w == "on";
            c.save(&path).unwrap_or_else(|e| fail(4, &e.to_string()));
            println!("sharing {w}");
            // A daemon that serves a group has to be there without a program asking for it.
            match symbiotic_core::autostart::set(c.share || c.registry_listen.is_some()) {
                Ok(m) => println!("{m}"),
                Err(e) => println!("start at login: {e}"),
            }
            println!("{}", symbiotic_core::client::restart_daemon_if_idle(c.share || c.registry_listen.is_some()));
        }
        Some("follow") => {
            // Take an alias over when the USB instrument it is bound to has come to this computer and left its owner.
            let mut c = load();
            match args.get(1).map(String::as_str) {
                Some(w @ ("on" | "off")) => {
                    c.follow = w == "on";
                    c.save(&path).unwrap_or_else(|e| fail(4, &e.to_string()));
                    println!("follow {w}");
                    println!("{}", symbiotic_core::client::restart_daemon_if_idle(c.share || c.registry_listen.is_some()));
                }
                None => println!("follow is {}", if c.follow { "on" } else { "off" }),
                _ => usage(),
            }
        }
        Some("autostart") => {
            use symbiotic_core::autostart::{self, State};
            match args.get(1).map(String::as_str) {
                Some(w @ ("on" | "off")) => match autostart::set(w == "on") {
                    Ok(m) => println!("{m}"),
                    Err(e) => fail(4, &e),
                },
                None => {}
                _ => usage(),
            }
            match autostart::status() {
                State::On => println!("the daemon starts when you log in"),
                State::Off => println!("the daemon does not start at login"),
                State::Unsupported(why) => println!("start at login is not available: {why}"),
            }
        }
        Some(w @ ("add" | "remove")) => {
            let a = args.get(1).unwrap_or_else(|| usage());
            conf::set_shared(a, w == "add").unwrap_or_else(|e| fail(1, &e.to_string()));
            if w == "add" && symbiotic_core::alias::resolve(a).is_none() {
                println!("{a} is not in aliases.conf yet: it is shared once it is (symbiotic-ctl alias add {a} RESOURCE)");
            } else {
                println!("{a} {}", if w == "add" { "is shared" } else { "is no longer shared" });
            }
        }
        Some("pictures") => {
            // The group's pictures (the registry keeps them; this computer holds a copy in group-pictures/ in the configuration directory).
            let c = load();
            let keys = keys::load_or_create(&path.with_file_name("share.key"), false).unwrap_or_else(|e| fail(4, &e.to_string()));
            let m = Member::new(without_sharing(c), keys.noise, path.with_file_name("share.dir").into(), symbiotic_core::alias::load, conf::load_shares);
            let cache = symbiotic_core::paths::config_file("group-pictures/pictures.conf").map(symbiotic_core::pictures::Store::at).unwrap_or_else(|| fail(4, "no configuration directory"));
            match args.get(1).map(String::as_str) {
                None | Some("sync") => {
                    let changed = m.pic_sync(&cache).unwrap_or_else(|e| fail(4, &e.to_string()));
                    println!("{} (the copy is {})", if changed { "updated" } else { "nothing new" }, cache.dir().display());
                    for r in cache.load() {
                        println!("  {} = {}{}", r.pattern, r.file, if r.dark_keep { " dark=keep" } else { "" });
                    }
                }
                Some("adopt") => {
                    let local = symbiotic_core::pictures::Store::local().unwrap_or_else(|| fail(4, "no place for the pictures"));
                    let done = m.pic_adopt(&local, &cache).unwrap_or_else(|e| fail(4, &e.to_string()));
                    if done.is_empty() {
                        println!("no pictures of this computer's own to move");
                    }
                    for (pattern, problem) in done {
                        match problem {
                            None => println!("{pattern}: moved to the group"),
                            Some(e) => println!("{pattern}: skipped ({e})"),
                        }
                    }
                }
                Some(_) => usage(),
            }
        }
        Some("group") => {
            // The group's LAN aliases (a LAN instrument belongs to the group, not to a computer): any member may change them.
            let c = load();
            let keys = keys::load_or_create(&path.with_file_name("share.key"), false).unwrap_or_else(|e| fail(4, &e.to_string()));
            let m = Member::new(without_sharing(c), keys.noise, path.with_file_name("share.dir").into(), symbiotic_core::alias::load, conf::load_shares);
            let ok = |r: Result<(), symbiotic_core::share::regclient::Error>, what: String| match r {
                Ok(()) => println!("{what}"),
                Err(e) => fail(4, &e.to_string()),
            };
            match args.get(1).map(String::as_str) {
                None | Some("list") => {
                    let _ = m.sync();
                    let d = m.directory().unwrap_or_else(|| fail(4, "no directory: the registry has not been reached"));
                    for cl in d.claims.iter().filter(|c| c.owner == symbiotic_core::share::registry::GROUP_OWNER) {
                        let unit = symbiotic_core::identity::Identity::parse_line(&cl.identity).map(|i| format!("   [{} {} serial {}]", i.vendor, i.model, i.serial)).unwrap_or_default();
                        println!("{} = {}{unit}", cl.alias, cl.resource);
                    }
                }
                Some("set") => {
                    let (Some(a), Some(r)) = (args.get(2), args.get(3)) else { usage() };
                    // `--keep-unit`: the alias stays bound to its unit, which is now at this address (what following does by itself).
                    let keep = args.get(4).is_some_and(|f| f == "--keep-unit");
                    let unit = if keep {
                        let _ = m.sync();
                        m.directory().and_then(|d| d.claim(a).map(|c| c.identity.clone())).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    ok(m.group_set(a, r, &unit), format!("{a} is a group alias for {r}{}", if unit.is_empty() { format!(" (not bound to a unit: symbiotic-ctl share group bind {a})") } else { ", still bound to its unit".to_string() }));
                }
                Some("remove") => {
                    let a = args.get(2).unwrap_or_else(|| usage());
                    ok(m.group_clear(a), format!("{a} is no longer a group alias"));
                }
                Some("bind") => {
                    // The instrument is asked `*IDN?` once; from then on the alias follows that unit when it takes another address.
                    let a = args.get(2).unwrap_or_else(|| usage());
                    let _ = m.sync();
                    match m.group_bind(a, |r| symbiotic_core::identity::ask_idn(r, std::time::Duration::from_millis(3000))) {
                        Ok(id) => println!("{a} is {} {} serial {}: it follows that unit", id.vendor, id.model, id.serial),
                        Err(e) => fail(4, &e),
                    }
                }
                Some("adopt") => {
                    let names: Vec<String> = args[2..].to_vec();
                    let done = m
                        .adopt_lan(&names, |r| symbiotic_core::identity::ask_idn(r, std::time::Duration::from_millis(2000)), |a| {
                            symbiotic_core::alias::set(a, None)?;
                            conf::set_shared(a, false)
                        })
                        .unwrap_or_else(|e| fail(4, &e.to_string()));
                    if done.is_empty() {
                        println!("no alias for a LAN instrument here");
                    }
                    for (a, r) in done {
                        match r {
                            Ok(what) => println!("{a}: moved to the group ({what})"),
                            Err(e) => println!("{a}: not moved ({e})"),
                        }
                    }
                }
                Some(_) => usage(),
            }
        }
        Some("usage") => {
            // Who used this machine's shared instruments, newest last.
            let n = match args.get(1).map(String::as_str) {
                Some("-n") => args.get(2).and_then(|n| n.parse().ok()).unwrap_or_else(|| usage()),
                None => 20,
                _ => usage(),
            };
            let p = symbiotic_core::share::usage::path();
            let recs = symbiotic_core::share::usage::read_from(&p, n);
            println!("{}", p.display());
            if recs.is_empty() {
                println!("  nobody has used this machine's shared instruments (since the file began)");
            }
            let offset = symbiotic_core::iotrace::local_offset_s();
            for r in recs {
                println!("  {}", r.show(offset));
            }
        }
        Some("ping") => {
            // Reach a shared alias the way an application will, and ask its owner's daemon whether
            // the instrument answers (a PROBE: nothing is written to it).
            let a = args.get(1).unwrap_or_else(|| usage());
            let c = load();
            let keys = keys::load_or_create(&path.with_file_name("share.key"), false).unwrap_or_else(|e| fail(4, &e.to_string()));
            let m = Member::new(without_sharing(c), keys.noise, path.with_file_name("share.dir").into(), symbiotic_core::alias::load, conf::load_shares);
            if let Err(e) = m.sync() {
                println!("registry: {e} (using the cached directory)");
            }
            let (mut ch, opened) = m.connect_peer(a).unwrap_or_else(|e| fail(4, &e.to_string()));
            println!("{a}: {} on the owner (reached over an encrypted channel)", opened.resource);
            let q = symbiotic_core::ipc::Req { op: symbiotic_core::ipc::op::PROBE, tmo: 2000, ..Default::default() };
            let r = symbiotic_core::share::peer::call(&mut ch, &q, opened.header).unwrap_or_else(|e| fail(4, &e.to_string()));
            match r.error() {
                None => println!("the instrument answers"),
                Some(e) => println!("the owner's daemon says: {e:?}"),
            }
        }
        Some("registry") => {
            let line = args[1..].join(" ");
            let mut c = Client::connect(None).unwrap_or_else(|e| fail(4, &e.to_string()));
            match c.share_admin(&line) {
                Ok(t) => print!("{t}"),
                Err(e) => fail(4, &e.to_string()),
            }
        }
        _ => usage(),
    }
}

/// `status` looks at the group without joining in: sharing off, whatever the file says.
fn without_sharing(mut c: symbiotic_core::share::conf::ShareConf) -> symbiotic_core::share::conf::ShareConf {
    c.share = false;
    c
}

fn bridge(args: &[String]) {
    use symbiotic_core::prologix::{self, Dialect};
    let port = |a: Option<&String>| {
        let a = a.unwrap_or_else(|| usage());
        symbiotic_core::serial::port_number(a)
            .unwrap_or_else(|| fail(1, &format!("{a:?} is not COM<n> (or on Linux a tty such as /dev/ttyACM0)")))
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let p = prologix::conf_path().map(|p| p.display().to_string()).unwrap_or_default();
            println!("{p}");
            let ports = symbiotic_core::serial::list().unwrap_or_default();
            for (n, d) in prologix::configured() {
                let there = if ports.iter().any(|f| f.n == n) { "" } else { "  (not attached)" };
                let port = symbiotic_core::serial::port_name(n);
                println!("  {port}  {}{there}", d.map(|d| d.name()).unwrap_or("dialect found on connecting"));
            }
        }
        Some("add") => {
            let n = port(args.get(1));
            let d = args.get(2).map(|x| Dialect::from_name(x).unwrap_or_else(|| fail(1, &format!("{x:?} is not prologix or ar488"))));
            prologix::set_conf(n, &args[1], Some(d)).unwrap_or_else(|e| fail(4, &e.to_string()));
            println!("{} is a bridge now: its instruments are a GPIB board (symbiotic-ctl list)", symbiotic_core::serial::port_name(n));
        }
        Some("remove") => {
            let n = port(args.get(1));
            prologix::set_conf(n, &args[1], None).unwrap_or_else(|e| fail(4, &e.to_string()));
            println!("{} is a serial port again ({})", symbiotic_core::serial::port_name(n), symbiotic_core::resource::format_asrl(n));
        }
        _ => usage(),
    }
}

fn lan(args: &[String]) {
    use symbiotic_core::lan::{conf, name};
    let p = conf::path().unwrap_or_else(|| fail(4, "no place for lan.conf (LOCALAPPDATA or HOME is not set)"));
    let target = |n: Option<&String>| {
        let n = n.unwrap_or_else(|| usage());
        name::parse(n).unwrap_or_else(|| fail(1, &format!("{n:?} is not a TCPIP resource name")))
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            println!("{}  (discovery {})", p.display(), if conf::discover_in(&text) { "on" } else { "off" });
            for r in conf::check_all(conf::load_from(&p), conf::REACH) {
                match &r.result {
                    Ok(a) => println!("  {}  (answered at {a} in {} ms)", r.target.name(), r.took.as_millis()),
                    Err(e) => println!("  {}  (not answering: {e}, {} ms)", r.target.name(), r.took.as_millis()),
                }
            }
        }
        Some("add") => {
            let t = target(args.get(1));
            match conf::add(&p, &t) {
                Ok(true) => println!("added {}", t.name()),
                Ok(false) => println!("{} is already listed", t.name()),
                Err(e) => fail(4, &format!("{}: {e}", p.display())),
            }
        }
        Some("discover") => match args.get(1).map(String::as_str) {
            None => {
                let on = conf::discover_in(&std::fs::read_to_string(&p).unwrap_or_default());
                println!("discovery is {}", if on { "on" } else { "off" });
            }
            Some(w @ ("on" | "off")) => match conf::set_discover(&p, w == "on") {
                Ok(_) => println!("discovery is {w}"),
                Err(e) => fail(4, &format!("{}: {e}", p.display())),
            },
            _ => usage(),
        },
        Some("scan") => {
            use symbiotic_core::lan::{discover, vxi11};
            let listed: Vec<String> = conf::load_from(&p).iter().map(|t| t.name()).collect();
            if let Some(ip) = args.get(1) {
                // Find the instruments behind one gateway: user-initiated, it addresses the bus.
                let found = match vxi11::find_listeners(ip, "gpib0", std::time::Duration::from_secs(3), |_| {}) {
                    Ok(f) => f,
                    Err(symbiotic_core::lan::Error::Unsupported) => fail(2, "this gateway has no ATN control, so it cannot look for instruments: add the addresses by hand"),
                    Err(e) => fail(2, &format!("{ip}: {e}")),
                };
                for a in &found {
                    let n = format!("TCPIP0::{ip}::gpib0,{a}::INSTR");
                    println!("  {n}{}", if listed.contains(&n) { "  (configured)" } else { "" });
                }
                if found.is_empty() {
                    println!("  no listener on addresses 1-30");
                }
                println!("(REN was left alone: an instrument addressed while REN is asserted goes to Remote)");
                return;
            }
            for i in discover::interfaces() {
                println!("interface {} (broadcast {})", i.addr, i.broadcast);
            }
            let found = discover::discover_full(discover::WAIT);
            for g in &found.gateways {
                println!("  gateway {} (bus address {}): `symbiotic-ctl lan scan {}` looks for its instruments", g.ip, g.address, g.ip);
            }
            for t in &found.targets {
                println!("  {}{}", t.name(), if listed.contains(&t.name()) { "  (configured)" } else { "" });
            }
            if found.targets.is_empty() && found.gateways.is_empty() {
                println!("  nothing answered");
            }
        }
        Some("remove") => {
            let t = target(args.get(1));
            match conf::remove(&p, &t) {
                Ok(true) => println!("removed {}", t.name()),
                Ok(false) => fail(2, &format!("{} is not listed", t.name())),
                Err(e) => fail(4, &format!("{}: {e}", p.display())),
            }
        }
        _ => usage(),
    }
}

/// The I/O trace (iotrace.rs): each process using the library writes its own records while it is on; `show` merges them
/// in time order, `-f` follows new ones.
fn trace(args: &[String]) {
    use symbiotic_core::iotrace::{self, Cursor};
    let dir = iotrace::dir();
    let fail_io = |e: std::io::Error| -> ! { fail(4, &format!("{}: {e}", dir.display())) };
    match args.first().map(String::as_str).unwrap_or("status") {
        "status" => println!("I/O trace is {} ({})", if iotrace::is_on(&dir) { "on" } else { "off" }, dir.display()),
        "on" => {
            iotrace::set_on(&dir, true).unwrap_or_else(|e| fail_io(e));
            println!("I/O trace on: earlier records cleared; programs start writing within a second ({})", dir.display());
        }
        "off" => iotrace::set_on(&dir, false).unwrap_or_else(|e| fail_io(e)),
        "clear" => iotrace::clear(&dir).unwrap_or_else(|e| fail_io(e)),
        "show" => {
            let follow = args.get(1).is_some_and(|a| a == "-f" || a == "--follow");
            let mut c = Cursor::default();
            let mut named = std::collections::HashSet::new();
            let offset = iotrace::local_offset_s();
            loop {
                let recs = c.next(&dir);
                if recs.iter().any(|r| !named.contains(&r.pid)) {
                    let progs = iotrace::programs(&dir);
                    for r in &recs {
                        if named.insert(r.pid) {
                            println!("# process {} is {}", r.pid, progs.get(&r.pid).map(String::as_str).unwrap_or("?"));
                        }
                    }
                }
                for r in &recs {
                    println!("{}", iotrace::show(r, offset, iotrace::status_name));
                }
                if !follow {
                    if named.is_empty() {
                        println!("no records ({}; I/O trace is {})", dir.display(), if iotrace::is_on(&dir) { "on" } else { "off" });
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        _ => usage(),
    }
}

/// The end of the daemon's log, on this machine's clock; `-f` prints what is written after, as it comes.
fn log(args: &[String]) {
    let mut n = 50usize;
    let mut follow = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-f" | "--follow" => follow = true,
            "-n" => n = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            _ => usage(),
        }
    }
    let path = symbiotic_core::paths::log_file();
    let mut out = std::io::stdout();
    let text = symbiotic_core::diag::log_tail(n);
    if text.is_empty() {
        println!("nothing in {} yet", path.display());
    }
    let _ = symbiotic_core::diag::print_log(&mut out, &text);
    if !follow {
        return;
    }
    // Follow by size: what is past where we were; a file that is shorter (set aside as .1) is read from its start.
    let mut at = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < at {
            at = 0;
        }
        if len > at {
            if let Ok(b) = std::fs::read(&path) {
                let new = String::from_utf8_lossy(&b[at as usize..]).into_owned();
                // Whole lines only; a line still being written is read next time.
                let whole = new.rfind('\n').map_or(0, |i| i + 1);
                let _ = symbiotic_core::diag::print_log(&mut out, &new[..whole]);
                at += whole as u64;
            }
        }
    }
}

/// A number of seconds as a person reads it: "45 s", "12 min", "3 h 5 min".
fn span(s: u64) -> String {
    match s {
        0..=99 => format!("{s} s"),
        100..=5999 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

fn usage() -> ! {
    eprintln!("usage: symbiotic-ctl list\n       symbiotic-ctl [--board N] speed [low|normal|high]\n       symbiotic-ctl [--board N] ren [on|off]\n       symbiotic-ctl [--board N] bus [MAX-AGE-SECONDS]\n       symbiotic-ctl [--board N] forget\n       symbiotic-ctl stop\n       symbiotic-ctl clients\n       symbiotic-ctl locks\n       symbiotic-ctl lan [list | add NAME | remove NAME | discover [on|off] | scan [GATEWAY-IP]]\n       symbiotic-ctl usb\n       symbiotic-ctl serial\n       symbiotic-ctl alias [list | add NAME RESOURCE | remove NAME | bind NAME | unbind NAME]\n       symbiotic-ctl bridge [list | add PORT [prologix|ar488] | remove PORT]   (PORT: COMn, or /dev/tty... on Linux)\n       symbiotic-ctl share [status | join CODE [NAME] | on | off | add ALIAS | remove ALIAS | ping ALIAS | usage [-n N] | follow [on|off] | autostart [on|off] | group [list | set ALIAS RESOURCE [--keep-unit] | bind ALIAS | remove ALIAS | adopt [ALIAS...]] | pictures [sync | adopt] | registry COMMAND...]\n       symbiotic-ctl trace [status | on | off | clear | show [-f]]\n       symbiotic-ctl log [-n LINES] [-f]\n       symbiotic-ctl diag [FOLDER]");
    std::process::exit(1)
}

fn fail(code: i32, msg: &str) -> ! {
    eprintln!("symbiotic-ctl: {msg}");
    std::process::exit(code)
}
