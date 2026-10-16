//! Starting the daemon at login, for a machine that serves a sharing group: a sharing daemon is not started by a
//! program using VISA.
//!
//! Per user, never for the machine: Windows' `HKCU\...\Run`, a systemd user unit on Linux, a LaunchAgent on macOS.
//! Starting twice is harmless: the second daemon finds the first one's channel taken and exits.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    On,
    Off,
    /// This system has no way we know of (Linux without a systemd user manager, say).
    Unsupported(String),
}

/// The daemon to start: beside the running program, else its application bundle's (macOS), else an install's directory.
pub fn daemon_exe() -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "symbioticd.exe" } else { "symbioticd" };
    let own = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    let dir = match &own {
        Some(d) => crate::paths::daemon_dir_for(d),
        None => crate::paths::install_dirs().into_iter().find(|d| d.join(name).is_file()),
    };
    dir.map(|d| d.join(name)).ok_or_else(|| format!("{name} is not beside this program or in an install directory"))
}

/// The systemd user unit's text for a daemon at `exe`. `Restart=on-failure` brings it back if it
/// dies; a daemon asked to stop exits cleanly and stays stopped. Exit status 2 is "another daemon has
/// the channel": the one wanted is running, which is success, and is not tried again.
pub fn unit_text(exe: &str) -> String {
    format!(
        "[Unit]\nDescription=Symbiotic VISA daemon (serves this machine's shared aliases to its group)\n\n\
         [Service]\nExecStart={}\nRestart=on-failure\nSuccessExitStatus=2\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(exe)
    )
}

/// A path as one word of a systemd command line: `%` (a specifier) and `$` (a variable) doubled, and
/// quoted if it has white space, a quote or a backslash.
fn systemd_quote(s: &str) -> String {
    let s = s.replace('%', "%%").replace('$', "$$");
    if s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_string() }
}

