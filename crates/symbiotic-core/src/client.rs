//! The client side of the daemon: what the VISA DLL and symbiotic-ctl talk through.

use crate::adapter::{Error, ReadEnd, ReadResult};
use crate::backend::{Entry, Kind};
use crate::ipc::*;
use crate::local;
use crate::locks::Target;
use std::path::Path;
use std::time::Duration;

pub struct Client {
    s: local::Stream,
}

/// The daemon's channel if one answers. Another user's process serving the channel is an error.
fn dial() -> Result<Option<local::Stream>, Error> {
    match local::connect() {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(Error::Io(e.to_string())),
        Err(_) => Ok(None),
    }
}

fn spawn_daemon(dir: &Path) -> Result<(), Error> {
    let exe = dir.join(if cfg!(windows) { "symbioticd.exe" } else { "symbioticd" });
    spawn_detached(&exe).map_err(|e| Error::Io(spawn_error(&exe, &e)))
}

/// The error message when the daemon cannot be started, with a hint when its executable is missing.
fn spawn_error(exe: &Path, e: &std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("cannot start {}: {e}", exe.display());
    }
    let profile = exe.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str());
    let hint = match profile {
        Some("release") => "build it first: cargo build --release -p symbiotic-core --bin symbioticd",
        Some("debug") => "build it first: cargo build -p symbiotic-core --bin symbioticd",
        _ => "it belongs beside this program: reinstall",
    };
    format!("the daemon {} is missing; {hint}", exe.display())
}

/// Start the daemon detached from this process, in its own directory and with no inherited handles.
#[cfg(windows)]
fn spawn_detached(exe: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let mut cmd: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
    let dir: Option<Vec<u16>> = exe.parent().map(|d| d.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect());
    // SAFETY: plain-old-data structs, zeroed as the API expects; `cmd` is a writable NUL-terminated buffer.
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            std::ptr::null(),
            dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(exe);
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(d) = exe.parent() {
        c.current_dir(d);
    }
    // A process group of its own, so the caller's Ctrl-C does not reach it.
    c.process_group(0);
    let mut child = c.spawn()?;
    // Reap it if it exits while this process runs.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The build version of the daemon in `dir` (`symbioticd --version`), if it reports one.
fn build_of(dir: &Path) -> Option<String> {
    let exe = dir.join(if cfg!(windows) { "symbioticd.exe" } else { "symbioticd" });
    let o = std::process::Command::new(exe).arg("--version").stdin(std::process::Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && build_number(&v).is_some()).then_some(v)
}

/// Whether a daemon of IPC version `v` and build `build` is older than this client: a lower IPC version, or the same one
/// with a lower or missing build number. Build number 0 is not compared.
fn older(v: u32, build: &str) -> bool {
    if v != VERSION {
        return v < VERSION;
    }
    let mine = build_number(BUILD).filter(|b| b.2 > 0);
    match (build_number(build), mine) {
        (_, None) => false,
        (None, Some(_)) => build.is_empty(),
        (Some(theirs), Some(mine)) => theirs.2 > 0 && theirs < mine,
    }
}

/// A request for one instrument: a GPIB one by board and address, a USBTMC one by its number
/// (with F_USB), a serial one by its COM number (with F_ASRL). Further flags are or-ed in by the
/// caller.
fn at(t: Target) -> Req {
    match t {
        Target::Gpib(board, pad, sad) => Req { board, pad, sad, ..Default::default() },
        Target::Usb(n) => Req { board: n, flags: F_USB, ..Default::default() },
        Target::Asrl(n) => Req { board: n, flags: F_ASRL, ..Default::default() },
    }
}

/// A GPIB board's listeners, as `bus_scan` answers.
#[derive(Debug, Clone)]
pub struct BusScan {
    /// The addresses that answered.
    pub pads: Vec<u8>,
    /// How old the result is, seconds (0: scanned for this ask).
    pub age: u32,
    /// What the daemon knows each of them to be (`*IDN?`), by address, when asked for (`bus_scan_units`).
    pub units: Vec<(u8, String)>,
}

impl BusScan {
    fn parse(ext: u32, data: &[u8]) -> Option<BusScan> {
        let age = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
        if ext & 2 == 0 {
            return Some(BusScan { pads: data[4..].to_vec(), age, units: Vec::new() });
        }
        let (mut pads, mut units, mut i) = (Vec::new(), Vec::new(), 4);
        while i < data.len() {
            let pad = data[i];
            let len = u16::from_le_bytes(data.get(i + 1..i + 3)?.try_into().ok()?);
            i += 3;
            pads.push(pad);
            if len != 0xFFFF {
                let t = data.get(i..i + len as usize)?;
                units.push((pad, String::from_utf8_lossy(t).into_owned()));
                i += len as usize;
            }
        }
        Some(BusScan { pads, age, units })
    }
}

/// One USBTMC instrument number, as `usb_list` answers it.
#[derive(Debug, Clone)]
pub struct UsbListed {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    pub product: String,
    pub present: bool,
}

impl UsbListed {
    pub fn key(&self) -> String {
        crate::usbtmc::key(self.vid, self.pid, &self.serial)
    }
}

/// One board number, as `list` answers it.
#[derive(Debug, Clone)]
pub struct Listed {
    pub entry: Entry,
    pub present: bool,
    /// Absent and forgotten (boards.rs): a number kept so that the others do not move, not to be shown.
    pub forgotten: bool,
    /// What the adapter says it is (Backend::model), if it says.
    pub model: Option<String>,
}

/// Where a daemon is started from when the caller does not say: what the library set (`set_daemon_dir`), else beside
/// this program.
static DAEMON_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Start daemons from `dir` when a caller does not say where.
pub fn set_daemon_dir(dir: std::path::PathBuf) {
    let _ = DAEMON_DIR.set(dir);
}

/// Set while this process must not start a daemon (the configuration window after its user stopped the service).
/// `connect` then fails as `connect_existing` does when none is running.
static HOLD_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Hold (or let go of) the starting of a daemon by this process; a daemon already running is used either way.
pub fn hold_start(on: bool) {
    HOLD_START.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn start_held() -> bool {
    HOLD_START.load(std::sync::atomic::Ordering::SeqCst)
}

fn default_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = DAEMON_DIR.get() {
        return Some(d.clone());
    }
    let own = std::env::current_exe().ok()?.parent()?.to_path_buf();
    crate::paths::daemon_dir_for(&own).or(Some(own))
}

/// One of the daemon's connections, as CLIENTS reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Connected {
    /// Its process; None where the system does not say, and for a machine of the group.
    pub pid: Option<u32>,
    /// The machine of the group it serves ("member NAME (key) from ADDRESS, alias ALIAS").
    pub peer: Option<String>,
    /// How long it has been connected and since its last request, in seconds, if reported.
    pub secs: Option<u64>,
    pub idle: Option<u64>,
    /// The locks it holds: "GPIB0::20::INSTR exclusive" and the like.
    pub locks: Vec<String>,
}

