//! Symbiotic Explorer, the configuration window, as a web page in a native window (Tauri). The page (ui/) asks these
//! commands over Tauri's in-process channel: nothing listens on a port. What they read and change is what
//! `symbiotic-ctl` does, through the same `symbiotic-core` functions; instruments are talked to through our own C API
//! in-process (visa.rs), as an application talks to them.

// A window, not a console, on Windows (a release build).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod picpack;
mod share;
mod trace;
mod visa;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use symbiotic_core::client::Client;
use symbiotic_core::backend::Kind;
use symbiotic_core::instruments;
use symbiotic_core::proto::speed_name;

/// Blocking work (the daemon, instruments) off the window's thread; its error as text for the page.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

/// Where to save a file the user takes away (the diagnostics bundle, an exported trace): the system's save dialog over
/// `window`, starting in the Downloads folder with `name`, showing files of `kind` (a name the page gives, and the
/// extension). None when the user cancels. Called here, not by the page: it gets no dialog and no file system of its own.
/// Blocks; the plugin shows it on the main thread.
fn save_dialog(window: &tauri::WebviewWindow, name: &str, kind: &str, ext: &str) -> Result<Option<std::path::PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt;
    let mut d = window.dialog().file().set_parent(window).set_file_name(name).add_filter(kind, &[ext]);
    if let Some(dir) = symbiotic_core::paths::downloads() {
        d = d.set_directory(dir);
    }
    d.blocking_save_file().map(|p| p.into_path().map_err(|e| e.to_string())).transpose()
}

#[derive(Serialize)]
struct Board {
    board: String,
    kind: String,
    /// What the adapter says it is ("Viper488 Classic"); the kind's name stands in where it is not known.
    model: Option<String>,
    serial: String,
    present: bool,
    /// The T1 level remembered for it ("low", "normal", "high"); None: never set, so normal.
    speed: Option<String>,
    /// Can its T1 level be set (Viper488, NI, 82357B; not CONTEC or a Prologix/AR488 controller).
    speed_settable: bool,
    /// It holds REN asserted when opened (ren.conf); released unless so.
    ren: bool,
    /// Can REN be released on it (a Prologix holds it and cannot).
    ren_settable: bool,
}

#[derive(Serialize)]
struct Snapshot {
    /// The GPIB boards, as `symbiotic-ctl list` has them; an error if the service did not answer.
    boards: Result<Vec<Board>, String>,
    /// Every resource viFindRsrc finds, in the canonical form instruments.conf keys on.
    resources: Result<Vec<String>, String>,
    /// The LAN instruments configured, found or not (lan.conf).
    lan_configured: Vec<String>,
    /// The LAN/GPIB gateways discovery found (only while discovery is on): host and its bus address.
    lan_gateways: Vec<(String, u8)>,
}

/// Everything the tree shows. Nothing is sent to an instrument here: `*IDN?` is `identify`'s.
#[tauri::command]
async fn snapshot() -> Result<Snapshot, String> {
    blocking(|| {
        let levels = symbiotic_core::speedconf::load();
        let holds = symbiotic_core::renconf::load();
        let boards = Client::connect(None).and_then(|mut c| c.list()).map_err(|e| e.to_string()).map(|l| {
            l.into_iter()
                .enumerate()
                .filter(|(_, x)| !x.forgotten)
                .map(|(n, x)| Board {
                    board: format!("GPIB{n}"),
                    kind: x.entry.kind.name().to_string(),
                    model: x.model.clone(),
                    serial: x.entry.serial.clone(),
                    present: x.present,
                    speed: levels.get(&x.entry.key()).map(|&l| speed_name(l).to_string()),
                    speed_settable: matches!(x.entry.kind, Kind::Viper | Kind::Ni | Kind::Keysight),
                    ren: holds.contains(&x.entry.key()),
                    ren_settable: true,
                })
                .collect()
        });
        let resources = visa::find("?*").map(|l| {
            l.into_iter().map(|r| symbiotic_core::resource::canonical(&r).unwrap_or(r)).collect()
        });
        let lan_configured = symbiotic_core::lan::conf::load().into_iter().map(|t| t.name()).collect();
        use symbiotic_core::lan::{conf, discover};
        let discovering = conf::path().is_some_and(|p| conf::discover_in(&std::fs::read_to_string(p).unwrap_or_default()));
        let lan_gateways = if discovering {
            discover::discover_full(discover::WAIT).gateways.into_iter().map(|g| (g.ip.to_string(), g.address)).collect()
        } else {
            Vec::new()
        };
        Ok(Snapshot { boards, resources, lan_configured, lan_gateways })
    })
    .await
}

