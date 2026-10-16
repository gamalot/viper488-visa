//! Where things are: the configuration directory, and who the current user is.
//!
//! The configuration (lan.conf, aliases.conf, bridges.conf, speed.conf, the 82357B's firmware) lives in one directory:
//! `SYMBIOTIC_CONFIG_DIR` if set; else, on Windows, the one an all-users install wrote to
//! `HKLM\SOFTWARE\Viper488\Symbiotic\ConfigDir` (shared by every user of the machine); else the user's own,
//! `%LOCALAPPDATA%\Symbiotic` or `~/.config/symbiotic`.

use std::path::{Path, PathBuf};

const ENV: &str = "SYMBIOTIC_CONFIG_DIR";

pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os(ENV) {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        if let Some(d) = machine_config_dir() {
            return Some(d);
        }
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Symbiotic"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("symbiotic"))
}

/// `name` in the configuration directory.
pub fn config_file(name: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(name))
}

/// `name` in the directory of THIS user's own state, for what must not be shared with the other
/// users of the machine although the rest of the configuration is: a private key, and the user's
/// place in a sharing group. `SYMBIOTIC_CONFIG_DIR` still names it (tests); else on Windows the
/// user's `%LOCALAPPDATA%\Symbiotic` even after an all-users install, whose configuration
/// directory every user can write; elsewhere the configuration directory is already the user's.
pub fn user_file(name: &str) -> Option<PathBuf> {
    if cfg!(windows) && std::env::var_os(ENV).is_none() {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Symbiotic").join(name));
    }
    config_file(name)
}

/// The daemon's log, `symbioticd.log`, and `usage.log` beside it, in the user's own directory:
/// `%LOCALAPPDATA%\Symbiotic\logs` on Windows, `$XDG_STATE_HOME/symbiotic` or `~/.local/state/symbiotic` on Linux,
/// `~/Library/Logs/Symbiotic` on macOS.
pub fn log_file() -> PathBuf {
    const NAME: &str = "symbioticd.log";
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()).map(PathBuf::from);
        return local.map_or_else(std::env::temp_dir, |d| d.join("Symbiotic").join("logs")).join(NAME);
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library").join("Logs").join("Symbiotic"))
    } else {
        std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| home.map(|h| h.join(".local").join("state"))).map(|d| d.join("symbiotic"))
    };
    dir.unwrap_or_else(std::env::temp_dir).join(NAME)
}

/// Move the logs an earlier Windows build wrote to the temporary directory to where they are now (`log_file`), without
/// overwriting. Does nothing elsewhere.
pub fn move_earlier_logs() {
    if !cfg!(windows) {
        return;
    }
    let Some(now) = log_file().parent().map(Path::to_path_buf) else { return };
    let then = std::env::temp_dir();
    if then == now {
        return;
    }
    for name in ["symbioticd.log", "symbioticd.log.1", "usage.log", "usage.log.1"] {
        let (old, new) = (then.join(name), now.join(name));
        if !old.is_file() || new.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(&now);
        if std::fs::rename(&old, &new).is_err() && std::fs::copy(&old, &new).is_ok() {
            let _ = std::fs::remove_file(&old);
        }
    }
}

/// Where a file made for the user to take away goes (a diagnostics bundle, an exported trace): the Downloads folder,
/// else the home directory.
pub fn downloads() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()).map(PathBuf::from)?;
    let d = home.join("Downloads");
    Some(if d.is_dir() { d } else { home })
}

#[cfg(windows)]
fn machine_config_dir() -> Option<PathBuf> {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    reg_path(HKEY_LOCAL_MACHINE, "ConfigDir")
}

/// Where the install put the programs (the MSI's `InstallDir`). A copy of the library elsewhere starts the daemon from
/// its own directory.
#[cfg(windows)]
pub fn install_dirs() -> Vec<PathBuf> {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    reg_path(HKEY_LOCAL_MACHINE, "InstallDir").into_iter().collect()
}

#[cfg(not(windows))]
pub fn install_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// A path from `SOFTWARE\Viper488\Symbiotic` under `root`.
#[cfg(windows)]
fn reg_path(root: windows_sys::Win32::System::Registry::HKEY, name: &str) -> Option<PathBuf> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY};
    let key: Vec<u16> = r"SOFTWARE\Viper488\Symbiotic".encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // The 64-bit view whatever this process is, so visa32.dll reads what the installer wrote. SAFETY: the names are
    // NUL-terminated, `buf` holds `len` bytes.
    let r = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    let s = s.trim_end_matches('\0').trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(not(windows))]
fn machine_config_dir() -> Option<PathBuf> {
    None
}

/// The current process's user, as a string SID (`S-1-5-21-...`).
#[cfg(windows)]
pub fn user_sid() -> Option<String> {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the pseudo-handle of this process needs no closing.
    unsafe { process_token_sid(GetCurrentProcess()) }
}

/// The user a process runs as; None if it cannot be asked (another user's process, typically).
#[cfg(windows)]
pub fn process_sid(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: the handle is closed below.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let r = process_token_sid(h);
        CloseHandle(h);
        r
    }
}

#[cfg(windows)]
unsafe fn process_token_sid(process: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut token: HANDLE = std::ptr::null_mut();
    if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
        return None;
    }
    let mut buf = [0u64; 64]; // TOKEN_USER and the SID after it, aligned
    let mut len = 0u32;
    let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &mut len);
    CloseHandle(token);
    if ok == 0 {
        return None;
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut s: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
        return None;
    }
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
    LocalFree(s.cast());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_overrides_the_config_dir() {
        // One test touches the variable, so none races it.
        std::env::set_var(ENV, "/some/where");
        assert_eq!(config_dir(), Some(PathBuf::from("/some/where")));
        assert_eq!(config_file("lan.conf"), Some(PathBuf::from("/some/where/lan.conf")));
        std::env::remove_var(ENV);
        assert_ne!(config_dir(), Some(PathBuf::from("/some/where")));
    }
}
