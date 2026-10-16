//! Runs a console program without a console window, waits for it and exits with its exit code. The installer's
//! commands go through it (windows/symbiotic.wxs): a console program that Windows Installer starts, even one run as
//! SYSTEM, opens a window on the user's desktop for as long as it runs.
//!
//!     symbiotic-quiet "C:\path\program.exe" arguments...
//!
//! Everything after its own name is handed on exactly as written (no re-quoting): the program is started with
//! CREATE_NO_WINDOW, so it has a console nobody sees, and so do the console programs it starts in turn (PowerShell's
//! regasm, for one). Exit code 87 when there is no command, and the error code when it cannot be started. Windows
//! only; elsewhere it says so.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Environment::GetCommandLineW;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NO_WINDOW, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    };

    // SAFETY: GetCommandLineW returns the process's NUL-terminated command line, valid for its life.
    let line: Vec<u16> = unsafe {
        let p = GetCommandLineW();
        let n = (0..).take_while(|&i| *p.add(i) != 0).count();
        std::slice::from_raw_parts(p, n).to_vec()
    };
    let mut cmd: Vec<u16> = after_own_name(&line).to_vec();
    if cmd.is_empty() {
        std::process::exit(87); // ERROR_INVALID_PARAMETER
    }
    cmd.push(0);
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
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            std::process::exit(GetLastError() as i32);
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        std::process::exit(code as i32);
    }
}

/// The command line after the program's own name and the spaces that follow it. The name is read as Windows reads it: up
/// to the closing quote if it starts with one, else up to the first space or tab.
#[cfg_attr(not(windows), allow(dead_code))]
fn after_own_name(line: &[u16]) -> &[u16] {
    const QUOTE: u16 = b'"' as u16;
    let blank = |c: u16| c == b' ' as u16 || c == b'\t' as u16;
    let mut i = 0;
    if line.first() == Some(&QUOTE) {
        i = 1;
        while i < line.len() && line[i] != QUOTE {
            i += 1;
        }
        i += 1; // the closing quote
    } else {
        while i < line.len() && !blank(line[i]) {
            i += 1;
        }
    }
    while i < line.len() && blank(line[i]) {
        i += 1;
    }
    &line[i.min(line.len())..]
}

#[cfg(not(windows))]
fn main() {
    eprintln!("symbiotic-quiet is for Windows (the installer's commands)");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::after_own_name;

    fn rest(s: &str) -> String {
        let w: Vec<u16> = s.encode_utf16().collect();
        String::from_utf16(after_own_name(&w)).unwrap()
    }

    #[test]
    fn the_command_is_handed_on_as_written() {
        assert_eq!(rest(r#""C:\Program Files\Symbiotic\windows\symbiotic-quiet.exe" "C:\Windows\system32\icacls.exe" "C:\ProgramData\Symbiotic\." /grant x"#),
            r#""C:\Windows\system32\icacls.exe" "C:\ProgramData\Symbiotic\." /grant x"#);
        assert_eq!(rest("symbiotic-quiet.exe   taskkill /F /IM symbioticd.exe"), "taskkill /F /IM symbioticd.exe");
        assert_eq!(rest(r#""a b.exe""#), "");
        assert_eq!(rest("quiet"), "");
        assert_eq!(rest(r#""unclosed"#), "");
    }
}