/// What can change without anything being sent to an instrument: the adapters plugged in, the USBTMC instruments
/// plugged in (both through the daemon, which lists the system's devices and opens nothing new), and the serial ports.
/// The page asks every two seconds while it is shown and looks again (snapshot) when the answer changes. A GPIB
/// instrument switched on or off is not here: only a probe of the bus tells, and a probe addresses the instrument
/// (under REN that puts it in Remote), and could land between another program's own addressing.
#[derive(Serialize)]
struct Watch {
    boards: Vec<(String, bool)>,
    usb: Vec<(String, bool)>,
    serial: Vec<u16>,
}

#[tauri::command]
async fn watch() -> Result<Watch, String> {
    blocking(|| {
        let mut c = Client::connect(None).map_err(|e| e.to_string())?;
        let boards = c.list().map_err(|e| e.to_string())?.into_iter().filter(|x| !x.forgotten).map(|x| (x.entry.key(), x.present)).collect();
        let usb = c.usb_list().map_err(|e| e.to_string())?.into_iter().map(|u| (u.serial, u.present)).collect();
        let serial = symbiotic_core::serial::list().map_err(|e| e.to_string())?.into_iter().map(|f| f.n).collect();
        Ok(Watch { boards, usb, serial })
    })
    .await
}

/// One board's answer to `bus_scan`: the primary addresses that answered, or why the board could
/// not be asked (then nothing is concluded about its instruments).
#[derive(Serialize)]
struct BoardScan {
    board: String,
    found: Option<Vec<u8>>,
    error: Option<String>,
}

#[derive(Serialize)]
struct BusScan {
    /// Another program is connected to the service: nothing was probed.
    busy: bool,
    boards: Vec<BoardScan>,
}

/// Which GPIB addresses answer, on every board plugged in: the daemon's scan (`BUS_SCAN`), the one the heartbeat's
/// states read too, so a result it found a few seconds ago is taken as it is, and else it scans now -- only while no
/// other program is connected to the service, so no program's instruments are touched while it uses them. A board the
/// console has a session on is not asked at all: a query is a write and a read, two requests, and a probe between them
/// would address the bus under the console's feet. The tree's Remove on an adapter that is not attached: the daemon
/// forgets its board number (boards.rs).
#[tauri::command]
async fn forget_board(board: u16) -> Result<(), String> {
    blocking(move || {
        Client::connect(None).and_then(|mut c| c.forget(board)).map_err(|e| match e {
            symbiotic_core::adapter::Error::Gone => format!("GPIB{board}'s adapter is attached"),
            e => e.to_string(),
        })
    })
    .await
}

#[tauri::command]
async fn bus_scan(sessions: tauri::State<'_, Arc<Sessions>>) -> Result<BusScan, String> {
    let held: Vec<String> = sessions.0.lock().unwrap().keys().cloned().collect();
    blocking(move || {
        use symbiotic_core::adapter::Error;
        // A result this young is the daemon's own periodic scan's, or the last ask's.
        const MAX_AGE: u32 = 12;
        let mut c = Client::connect(None).map_err(|e| e.to_string())?;
        let mut boards = Vec::new();
        for (n, b) in c.list().map_err(|e| e.to_string())?.into_iter().enumerate() {
            if !b.present {
                continue;
            }
            let (board, n) = (format!("GPIB{n}"), n as u16);
            if held.iter().any(|r| r.starts_with(&format!("{board}::"))) {
                boards.push(BoardScan { board, found: None, error: Some("the console has a session on this board".into()) });
                continue;
            }
            match c.bus_scan(n, MAX_AGE) {
                Ok(r) => boards.push(BoardScan { board, found: Some(r.pads), error: None }),
                Err(Error::Busy) => return Ok(BusScan { busy: true, boards: Vec::new() }),
                Err(e) => boards.push(BoardScan { board, found: None, error: Some(e.to_string()) }),
            }
        }
        Ok(BusScan { busy: false, boards })
    })
    .await
}