/// The LaunchAgent's text for a daemon at `exe`.
pub fn plist_text(exe: &str) -> String {
    let esc = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>com.viper488.symbiotic</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{esc}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n"
    )
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::System::Registry::*;

    const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE: &str = "Symbiotic";

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn status() -> State {
        let (k, v) = (w(KEY), w(VALUE));
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated names; h is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, KEY_READ, &mut h) != 0 {
                return State::Off;
            }
            let r = RegQueryValueExW(h, v.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            RegCloseKey(h);
            if r == 0 { State::On } else { State::Off }
        }
    }

    pub fn set(on: bool) -> Result<String, String> {
        let (k, v) = (w(KEY), w(VALUE));
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: as above; the value's bytes are the UTF-16 string with its NUL.
        unsafe {
            if RegCreateKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, std::ptr::null(), 0, KEY_SET_VALUE, std::ptr::null(), &mut h, std::ptr::null_mut()) != 0 {
                return Err("cannot open the Run key".into());
            }
            let r = if on {
                let exe = daemon_exe()?;
                let cmd = w(&format!("\"{}\"", exe.display()));
                RegSetValueExW(h, v.as_ptr(), 0, REG_SZ, cmd.as_ptr() as *const u8, (cmd.len() * 2) as u32)
            } else {
                match RegDeleteValueW(h, v.as_ptr()) {
                    2 => 0, // not there: already off
                    e => e,
                }
            };
            RegCloseKey(h);
            if r != 0 {
                return Err(format!("the registry refused ({r})"));
            }
        }
        Ok(if on { "the daemon starts when you log in (HKCU Run)".into() } else { "the daemon no longer starts at login".into() })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::process::Command;

    fn unit_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("systemd/user/symbiotic.service"))
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("systemctl").arg("--user").args(args).output().map_err(|e| format!("systemctl: {e}"))
    }

    /// Whether a systemd user manager is there to ask.
    fn manager() -> Result<(), String> {
        match systemctl(&["show", "--property=Version"]) {
            Ok(o) if o.status.success() => Ok(()),
            Ok(_) => Err("no systemd user manager is running (WSL needs systemd=true in /etc/wsl.conf); start symbioticd from your session's startup instead".into()),
            Err(e) => Err(e),
        }
    }

    pub fn status() -> State {
        if let Err(e) = manager() {
            return State::Unsupported(e);
        }
        match systemctl(&["is-enabled", "symbiotic.service"]) {
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "enabled" => State::On,
            _ => State::Off,
        }
    }

    pub fn set(on: bool) -> Result<String, String> {
        manager()?;
        let p = unit_path().ok_or("no HOME")?;
        if on {
            let exe = daemon_exe()?;
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            std::fs::write(&p, unit_text(&exe.to_string_lossy())).map_err(|e| e.to_string())?;
            systemctl(&["daemon-reload"])?;
            // Not `--now`: the daemon may be running already, and starting it is the caller's.
            let o = systemctl(&["enable", "symbiotic.service"])?;
            if !o.status.success() {
                return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
            }
            Ok(format!("the daemon starts when your user session does ({})", p.display()))
        } else {
            let _ = systemctl(&["disable", "symbiotic.service"]);
            let _ = std::fs::remove_file(&p);
            let _ = systemctl(&["daemon-reload"]);
            Ok("the daemon no longer starts with your session".into())
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    fn plist_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/LaunchAgents/com.viper488.symbiotic.plist"))
    }

    pub fn status() -> State {
        if plist_path().is_some_and(|p| p.is_file()) { State::On } else { State::Off }
    }

    pub fn set(on: bool) -> Result<String, String> {
        let p = plist_path().ok_or("no HOME")?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if on {
            // launchd keeps a program that is not an application bundle's main program out of the local network, so
            // only the daemon of Symbiotic.app (its helper bundle) can reach the registry.
            let exe = daemon_exe()?;
            let bundled = exe.parent().is_some_and(|d| d.ends_with(crate::paths::BUNDLE_DAEMON_DIR));
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            std::fs::write(&p, plist_text(&exe.to_string_lossy())).map_err(|e| e.to_string())?;
            // Takes effect at the next login; bootstrapping now would start a second daemon.
            let _ = Command::new("launchctl").args(["bootout", &domain, &p.to_string_lossy()]).output();
            Ok(match bundled {
                true => format!("the daemon starts when you log in ({}). The first time, macOS asks at the screen whether Symbiotic may use the local network: allow it", p.display()),
                false => format!(
                    "the daemon starts when you log in ({}), but started by launchd outside Symbiotic.app it cannot reach the local network; install Symbiotic.app and turn this on again",
                    p.display()
                ),
            })
        } else {
            let _ = Command::new("launchctl").args(["bootout", &domain, &p.to_string_lossy()]).output();
            let _ = std::fs::remove_file(&p);
            Ok("the daemon no longer starts at login".into())
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;
    pub fn status() -> State {
        State::Unsupported("no autostart for this system".into())
    }
    pub fn set(_: bool) -> Result<String, String> {
        Err("no autostart for this system".into())
    }
}

pub fn status() -> State {
    imp::status()
}

/// Turn the start at login on or off. The text says what was done.
pub fn set(on: bool) -> Result<String, String> {
    imp::set(on)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_and_the_agent() {
        let u = unit_text("/home/me/Symbiotic Visa/symbioticd");
        assert!(u.contains("ExecStart=\"/home/me/Symbiotic Visa/symbioticd\"\n"), "{u}");
        assert!(u.contains("SuccessExitStatus=2") && u.contains("WantedBy=default.target"));
        assert!(unit_text("/usr/bin/symbioticd").contains("ExecStart=/usr/bin/symbioticd\n"));
        assert!(unit_text("/opt/100%/$HOME/symbioticd").contains("ExecStart=/opt/100%%/$$HOME/symbioticd\n"), "a specifier or a variable is not expanded");
        let p = plist_text("/Applications/S&T/symbioticd");
        assert!(p.contains("<string>/Applications/S&amp;T/symbioticd</string>") && p.contains("<key>RunAtLoad</key>"));
    }
}
