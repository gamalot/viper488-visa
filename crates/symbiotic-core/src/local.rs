//! Local inter-process channel between a client and the daemon: a named pipe on Windows, a
//! Unix domain socket elsewhere. Not the network: no port, no firewall, and access follows the
//! operating system's permissions (on Windows the pipe refuses remote clients, which named pipes
//! would otherwise accept over SMB).

use std::io;

/// Override the channel's name (a pipe name, or a socket path), e.g. to run a test daemon
/// beside a real one.
const ENV: &str = "SYMBIOTIC_PIPE";

#[cfg(windows)]
mod imp {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::FromRawHandle;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    pub type Stream = File;
    const ERROR_PIPE_BUSY: i32 = 231;

    /// The process at the other end of an accepted connection.
    pub fn peer_pid(s: &Stream) -> Option<u32> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
        let mut pid = 0u32;
        // SAFETY: `s` is an open pipe handle; pid is a u32 to fill.
        (unsafe { GetNamedPipeClientProcessId(s.as_raw_handle() as HANDLE, &mut pid) } != 0).then_some(pid)
    }

    /// Has the client at the other end of an accepted connection gone (closed its end)? Looks without
    /// reading: what the client sent stays for the connection's own read.
    pub fn hung_up(s: &Stream) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let mut avail = 0u32;
        // SAFETY: `s` is an open pipe handle; nothing is read, only the count of what is waiting.
        let ok = unsafe { PeekNamedPipe(s.as_raw_handle() as HANDLE, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) };
        // ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED: the client closed it.
        ok == 0 && matches!(unsafe { GetLastError() }, 109 | 233)
    }

    /// One daemon per user: pipe names are machine-wide, so the user's SID is in the name.
    fn path() -> String {
        std::env::var(ENV).unwrap_or_else(|_| match crate::paths::user_sid() {
            Some(sid) => format!(r"\\.\pipe\symbiotic-{sid}"),
            None => r"\\.\pipe\symbiotic".to_string(),
        })
    }

    pub fn connect() -> io::Result<Stream> {
        let p = path();
        for _ in 0..100 {
            match OpenOptions::new().read(true).write(true).open(&p) {
                // Every instance is momentarily taken: the daemon makes the next one right away.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Ok(f) => return check_server(f),
                r => return r,
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the daemon's pipe stayed busy"))
    }

    /// The name can be guessed, so another user could make the pipe first and pose as our daemon:
    /// whoever serves it must run as this user.
    fn check_server(f: File) -> io::Result<File> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
        let mut pid = 0u32;
        // SAFETY: `f` is an open pipe handle.
        if unsafe { GetNamedPipeServerProcessId(f.as_raw_handle() as HANDLE, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let me = crate::paths::user_sid();
        let them = crate::paths::process_sid(pid);
        if me.is_some() && me == them {
            return Ok(f);
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the daemon's pipe is served by process {pid}, which is not running as this user ({})", them.as_deref().unwrap_or("its user cannot be read")),
        ))
    }

    pub struct Listener {
        name: Vec<u16>,
        next: Mutex<usize>, // an instance already created, waiting for the next client
        /// The pipe's security descriptor (`only_this_user`), for every instance; 0: the default one.
        sd: usize,
    }

    /// A security descriptor that lets only this user (any of its processes, elevated or not) and SYSTEM open the pipe.
    /// None if this user cannot be named.
    fn only_this_user() -> Option<usize> {
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        let sid = crate::paths::user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)").encode_utf16().chain(Some(0)).collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: a NUL-terminated string; `sd` receives a descriptor LocalAlloc'd for us, kept for
        // the listener's life (the process's).
        let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) };
        (ok != 0 && !sd.is_null()).then_some(sd as usize)
    }

    fn create(name: &[u16], first: bool, sd: usize) -> io::Result<usize> {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        let open = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd as *mut _, bInheritHandle: 0 };
        let sa_ptr = if sd == 0 { std::ptr::null() } else { &sa as *const SECURITY_ATTRIBUTES };
        // SAFETY: `name` is NUL-terminated; `sa` (with the descriptor it points to) outlives the call,
        // and null is the default descriptor.
        let h = unsafe { CreateNamedPipeW(name.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, sa_ptr) };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(h as usize)
        }
    }

    impl Listener {
        /// Fails if a daemon already owns the pipe (the first instance is created exclusively).
        pub fn bind() -> io::Result<Listener> {
            let name: Vec<u16> = path().encode_utf16().chain(Some(0)).collect();
            let sd = only_this_user().unwrap_or(0);
            // Another daemon has the name: the first instance of a name is refused as "access denied"
            // (or, all its instances in use, "pipe busy"), which is said as what it is.
            let first = create(&name, true, sd).map_err(|e| match e.raw_os_error() {
                Some(5 | 231) => io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running"),
                _ => e,
            })?;
            Ok(Listener { name, next: Mutex::new(first), sd })
        }

        pub fn accept(&self) -> io::Result<Stream> {
            let h = {
                let mut g = self.next.lock().unwrap();
                let h = *g;
                *g = create(&self.name, false, self.sd)?;
                h
            } as HANDLE;
            // Owned from here, so a failed connect closes the instance instead of leaking it.
            // SAFETY: `h` is a pipe instance this listener created and has not shared.
            let f = unsafe { File::from_raw_handle(h as _) };
            let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
            if ok == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                return Err(io::Error::last_os_error()); // e.g. the client came and went already
            }
            Ok(f)
        }

        pub fn cleanup(&self) {}
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    /// The process at the other end of an accepted connection (Linux: SO_PEERCRED; macOS:
    /// LOCAL_PEERPID; elsewhere the system is not asked, and a caller treats the process as unknown).
    #[cfg(target_os = "linux")]
    pub fn peer_pid(s: &Stream) -> Option<u32> {
        use std::os::unix::io::AsRawFd;
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a connected socket; cred and len describe a ucred to fill.
        let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
        (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
    #[cfg(target_os = "macos")]
    pub fn peer_pid(s: &Stream) -> Option<u32> {
        use std::os::unix::io::AsRawFd;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: a connected socket; pid and len describe a pid_t to fill.
        let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, &mut pid as *mut _ as *mut libc::c_void, &mut len) };
        (r == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn peer_pid(_: &Stream) -> Option<u32> {
        None
    }

    /// Where the socket is: `SYMBIOTIC_PIPE` if set; else in `$XDG_RUNTIME_DIR` (the user's own,
    /// made by the system); else in a directory of the user's own that `private_dir` names and
    /// `Listener::bind` makes sure of -- never a file in a `/tmp` every user shares, where another
    /// user could have made one first and kept the daemon from starting.
    fn path() -> String {
        if let Ok(p) = std::env::var(ENV) {
            return p;
        }
        match std::env::var("XDG_RUNTIME_DIR") {
            Ok(d) if !d.is_empty() => format!("{d}/symbiotic.sock"),
            _ => format!("{}/symbiotic.sock", private_dir().display()),
        }
    }

    /// The directory for the socket where the system gives no runtime directory. macOS: the user's
    /// own temporary directory (`/var/folders/.../T`, mode 0700), asked of the system rather than
    /// read from TMPDIR, so a daemon launchd starts and a program started in a terminal agree.
    /// Elsewhere, and if macOS does not say: `/tmp/symbiotic-<uid>`, which `own_dir` makes 0700.
    fn private_dir() -> std::path::PathBuf {
        #[cfg(target_os = "macos")]
        {
            let mut b = vec![0u8; 1024];
            // SAFETY: b is a buffer of b.len() bytes for the string and its NUL.
            let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, b.as_mut_ptr() as *mut libc::c_char, b.len()) };
            if n > 1 && n <= b.len() {
                b.truncate(n - 1);
                if let Ok(s) = String::from_utf8(b) {
                    return std::path::PathBuf::from(s.trim_end_matches('/'));
                }
            }
        }
        // SAFETY: no arguments, cannot fail.
        std::path::PathBuf::from(format!("/tmp/symbiotic-{}", unsafe { libc::geteuid() }))
    }

    /// Make `dir` (mode 0700) if it is not there, and check it is a directory -- not a link to one
    /// -- of this user's that nobody else may enter. What is not is refused, with where and why: a
    /// directory another user made first could hold a socket of theirs, or take ours away.
    pub(super) fn own_dir(dir: &std::path::Path) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
        }
        let m = std::fs::symlink_metadata(dir)?;
        // SAFETY: no arguments, cannot fail.
        let me = unsafe { libc::geteuid() };
        let why = if !m.file_type().is_dir() {
            Some("is not a directory")
        } else if m.uid() != me {
            Some("belongs to another user")
        } else if m.mode() & 0o077 != 0 {
            Some("is open to other users")
        } else {
            None
        };
        match why {
            None => Ok(()),
            Some(w) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} {w}: the daemon's socket goes only in a directory of this user's alone (set XDG_RUNTIME_DIR, or remove it)", dir.display()),
            )),
        }
    }

    /// Has the client at the other end of an accepted connection gone (closed its end)? Looks without
    /// reading, and without blocking: what the client sent stays for the connection's own read.
    pub fn hung_up(s: &Stream) -> bool {
        use std::os::unix::io::AsRawFd;
        let mut b = 0u8;
        // SAFETY: a connected socket; at most one byte is peeked into `b`, and nothing waits.
        let n = unsafe { libc::recv(s.as_raw_fd(), &mut b as *mut u8 as *mut libc::c_void, 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        match n {
            0 => true,
            n if n > 0 => false,
            _ => !matches!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted),
        }
    }

    /// The user the process at the other end of a connected socket runs as, if the system says.
    fn peer_uid(s: &Stream) -> Option<u32> {
        use std::os::unix::io::AsRawFd;
        #[cfg(target_os = "linux")]
        {
            let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: a connected socket; cred and len describe a ucred to fill.
            let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
            (r == 0).then_some(cred.uid)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let (mut uid, mut gid) = (0, 0);
            // SAFETY: a connected socket; uid and gid are filled.
            let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
            (r == 0).then_some(uid)
        }
    }

    /// Connects only to a daemon of this user (the Windows client checks the same by SID).
    pub fn connect() -> io::Result<Stream> {
        let s = UnixStream::connect(path())?;
        // SAFETY: no arguments, cannot fail.
        if peer_uid(&s) != Some(unsafe { libc::geteuid() }) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the daemon on the channel runs as another user"));
        }
        Ok(s)
    }

    pub struct Listener(UnixListener, String);

    impl Listener {
        pub fn bind() -> io::Result<Listener> {
            let p = path();
            // The directory `private_dir` names is ours to make and to check; one the user set
            // (SYMBIOTIC_PIPE) or the system made (XDG_RUNTIME_DIR) is taken as it is.
            if std::env::var_os(ENV).is_none() && std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|d| d.is_empty()) {
                own_dir(&private_dir())?;
            }
            if UnixStream::connect(&p).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running"));
            }
            let _ = std::fs::remove_file(&p); // a stale socket from a daemon that died
            let l = UnixListener::bind(&p)?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
            Ok(Listener(l, p))
        }

        pub fn accept(&self) -> io::Result<Stream> {
            self.0.accept().map(|(s, _)| s)
        }

        pub fn cleanup(&self) {
            let _ = std::fs::remove_file(&self.1);
        }
    }
}

pub use imp::{connect, hung_up, peer_pid, Listener, Stream};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    #[test]
    fn the_socket_goes_only_in_a_directory_of_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("symbiotic-owndir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let fresh = base.join("fresh");
        super::imp::own_dir(&fresh).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700, "made 0700");
        super::imp::own_dir(&fresh).unwrap(); // and found again
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = super::imp::own_dir(&open).unwrap_err();
        assert!(e.to_string().contains("is open to other users"), "{e}");
        let link = base.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(super::imp::own_dir(&link).unwrap_err().to_string().contains("is not a directory"), "a link to one is not taken");
        let file = base.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(super::imp::own_dir(&file).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_process_at_the_other_end_is_known() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(super::peer_pid(&a), Some(std::process::id()));
    }

    #[test]
    fn a_client_that_went_is_seen_without_reading_what_it_sent() {
        use std::io::{Read, Write};
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!super::hung_up(&a), "there, and quiet");
        b.write_all(b"x").unwrap();
        assert!(!super::hung_up(&a), "there, and has sent something");
        let mut x = [0u8; 1];
        a.read_exact(&mut x).unwrap();
        assert_eq!(&x, b"x", "the byte is still there to read");
        drop(b);
        assert!(super::hung_up(&a), "gone");
    }
}