/// What `identify` found: the answer, or that another program is using the instrument now.
#[derive(Serialize)]
struct Identified {
    text: Option<String>,
    in_use: bool,
}

/// `*IDN?` to one instrument, 2 s to answer. The page asks only for instruments whose settings allow it
/// (instruments.conf). `quiet`, for what the page asks by itself (the window opened or refreshed, an instrument
/// appeared): a GPIB, USBTMC or serial instrument another program used in the last 30 s is not asked -- our `*IDN?`
/// would make it drop the answer that program has not read (ipc.rs, `F_IF_QUIET`). The page asks again later. An alias,
/// a shared alias and a LAN instrument are asked as before: the daemon does not see who uses them.
#[tauri::command]
async fn identify(resource: String, quiet: Option<bool>) -> Result<Identified, String> {
    blocking(move || {
        use symbiotic_core::{identity, resource as rs};
        let by_daemon = rs::parse(&resource).is_some() || rs::parse_usb(&resource).is_some() || rs::parse_asrl(&resource).is_some();
        if quiet.unwrap_or(false) && by_daemon {
            return match identity::ask_idn_as(&resource, std::time::Duration::from_millis(2000), true) {
                Ok(t) => Ok(Identified { text: Some(t), in_use: false }),
                Err(e) if e == identity::IN_USE => Ok(Identified { text: None, in_use: true }),
                Err(e) => Err(e),
            };
        }
        let s = visa::Session::open(&resource)?;
        s.setup(2000, Some(b'\n'))?;
        s.write(b"*IDN?\n")?;
        // An answer that ended in a timeout (no newline, no EOI) is still the instrument's answer.
        let answer = match s.read() {
            Ok(a) => a,
            Err((part, _)) if !part.is_empty() => part,
            Err((_, e)) => return Err(e),
        };
        Ok(Identified { text: Some(String::from_utf8_lossy(&answer).trim().to_string()), in_use: false })
    })
    .await
}

/// The console's sessions, one per resource, open from the first operation to `close_session` (or
/// the window's end): a Write and the Read after it go through one session, as in an application.
#[derive(Default)]
struct Sessions(Mutex<HashMap<String, Arc<visa::Session>>>);

#[derive(Deserialize)]
struct IoRequest {
    resource: String,
    /// "query", "write", "read", "stb" or "clear".
    op: String,
    /// What to send, with `\n`, `\r`, `\t`, `\xNN` and `\\` escapes.
    data: String,
    timeout_ms: u32,
    /// "lf", "crlf" or "none": appended to what is sent (unless it already ends so) and, but for
    /// "none", the character a read stops at; with "none" a read ends at EOI (or its transport's end).
    ending: String,
}

#[derive(Serialize, Default)]
struct IoResult {
    /// What was written, escaped as `shown` does.
    sent: Option<String>,
    received: Option<String>,
    bytes: Option<usize>,
    stb: Option<u16>,
    error: Option<String>,
    /// From the operation's start to its end, opening the session included.
    ms: u64,
    /// Whether the console holds a session to the resource after it (it stays open after an operation that failed, once made).
    connected: bool,
}

/// The console's escapes: `\n`, `\r`, `\t`, `\\` and `\xNN`; any other backslash is itself.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let hex = |j: usize| b.get(j).and_then(|&c| (c as char).to_digit(16)).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (byte, used) = match (b[i], b.get(i + 1)) {
            (b'\\', Some(b'n')) => (b'\n', 2),
            (b'\\', Some(b'r')) => (b'\r', 2),
            (b'\\', Some(b't')) => (b'\t', 2),
            (b'\\', Some(b'\\')) => (b'\\', 2),
            (b'\\', Some(b'x')) => match (hex(i + 2), hex(i + 3)) {
                (Some(h), Some(l)) => (h * 16 + l, 4),
                _ => (b'\\', 1),
            },
            (c, _) => (c, 1),
        };
        out.push(byte);
        i += used;
    }
    out
}

/// Bytes as the console shows them: printable ASCII as it is, the rest escaped as `unescape` reads.
fn shown(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            b'\n' => "\\n".into(),
            b'\r' => "\\r".into(),
            b'\t' => "\\t".into(),
            b'\\' => "\\\\".into(),
            0x20..0x7F => (c as char).to_string(),
            _ => format!("\\x{c:02X}"),
        })
        .collect()
}

