//! Gives USB devices that have no driver, and are ones Symbiotic uses, Windows' own WinUSB driver: the in-box,
//! Microsoft-signed winusb.inf's "WinUsb Device", chosen for the one device the way Device Manager's "Let me pick" does.
//! No INF of ours and no certificate, so Windows on Arm, which refuses driver packages not signed by Microsoft, allows it
//! as x64 Windows does.
//!
//!     symbiotic-bind                  run as the service SymbioticBind (LocalSystem; the MSI installs it)
//!     symbiotic-bind scan             one pass now, in this console, saying what it did
//!     symbiotic-bind bind VVVV:PPPP   this VID/PID onto WinUSB from whatever driver it has, now and whenever it comes back
//!     symbiotic-bind unbind VVVV:PPPP forget that, and give the device back to Windows to choose its driver again
//!     symbiotic-bind list             the VID/PIDs bound by hand
//!     symbiotic-bind release          stop the service and give every device it bound back to Windows (the MSI's uninstall)
//!
//! What the service binds, every 2 seconds: a device node with no driver, not running, that is a USBTMC interface
//! (compatible ID USB\Class_FE&SubClass_03&Prot_00 or _01) or one of the GPIB adapters (NI, Keysight, CONTEC; a Viper488
//! asks for WinUSB itself); and a device whose VID/PID was bound by hand, whatever driver it has. A device with a driver
//! is otherwise left where it is: a vendor's driver goes first. The VID/PIDs bound by hand are in
//! HKLM\SOFTWARE\Viper488\Symbiotic\WinUsb, which only an administrator can change: not in the shared configuration
//! folder, which every user can write and which would let any of them take any device (a keyboard) off its driver.
//! Each device node it binds is remembered in HKLM\SOFTWARE\Viper488\Symbiotic\WinUsb\Bound, so that `release` can give
//! back those and no others: Windows' own winusb.inf has nothing to take away on uninstall, as a package of ours had, and
//! an instrument left on WinUSB would stay out of reach of a vendor's VISA installed afterwards.
//! Log: %SystemRoot%\Logs\Symbiotic\bind.log. Windows only; elsewhere it says so.

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => win::service(),
        ["scan"] => {
            win::pass(&mut win::Failed::default(), true);
            0
        }
        ["bind", id] => win::by_hand(id, true),
        ["unbind", id] => win::by_hand(id, false),
        ["list"] => {
            for id in win::chosen() {
                println!("{id}");
            }
            0
        }
        ["release"] => win::release(),
        _ => {
            eprintln!("usage: symbiotic-bind [scan | bind VVVV:PPPP | unbind VVVV:PPPP | list | release] (no argument: the service)");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("symbiotic-bind is for Windows: elsewhere a device needs no driver bound to it (linux/70-symbiotic.rules gives access)");
    std::process::exit(1);
}

