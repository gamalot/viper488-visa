//! A USBTMC instrument on the Linux kernel's usbtmc driver, through `/dev/usbtmcN` and the pass-through ioctls the IVI
//! Foundation added to that driver (IVI-6.2 Appendix A; the interface is the uapi header `linux/usb/tmc.h`).
//!
//! The driver's own read() and write() put USBTMC headers on what they move; they are not used. USBTMC_IOCTL_WRITE /
//! USBTMC_IOCTL_READ move bulk bytes unchanged and USBTMC_IOCTL_CTRL_REQUEST sends any control request, so the protocol
//! stays in `usbtmc.rs`. The interrupt endpoint is the driver's: the status byte is read with USBTMC488_IOCTL_READ_STB,
//! and a service request waited for with USBTMC488_IOCTL_WAIT_SRQ. The driver lets several programs open one
//! instrument.

use crate::adapter::Error;
use crate::usbtmc::{endpoints_in, Endpoints, Interrupts, Link, LinkError};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// The ioctl numbers, as the kernel's _IOC encodes them (x86, ARM: direction in bits 30-31, size in
// 16-29, type in 8-15, number in 0-7); the type is USBTMC_IOC_NR, 91.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (91 << 8) | nr
}
const NONE: u64 = 0;
const W: u64 = 1;
const R: u64 = 2;

const CLEAR_OUT_HALT: u64 = ioc(NONE, 6, 0);
const CLEAR_IN_HALT: u64 = ioc(NONE, 7, 0);
const CTRL_REQUEST: u64 = ioc(R | W, 8, std::mem::size_of::<CtrlRequest>());
const SET_TIMEOUT: u64 = ioc(W, 10, 4);
const WRITE: u64 = ioc(R | W, 13, std::mem::size_of::<Message>());
const READ: u64 = ioc(R | W, 14, std::mem::size_of::<Message>());
const API_VERSION: u64 = ioc(R, 16, 4);
const READ_STB: u64 = ioc(R, 18, 1);
const WAIT_SRQ: u64 = ioc(W, 23, 4);

/// struct usbtmc_request and struct usbtmc_ctrlrequest.
#[repr(C, packed)]
struct CtrlRequest {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    data: *mut u8,
}

/// struct usbtmc_message: `flags` 0 is synchronous.
#[repr(C, packed)]
struct Message {
    transfer_size: u32,
    transferred: u32,
    flags: u32,
    message: *mut u8,
}

/// The driver refuses a timeout under 100 ms (EINVAL).
const LEAST_TIMEOUT_MS: u32 = 100;

/// The device file the kernel usbtmc driver made for this device's interface `iface`: under the
/// interface's sysfs directory, `usbmisc/usbtmcN`.
pub(crate) fn device_file(dev: &Path, iface: u8) -> Option<String> {
    let name = dev.file_name()?.to_str()?;
    let config = std::fs::read_to_string(dev.join("bConfigurationValue")).ok()?;
    let misc = dev.join(format!("{name}:{}.{iface}", config.trim())).join("usbmisc");
    std::fs::read_dir(misc)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .find(|n| n.starts_with("usbtmc"))
        .map(|n| format!("/dev/{n}"))
}

struct Fd(File);

impl Fd {
    /// One ioctl; its return value, or the error errno named.
    fn ioctl_raw<T>(&self, request: u64, arg: *mut T) -> std::io::Result<i32> {
        // SAFETY: `arg` points at what this request takes (the sizes are in the request number), and
        // outlives the call, which is synchronous.
        let r = unsafe { libc::ioctl(self.0.as_raw_fd(), request as _, arg) };
        if r < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    fn ioctl<T>(&self, request: u64, arg: *mut T) -> Result<i32, LinkError> {
        self.ioctl_raw(request, arg).map_err(errno_error)
    }

    fn set_timeout(&self, wait: Duration) -> Result<(), LinkError> {
        let mut ms = (wait.as_millis().min(u32::MAX as u128) as u32).max(LEAST_TIMEOUT_MS);
        self.ioctl(SET_TIMEOUT, &mut ms).map(|_| ())
    }

    /// The status byte. `wait`: set as the file's timeout first; None leaves the timeout as it is, for
    /// the interrupt thread, which must not change it under a transfer the link is starting (the timeout
    /// is one for the open file, read as each transfer begins).
    fn read_stb(&self, wait: Option<Duration>) -> Result<u8, LinkError> {
        if let Some(w) = wait {
            self.set_timeout(w)?;
        }
        let mut stb = 0u8;
        self.ioctl(READ_STB, &mut stb)?;
        Ok(stb)
    }

    fn control(&self, request_type: u8, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, LinkError> {
        let mut c = CtrlRequest {
            request_type,
            request,
            value,
            index,
            length: buf.len() as u16,
            data: if buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr() },
        };
        self.ioctl(CTRL_REQUEST, &mut c).map(|n| n as usize)
    }
}

fn errno_error(e: std::io::Error) -> LinkError {
    match e.raw_os_error() {
        Some(libc::ETIMEDOUT) | Some(libc::ECANCELED) => LinkError::Timeout,
        Some(libc::ENODEV) | Some(libc::ESHUTDOWN) | Some(libc::ENXIO) => LinkError::Gone,
        Some(libc::EPIPE) => LinkError::Stall,
        _ => LinkError::Other(e.to_string()),
    }
}

pub(crate) struct LinuxLink {
    fd: Arc<Fd>,
    intr: bool,
}

/// Open `/dev/usbtmcN` and read the USBTMC interface's endpoints from the configuration descriptor
/// (GET_DESCRIPTOR through the driver).
pub(crate) fn open(path: &str, iface: u8) -> Result<(Box<dyn Link>, Endpoints), Error> {
    let f = File::options().read(true).write(true).open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            Error::Io(format!("{path}: {e} (no permission on the usbtmc device file: a udev rule must give this user access; linux/install-udev.sh)"))
        } else {
            Error::Io(format!("{path}: {e}"))
        }
    })?;
    let fd = Fd(f);
    // The pass-through ioctls came with the API version (Linux 5.0); a kernel without them is told apart here.
    let mut version = 0u32;
    fd.ioctl(API_VERSION, &mut version)
        .map_err(|e| Error::Io(format!("{path}: this kernel's usbtmc driver has no IVI pass-through ioctls ({e:?})")))?;
    let link = LinuxLink { fd: Arc::new(fd), intr: false };
    let wait = Duration::from_millis(1000);
    let head = link.control_in(0x80, 6, 0x0200, 0, 9, wait).map_err(|e| e.into_error("its configuration descriptor"))?;
    let total = if head.len() >= 4 { u16::from_le_bytes([head[2], head[3]]) } else { 0 };
    let config = link.control_in(0x80, 6, 0x0200, 0, total.max(9), wait).map_err(|e| e.into_error("its configuration descriptor"))?;
    let eps = endpoints_in(&config, iface).ok_or_else(|| Error::Io("its USBTMC interface has no bulk endpoint pair".into()))?;
    crate::server::log(&format!("USBTMC {path}: kernel usbtmc driver API version {version}"));
    Ok((Box::new(LinuxLink { intr: eps.intr.is_some(), ..link }), eps))
}