/// One console operation. An error after the write is reported with what was sent.
#[tauri::command]
async fn io(sessions: tauri::State<'_, Arc<Sessions>>, req: IoRequest) -> Result<IoResult, String> {
    let sessions = sessions.inner().clone();
    blocking(move || {
        let start = Instant::now();
        let mut r = IoResult::default();
        let held = sessions.0.lock().unwrap().get(&req.resource).cloned();
        let run = |r: &mut IoResult| -> Result<(), String> {
            let s = match held {
                Some(s) => s,
                None => {
                    let s = Arc::new(visa::Session::open(&req.resource)?);
                    sessions.0.lock().unwrap().insert(req.resource.clone(), s.clone());
                    s
                }
            };
            let ending: &[u8] = match req.ending.as_str() {
                "crlf" => b"\r\n",
                "none" => b"",
                _ => b"\n",
            };
            s.setup(req.timeout_ms, ending.last().copied())?;
            if matches!(req.op.as_str(), "query" | "write") {
                let mut msg = unescape(&req.data);
                if !msg.ends_with(ending) {
                    msg.extend_from_slice(ending);
                }
                s.write(&msg)?;
                r.sent = Some(shown(&msg));
            }
            match req.op.as_str() {
                "query" | "read" => {
                    // What arrived is shown even when the read ends in an error (a timeout).
                    let (got, err) = match s.read() {
                        Ok(g) => (g, None),
                        Err((g, e)) => (g, Some(e)),
                    };
                    if err.is_none() || !got.is_empty() {
                        r.bytes = Some(got.len());
                        r.received = Some(shown(&got));
                    }
                    if let Some(e) = err {
                        return Err(e);
                    }
                }
                "stb" => r.stb = Some(s.read_stb()?),
                "clear" => s.clear()?,
                "write" => {}
                other => return Err(format!("no console operation {other:?}")),
            }
            Ok(())
        };
        if let Err(e) = run(&mut r) {
            r.error = Some(e);
        }
        r.ms = start.elapsed().as_millis() as u64;
        r.connected = sessions.0.lock().unwrap().contains_key(&req.resource);
        Ok(r)
    })
    .await
}

/// Close the console's session to `resource` (releasing, say, a lock the instrument holds for it).
#[tauri::command]
fn close_session(sessions: tauri::State<'_, Arc<Sessions>>, resource: String) -> bool {
    sessions.0.lock().unwrap().remove(&resource).is_some()
}

/// Close the console's sessions to anything not in `keep` (what the tree has answering): an
/// instrument that went holds no session, and a serial port or USBTMC instrument that comes back is
/// not kept from other programs by one. The resources whose sessions were closed.
#[tauri::command]
fn close_sessions_except(sessions: tauri::State<'_, Arc<Sessions>>, keep: Vec<String>) -> Vec<String> {
    let mut held = sessions.0.lock().unwrap();
    let gone: Vec<String> = held.keys().filter(|r| !keep.contains(r)).cloned().collect();
    for r in &gone {
        held.remove(r);
    }
    gone
}

#[derive(Serialize)]
struct GatewayScan {
    ip: String,
    /// The addresses (interface gpib0) that have a listener; None: not asked, or no conclusion.
    found: Option<Vec<u8>>,
    error: Option<String>,
}

/// Which addresses on each LAN/GPIB gateway have a listener -- what `bus_scan` is for the boards. The same probe as
/// `symbiotic-ctl lan scan`; REN is not touched (other clients may be using the gateway), and a gateway that a console
/// of this window has a session on is not probed: a query is a write and a read, and a probe between them would address
/// the bus under the console's feet.
#[tauri::command]
async fn gateway_scan(sessions: tauri::State<'_, Arc<Sessions>>, ips: Vec<String>) -> Result<Vec<GatewayScan>, String> {
    let held: Vec<String> = sessions.0.lock().unwrap().keys().map(|r| r.to_ascii_lowercase()).collect();
    blocking(move || {
        use symbiotic_core::lan::{vxi11, Error};
        let mut out = Vec::new();
        for ip in ips {
            if held.iter().any(|r| r.starts_with(&format!("tcpip0::{}::", ip.to_ascii_lowercase()))) {
                out.push(GatewayScan { ip, found: None, error: Some("the console has a session on this gateway".into()) });
                continue;
            }
            match vxi11::find_listeners(&ip, "gpib0", std::time::Duration::from_secs(3), |_| {}) {
                Ok(found) => out.push(GatewayScan { ip, found: Some(found), error: None }),
                Err(Error::Unsupported) => out.push(GatewayScan { ip, found: None, error: Some("unsupported".into()) }),
                Err(e) => out.push(GatewayScan { ip, found: None, error: Some(e.to_string()) }),
            }
        }
        Ok(out)
    })
    .await
}