impl Connected {
    fn parse(line: &str) -> Connected {
        let mut f = line.split('\t');
        let pid = f.next().and_then(|p| p.parse().ok());
        let peer = f.next().filter(|p| !p.is_empty()).map(String::from);
        let secs = f.next().and_then(|s| s.parse().ok());
        let idle = f.next().and_then(|s| s.parse().ok());
        let locks = f.next().map(|l| l.split(", ").filter(|x| !x.is_empty()).map(String::from).collect()).unwrap_or_default();
        Connected { pid, peer, secs, idle, locks }
    }

    /// Who it is, in a few words.
    pub fn who(&self) -> String {
        match (&self.peer, self.pid) {
            (Some(p), _) => p.clone(),
            (None, Some(p)) => format!("process {p}"),
            (None, None) => "a process the system does not name".into(),
        }
    }
}

impl Client {
    /// Connect to the daemon, starting `symbioticd` from `daemon_dir` (else `set_daemon_dir`'s, else beside this program)
    /// first if none is running.
    pub fn connect(daemon_dir: Option<&Path>) -> Result<Client, Error> {
        let mut s = dial()?;
        if s.is_none() && start_held() {
            return Err(Error::Io("the service is stopped".into()));
        }
        if s.is_none() {
            let dir = daemon_dir.map(Path::to_path_buf).or_else(default_dir);
            let dir = dir.ok_or_else(|| Error::Io("cannot tell where symbioticd is".into()))?;
            spawn_daemon(&dir)?;
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(x) = dial()? {
                    s = Some(x);
                    break;
                }
            }
        }
        let mut c = Client { s: s.ok_or_else(|| Error::Io("symbioticd does not answer".into()))? };
        let (v, build) = c.ping()?;
        if older(v, &build) {
            let dir = daemon_dir.map(Path::to_path_buf).or_else(default_dir);
            return Self::take_over(c, v, &build, dir.as_deref());
        }
        if v != VERSION {
            return Err(Error::Io(format!(
                "symbioticd {} speaks IPC v{v}, this client ({BUILD}) v{VERSION}: it is newer than this program's library",
                if build.is_empty() { "(an older one)" } else { &build }
            )));
        }
        Ok(c)
    }

    /// The daemon's IPC version and build version ("" if it reports none).
    pub fn daemon_version(&mut self) -> Result<(u32, String), Error> {
        self.ping()
    }

    fn ping(&mut self) -> Result<(u32, String), Error> {
        let r = self.call(Req { op: op::PING, ..Default::default() })?;
        Ok((r.ext, String::from_utf8_lossy(&r.data).trim().to_string()))
    }

    /// `c` is to an older daemon: stop it, if nothing else is using it, and start this build's from `dir`. If something
    /// else is using it, it is left running and named in the error.
    fn take_over(mut c: Client, v: u32, build: &str, dir: Option<&Path>) -> Result<Client, Error> {
        let theirs = if build.is_empty() { format!("IPC v{v}") } else { format!("{build} (IPC v{v})") };
        // Check the daemon to be started is this build's before stopping the running one.
        let dir = dir.ok_or_else(|| Error::Io("cannot tell where symbioticd is".into()))?;
        match build_of(dir) {
            Some(b) if b == BUILD => {}
            found => {
                return Err(Error::Io(format!(
                    "symbioticd {theirs} is older than this client ({BUILD}), but the one beside it ({}) is {}: the running one is left as it is",
                    dir.display(),
                    found.as_deref().unwrap_or("not one that says its version")
                )))
            }
        }
        let dir = Some(dir);
        let others = c.clients().map_err(|e| Error::Io(format!("symbioticd {theirs} is older than this client ({BUILD}) and cannot say who uses it ({e}): stop it (symbiotic-ctl stop) and retry")))?;
        if !others.is_empty() {
            let who: Vec<String> = others.iter().map(Connected::who).collect();
            return Err(Error::Io(format!(
                "symbioticd {theirs} is older than this client ({BUILD}) and is in use ({}): close those, or stop it (symbiotic-ctl stop), and retry",
                who.join(", ")
            )));
        }
        let _ = c.shutdown();
        drop(c);
        // Wait until the old daemon has released the channel.
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            matches!(dial(), Ok(None))
        });
        if !gone {
            return Err(Error::Io(format!("symbioticd {theirs} was asked to stop for this client ({BUILD}) and is still there")));
        }
        let mut n = Client::connect_new(dir)?;
        let (nv, nbuild) = n.ping()?;
        if nv != VERSION || older(nv, &nbuild) {
            return Err(Error::Io(format!(
                "the symbioticd started from {} is {} (IPC v{nv}), not this client's {BUILD} (IPC v{VERSION}): it does not belong with this library",
                dir.map_or("here".to_string(), |d| d.display().to_string()),
                if nbuild.is_empty() { "an older one" } else { &nbuild }
            )));
        }
        Ok(n)
    }

    /// Start the daemon from `dir` and connect to it (another may have the channel by then: that one is connected to).
    fn connect_new(dir: Option<&Path>) -> Result<Client, Error> {
        let dir = dir.ok_or_else(|| Error::Io("cannot tell where symbioticd is".into()))?;
        spawn_daemon(dir)?;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(s) = dial()? {
                return Ok(Client { s });
            }
        }
        Err(Error::Io("symbioticd does not answer".into()))
    }

    /// Connect only if a daemon is already running; never start one.
    pub fn connect_existing() -> Result<Client, Error> {
        Ok(Client { s: dial()?.ok_or(Error::Gone)? })
    }

    /// One exchange. A transport failure is `Gone`: the connection to the daemon is lost.
    fn raw(&mut self, q: Req) -> Result<Resp, Error> {
        // A request that could not be written never reached the daemon (`NotSent`), so the caller may resend it on a
        // new connection; one written but not answered may have run.
        q.write_to(&mut self.s).map_err(|_| Error::NotSent)?;
        Resp::read_from(&mut self.s).map_err(|_| Error::Gone)
    }

    fn call(&mut self, q: Req) -> Result<Resp, Error> {
        let r = self.raw(q)?;
        match r.error() {
            Some(e) => Err(e),
            None => Ok(r),
        }
    }

    fn simple(&mut self, op: u8, board: u16, pad: u8) -> Result<Resp, Error> {
        self.call(Req { op, board, pad, ..Default::default() })
    }

    /// Every board number the daemon has given out, in order: the adapter behind it and whether
    /// it is attached now. A number stays with its adapter while the daemon runs (boards.rs), so an
    /// unplugged one is listed absent rather than dropped from the list.
    pub fn list(&mut self) -> Result<Vec<Listed>, Error> {
        self.list_flags(0)
    }

    /// `list`, by which this connection checks the numbers' adapters (ipc.rs, `F_BIND`): its later requests for a number that has
    /// gone to another adapter since are refused (`Error::Gone`).
    pub fn list_bound(&mut self) -> Result<Vec<Listed>, Error> {
        self.list_flags(F_BIND)
    }

    fn list_flags(&mut self, flags: u8) -> Result<Vec<Listed>, Error> {
        let r = self.call(Req { op: op::LIST, flags, ..Default::default() })?;
        Ok(String::from_utf8_lossy(&r.data)
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let (serial, pid, kind) = (f.next()?, f.next()?, f.next()?);
                let entry = Entry { serial: serial.to_string(), pid: pid.parse().ok()?, kind: Kind::from_name(kind)? };
                let rest: Vec<&str> = f.collect();
                let absent = rest.first() == Some(&"absent");
                let forgotten = absent && rest.get(1) == Some(&"forgotten");
                let model = rest.iter().find_map(|x| x.strip_prefix("model=")).map(str::to_string);
                Some(Listed { entry, present: !absent, forgotten, model })
            })
            .collect())
    }

    /// The daemon's other connections.
    pub fn clients(&mut self) -> Result<Vec<Connected>, Error> {
        let r = self.call(Req { op: op::CLIENTS, count: 1, ..Default::default() })?;
        Ok(String::from_utf8_lossy(&r.data).lines().filter(|l| !l.is_empty()).map(Connected::parse).collect())
    }

    /// The addresses of `board` that have a listener, by the daemon's scan (ipc.rs, BUS_SCAN): a result not older than `max_age` seconds
    /// is taken as it is, else the daemon scans now, unless another program is using the service (`Error::Busy`).
    pub fn bus_scan(&mut self, board: u16, max_age: u32) -> Result<BusScan, Error> {
        self.scan_with(board, max_age, 0)
    }

    /// `bus_scan`, with what the daemon knows each listener to be.
    pub fn bus_scan_units(&mut self, board: u16, max_age: u32) -> Result<BusScan, Error> {
        self.scan_with(board, max_age, F_UNITS)
    }

    fn scan_with(&mut self, board: u16, max_age: u32, flags: u8) -> Result<BusScan, Error> {
        let r = self.call(Req { op: op::BUS_SCAN, board, count: max_age, flags, data: std::process::id().to_le_bytes().to_vec(), ..Default::default() })?;
        BusScan::parse(r.ext, &r.data).ok_or(Error::Io("the daemon's bus scan answer is short".into()))
    }

    /// Have the daemon refresh the group's directory if it is old (ipc.rs, SHARE_REFRESH).
    pub fn share_refresh(&mut self) -> Result<(), Error> {
        self.call(Req { op: op::SHARE_REFRESH, ..Default::default() })?;
        Ok(())
    }

    /// The same, now, whatever the age of what the daemon has.
    pub fn share_refresh_now(&mut self) -> Result<(), Error> {
        self.call(Req { op: op::SHARE_REFRESH, count: 1, ..Default::default() })?;
        Ok(())
    }

    /// Make this connection a tunnel to a shared alias another machine owns (ipc.rs, PEER_CONNECT).
    /// Afterwards every request on it is that instrument's, whatever target it names.
    pub fn peer_connect(&mut self, alias: &str) -> Result<crate::share::peer::Opened, Error> {
        let r = self.call(Req { op: op::PEER_CONNECT, data: alias.as_bytes().to_vec(), ..Default::default() })?;
        match r.error() {
            Some(e) => Err(e),
            None => crate::share::peer::decode_opened(&r.data).map_err(Error::Io),
        }
    }

    /// An administrator command for the registry embedded in the daemon (share/admin.rs).
    pub fn share_admin(&mut self, line: &str) -> Result<String, Error> {
        let r = self.call(Req { op: op::SHARE_ADMIN, data: line.as_bytes().to_vec(), ..Default::default() })?;
        match r.error() {
            Some(e) => Err(e),
            None => Ok(String::from_utf8_lossy(&r.data).into_owned()),
        }
    }

    /// Every USBTMC instrument number the daemon has given out, in order, and whether its
    /// instrument is attached now (numbers stay put, as board numbers do).
    pub fn usb_list(&mut self) -> Result<Vec<UsbListed>, Error> {
        let r = self.simple(op::USB_LIST, 0, 0)?;
        Ok(String::from_utf8_lossy(&r.data)
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let (vid, pid, serial, product) = (f.next()?, f.next()?, f.next()?, f.next().unwrap_or(""));
                Some(UsbListed {
                    vid: u16::from_str_radix(vid, 16).ok()?,
                    pid: u16::from_str_radix(pid, 16).ok()?,
                    serial: serial.to_string(),
                    product: product.to_string(),
                    present: f.next() != Some("absent"),
                })
            })
            .collect())
    }

    /// Open USBTMC instrument `n` in the daemon, if it is not open yet: whether that worked, and
    /// its interrupt IN endpoint's packet size (0 without one).
    pub fn usb_probe(&mut self, n: u16) -> Result<(bool, u16), Error> {
        let ext = self.on(Target::Usb(n), op::PROBE)?.ext;
        Ok((ext & 1 != 0, (ext >> 16) as u16))
    }

    /// A USB control transfer to USBTMC instrument `n`: what came in (nothing, going out).
    pub fn usb_control(&mut self, n: u16, request_type: u8, request: u8, value: u16, index: u16, length: u16, data: &[u8]) -> Result<Vec<u8>, Error> {
        let mut q = at(Target::Usb(n));
        q.op = op::USB_CTRL;
        q.count = request_type as u32 | (request as u32) << 8 | (value as u32) << 16;
        q.data = index.to_le_bytes().iter().chain(length.to_le_bytes().iter()).chain(data).copied().collect();
        Ok(self.call(q)?.data)
    }

    /// Open serial port COM`n` in the daemon for this connection (it stays open while the
    /// connection is there). `NoBoard`: no such port; `Busy`: another program has it.
    pub fn asrl_open(&mut self, n: u16) -> Result<(), Error> {
        self.on(Target::Asrl(n), op::PROBE).map(|_| ())
    }

    /// A serial write, ending with a break of `break_ms` if asked.
    pub fn asrl_write(&mut self, n: u16, data: &[u8], break_ms: Option<u16>, tmo: u32) -> Result<usize, (usize, Error)> {
        let mut q = at(Target::Asrl(n));
        q.op = op::WRITE;
        if let Some(ms) = break_ms {
            q.flags |= F_EOI;
            q.count = ms as u32;
        }
        q.tmo = tmo;
        q.data = data.to_vec();
        let r = self.raw(q).map_err(|e| (0, e))?;
        match r.error() {
            None => Ok(r.ext as usize),
            Some(e) => Err((r.ext as usize, e)),
        }
    }

    /// viClear on a serial port: `*CLS\n` if `strings`, else a break of `break_ms`.
    pub fn asrl_clear(&mut self, n: u16, strings: bool, break_ms: u16, tmo: u32) -> Result<(), Error> {
        let mut q = at(Target::Asrl(n));
        q.op = op::CLEAR;
        q.flags |= if strings { F_ON } else { 0 };
        q.count = break_ms as u32;
        q.tmo = tmo;
        self.call(q).map(|_| ())
    }

    /// `*TRG\n` to a serial instrument.
    pub fn asrl_trigger(&mut self, n: u16, tmo: u32) -> Result<(), Error> {
        let mut q = at(Target::Asrl(n));
        q.op = op::TRIGGER;
        q.tmo = tmo;
        self.call(q).map(|_| ())
    }

    pub fn asrl_set(&mut self, n: u16, field: u32, value: u32) -> Result<(), Error> {
        let mut q = at(Target::Asrl(n));
        q.op = op::ASRL_SET;
        q.count = field;
        q.data = value.to_le_bytes().to_vec();
        self.call(q).map(|_| ())
    }

    pub fn asrl_get(&mut self, n: u16) -> Result<crate::serial::Status, Error> {
        let r = self.on(Target::Asrl(n), op::ASRL_GET)?;
        crate::serial::Status::decode(&r.data).ok_or_else(|| Error::Io("daemon sent a bad serial status".into()))
    }

    pub fn asrl_flush(&mut self, n: u16, what: u32, tmo: u32) -> Result<(), Error> {
        let mut q = at(Target::Asrl(n));
        q.op = op::FLUSH;
        q.count = what;
        q.tmo = tmo;
        self.call(q).map(|_| ())
    }

    /// Is an instrument there? At a secondary address (not `NO_SAD`) only whether the adapter can
    /// reach one (backend.rs, `probe_sad`).
    pub fn probe(&mut self, board: u16, pad: u8, sad: u8) -> Result<bool, Error> {
        let q = Req { op: op::PROBE, board, pad, sad, ..Default::default() };
        Ok(self.call(q)?.ext != 0)
    }

    /// Bytes taken by the adapter; on error also how many were taken before it.
    pub fn write(&mut self, t: Target, data: &[u8], eoi: bool, tmo: u32) -> Result<usize, (usize, Error)> {
        self.write_with(t, data, eoi, tmo, 0)
    }

    /// `write`, unless another connection used the instrument lately (ipc.rs, `F_IF_QUIET`): `Error::Busy` then.
    pub fn write_if_quiet(&mut self, t: Target, data: &[u8], eoi: bool, tmo: u32) -> Result<usize, (usize, Error)> {
        self.write_with(t, data, eoi, tmo, F_IF_QUIET)
    }

    fn write_with(&mut self, t: Target, data: &[u8], eoi: bool, tmo: u32, flags: u8) -> Result<usize, (usize, Error)> {
        let mut q = at(t);
        q.op = op::WRITE;
        q.flags |= flags | if eoi { F_EOI } else { 0 };
        q.tmo = tmo;
        q.data = data.to_vec();
        let r = self.raw(q).map_err(|e| (0, e))?;
        match r.error() {
            None => Ok(r.ext as usize),
            Some(e) => Err((r.ext as usize, e)),
        }
    }

    pub fn read(&mut self, t: Target, count: u32, term: Option<u8>, tmo: u32) -> Result<ReadResult, Error> {
        let mut q = at(t);
        q.op = op::READ;
        q.flags |= if term.is_some() { F_TERM } else { 0 };
        q.term = term.unwrap_or(0);
        q.tmo = tmo;
        q.count = count;
        let r = self.call(q)?;
        let end = ReadEnd::from_u32(r.ext).ok_or_else(|| Error::Io("daemon sent a bad read result".into()))?;
        Ok(ReadResult { data: r.data, end })
    }

    /// One request for an instrument, with nothing but the operation.
    fn on(&mut self, t: Target, op: u8) -> Result<Resp, Error> {
        let mut q = at(t);
        q.op = op;
        self.call(q)
    }

    pub fn clear(&mut self, t: Target) -> Result<(), Error> {
        self.on(t, op::CLEAR).map(|_| ())
    }

    pub fn trigger(&mut self, t: Target) -> Result<(), Error> {
        self.on(t, op::TRIGGER).map(|_| ())
    }

    pub fn spoll(&mut self, t: Target, tmo: u32) -> Result<u8, Error> {
        let mut q = at(t);
        q.op = op::SPOLL;
        q.tmo = tmo;
        Ok(self.call(q)?.ext as u8)
    }

    pub fn local(&mut self, t: Target) -> Result<(), Error> {
        self.on(t, op::LOCAL).map(|_| ())
    }

    // On a GPIB board LLO, REN and IFC act on the whole bus; the target is the session's
    // instrument, which is what a lock another session holds is checked against. A USBTMC
    // instrument takes LLO and REN as requests of its own (USB488).
    pub fn llo(&mut self, t: Target) -> Result<(), Error> {
        self.on(t, op::LLO).map(|_| ())
    }

    pub fn ren(&mut self, t: Target, enable: bool) -> Result<(), Error> {
        let mut q = at(t);
        q.op = op::REN;
        q.flags |= if enable { F_ON } else { 0 };
        self.call(q).map(|_| ())
    }

    pub fn ifc(&mut self, board: u16, pad: u8) -> Result<(), Error> {
        self.simple(op::IFC, board, pad).map(|_| ())
    }

    /// The interface level (a GPIB INTFC session): interface messages with ATN.
    pub fn command(&mut self, board: u16, cmds: &[u8], tmo: u32) -> Result<(), Error> {
        let q = Req { op: op::CMD, board, pad: crate::backend::RAW, tmo, data: cmds.to_vec(), ..Default::default() };
        self.call(q).map(|_| ())
    }

    /// Data out as whatever the commands made the controller; nobody addressed.
    pub fn raw_write(&mut self, board: u16, data: &[u8], eoi: bool, tmo: u32) -> Result<usize, (usize, Error)> {
        let q = Req {
            op: op::RAW_WRITE, board, pad: crate::backend::RAW, flags: if eoi { F_EOI } else { 0 }, tmo,
            data: data.to_vec(), ..Default::default()
        };
        let r = self.raw(q).map_err(|e| (0, e))?;
        match r.error() {
            None => Ok(r.ext as usize),
            Some(e) => Err((r.ext as usize, e)),
        }
    }

    /// Data in, the same way.
    pub fn raw_read(&mut self, board: u16, count: u32, term: Option<u8>, tmo: u32) -> Result<ReadResult, Error> {
        let q = Req {
            op: op::RAW_READ, board, pad: crate::backend::RAW, flags: if term.is_some() { F_TERM } else { 0 },
            term: term.unwrap_or(0), tmo, count, ..Default::default()
        };
        let r = self.call(q)?;
        let end = ReadEnd::from_u32(r.ext).ok_or_else(|| Error::Io("daemon sent a bad read result".into()))?;
        Ok(ReadResult { data: r.data, end })
    }

    /// viLock on one instrument: `shared` or exclusive, waiting up to `timeout_ms` (u32::MAX:
    /// forever), with the requested shared key. Returns whether it nested, and the access key.
    pub fn lock(&mut self, t: Target, shared: bool, timeout_ms: u32, key: Option<&str>) -> Result<(bool, String), Error> {
        let mut q = at(t);
        q.op = op::LOCK;
        q.flags |= if shared { 2 } else { 1 };
        q.count = timeout_ms;
        q.data = key.unwrap_or("").as_bytes().to_vec();
        let r = self.call(q)?;
        Ok((r.ext != 0, String::from_utf8_lossy(&r.data).into_owned()))
    }

    /// viUnlock. Returns what the session still holds there: 0 nothing, 1 exclusive, 2 shared.
    pub fn unlock(&mut self, t: Target) -> Result<u32, Error> {
        Ok(self.on(t, op::UNLOCK)?.ext)
    }

    /// The instrument's lock state, whoever holds it: 0 none, 1 exclusive, 2 shared.
    pub fn lock_state(&mut self, t: Target) -> Result<u32, Error> {
        Ok(self.on(t, op::LOCK_STATE)?.ext)
    }

    /// Select (1 low, 2 normal, 3 high) or, with 0, read the adapter's T1 level.
    pub fn speed(&mut self, board: u16, level: u8) -> Result<u8, Error> {
        Ok(self.call(Req { op: op::SPEED, board, count: level as u32, ..Default::default() })?.ext as u8)
    }

    /// The bus lines (proto::BUS_*, set = asserted) and which of them the adapter can tell.
    pub fn lines(&mut self, board: u16) -> Result<(u16, u16), Error> {
        let ext = self.simple(op::LINES, board, 0)?.ext;
        Ok((ext as u16, !((ext >> 16) as u16)))
    }

    /// Ask to be told when the instrument requests service. Returns the subscription id.
    pub fn srq_subscribe(&mut self, t: Target) -> Result<u32, Error> {
        Ok(self.on(t, op::SRQ_SUB)?.ext)
    }

    pub fn srq_unsubscribe(&mut self, id: u32) -> Result<(), Error> {
        self.call(Req { op: op::SRQ_UNSUB, count: id, ..Default::default() }).map(|_| ())
    }

    /// Wait up to `ms` for a service request; the status byte the daemon's poll read.
    pub fn srq_wait(&mut self, id: u32, ms: u32) -> Result<Option<u8>, Error> {
        let r = self.call(Req { op: op::SRQ_WAIT, count: id, tmo: ms, ..Default::default() })?;
        Ok((r.ext == 1).then(|| r.data.first().copied().unwrap_or(0)))
    }

    /// Forget an absent adapter's board number (boards.rs). Gone: it is attached.
    pub fn forget(&mut self, board: u16) -> Result<(), Error> {
        self.simple(op::FORGET, board, 0).map(|_| ())
    }

    /// Ask the daemon to exit.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        self.simple(op::SHUTDOWN, 0, 0).map(|_| ())
    }
}