impl Link for LinuxLink {
    fn bulk_out(&mut self, data: &[u8], wait: Duration) -> Result<(), LinkError> {
        self.fd.set_timeout(wait)?;
        let mut m = Message { transfer_size: data.len() as u32, transferred: 0, flags: 0, message: data.as_ptr().cast_mut() };
        self.fd.ioctl(WRITE, &mut m)?;
        let sent = m.transferred;
        if (sent as usize) < data.len() {
            return Err(LinkError::Other(format!("{sent} of {} bytes sent", data.len())));
        }
        Ok(())
    }

    /// The driver takes more than 4 KiB only in whole 4 KiB (it keeps nothing it has not been asked
    /// for), so a longer read may come back shorter; the caller reads on.
    fn bulk_in(&mut self, max: usize, wait: Duration) -> Result<Vec<u8>, LinkError> {
        self.fd.set_timeout(wait)?;
        let mut buf = vec![0u8; max];
        let mut m = Message { transfer_size: max as u32, transferred: 0, flags: 0, message: buf.as_mut_ptr() };
        self.fd.ioctl(READ, &mut m)?;
        let got = m.transferred as usize;
        buf.truncate(got.min(max));
        Ok(buf)
    }

    fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, length: u16, _wait: Duration) -> Result<Vec<u8>, LinkError> {
        // The driver's own control timeout applies (USB_CTRL_GET_TIMEOUT, 5 s); there is no setting it.
        let mut buf = vec![0u8; length as usize];
        let n = self.fd.control(request_type, request, value, index, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], _wait: Duration) -> Result<(), LinkError> {
        let mut buf = data.to_vec();
        self.fd.control(request_type, request, value, index, &mut buf).map(|_| ())
    }

    fn clear_halt(&mut self, out: bool) {
        let _ = self.fd.ioctl::<u8>(if out { CLEAR_OUT_HALT } else { CLEAR_IN_HALT }, std::ptr::null_mut());
    }

    fn interrupts(&mut self) -> Option<Box<dyn Interrupts>> {
        std::mem::take(&mut self.intr).then(|| Box::new(LinuxInterrupts { fd: self.fd.clone() }) as Box<dyn Interrupts>)
    }

    fn read_stb(&mut self, wait: Duration) -> Option<Result<u8, LinkError>> {
        Some(self.fd.read_stb(Some(wait)))
    }
}

/// Service requests, as the driver reports them: WAIT_SRQ returns when one came, and READ_STB then
/// gives the status byte it came with (the driver keeps it for each open file). Handed on as the
/// interrupt endpoint's own notification would be, 0x81 and the byte.
struct LinuxInterrupts {
    fd: Arc<Fd>,
}

impl Interrupts for LinuxInterrupts {
    fn wait(&mut self, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let mut ms = wait.as_millis().min(u32::MAX as u128) as u32;
        match self.fd.ioctl_raw(WAIT_SRQ, &mut ms) {
            // The byte the request came with, kept by the driver: answered at once, so no timeout of its own.
            Ok(_) => Ok(vec![0x81, self.fd.read_stb(None)?]),
            // EFAULT: the driver found no interrupt endpoint after all; nothing will come.
            Err(e) if e.raw_os_error() == Some(libc::EFAULT) => Err(LinkError::Gone),
            Err(e) => Err(errno_error(e)),
        }
    }

    fn clear_halt(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_numbers_are_the_kernels() {
        // As `_IO`, `_IOR`, `_IOW` and `_IOWR` work them out in linux/usb/tmc.h, on x86-64.
        assert_eq!(std::mem::size_of::<CtrlRequest>(), 16);
        assert_eq!(std::mem::size_of::<Message>(), 20);
        assert_eq!(CLEAR_OUT_HALT, 0x5B06);
        assert_eq!(CLEAR_IN_HALT, 0x5B07);
        assert_eq!(CTRL_REQUEST, 0xC010_5B08);
        assert_eq!(SET_TIMEOUT, 0x4004_5B0A);
        assert_eq!(WRITE, 0xC014_5B0D);
        assert_eq!(READ, 0xC014_5B0E);
        assert_eq!(API_VERSION, 0x8004_5B10);
        assert_eq!(READ_STB, 0x8001_5B12);
        assert_eq!(WAIT_SRQ, 0x4004_5B17);
    }
}