/// One resource an instrument answers to, as the main window knows it when it opens the console.
#[derive(Serialize, Deserialize, Clone)]
struct ConsoleMember {
    resource: String,
    online: bool,
    /// The protocol's name in the language in use ("HiSLIP"), for the selector.
    proto: String,
}

/// What a console window is given to talk to: the instrument, the resources it answers to and the one that answered.
#[derive(Serialize, Deserialize, Clone)]
struct ConsoleInit {
    resource: String,
    members: Vec<ConsoleMember>,
    via: String,
}

/// The console windows' initial data by window label, kept until the window is gone: a window's own page asks for it
/// (`console_init`), so nothing has to be put in an address, and what the window used is found again when it closes.
#[derive(Default)]
struct ConsoleInits(Mutex<HashMap<String, ConsoleInit>>);

/// Does closing the main window close the consoles too (the settings page's switch; the page tells us)? Not by default.
#[derive(Default)]
struct CloseConsoles(std::sync::atomic::AtomicBool);

#[tauri::command]
fn set_close_consoles(state: tauri::State<'_, Arc<CloseConsoles>>, on: bool) {
    state.0.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The label of an instrument's console window: one window for each, found again by it.
fn console_label(resource: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in resource.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("console-{h:016x}")
}

/// Open the console window of `resource`, or bring it forward if it is open. An async command: a window made
/// from a synchronous one stops the program on Windows.
#[tauri::command]
async fn open_console(app: tauri::AppHandle, resource: String, members: Vec<ConsoleMember>, via: String, title: String) -> Result<(), String> {
    let label = console_label(&resource);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        let _ = w.show();
        return w.set_focus().map_err(|e| e.to_string());
    }
    app.state::<Arc<ConsoleInits>>().0.lock().unwrap().insert(label.clone(), ConsoleInit { resource: resource.clone(), members, via });
    // As wide as the main window is made (tauri.conf.json), so a long answer has the room the main window has.
    let width = app.config().app.windows.first().map_or(1280.0, |w| w.width);
    own_title_bar(tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App("console.html".into()))
        .title(&title)
        .inner_size(width, 560.0)
        .min_inner_size(480.0, 360.0))
        .build()
        .map(|_| ())
        .map_err(|e| {
            app.state::<Arc<ConsoleInits>>().0.lock().unwrap().remove(&label);
            e.to_string()
        })
}

/// An instrument as the pictures window needs it: what to call it, and the texts a picture's pattern is tried on.
#[derive(Clone, Serialize, Deserialize)]
struct PictureInstrument {
    name: String,
    texts: Vec<String>,
}

/// The instruments the pictures window was last given (it asks for them when it starts).
#[derive(Default)]
struct PicturesInit(Mutex<Vec<PictureInstrument>>);