/// Have the daemon read `share.conf` again by restarting it, but only if no program is using it. `start`: also start
/// one if none runs. Returns a message saying what happened.
pub fn restart_daemon_if_idle(start: bool) -> String {
    let Ok(mut c) = Client::connect_existing() else {
        if !start {
            return "no daemon is running; the next one reads the new setting".to_string();
        }
        return match Client::connect(None) {
            Ok(_) => "the daemon was started".to_string(),
            Err(e) => format!("the daemon could not be started: {e}"),
        };
    };
    let using = c.clients().map(|v| v.len()).unwrap_or(1);
    if using > 0 {
        return format!("{using} program(s) are using the daemon, so it was not restarted: `symbiotic-ctl stop` when they are done, and the next one reads the new setting");
    }
    let _ = c.shutdown();
    drop(c);
    // Wait until the old daemon has released the channel.
    let gone = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        matches!(dial(), Ok(None))
    });
    if !gone {
        return "the daemon was asked to stop and is still there: the next one reads the new setting".to_string();
    }
    if !start {
        return "the daemon was stopped; the next one reads the new setting".to_string();
    }
    match Client::connect(None) {
        Ok(_) => "the daemon was restarted with the new setting".to_string(),
        Err(e) => format!("the daemon was stopped but not started again: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_is_read_from_either_daemons_line() {
        assert_eq!(Connected::parse("1234"), Connected { pid: Some(1234), ..Default::default() });
        let c = Connected::parse("?\tmember BOX (ab12cd34) from 10.0.0.2:5000, alias DMM\t60\t5\tGPIB0::20::INSTR exclusive, ASRL3::INSTR shared");
        assert_eq!((c.pid, c.secs, c.idle), (None, Some(60), Some(5)));
        assert_eq!(c.who(), "member BOX (ab12cd34) from 10.0.0.2:5000, alias DMM");
        assert_eq!(c.locks, ["GPIB0::20::INSTR exclusive", "ASRL3::INSTR shared"]);
        assert!(Connected::parse("77\t\t1\t0\t").locks.is_empty() && Connected::parse("77\t\t1\t0\t").peer.is_none());
    }

    #[test]
    fn only_an_older_daemon_is_handed_over() {
        let (maj, min, n) = build_number(BUILD).unwrap();
        let v = |d: i64| format!("{maj}.{min}.{}", n as i64 + d);
        if n == 0 {
            // Built without git: no build is compared, only the IPC version.
            assert!(!older(VERSION, &v(-1)) && !older(VERSION, ""));
        } else {
            assert!(older(VERSION, &v(-1)), "the same IPC, an older build");
            assert!(!older(VERSION, &v(0)) && !older(VERSION, &v(1)), "the same build or a newer one is used as it is");
            assert!(older(VERSION, ""), "a daemon that says no build predates saying it");
            assert!(!older(VERSION, "garbage"));
        }
        assert!(older(VERSION - 1, &v(5)), "a lower IPC version is older whatever its build says");
        assert!(!older(VERSION + 1, ""), "a newer IPC version is never replaced by this one");
        assert_eq!(build_number("0.1.140"), Some((0, 1, 140)));
        assert_eq!(build_number("0.1"), None);
    }

    #[test]
    fn a_missing_daemon_says_how_to_get_one() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        let m = spawn_error(Path::new("/src/target/release/symbioticd"), &gone);
        assert!(m.contains("cargo build --release -p symbiotic-core --bin symbioticd"), "{m}");
        let m = spawn_error(Path::new("/src/target/debug/symbioticd"), &gone);
        assert!(m.contains("cargo build -p symbiotic-core --bin symbioticd") && !m.contains("--release"), "{m}");
        let m = spawn_error(Path::new("/opt/symbiotic/symbioticd"), &gone);
        assert!(m.contains("reinstall"), "{m}");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(spawn_error(Path::new("/opt/symbiotic/symbioticd"), &denied).starts_with("cannot start"));
    }
}