#[cfg(windows)]
mod win {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::time::Duration;
    use windows_sys::core::{GUID, PWSTR};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_SUCCESS, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    };
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
        SERVICE_QUERY_STATUS, SERVICE_STOP,
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
        SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS,
        SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };

    const SERVICE: &str = "SymbioticBind";
    const KEY: &str = r"SOFTWARE\Viper488\Symbiotic\WinUsb";
    /// The class of winusb.inf, USBDevice: the class driver list is built for it.
    const USB_DEVICE_CLASS: GUID = GUID::from_u128(0x88bae032_5a81_49f0_bc3d_a4ff138216d6);
    const USB_DEVICE_CLASS_TEXT: &str = "{88BAE032-5A81-49F0-BC3D-A4FF138216D6}";
    /// The GPIB adapters symbioticd drives over WinUSB, as in linux/70-symbiotic.rules (a Viper488 asks for WinUSB itself).
    const ADAPTERS: &[&str] = &[r"USB\VID_3923&PID_709B", r"USB\VID_0957&PID_0518", r"USB\VID_0957&PID_0718", r"USB\VID_06CE&PID_8314"];
    /// USBTMC and USB488 interfaces (IVI-6.2's level that a VISA library may take), whatever the device is.
    const USBTMC: &[&str] = &[r"USB\Class_FE&SubClass_03&Prot_00", r"USB\Class_FE&SubClass_03&Prot_01"];

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(b: &[u16]) -> String {
        String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
    }

    /// One line to bind.log; and to the console when `say`.
    fn log(say: bool, msg: &str) {
        use std::io::Write;
        if say {
            println!("{msg}");
        }
        let dir = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join(r"Logs\Symbiotic");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("bind.log");
        if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
            let _ = std::fs::rename(&p, p.with_extension("log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let _ = writeln!(f, "{t} pid {} {msg}", std::process::id());
        }
    }

    // ---- The VID/PIDs bound by hand: HKLM\SOFTWARE\Viper488\Symbiotic\WinUsb, a DWORD 1 named VVVV:PPPP each; the device
    // nodes the service bound: its Bound subkey, a DWORD 1 named by each one's instance ID.

    fn key(write: bool) -> Option<HKEY> {
        key_at(KEY, write)
    }

    fn key_at(sub: &str, write: bool) -> Option<HKEY> {
        let mut k: HKEY = std::ptr::null_mut();
        let path = wide(sub);
        // SAFETY: valid NUL-terminated path and out pointer.
        let r = unsafe {
            if write {
                RegCreateKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut k, std::ptr::null_mut())
            } else {
                RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut k)
            }
        };
        (r == ERROR_SUCCESS).then_some(k)
    }

    pub fn chosen() -> Vec<String> {
        names(KEY).iter().filter_map(|n| parse_id(n)).collect()
    }

    /// The value names of a key under HKLM.
    fn names(sub: &str) -> Vec<String> {
        let Some(k) = key_at(sub, false) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = [0u16; 1024];
            let mut n = name.len() as u32;
            // SAFETY: `name` holds `n` u16s; no type or data asked for.
            let r = unsafe { RegEnumValueW(k, i, name.as_mut_ptr(), &mut n, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(from_wide(&name));
        }
        // SAFETY: opened above.
        unsafe { RegCloseKey(k) };
        out
    }

    /// `VVVV:PPPP` in upper case, or None.
    fn parse_id(s: &str) -> Option<String> {
        let (v, p) = s.split_once(':')?;
        let hex4 = |x: &str| x.len() == 4 && x.chars().all(|c| c.is_ascii_hexdigit());
        (hex4(v) && hex4(p)).then(|| format!("{}:{}", v.to_ascii_uppercase(), p.to_ascii_uppercase()))
    }

    fn hwid(id: &str) -> String {
        let (v, p) = id.split_once(':').unwrap_or((id, ""));
        format!(r"USB\VID_{v}&PID_{p}")
    }

    // ---- Device nodes.

    struct Set(HDEVINFO);
    impl Drop for Set {
        fn drop(&mut self) {
            // SAFETY: a set this module created.
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    fn info() -> SP_DEVINFO_DATA {
        // SAFETY: plain-old-data.
        let mut d: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        d
    }

    /// A string or multi-string registry property of the device, as its strings.
    fn prop(set: &Set, d: &SP_DEVINFO_DATA, which: u32) -> Vec<String> {
        let mut buf = vec![0u16; 2048];
        let mut need = 0u32;
        // SAFETY: `buf` holds its length in bytes.
        let ok = unsafe { SetupDiGetDeviceRegistryPropertyW(set.0, d, which, std::ptr::null_mut(), buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut need) };
        if ok == 0 {
            return Vec::new();
        }
        buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }

    struct Node {
        id: String,
        hardware: Vec<String>,
        compatible: Vec<String>,
        has_driver: bool,
        started: bool,
        service: String,
    }

    fn nodes() -> Vec<Node> {
        let usb = wide("USB");
        // SAFETY: every present device node the USB enumerator has, of any class.
        let set = Set(unsafe { SetupDiGetClassDevsW(std::ptr::null(), usb.as_ptr(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_ALLCLASSES) });
        if set.0 == INVALID_HANDLE_VALUE as HDEVINFO {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0.. {
            let mut d = info();
            // SAFETY: `d` is sized.
            if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut d) } == 0 {
                break;
            }
            let mut id = [0u16; 512];
            // SAFETY: `id` holds its length.
            if unsafe { SetupDiGetDeviceInstanceIdW(set.0, &d, id.as_mut_ptr(), id.len() as u32, std::ptr::null_mut()) } == 0 {
                continue;
            }
            let (mut status, mut problem) = (0u32, 0u32);
            // SAFETY: out pointers; the node is present.
            let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, d.DevInst, 0) };
            out.push(Node {
                id: from_wide(&id),
                hardware: prop(&set, &d, SPDRP_HARDWAREID),
                compatible: prop(&set, &d, SPDRP_COMPATIBLEIDS),
                has_driver: !prop(&set, &d, SPDRP_DRIVER).is_empty(),
                started: cr == CR_SUCCESS && status & DN_STARTED != 0,
                service: prop(&set, &d, SPDRP_SERVICE).into_iter().next().unwrap_or_default(),
            });
        }
        out
    }

    fn is_winusb(n: &Node) -> bool {
        n.service.eq_ignore_ascii_case("WINUSB")
    }

    /// The node itself, not one of a composite device's interfaces (`...&MI_nn`), is this VID/PID.
    fn is_device_of(n: &Node, hw: &str) -> bool {
        n.id.len() > hw.len() && n.id[..hw.len()].eq_ignore_ascii_case(hw) && n.id.as_bytes()[hw.len()] == b'\\'
    }

    fn is_composite(n: &Node) -> bool {
        n.compatible.iter().any(|c| c.eq_ignore_ascii_case(r"USB\COMPOSITE"))
    }

    /// Why the service binds this node unasked, if it does: no driver and not running. Whether Windows gave up on it
    /// (problem 28), never tried (1), or was given no driver on purpose (`unbind` with no vendor driver to go back to: no
    /// problem at all), it has nothing; and `pass` acts only while Windows has no device install of its own pending.
    fn wanted(n: &Node) -> Option<&'static str> {
        if n.has_driver || n.started {
            return None;
        }
        if n.compatible.iter().any(|c| USBTMC.iter().any(|u| c.eq_ignore_ascii_case(u))) {
            return Some("a USBTMC interface with no driver");
        }
        if ADAPTERS.iter().any(|a| is_device_of(n, a)) {
            return Some("a GPIB adapter with no driver");
        }
        None
    }

    /// The in-box winusb.inf's "WinUsb Device" onto this one node.
    fn install(id: &str) -> Result<(), String> {
        let err = |what: &str| format!("{what} failed: Win32 error {}", unsafe { GetLastError() });
        // SAFETY: an empty set of no class; every call below is given sized structs and this set.
        unsafe {
            let set = Set(SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut()));
            if set.0 == INVALID_HANDLE_VALUE as HDEVINFO {
                return Err(err("SetupDiCreateDeviceInfoList"));
            }
            let mut d = info();
            let wid = wide(id);
            if SetupDiOpenDeviceInfoW(set.0, wid.as_ptr(), std::ptr::null_mut(), 0, &mut d) == 0 {
                return Err(err("SetupDiOpenDeviceInfo"));
            }
            // The class driver list holds only INFs of the device's own class: none for a device with no driver yet, so
            // winusb.inf's (USBDevice) is listed; a device on a vendor's driver has that driver's class, and the list would
            // be empty. So the device is put in USBDevice first, as Device Manager does when a driver of another class is
            // picked for it.
            let c = &d.ClassGuid;
            let usb = &USB_DEVICE_CLASS;
            if (c.data1, c.data2, c.data3, c.data4) != (usb.data1, usb.data2, usb.data3, usb.data4) {
                let class: Vec<u8> = wide(USB_DEVICE_CLASS_TEXT).iter().flat_map(|c| c.to_le_bytes()).collect();
                if SetupDiSetDeviceRegistryPropertyW(set.0, &mut d, SPDRP_CLASSGUID, class.as_ptr(), class.len() as u32) == 0 {
                    return Err(err("setting the device's class to USBDevice"));
                }
            }
            d.ClassGuid = USB_DEVICE_CLASS;
            let mut p: SP_DEVINSTALL_PARAMS_W = std::mem::zeroed();
            p.cbSize = std::mem::size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
            if SetupDiGetDeviceInstallParamsW(set.0, &d, &mut p) == 0 {
                return Err(err("SetupDiGetDeviceInstallParams"));
            }
            // This INF alone, and its nodes even where they would be excluded from a list Windows makes itself.
            p.Flags |= DI_ENUMSINGLEINF;
            p.FlagsEx |= DI_FLAGSEX_ALLOWEXCLUDEDDRVS;
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let inf: Vec<u16> = format!(r"{root}\INF\winusb.inf").encode_utf16().collect();
            if inf.len() >= p.DriverPath.len() {
                return Err("the path of winusb.inf is too long".into());
            }
            p.DriverPath[..inf.len()].copy_from_slice(&inf);
            if SetupDiSetDeviceInstallParamsW(set.0, &d, &p) == 0 {
                return Err(err("SetupDiSetDeviceInstallParams"));
            }
            if SetupDiBuildDriverInfoList(set.0, &mut d, SPDIT_CLASSDRIVER) == 0 {
                return Err(err("SetupDiBuildDriverInfoList"));
            }
            let size = std::mem::size_of::<SP_DRVINFO_DETAIL_DATA_W>();
            let at = std::mem::offset_of!(SP_DRVINFO_DETAIL_DATA_W, HardwareID);
            let mut buf = vec![0u64; 2048]; // aligned for the struct
            for i in 0.. {
                let mut drv: SP_DRVINFO_DATA_V2_W = std::mem::zeroed();
                drv.cbSize = std::mem::size_of::<SP_DRVINFO_DATA_V2_W>() as u32;
                if SetupDiEnumDriverInfoW(set.0, &d, SPDIT_CLASSDRIVER, i, &mut drv) == 0 {
                    return Err(format!("winusb.inf has no USB\\MS_COMP_WINUSB node ({i} looked at)"));
                }
                let detail = buf.as_mut_ptr().cast::<SP_DRVINFO_DETAIL_DATA_W>();
                (*detail).cbSize = size as u32;
                if SetupDiGetDriverInfoDetailW(set.0, &d, &drv, detail, (buf.len() * 8) as u32, std::ptr::null_mut()) == 0 {
                    continue;
                }
                let hw = std::slice::from_raw_parts(buf.as_ptr().cast::<u8>().add(at).cast::<u16>(), (buf.len() * 8 - at) / 2);
                if !from_wide(hw).eq_ignore_ascii_case(r"USB\MS_COMP_WINUSB") {
                    continue;
                }
                if SetupDiSetSelectedDriverW(set.0, &mut d, &mut drv) == 0 {
                    return Err(err("SetupDiSetSelectedDriver"));
                }
                let mut reboot = 0;
                if DiInstallDevice(std::ptr::null_mut(), set.0, &d, &drv, 0, &mut reboot) == 0 {
                    return Err(err("DiInstallDevice"));
                }
                return Ok(());
            }
            unreachable!()
        }
    }

    /// Give the node the driver Windows itself would choose for it from the driver store (a vendor's, if there is one), or,
    /// with none, no driver: the node stays where it is (removed and found again, a device passed into a virtual machine
    /// may not come back).
    fn reinstall(id: &str) -> Result<String, String> {
        let err = |what: &str| format!("{what} failed: Win32 error {}", unsafe { GetLastError() });
        // SAFETY: as in `install`.
        unsafe {
            let set = Set(SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut()));
            if set.0 == INVALID_HANDLE_VALUE as HDEVINFO {
                return Err(err("SetupDiCreateDeviceInfoList"));
            }
            let mut d = info();
            let wid = wide(id);
            if SetupDiOpenDeviceInfoW(set.0, wid.as_ptr(), std::ptr::null_mut(), 0, &mut d) == 0 {
                return Err(err("SetupDiOpenDeviceInfo"));
            }
            let mut reboot = 0;
            if SetupDiBuildDriverInfoList(set.0, &mut d, SPDIT_COMPATDRIVER) != 0 && SetupDiSelectBestCompatDrv(set.0, &mut d) != 0 {
                let mut drv: SP_DRVINFO_DATA_V2_W = std::mem::zeroed();
                drv.cbSize = std::mem::size_of::<SP_DRVINFO_DATA_V2_W>() as u32;
                if SetupDiGetSelectedDriverW(set.0, &d, &mut drv) == 0 {
                    return Err(err("SetupDiGetSelectedDriver"));
                }
                if DiInstallDevice(std::ptr::null_mut(), set.0, &d, &drv, 0, &mut reboot) == 0 {
                    return Err(err("DiInstallDevice"));
                }
                return Ok(format!("now on {} ({})", from_wide(&drv.Description), from_wide(&drv.ProviderName)));
            }
            if DiInstallDevice(std::ptr::null_mut(), set.0, &d, std::ptr::null(), DIIDFLAG_INSTALLNULLDRIVER, &mut reboot) == 0 {
                return Err(err("DiInstallDevice (no driver)"));
            }
            Ok("no driver for it here: it has none now".into())
        }
    }

    /// Nodes a bind failed on, not tried again while they stay plugged in (one line in the log each, not one every 2 s).
    #[derive(Default)]
    pub struct Failed(HashSet<String>);

    /// One look at every USB device node, binding what is wanted.
    pub fn pass(failed: &mut Failed, say: bool) {
        // Not while Windows is installing a device itself (it may be finding this one its driver, from Windows Update).
        // SAFETY: no arguments but a timeout.
        if unsafe { CMP_WaitNoPendingInstallEvents(0) } != windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
            if say {
                println!("Windows is installing a device: try again in a moment");
            }
            return;
        }
        let all = nodes();
        failed.0.retain(|id| all.iter().any(|n| &n.id == id));
        let chosen: Vec<String> = chosen().iter().map(|c| hwid(c)).collect();
        let mut did = false;
        for n in &all {
            let why = if let Some(hw) = chosen.iter().find(|hw| is_device_of(n, hw)) {
                if is_winusb(n) || is_composite(n) {
                    None
                } else {
                    Some(format!("bound by hand ({hw}), on {}", if n.service.is_empty() { "no driver" } else { &n.service }))
                }
            } else {
                wanted(n).map(str::to_string)
            };
            let Some(why) = why else { continue };
            if failed.0.contains(&n.id) {
                continue;
            }
            did = true;
            match install(&n.id) {
                Ok(()) => {
                    remember(&n.id);
                    log(say, &format!("{} ({}): {why}: now on WinUSB", n.id, n.hardware.first().map_or("", |s| s)));
                }
                Err(e) => {
                    log(say, &format!("{} ({}): {why}: not bound: {e}", n.id, n.hardware.first().map_or("", |s| s)));
                    failed.0.insert(n.id.clone());
                }
            }
        }
        if say && !did {
            println!("nothing to bind");
        }
    }

    /// `bind`/`unbind VVVV:PPPP`, run by an administrator.
    pub fn by_hand(arg: &str, bind: bool) -> i32 {
        let Some(id) = parse_id(arg) else {
            eprintln!("{arg}: not a VID:PID (four hex digits each, as F4EC:1103)");
            return 2;
        };
        let hw = hwid(&id);
        let mine: Vec<Node> = nodes().into_iter().filter(|n| is_device_of(n, &hw)).collect();
        if bind && mine.iter().any(is_composite) {
            // Binding the whole of it would take its other functions away too.
            eprintln!("{hw} is a composite device: binding one of its interfaces is not done");
            return 1;
        }
        let Some(k) = key(true) else {
            eprintln!("cannot write HKLM\\{KEY}: run it as an administrator");
            return 1;
        };
        let name = wide(&id);
        // SAFETY: the key is open for writing; the value is a DWORD.
        let r = unsafe {
            if bind {
                let one = 1u32.to_le_bytes();
                RegSetValueExW(k, name.as_ptr(), 0, REG_DWORD, one.as_ptr(), 4)
            } else {
                let r = RegDeleteValueW(k, name.as_ptr());
                if r == windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND { ERROR_SUCCESS } else { r }
            }
        };
        // SAFETY: opened above.
        unsafe { RegCloseKey(k) };
        if r != ERROR_SUCCESS {
            eprintln!("cannot change HKLM\\{KEY} (error {r})");
            return 1;
        }
        if bind {
            log(true, &format!("{id} bound by hand"));
            if mine.is_empty() {
                println!("(not plugged in: it goes onto WinUSB when it is)");
            }
            pass(&mut Failed::default(), true);
        } else {
            log(true, &format!("{id} no longer bound by hand"));
            for n in mine.iter().filter(|n| is_winusb(n)) {
                match reinstall(&n.id) {
                    Ok(what) => log(true, &format!("{}: {what}", n.id)),
                    Err(e) => log(true, &format!("{}: {e}", n.id)),
                }
            }
            for n in nodes().into_iter().filter(|n| is_device_of(n, &hw)) {
                println!("  {}  {}", n.id, if n.service.is_empty() { "no driver" } else { &n.service });
            }
        }
        0
    }

    const BOUND: &str = r"SOFTWARE\Viper488\Symbiotic\WinUsb\Bound";

    /// A device node the service has put on WinUSB, for `release`.
    fn remember(id: &str) {
        if let Some(k) = key_at(BOUND, true) {
            let name = wide(id);
            let one = 1u32.to_le_bytes();
            // SAFETY: the key is open for writing; the value is a DWORD.
            unsafe {
                RegSetValueExW(k, name.as_ptr(), 0, REG_DWORD, one.as_ptr(), 4);
                RegCloseKey(k);
            }
        }
    }

    /// Stop the service SymbioticBind if it runs, so that it binds nothing more (it would take back a device just given
    /// away with no driver); waits up to 10 s for it to stop.
    fn stop_service() {
        // SAFETY: handles opened here and closed here; SERVICE_STATUS is plain-old-data.
        unsafe {
            let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
            if scm.is_null() {
                return;
            }
            let name = wide(SERVICE);
            let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS);
            if !svc.is_null() {
                let mut st: SERVICE_STATUS = std::mem::zeroed();
                ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
                for _ in 0..100 {
                    if QueryServiceStatus(svc, &mut st) == 0 || st.dwCurrentState == SERVICE_STOPPED {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                CloseServiceHandle(svc);
            }
            CloseServiceHandle(scm);
        }
    }

    /// Remove a device node that is not plugged in, so that Windows takes the device for new (and chooses its driver
    /// afresh) when it comes back.
    fn forget_node(id: &str) -> Result<(), String> {
        let err = |what: &str| format!("{what} failed: Win32 error {}", unsafe { GetLastError() });
        // SAFETY: as in `install`.
        unsafe {
            let set = Set(SetupDiCreateDeviceInfoList(std::ptr::null(), std::ptr::null_mut()));
            let mut d = info();
            let wid = wide(id);
            if SetupDiOpenDeviceInfoW(set.0, wid.as_ptr(), std::ptr::null_mut(), 0, &mut d) == 0 {
                return Err(err("SetupDiOpenDeviceInfo"));
            }
            if SetupDiCallClassInstaller(DIF_REMOVE, set.0, &d) == 0 {
                return Err(err("removing the device node"));
            }
        }
        Ok(())
    }

    /// `release`, run by the MSI as SYSTEM when its feature goes: the service stopped, every device node it bound and that
    /// is still on WinUSB given back (one plugged in to the driver Windows chooses for it, a vendor's if there is one, or
    /// none; one not plugged in forgotten, so that it is new to Windows next time), and the key with the VID/PIDs bound by
    /// hand and the nodes bound taken away. Nodes Windows put on WinUSB itself (a Viper488) are not in the list.
    pub fn release() -> i32 {
        stop_service();
        let present = nodes();
        for id in names(BOUND) {
            match present.iter().find(|n| n.id.eq_ignore_ascii_case(&id)) {
                Some(n) if is_winusb(n) => match reinstall(&n.id) {
                    Ok(what) => log(true, &format!("{id}: released: {what}")),
                    Err(e) => log(true, &format!("{id}: not released: {e}")),
                },
                Some(n) => log(true, &format!("{id}: left as it is (on {}, not WinUSB any more)", if n.service.is_empty() { "no driver" } else { &n.service })),
                None => match forget_node(&id) {
                    Ok(()) => log(true, &format!("{id}: not plugged in: its node removed, so it is new to Windows next time")),
                    Err(e) => log(true, &format!("{id}: not plugged in, its node not removed: {e}")),
                },
            }
        }
        let path = wide(KEY);
        // SAFETY: a NUL-terminated path under HKLM.
        let r = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, path.as_ptr()) };
        if r == ERROR_SUCCESS || r == windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            log(true, &format!("released; HKLM\\{KEY} removed"));
            0
        } else {
            log(true, &format!("released; HKLM\\{KEY} not removed (error {r})"));
            1
        }
    }

    // ---- The service.

    static STOP: AtomicBool = AtomicBool::new(false);
    static HANDLE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

    fn status(state: u32) {
        let s = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: if state == SERVICE_STOP_PENDING { 5000 } else { 0 },
        };
        // SAFETY: the handle RegisterServiceCtrlHandlerExW gave.
        unsafe { SetServiceStatus(HANDLE.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE, &s) };
    }

    unsafe extern "system" fn control(code: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
        match code {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                status(SERVICE_STOP_PENDING);
                STOP.store(true, Ordering::SeqCst);
            }
            SERVICE_CONTROL_INTERROGATE => {}
            _ => return 120, // ERROR_CALL_NOT_IMPLEMENTED
        }
        0
    }

    unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
        let name = wide(SERVICE);
        // SAFETY: a static handler function and a NUL-terminated name.
        let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null()) };
        if h.is_null() {
            return;
        }
        HANDLE.store(h, Ordering::SeqCst);
        status(SERVICE_RUNNING);
        log(false, &format!("started, version {}", symbiotic_core::ipc::BUILD));
        let mut failed = Failed::default();
        while !STOP.load(Ordering::SeqCst) {
            pass(&mut failed, false);
            for _ in 0..20 {
                if STOP.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        log(false, "stopped");
        status(SERVICE_STOPPED);
    }

    pub fn service() -> i32 {
        std::panic::set_hook(Box::new(|i| log(false, &format!("PANIC: {i}"))));
        let mut name = wide(SERVICE);
        let table = [
            SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
            SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None },
        ];
        // SAFETY: a NULL-terminated table whose name outlives the call (it returns when the service stops).
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            eprintln!("symbiotic-bind runs as the service {SERVICE} (the installer sets it up); `symbiotic-bind scan` does one pass here");
            return 1;
        }
        0
    }
}