/// Open the pictures window (one, found again by its label), or bring it forward and give it the instruments as they are now.
/// An async command, like `open_console`.
#[tauri::command]
async fn open_pictures(app: tauri::AppHandle, instruments: Vec<PictureInstrument>, title: String) -> Result<(), String> {
    use tauri::Emitter;
    *app.state::<Arc<PicturesInit>>().0.lock().unwrap() = instruments.clone();
    if let Some(w) = app.get_webview_window("pictures") {
        let _ = w.emit("pictures-instruments", instruments);
        let _ = w.unminimize();
        let _ = w.show();
        return w.set_focus().map_err(|e| e.to_string());
    }
    own_title_bar(tauri::WebviewWindowBuilder::new(&app, "pictures", tauri::WebviewUrl::App("pictures.html".into()))
        .title(&title)
        .inner_size(760.0, 640.0)
        .min_inner_size(480.0, 360.0))
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// What the pictures window is for: the instruments its editor tries a pattern on.
#[tauri::command]
fn pictures_init(inits: tauri::State<'_, Arc<PicturesInit>>) -> Vec<PictureInstrument> {
    inits.0.lock().unwrap().clone()
}

/// An instrument's resource name the main window knows, and whether it answers: the aliases window lists those with no alias
/// too, to give one, and says which aliases' instruments are there.
#[derive(Clone, Serialize, Deserialize)]
struct KnownResource {
    resource: String,
    online: bool,
}

/// The resource names the main window knows, for the aliases window (it asks for them when it starts; the main window tells
/// it of a change).
#[derive(Default)]
struct AliasesInit(Mutex<Vec<KnownResource>>);

/// Open the aliases window (one, found again by its label), or bring it forward and give it the resource names known now.
/// An async command, like `open_console`.
#[tauri::command]
async fn open_aliases(app: tauri::AppHandle, resources: Vec<KnownResource>, title: String) -> Result<(), String> {
    use tauri::Emitter;
    *app.state::<Arc<AliasesInit>>().0.lock().unwrap() = resources.clone();
    if let Some(w) = app.get_webview_window("aliases") {
        let _ = w.emit("aliases-resources", resources);
        let _ = w.unminimize();
        let _ = w.show();
        return w.set_focus().map_err(|e| e.to_string());
    }
    own_title_bar(tauri::WebviewWindowBuilder::new(&app, "aliases", tauri::WebviewUrl::App("aliases.html".into()))
        .title(&title)
        .inner_size(820.0, 520.0)
        .min_inner_size(560.0, 300.0))
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The resource names known when the aliases window was opened.
#[tauri::command]
fn aliases_init(inits: tauri::State<'_, Arc<AliasesInit>>) -> Vec<KnownResource> {
    inits.0.lock().unwrap().clone()
}

/// What the console window that asks is for.
#[tauri::command]
fn console_init(window: tauri::WebviewWindow, inits: tauri::State<'_, Arc<ConsoleInits>>) -> Result<ConsoleInit, String> {
    inits.0.lock().unwrap().get(window.label()).cloned().ok_or_else(|| "this window has no instrument".to_string())
}

#[derive(Serialize, Deserialize)]
struct InstrumentSettings {
    resource: String,
    idn: bool,
    name: Option<String>,
    hidden: bool,
    /// How VISA users see the resource: `name`, `alias`, `both`, `none`; absent is the default. Read-only here:
    /// `config::set_names` writes it, and saving the rest keeps it.
    #[serde(default)]
    expose: Option<String>,
}

/// The per-instrument settings that differ from the defaults.
#[tauri::command]
fn instrument_settings() -> Vec<InstrumentSettings> {
    instruments::load()
        .into_iter()
        .map(|(resource, s)| InstrumentSettings { resource, idn: s.idn, name: s.name, hidden: s.hidden, expose: s.expose.map(|e| e.word().to_string()) })
        .collect()
}

/// What the window does with an instrument another computer shares, by alias (instruments.conf's `shared:` lines).
#[derive(Serialize, Deserialize)]
struct SharedSettings {
    alias: String,
    idn: bool,
    hidden: bool,
}

/// The shared instruments' settings that differ from the defaults (not asked, shown).
#[tauri::command]
fn shared_settings() -> Vec<SharedSettings> {
    instruments::load_shared().into_iter().map(|(alias, v)| SharedSettings { alias, idn: v.idn, hidden: v.hidden }).collect()
}

#[tauri::command]
fn set_shared_settings(settings: SharedSettings) -> Result<(), String> {
    instruments::set_shared(&settings.alias, instruments::SharedView { idn: settings.idn, hidden: settings.hidden }).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_instrument_settings(settings: InstrumentSettings) -> Result<(), String> {
    symbiotic_core::lanunits::writable(&settings.resource)?;
    let name = settings.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    // The exposure is the names panel's: saving the rest of a resource's settings keeps what is there.
    let expose = instruments::get(&settings.resource).expose;
    instruments::set(&settings.resource, &instruments::Settings { idn: settings.idn, name, hidden: settings.hidden, expose }).map_err(|e| e.to_string())
}

/// The first start after `hidden` stopped keeping an instrument out of `viFindRsrc`: see `instruments::migrate_hidden`.
#[tauri::command]
fn migrate_hidden(resources: Vec<String>) -> Result<(), String> {
    instruments::migrate_hidden(&resources).map_err(|e| e.to_string())
}

/// Does `viFindRsrc` list the hidden and the absent instruments too (instruments.conf, `list=all`)?
#[tauri::command]
fn list_all() -> bool {
    instruments::list_all()
}

#[tauri::command]
fn set_list_all(on: bool) -> Result<(), String> {
    instruments::set_list_all(on).map_err(|e| e.to_string())
}

/// Which features the window shows (features.conf). The file is written, every feature at its default, when there is
/// none; one that cannot be written leaves them at their defaults all the same.
#[derive(Serialize)]
struct Features {
    sharing: bool,
    service: bool,
    trace: bool,
    display_name: bool,
    alias_binding: bool,
    version: bool,
    tree_counts: bool,
}

#[tauri::command]
fn features() -> Features {
    let _ = symbiotic_core::features::create();
    let f = symbiotic_core::features::load();
    Features { sharing: f.sharing, service: f.service, trace: f.trace, display_name: f.display_name, alias_binding: f.alias_binding, version: f.version, tree_counts: f.tree_counts }
}

#[derive(Serialize)]
struct About {
    /// The build's (0.1.<commit count>, as the MSI and the Windows version resource).
    version: String,
    os: &'static str,
    arch: &'static str,
    config_dir: Option<String>,
}

#[tauri::command]
fn about(app: tauri::AppHandle) -> About {
    About {
        version: app.package_info().version.to_string(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        config_dir: symbiotic_core::paths::config_dir().map(|d| d.display().to_string()),
    }
}

/// `--lang <code>` and `--theme system|light|dark`: this run starts with them, over what the page
/// remembered; `--page` opens on one page: `settings`, `about`, `board:GPIB0`, `group:lan` (usb, serial),
/// `instrument:<resource>`. For shortcuts and screenshots.
#[derive(Serialize, Default)]
struct StartOptions {
    lang: Option<String>,
    theme: Option<String>,
    page: Option<String>,
}

#[tauri::command]
fn start_options() -> StartOptions {
    let mut o = StartOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lang" => o.lang = args.next(),
            "--theme" => o.theme = args.next(),
            "--page" => o.page = args.next(),
            _ => {}
        }
    }
    o
}

/// Every window draws its own title bar: on Windows and Linux the system's is gone and the page has the buttons; on
/// macOS the system's three buttons stay, over the page, which leaves them room, set down to sit in the middle of the
/// page's 40 px bar (tao's y is the height of the buttons' container less a button's; 22 puts their middle at 20). A
/// window of its own is given its title in the page's language when it is made (the `title` of the open_* commands): on
/// macOS a later title change puts the buttons back where AppKit keeps them (common.js).
pub(crate) fn own_title_bar<'a, R: tauri::Runtime, M: Manager<R>>(b: tauri::WebviewWindowBuilder<'a, R, M>) -> tauri::WebviewWindowBuilder<'a, R, M> {
    #[cfg(target_os = "macos")]
    return b.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true).traffic_light_position(tauri::LogicalPosition::new(16.0, 22.0));
    #[cfg(not(target_os = "macos"))]
    return b.decorations(false);
}

