//! The settings outside VISA, as `symbiotic-ctl` has them (the same `symbiotic-core` functions): a board's T1 level,
//! the LAN instruments (lan.conf, discovery, a scan), which serial ports are Prologix/AR488 controllers (bridges.conf),
//! the USBTMC devices plugged in, aliases (aliases.conf), the service, and on Windows the three registrations.

use serde::Serialize;
use symbiotic_core::client::Client;
use symbiotic_core::lan::{conf, discover, name};
use symbiotic_core::proto::{speed_from_name, speed_name};
use symbiotic_core::share::names;
use symbiotic_core::{alias, pictures, prologix, resource, serial, usbtmc};

use crate::blocking;

/// Set a board's T1 level ("low", "normal" or "high"); the daemon remembers it for that adapter.
#[tauri::command]
pub async fn set_speed(board: u16, level: String) -> Result<String, String> {
    blocking(move || {
        let l = speed_from_name(&level).ok_or_else(|| format!("no T1 level {level:?}"))?;
        let mut c = Client::connect(None).map_err(|e| e.to_string())?;
        c.speed(board, l).map(|l| speed_name(l).to_string()).map_err(|e| e.to_string())
    })
    .await
}

/// Whether a board holds REN asserted (ren.conf): the setting is written, and put on the line now if the adapter can.
#[tauri::command]
pub async fn set_ren(board: u16, on: bool) -> Result<(), String> {
    blocking(move || {
        let mut c = Client::connect(None).map_err(|e| e.to_string())?;
        let key = c.list().map_err(|e| e.to_string())?.get(board as usize).map(|x| x.entry.key()).ok_or("no such board")?;
        symbiotic_core::renconf::set(&key, on).map_err(|e| e.to_string())?;
        let t = symbiotic_core::locks::Target::Gpib(board, 0, symbiotic_core::backend::NO_SAD);
        c.ren(t, on).map_err(|e| format!("saved, but the adapter did not take it now: {e}"))
    })
    .await
}

// ---- LAN ---------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Lan {
    /// lan.conf, where the list lives.
    path: Option<String>,
    discover: bool,
}

#[tauri::command]
pub fn lan() -> Lan {
    Lan { path: conf::path().map(|p| p.display().to_string()), discover: conf::discover_enabled() }
}

#[derive(Serialize)]
pub struct Reach {
    name: String,
    /// Where it answered, or why it did not.
    answered: Option<String>,
    error: Option<String>,
    ms: u64,
}

/// Every configured LAN instrument, and whether something takes a connection where it would be.
#[tauri::command]
pub async fn lan_check() -> Result<Vec<Reach>, String> {
    blocking(|| {
        Ok(conf::check_all(conf::load(), conf::REACH)
            .into_iter()
            .map(|r| Reach {
                name: r.target.name(),
                answered: r.result.as_ref().ok().map(|a| a.to_string()),
                error: r.result.err(),
                ms: r.took.as_millis() as u64,
            })
            .collect())
    })
    .await
}

fn lan_path() -> Result<std::path::PathBuf, String> {
    conf::path().ok_or_else(|| "no place for lan.conf (LOCALAPPDATA or HOME is not set)".to_string())
}

fn target(text: &str) -> Result<name::Target, String> {
    name::parse(text.trim()).ok_or_else(|| format!("{:?} is not a TCPIP resource name", text.trim()))
}