fn main() {
    // WebKitGTK's DMA-BUF renderer draws the page as torn stripes on a Raspberry Pi 5; shared
    // memory draws it right, and a page this size does not miss the GPU. The user's own setting wins.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    // The tree is what viFindRsrc finds; it must include what the setting keeps out of other programs' lists.
    instruments::see_everything();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(Sessions::default()))
        .manage(Arc::new(ConsoleInits::default()))
        .manage(Arc::new(CloseConsoles::default()))
        .manage(Arc::new(PicturesInit::default()))
        .manage(Arc::new(AliasesInit::default()))
        .manage(Arc::new(trace::TraceCursor::default()))
        // The main window is made here rather than by Tauri from the configuration, to have its own title bar.
        .setup(|app| {
            let config = app.config().app.windows[0].clone();
            own_title_bar(tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?).build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if !matches!(event, tauri::WindowEvent::Destroyed) {
                return;
            }
            let app = window.app_handle();
            let label = window.label();
            if label == "main" {
                // With the switch on, the consoles go with the window that opened them and the program ends; without it
                // they stay, and the program ends when the last one is closed (the default when no window is left).
                if app.state::<Arc<CloseConsoles>>().0.load(std::sync::atomic::Ordering::Relaxed) {
                    app.exit(0);
                }
            } else if label == "trace" {
                trace::window_closed();
            } else if label.starts_with("console-") {
                // The console's sessions go with its window (a lock the instrument holds for one, a serial port).
                if let Some(init) = app.state::<Arc<ConsoleInits>>().0.lock().unwrap().remove(label) {
                    let sessions = app.state::<Arc<Sessions>>();
                    let mut held = sessions.0.lock().unwrap();
                    for m in &init.members {
                        held.remove(&m.resource);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            identify,
            watch,
            bus_scan,
            forget_board,
            gateway_scan,
            io,
            close_session,
            open_console,
            console_init,
            open_pictures,
            pictures_init,
            open_aliases,
            aliases_init,
            set_close_consoles,
            close_sessions_except,
            instrument_settings,
            set_instrument_settings,
            shared_settings,
            set_shared_settings,
            migrate_hidden,
            config::set_names,
            config::set_expose,
            config::web_page,
            picpack::export_picture_pack,
            picpack::read_picture_pack,
            config::open_web,
            config::pictures,
            config::pictures_sync,
            config::pictures_adopt,
            config::picture_data,
            config::save_picture,
            config::picture_original,
            config::remove_picture,
            list_all,
            set_list_all,
            about,
            start_options,
            features,
            config::set_speed,
            config::set_ren,
            config::lan,
            config::lan_check,
            config::lan_add,
            config::lan_remove,
            config::lan_set_discover,
            config::lan_scan,
            config::lan_gateway_find,
            config::lan_reach,
            config::serial_ports,
            config::set_bridge,
            config::usb_devices,
            config::aliases,
            config::aliases_move,
            config::set_alias,
            config::identities,
            config::bind_alias,
            config::unbind_alias,
            config::lan_unit_seen,
            config::lan_unit_settle,
            config::follow_bound,
            config::units_check,
            config::forget_instrument,
            config::service_running,
            config::start_service,
            config::stop_service,
            config::log_tail,
            config::diag_bundle,
            config::show_in_folder,
            config::release_idle,
            config::set_release_idle,
            config::registrations,
            config::set_registration,
            share::share_status,
            share::share_probe,
            share::share_join,
            share::share_set,
            share::share_alias,
            share::share_follow,
            share::share_registry,
            trace::trace_state,
            trace::trace_set,
            trace::trace_clear,
            trace::trace_read,
            trace::trace_export,
            trace::open_trace
        ])
        .build(tauri::generate_context!())
        .expect("the window could not start")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                trace::program_ends();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_go_both_ways() {
        assert_eq!(unescape(r"*IDN?\n"), b"*IDN?\n");
        assert_eq!(unescape(r"a\x00\xfFb\\\r\t"), b"a\x00\xFFb\\\r\t");
        assert_eq!(unescape(r"C:\path\x4"), b"C:\\path\\x4", "an unknown or short escape is itself");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unescape(&shown(&all)), all);
        // Text that is not ASCII goes as its UTF-8, and a backslash at the very end is itself.
        assert_eq!(unescape("VOLT 1,\u{6d4b}\\n"), [&b"VOLT 1,"[..], "\u{6d4b}".as_bytes(), b"\n"].concat());
        assert_eq!(unescape("\u{e9}\\n"), [0xC3, 0xA9, b'\n']);
        assert_eq!(unescape("end\\"), b"end\\");
        assert_eq!(shown(&[0xC3, 0xA9]), "\\xC3\\xA9", "bytes, not text: the console shows what was on the wire");
    }

    #[test]
    fn a_console_label_is_the_instruments_own_and_a_label_tauri_takes() {
        let a = console_label("GPIB0::15::INSTR");
        assert_eq!(a, console_label("GPIB0::15::INSTR"), "found again by its resource");
        assert_ne!(a, console_label("GPIB0::16::INSTR"));
        assert_ne!(console_label("K2015"), console_label("k2015"), "the name as given (an alias and its resource are told apart elsewhere)");
        // Tauri's labels: letters, digits, '-', '/', ':', '_'.
        for r in ["GPIB0::15::INSTR", "TCPIP0::198.18.68.57::hislip0::INSTR", "\u{6d4b}\u{8bd5}", ""] {
            assert!(console_label(r).chars().all(|c| c.is_ascii_alphanumeric() || "-/:_".contains(c)), "{r}");
        }
    }
}