/// Add a LAN instrument; its canonical name, and false if it was already listed.
#[tauri::command]
pub fn lan_add(resource: String) -> Result<(String, bool), String> {
    let t = target(&resource)?;
    conf::add(&lan_path()?, &t).map(|added| (t.name(), added)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn lan_remove(resource: String) -> Result<bool, String> {
    conf::remove(&lan_path()?, &target(&resource)?).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn lan_set_discover(on: bool) -> Result<(), String> {
    conf::set_discover(&lan_path()?, on).map(|_| ()).map_err(|e| e.to_string())
}

/// Does something take a connection where each of these LAN resources would be reached? For the
/// window's live view of every LAN instrument it shows, configured or discovered.
#[tauri::command]
pub async fn lan_reach(resources: Vec<String>) -> Result<Vec<(String, bool)>, String> {
    blocking(move || {
        let targets: Vec<name::Target> = resources.iter().filter_map(|r| name::parse(r)).collect();
        Ok(conf::check_all(targets, conf::REACH).into_iter().map(|r| (r.target.name(), r.result.is_ok())).collect())
    })
    .await
}

#[derive(Serialize)]
pub struct Gateway {
    ip: String,
    address: u8,
}

#[derive(Serialize)]
pub struct Scan {
    names: Vec<String>,
    gateways: Vec<Gateway>,
}

/// What answers a VXI-11 broadcast or mDNS now (about a second); LAN/GPIB gateways are told apart
/// from plain instruments and not listed as `inst0`.
#[tauri::command]
pub async fn lan_scan() -> Result<Scan, String> {
    blocking(|| {
        let f = discover::discover_full(discover::WAIT);
        Ok(Scan {
            names: f.targets.into_iter().map(|t| t.name()).collect(),
            gateways: f.gateways.into_iter().map(|g| Gateway { ip: g.ip.to_string(), address: g.address }).collect(),
        })
    })
    .await
}

/// The instruments behind one gateway, as resource names. This addresses the gateway's bus, so it runs only when the
/// user asks.
#[tauri::command]
pub async fn lan_gateway_find(ip: String) -> Result<Vec<String>, String> {
    blocking(move || {
        use symbiotic_core::lan::{vxi11, Error};
        match vxi11::find_listeners(&ip, "gpib0", std::time::Duration::from_secs(3), |_| {}) {
            Ok(f) => Ok(f.into_iter().map(|a| format!("TCPIP0::{ip}::gpib0,{a}::INSTR")).collect()),
            Err(Error::Unsupported) => Err("unsupported".into()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
}

// ---- serial ports and USB devices ------------------------------------------------------------------

#[derive(Serialize)]
pub struct Port {
    n: u16,
    /// COM5, or on Linux the tty.
    name: String,
    /// The driver's name for it (`\Device\USBSER000`, `cdc_acm`).
    device: String,
    /// "prologix", "ar488" or "auto" when it is a GPIB controller; None when it is a serial port.
    bridge: Option<String>,
    /// Its resource name while it is a serial port.
    resource: String,
}

/// The serial ports (nothing is opened), and those configured as controllers that are unplugged.
#[tauri::command]
pub async fn serial_ports() -> Result<Vec<Port>, String> {
    blocking(|| {
        let bridges = prologix::configured();
        let dialect = |n: u16| bridges.iter().find(|(m, _)| *m == n).map(|(_, d)| d.map(|d| d.name()).unwrap_or("auto").to_string());
        let mut out: Vec<Port> = serial::list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|f| Port { n: f.n, name: serial::port_name(f.n), device: f.device, bridge: dialect(f.n), resource: resource::format_asrl(f.n) })
            .collect();
        for (n, _) in &bridges {
            if !out.iter().any(|p| p.n == *n) {
                out.push(Port { n: *n, name: serial::port_name(*n), device: String::new(), bridge: dialect(*n), resource: resource::format_asrl(*n) });
            }
        }
        out.sort_by_key(|p| p.n);
        Ok(out)
    })
    .await
}

/// Make port `n` a GPIB controller ("prologix", "ar488" or "auto": found on connecting), or with
/// None a serial port again.
#[tauri::command]
pub fn set_bridge(n: u16, dialect: Option<String>) -> Result<(), String> {
    let entry = match dialect.as_deref() {
        None => None,
        Some("auto") => Some(None),
        Some(d) => Some(Some(prologix::Dialect::from_name(d).ok_or_else(|| format!("no controller kind {d:?}"))?)),
    };
    prologix::set_conf(n, &serial::port_name(n), entry).map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct UsbDevice {
    resource: String,
    manufacturer: String,
    product: String,
    driver: String,
    /// Why this runtime does not use it (another driver has it), or None when it does.
    not_ours: Option<String>,
}

/// Every USBTMC device plugged in, ours or not.
#[tauri::command]
pub async fn usb_devices() -> Result<Vec<UsbDevice>, String> {
    blocking(|| {
        Ok(usbtmc::list_all()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|f| UsbDevice {
                resource: format!("USB0::0x{:04X}::0x{:04X}::{}::INSTR", f.vid, f.pid, f.serial),
                manufacturer: f.manufacturer,
                product: f.product,
                driver: f.driver,
                not_ours: f.not_ours,
            })
            .collect())
    })
    .await
}

// ---- aliases ----------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Alias {
    alias: String,
    resource: String,
    /// The group's: a network instrument's alias on a computer in a group, which any computer of the group may change.
    group: bool,
}

/// Every alias: this computer's (aliases.conf) and the group's for network instruments. An alias of a network instrument still kept
/// here is moved to the group first (`aliases_move` says what became of it).
#[tauri::command]
pub async fn aliases() -> Result<Vec<Alias>, String> {
    blocking(|| {
        let mut out: Vec<Alias> = alias::load().into_iter().map(|(alias, resource)| Alias { alias, resource, group: false }).collect();
        out.extend(names::group_lan().into_iter().map(|g| Alias { alias: g.alias, resource: g.resource, group: true }));
        Ok(out)
    })
    .await
}

/// Move the aliases of network instruments still kept on this computer to the group (the window does it when it starts): one line for
/// each that was moved, or why it was not; nothing when there was nothing to move.
#[tauri::command]
pub async fn aliases_move() -> Result<Vec<String>, String> {
    blocking(|| match names::move_local() {
        None => Ok(Vec::new()),
        Some(Err(e)) => Err(e),
        Some(Ok(done)) => Ok(done.into_iter().map(|(a, r)| match r {
            Ok(what) => format!("{a}: {what}"),
            Err(e) => format!("{a}: {e}"),
        }).collect()),
    })
    .await
}

/// Add or change an alias (a name viOpen takes for `resource`), or with None remove it, where it is kept (share::names).
#[tauri::command]
pub async fn set_alias(alias: String, resource: Option<String>) -> Result<(), String> {
    blocking(move || names::set(&alias, resource.as_deref())).await
}

/// One resource's alias and how VISA users see it, saved together (the names panel): `alias` None or empty takes the
/// resource's alias away, `expose` None, "default" or "alias" is the default, alias only, which writes no setting. The
/// new alias is made before the old one is taken away, so a name that is refused leaves both as they were.
#[tauri::command]
pub async fn set_names(resource: String, alias: Option<String>, expose: Option<String>) -> Result<(), String> {
    blocking(move || set_names_now(resource, alias, expose)).await
}

fn set_names_now(resource: String, alias: Option<String>, expose: Option<String>) -> Result<(), String> {
    use symbiotic_core::instruments;
    let r = resource::canonical(resource.trim()).ok_or_else(|| format!("{:?} is not a resource name", resource.trim()))?;
    let alias = alias.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
    let expose = expose_of(expose)?;
    if instruments::get(&r).expose != expose {
        symbiotic_core::lanunits::writable(&r)?;
    }
    let old = names::alias_of(&r);
    if old != alias {
        if let Some(a) = &alias {
            names::set(a, Some(&r))?;
        }
        if let Some(o) = old.filter(|o| alias.as_ref().is_none_or(|a| !a.eq_ignore_ascii_case(o))) {
            names::set(&o, None)?;
        }
    }
    let mut s = instruments::get(&r);
    if s.expose != expose {
        s.expose = expose;
        instruments::set(&r, &s).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `expose` as instruments.conf has it: None, "", "default" or "alias" is the default, alias only, which is no setting.
fn expose_of(expose: Option<String>) -> Result<Option<symbiotic_core::instruments::Expose>, String> {
    match expose.as_deref().map(str::trim) {
        None | Some("") | Some("default") | Some("alias") => Ok(None),
        Some(w) => symbiotic_core::instruments::Expose::parse(w).map(Some).ok_or_else(|| format!("{w:?} is not a way to expose a resource")),
    }
}

/// How VISA users see one resource name, alone (the aliases window, a row of which is an alias of it): its alias stays as it is.
#[tauri::command]
pub async fn set_expose(resource: String, expose: Option<String>) -> Result<(), String> {
    blocking(move || {
        use symbiotic_core::instruments;
        let r = resource::canonical(resource.trim()).ok_or_else(|| format!("{:?} is not a resource name", resource.trim()))?;
        let expose = expose_of(expose)?;
        let mut s = instruments::get(&r);
        if s.expose != expose {
            symbiotic_core::lanunits::writable(&r)?;
        }
        if s.expose != expose {
            s.expose = expose;
            instruments::set(&r, &s).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
}

// ---- pictures of instruments ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct PictureRule {
    pattern: String,
    file: String,
    dark_keep: bool,
}

#[derive(Serialize)]
pub struct Pictures {
    /// The rules, in file order: the first that matches an instrument is its picture.
    rules: Vec<PictureRule>,
    /// Where the rules are and where the pictures are, for the page to say.
    conf: Option<String>,
    dir: Option<String>,
    /// These are the group's (this computer has joined one): any member can change them.
    group: bool,
}

/// The group's pictures, when this computer has joined a group: the copy in `group-pictures/` in the configuration
/// directory, brought up to date from the registry. Else this computer's own.
fn group_store() -> Option<pictures::Store> {
    symbiotic_core::paths::config_file("group-pictures/pictures.conf").map(pictures::Store::at)
}

/// The store the window reads: the group's copy once joined, else the local one.
fn reading() -> Option<pictures::Store> {
    if crate::share::joined() { group_store() } else { pictures::Store::local() }
}

fn rules_of(store: &Option<pictures::Store>) -> Vec<PictureRule> {
    store.as_ref().map(|s| s.load()).unwrap_or_default().into_iter().map(|r| PictureRule { pattern: r.pattern, file: r.file, dark_keep: r.dark_keep }).collect()
}

#[tauri::command]
pub async fn pictures() -> Pictures {
    blocking(|| {
        let group = crate::share::joined();
        if group {
            // Best effort: the copy is what is shown when the registry cannot be reached.
            if let (Ok(m), Some(cache)) = (crate::share::member(), group_store()) {
                let _ = m.pic_sync(&cache);
            }
        }
        let store = reading();
        Ok::<_, String>(Pictures { rules: rules_of(&store), conf: store.as_ref().map(|s| s.conf.display().to_string()), dir: store.as_ref().map(|s| s.dir().display().to_string()), group })
    })
    .await
    .unwrap_or(Pictures { rules: Vec::new(), conf: None, dir: None, group: false })
}

/// Bring the copy of the group's pictures up to date; whether anything changed (false where there is no group).
#[tauri::command]
pub async fn pictures_sync() -> Result<bool, String> {
    blocking(|| {
        if !crate::share::joined() {
            return Ok(false);
        }
        let cache = group_store().ok_or("no configuration directory")?;
        crate::share::member()?.pic_sync(&cache).map_err(|e| e.to_string())
    })
    .await
}

/// A picture file as a data URL: the pictures are in the configuration directory, which the page cannot read itself.
#[tauri::command]
pub fn picture_data(file: String) -> Result<String, String> {
    let bytes = reading().ok_or("no place for the pictures")?.read_png(&file).map_err(|e| e.to_string())?;
    Ok(format!("data:image/png;base64,{}", pictures::base64_encode(&bytes)))
}

/// The part of a data URL after the comma (the base64), or the text as it is if it is not one.
fn base64_body(text: &str) -> &str {
    text.split_once(',').filter(|_| text.starts_with("data:")).map_or(text, |(_, b)| b)
}

/// Make the PNG (base64, with or without the `data:` head) the picture of whatever `pattern` matches; the file it was
/// given. `replaces` is the pattern of the rule being edited: it becomes this one, in its place. `original_base64` (a
/// data URL or the base64 alone) and its `original_ext` are the image the picture was made of, as it came, and `edit`
/// what was done to it (JSON): kept with the picture so that it can be made again from the whole image. Once joined,
/// the picture goes to the group (the registry keeps it, and every computer's copy follows).
#[tauri::command]
pub async fn save_picture(
    pattern: String,
    dark_keep: bool,
    png_base64: String,
    replaces: Option<String>,
    original_base64: Option<String>,
    original_ext: Option<String>,
    edit: Option<String>,
) -> Result<String, String> {
    blocking(move || {
        let bytes = pictures::base64_decode(base64_body(&png_base64)).ok_or_else(|| "the picture is not base64".to_string())?;
        let original = match (original_base64, original_ext) {
            (Some(o), Some(ext)) => Some((ext.to_ascii_lowercase(), pictures::base64_decode(base64_body(&o)).ok_or_else(|| "the original is not base64".to_string())?)),
            (None, None) => None,
            _ => return Err("the original and its kind come together".into()),
        };
        if crate::share::joined() {
            let m = crate::share::member()?;
            let file = m
                .pic_save(&pattern, dark_keep, &bytes, replaces.as_deref().unwrap_or(""), original.as_ref().map(|(e, b)| (e.as_str(), b.as_slice())), edit.as_deref().unwrap_or(""))
                .map_err(|e| e.to_string())?;
            if let Some(cache) = group_store() {
                let _ = m.pic_sync(&cache);
            }
            return Ok(file);
        }
        pictures::save(&pictures::Save {
            pattern: &pattern,
            dark_keep,
            png: &bytes,
            replaces: replaces.as_deref(),
            original: original.as_ref().map(|(e, b)| (e.as_str(), b.as_slice())),
            edit: edit.as_deref(),
        })
        .map_err(|e| e.to_string())
    })
    .await
}

#[derive(Serialize)]
pub struct PictureOriginal {
    /// The original image as a data URL.
    data_url: String,
    /// What was done to it (the JSON the page wrote), if that was kept.
    edit: Option<String>,
}

fn mime_of(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// The original a picture was made of, if it has one (the group's are asked of the registry when wanted: the copies do not keep them).
#[tauri::command]
pub async fn picture_original(file: String) -> Result<Option<PictureOriginal>, String> {
    blocking(move || {
        let (ext, bytes, edit) = if crate::share::joined() {
            match crate::share::member()?.pic_original(&file).map_err(|e| e.to_string())? {
                Some((e, b, ed)) => (e, b, Some(ed).filter(|x| !x.is_empty())),
                None => return Ok(None),
            }
        } else {
            match pictures::read_original(&file).map_err(|e| e.to_string())? {
                Some(o) => (o.ext, o.bytes, o.edit),
                None => return Ok(None),
            }
        };
        Ok(Some(PictureOriginal { data_url: format!("data:{};base64,{}", mime_of(&ext), pictures::base64_encode(&bytes)), edit }))
    })
    .await
}

#[tauri::command]
pub async fn remove_picture(pattern: String) -> Result<bool, String> {
    blocking(move || {
        if crate::share::joined() {
            let m = crate::share::member()?;
            let gone = match m.pic_remove(&pattern) {
                Ok(()) => true,
                Err(symbiotic_core::share::regclient::Error::Refused(symbiotic_core::share::registry::Code::NotFound, _)) => false,
                Err(e) => return Err(e.to_string()),
            };
            if let Some(cache) = group_store() {
                let _ = m.pic_sync(&cache);
            }
            return Ok(gone);
        }
        pictures::remove(&pattern).map_err(|e| e.to_string())
    })
    .await
}

#[derive(Serialize)]
pub struct Moved {
    pattern: String,
    /// None: it is the group's now (and no longer this computer's); else why not (the group has a picture for that pattern already, ...).
    problem: Option<String>,
}

/// Move this computer's own pictures to the group (Member::pic_adopt): a pattern the group has a picture for already is skipped and stays
/// here; one that was moved is taken off this computer.
#[tauri::command]
pub async fn pictures_adopt() -> Result<Vec<Moved>, String> {
    blocking(|| {
        let local = pictures::Store::local().ok_or("no place for the pictures")?;
        let cache = group_store().ok_or("no configuration directory")?;
        let done = crate::share::member()?.pic_adopt(&local, &cache).map_err(|e| e.to_string())?;
        Ok(done.into_iter().map(|(pattern, problem)| Moved { pattern, problem }).collect())
    })
    .await
}

// ---- the service -----------------------------------------------------------------------------------

/// Is the daemon running now? It exits by itself ten seconds after its last client, and is started
/// again by the next one, so "not running" is its usual state while nothing uses an instrument.
#[tauri::command]
pub async fn service_running() -> Result<bool, String> {
    blocking(|| Ok(Client::connect_existing().is_ok())).await
}

/// The end of the daemon's log, `lines` long, each line on this machine's clock (diag.rs); and where the log is.
#[tauri::command]
pub async fn log_tail(lines: usize) -> Result<(String, String), String> {
    blocking(move || {
        let offset = symbiotic_core::iotrace::local_offset_s();
        let text: String = symbiotic_core::diag::log_tail(lines.min(5000)).lines().map(|l| symbiotic_core::diag::show_log_line(l, offset) + "\n").collect();
        Ok((text, symbiotic_core::paths::log_file().display().to_string()))
    })
    .await
}

/// A diagnostics bundle (diag.rs: no daemon started, no private key in it) where the user says in the save dialog; its
/// path, or None when the user cancels.
#[tauri::command]
pub async fn diag_bundle(window: tauri::WebviewWindow, kind: String) -> Result<Option<String>, String> {
    blocking(move || {
        let Some(path) = crate::save_dialog(&window, &symbiotic_core::diag::bundle_name(), &kind, "zip")? else { return Ok(None) };
        symbiotic_core::diag::write_bundle_to(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some(path.display().to_string()))
    })
    .await
}

/// Show a file the window wrote (the diagnostics bundle) in the system's file manager: selected in Explorer and Finder,
/// its folder opened elsewhere.
#[tauri::command]
pub async fn show_in_folder(path: String) -> Result<(), String> {
    blocking(move || {
        let p = std::path::PathBuf::from(&path);
        if !p.exists() {
            return Err(format!("{path} is not there any more"));
        }
        #[cfg(windows)]
        let r = {
            use std::os::windows::process::CommandExt;
            // explorer's own parser: "/select," and the path as one argument, quoted.
            std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", p.display())).spawn()
        };
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").arg("-R").arg(&p).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(&p)).spawn();
        r.map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

// ---- a LAN instrument's web page -----------------------------------------------------------------

/// Whether `host` (a LAN instrument's) serves a web page on port 80, as LXI asks of it: one request for LXI's
/// `/lxi/identification`, and an HTTP answer of any status is a yes. Not `/`, which some instruments are slow to make.
/// The page's address, or None.
#[tauri::command]
pub async fn web_page(host: String) -> Result<Option<String>, String> {
    blocking(move || {
        use std::io::{Read, Write};
        use std::net::{TcpStream, ToSocketAddrs};
        use std::time::Duration;
        if !web_host(&host) {
            return Err(format!("{host:?} is not a host name"));
        }
        let Some(addr) = (host.as_str(), 80).to_socket_addrs().ok().and_then(|mut a| a.next()) else { return Ok(None) };
        let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else { return Ok(None) };
        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = s.set_write_timeout(Some(Duration::from_millis(1500)));
        if s.write_all(format!("GET /lxi/identification HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", url_host(&host)).as_bytes()).is_err() {
            return Ok(None);
        }
        let mut head = [0u8; 5];
        let mut got = 0;
        while got < head.len() {
            match s.read(&mut head[got..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        Ok((&head[..got] == b"HTTP/").then(|| format!("http://{}/", url_host(&host))))
    })
    .await
}

/// A host as a resource name has it: letters, digits, dots, dashes, and colons (an IPv6 address); nothing that would end the
/// URL or the command line it is handed on.
fn web_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= 253 && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// `host` as a URL has it: an IPv6 address in brackets (RFC 3986), anything else as it is.
fn url_host(host: &str) -> String {
    if host.contains(':') { format!("[{host}]") } else { host.to_string() }
}

/// Open `url`, a web page `web_page` found, in the system's browser. Only `http://<host>/` is taken.
#[tauri::command]
pub async fn open_web(url: String) -> Result<(), String> {
    let host = url.strip_prefix("http://").and_then(|r| r.strip_suffix('/')).map(|h| h.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(h));
    let Some(host) = host.filter(|h| web_host(h)) else { return Err(format!("{url:?} is not a page Symbiotic Explorer opens")) };
    let url = format!("http://{}/", url_host(host));
    blocking(move || {
        #[cfg(windows)]
        let r = std::process::Command::new("explorer").arg(&url).spawn();
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = std::process::Command::new("xdg-open").arg(&url).spawn();
        r.map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

/// Ask the daemon to exit (it gives the adapters back); an error if none is running. This window then starts no daemon
/// (its polling would start one again at once) until `start_service`, the service page's Start button.
#[tauri::command]
pub async fn stop_service() -> Result<(), String> {
    blocking(|| match Client::connect_existing() {
        Ok(mut c) => {
            symbiotic_core::client::hold_start(true);
            c.shutdown().map_err(|e| {
                symbiotic_core::client::hold_start(false);
                e.to_string()
            })
        }
        Err(_) => Err("no service is running".into()),
    })
    .await
}

/// Let this window start the daemon again, and start it.
#[tauri::command]
pub async fn start_service() -> Result<(), String> {
    blocking(|| {
        symbiotic_core::client::hold_start(false);
        Client::connect(None).map(|_| ()).map_err(|e| e.to_string())
    })
    .await
}

/// Whether a daemon that stays lets go of the hardware when idle (daemon.conf, `release_idle`); off by default.
#[tauri::command]
pub fn release_idle() -> bool {
    symbiotic_core::daemonconf::release_idle()
}

#[tauri::command]
pub fn set_release_idle(on: bool) -> Result<(), String> {
    symbiotic_core::daemonconf::set_release_idle(on).map_err(|e| e.to_string())
}

// ---- registrations (Windows) -----------------------------------------------------------------------

#[derive(Serialize)]
pub struct Registration {
    /// "visa", "visanet" or "visacom".
    id: &'static str,
    /// What the registry says (the DLL's path, VISA.NET's version), or None when not registered.
    state: Option<String>,
}

#[cfg(windows)]
const REGISTRATIONS: [(&str, &str, &str); 3] = [
    ("visa", "register-visa.ps1", r"HKLM\SOFTWARE\VXIPNP_Alliance\VISA_Installs\CurrentVersion\6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64"),
    ("visanet", "register-visanet.ps1", r"HKLM\SOFTWARE\WOW6432Node\IVI\VISA.NET\71ADCC3F-7D29-42A2-873E-1F1418FCA388"),
    ("visacom", "register-visacom.ps1", r"HKCR\VISA.GlobalRM\VendorSpecificRMs\6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64"),
];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn reg_query(key: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg").args(["query", key, "/s"]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let location = text.lines().find(|l| l.trim_start().starts_with("Location")).map(|l| l.split("REG_SZ").nth(1).unwrap_or("").trim().to_string());
    // A path as it is; an assembly's full name (VISA.NET) by its version alone.
    Some(match location {
        Some(l) if l.contains("Version=") => l.split(", ").find_map(|p| p.strip_prefix("Version=")).map(|v| format!("version {v}")).unwrap_or(l),
        Some(l) => l,
        None => "registered".into(),
    })
}

/// The three registrations and their state; empty where there are none (not Windows).
#[tauri::command]
pub async fn registrations() -> Result<Vec<Registration>, String> {
    #[cfg(windows)]
    return blocking(|| Ok(REGISTRATIONS.iter().map(|(id, _, key)| Registration { id, state: reg_query(key) }).collect())).await;
    #[cfg(not(windows))]
    Ok(Vec::new())
}

/// Register or unregister one: its script from the install's `windows` folder, elevated (Windows asks
/// for the administrator). An error if the script failed or the user said no.
#[tauri::command]
pub async fn set_registration(id: String, on: bool) -> Result<(), String> {
    #[cfg(windows)]
    return blocking(move || {
        use std::os::windows::process::CommandExt;
        let (_, script, _) = REGISTRATIONS.iter().find(|(i, _, _)| *i == id).ok_or_else(|| format!("no registration {id:?}"))?;
        let dir = std::env::current_exe().map_err(|e| e.to_string())?.parent().map(|p| p.join("windows")).ok_or("no install folder")?;
        let path = dir.join(script);
        if !path.exists() {
            return Err(format!("{} is missing (the registrations need an installed copy)", path.display()));
        }
        // The path goes in a PowerShell single-quoted string, where a quote is written twice (a user
        // folder can hold one: O'Brien).
        let args = format!(
            "-NoProfile -Command \"Start-Process powershell -Verb RunAs -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'{}\"",
            path.display().to_string().replace('\'', "''"),
            if on { "" } else { ",'-Uninstall'" }
        );
        let st = std::process::Command::new("powershell").raw_arg(args).creation_flags(CREATE_NO_WINDOW).status().map_err(|e| e.to_string())?;
        if st.success() { Ok(()) } else { Err(format!("the script failed or was cancelled ({st})")) }
    })
    .await;
    #[cfg(not(windows))]
    Err(format!("registration {id:?} ({on}) is Windows' only"))
}

// ---- aliases bound to one unit --------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Unit {
    alias: String,
    vendor: String,
    model: String,
    serial: String,
}

fn unit(alias: String, i: symbiotic_core::identity::Identity) -> Unit {
    Unit { alias, vendor: i.vendor, model: i.model, serial: i.serial }
}

/// The aliases bound to a unit: this computer's and the group's network instruments'.
#[tauri::command]
pub async fn identities() -> Result<Vec<Unit>, String> {
    blocking(|| {
        let mut out: Vec<Unit> = symbiotic_core::identity::load().into_iter().map(|(a, i)| unit(a, i)).collect();
        out.extend(names::group_lan().into_iter().filter_map(|g| g.identity.map(|i| unit(g.alias, i))));
        Ok(out)
    })
    .await
}

/// Bind `alias` to the unit at its resource: asks it `*IDN?` once. The user pressed the button, which is the word for it.
#[tauri::command]
pub async fn bind_alias(alias: String) -> Result<Unit, String> {
    blocking(move || names::bind(&alias).map(|i| unit(alias, i))).await
}

#[tauri::command]
pub async fn unbind_alias(alias: String) -> Result<(), String> {
    blocking(move || names::unbind(&alias)).await
}

// ---- a LAN address that has changed owner ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct LanUnit {
    vendor: String,
    model: String,
    serial: String,
}

fn lan_unit(u: symbiotic_core::lanunits::Unit) -> LanUnit {
    LanUnit { vendor: u.vendor, model: u.model, serial: u.serial }
}

#[derive(Serialize)]
pub struct KeptSetting {
    resource: String,
    idn: bool,
    hidden: bool,
    name: Option<String>,
    expose: Option<&'static str>,
}

#[derive(Serialize)]
pub struct KeptAlias {
    alias: String,
    resource: String,
    group: bool,
    bound: bool,
}

/// Another unit than the one recorded answered at an address: which one it was, and what the address keeps of it.
#[derive(Serialize)]
pub struct OwnerChange {
    was: LanUnit,
    settings: Vec<KeptSetting>,
    aliases: Vec<KeptAlias>,
}

/// `idn` answered at `resource` (the window's own `*IDN?`): recorded, or compared with the unit recorded there. None: nothing to say.
#[tauri::command]
pub async fn lan_unit_seen(resource: String, idn: String) -> Result<Option<OwnerChange>, String> {
    blocking(move || {
        use symbiotic_core::lanunits;
        let lanunits::Seen::Changed { was } = lanunits::seen(&resource, &idn).map_err(|e| e.to_string())? else { return Ok(None) };
        let key = lanunits::key_of(&resource).unwrap_or_default();
        let now = lanunits::Unit::from_idn(&idn).ok_or("not an *IDN? answer")?;
        let k = lanunits::kept_now(&key).left_by_another(&now);
        // Nothing the newcomer could inherit (its own bound alias is not): nothing to say, and the address is its.
        if k.is_empty() {
            lanunits::keep(&resource, &idn).map_err(|e| e.to_string())?;
            return Ok(None);
        }
        Ok(Some(OwnerChange {
            was: lan_unit(was),
            settings: k
                .settings
                .into_iter()
                .map(|(resource, s)| KeptSetting { resource, idn: s.idn, hidden: s.hidden, name: s.name, expose: s.expose.map(|e| e.word()) })
                .collect(),
            aliases: k.aliases.into_iter().map(|a| KeptAlias { alias: a.alias, resource: a.resource, group: a.group, bound: a.bound }).collect(),
        }))
    })
    .await
}

#[derive(Serialize)]
pub struct MacChange {
    host: String,
    was_mac: String,
    now_mac: Option<String>,
    /// The unit an `*IDN?` answer named there, if one ever did.
    was: Option<LanUnit>,
    settings: Vec<KeptSetting>,
    aliases: Vec<KeptAlias>,
}

#[derive(Serialize)]
pub struct UnitsState {
    /// Hosts whose settings do not count now: another instrument there, or none could be told.
    unconfirmed: Vec<String>,
    changed: Vec<MacChange>,
    /// Hosts whose settings went with their MAC to another host: (from, to).
    moved: Vec<(String, String)>,
}

/// The LAN resources the tree has answering: whose settings count, by their host's MAC, what each changed host keeps,
/// and what went where its MAC went. Asks no instrument anything.
#[tauri::command]
pub async fn units_check(resources: Vec<String>) -> Result<UnitsState, String> {
    blocking(move || {
        use symbiotic_core::{lanunits, neighbor};
        let r = lanunits::reconcile(&resources);
        let all = symbiotic_core::instruments::load();
        let aliases = lanunits::all_aliases();
        let units = lanunits::path().map(|p| lanunits::load_from(&p)).unwrap_or_default();
        let changed = r
            .changed
            .iter()
            .filter(|(h, ..)| r.unconfirmed.contains(h))
            .map(|(h, was, now)| {
                let on = |res: &str| lanunits::host_of(res).as_deref() == Some(h.as_str());
                MacChange {
                    host: h.clone(),
                    was_mac: neighbor::format(was),
                    now_mac: now.as_ref().map(neighbor::format),
                    was: units.iter().find(|(k, _)| k.starts_with(&format!("{h}#"))).map(|(_, u)| lan_unit(u.clone())),
                    settings: all
                        .iter()
                        .filter(|(res, s)| on(res) && *s != symbiotic_core::instruments::Settings::default())
                        .map(|(resource, s)| KeptSetting { resource: resource.clone(), idn: s.idn, hidden: s.hidden, name: s.name.clone(), expose: s.expose.map(|e| e.word()) })
                        .collect(),
                    aliases: aliases.iter().filter(|a| on(&a.resource)).map(|a| KeptAlias { alias: a.alias.clone(), resource: a.resource.clone(), group: a.group, bound: a.bound }).collect(),
                }
            })
            .collect();
        Ok(UnitsState { unconfirmed: r.unconfirmed.into_iter().collect(), changed, moved: r.moved })
    })
    .await
}

/// An instrument taken out of the tree (its Remove): the settings of its resource names and what was recorded of it go too.
#[tauri::command]
pub async fn forget_instrument(resources: Vec<String>) -> Result<(), String> {
    blocking(move || symbiotic_core::lanunits::forget(&resources).map_err(|e| e.to_string())).await
}

#[derive(serde::Deserialize)]
pub struct TreePlace {
    resources: Vec<String>,
    online: bool,
    idn: Option<String>,
}

#[derive(Serialize)]
pub struct AliasFollowed {
    alias: String,
    to: Vec<String>,
    moved: bool,
    error: Option<String>,
    settings: Vec<String>,
    replaced: Vec<String>,
}

/// The bound LAN aliases whose unit the tree sees at another address, alone, and not at theirs: moved there. `places`
/// are the tree's instruments; what was done, or why not.
#[tauri::command]
pub async fn follow_bound(places: Vec<TreePlace>) -> Result<Vec<AliasFollowed>, String> {
    blocking(move || {
        use symbiotic_core::lanunits;
        let places: Vec<lanunits::Place> = places.into_iter().map(|p| lanunits::Place { resources: p.resources, online: p.online, idn: p.idn }).collect();
        Ok(lanunits::follow_bound(&places).into_iter().map(|f| AliasFollowed { alias: f.alias, to: f.to, moved: f.moved, error: f.error, settings: f.settings, replaced: f.replaced }).collect())
    })
    .await
}

/// The user's word on what the address of `resource` keeps: `keep` it, `clear` it, or `move` it to `host`, where the unit it was is now.
/// `idn` is the answer there now, if there is one (empty: none yet); with the MAC the host has now, it is what the address is for from then on.
#[tauri::command]
pub async fn lan_unit_settle(resource: String, idn: String, how: String, host: Option<String>) -> Result<(), String> {
    blocking(move || {
        use symbiotic_core::lanunits;
        match (how.as_str(), host) {
            ("keep", _) => lanunits::keep(&resource, &idn),
            ("clear", _) => lanunits::clear(&resource, &idn),
            ("move", Some(h)) => lanunits::move_to(&resource, &idn, &h),
            _ => return Err(format!("{how:?} is not a way to settle an address")),
        }
        .map_err(|e| e.to_string())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lan_name_is_checked_and_trimmed() {
        assert_eq!(target("  TCPIP0::198.18.68.57::hislip0::INSTR \n").unwrap().name(), "TCPIP0::198.18.68.57::hislip0::INSTR");
        let e = target("GPIB0::15::INSTR").unwrap_err();
        assert!(e.contains("\"GPIB0::15::INSTR\" is not a TCPIP resource name"), "{e}");
        assert!(target("").is_err());
    }

    #[test]
    fn an_ipv6_host_is_bracketed_in_a_url() {
        assert_eq!(url_host("198.18.68.57"), "198.18.68.57");
        assert_eq!(url_host("fe80::1"), "[fe80::1]");
        assert!(web_host("fe80::1") && web_host("a34465a-1.local") && !web_host("[fe80::1]") && !web_host("x/y") && !web_host(""));
    }

    #[test]
    fn a_picture_comes_as_a_data_url_or_its_base64() {
        assert_eq!(base64_body("data:image/png;base64,iVBORw0K"), "iVBORw0K");
        assert_eq!(base64_body("iVBORw0K"), "iVBORw0K", "the base64 alone");
        assert_eq!(base64_body("x,y"), "x,y", "a comma without the data: head is the text's own");
        assert_eq!((mime_of("jpg"), mime_of("jpeg"), mime_of("webp"), mime_of("png"), mime_of("tiff")), ("image/jpeg", "image/jpeg", "image/webp", "image/png", "image/png"));
    }
}
