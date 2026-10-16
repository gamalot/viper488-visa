//! A USBTMC instrument on a vendor's USBTMC driver (Windows), through the interface IVI-6.2 specifies for every driver
//! bound to `USB\Class_FE&SubClass_03`. It is a pass-through: `WriteFile` and `ReadFile` move bytes on the bulk
//! endpoints unchanged, `DeviceIoControl` sends control requests, waits on the interrupt endpoint and clears halts; the
//! USBTMC protocol stays in `usbtmc.rs`.
//!
//! An ordinary user may open the device; a short packet ends a read; class and standard requests go through
//! `IOCTL_USBTMC_SEND_REQUEST`. The driver does not give a program the device to itself: other processes (a vendor
//! VISA) can have it open at the same time.

use crate::usbtmc::{endpoints_in, Endpoints, Interrupts, Link, LinkError};
use crate::adapter::Error;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};

/// The device interface class every USBTMC driver registers (IVI-6.2 2.6.1; also the setup class).
const USBTMC_GUID: GUID = GUID::from_u128(0xA9FDBB24_128A_11D5_9961_00108335E361);

const IOCTL_USBTMC_WAIT_INTERRUPT: u32 = 0x8000_2008;
const IOCTL_USBTMC_RESET_PIPE: u32 = 0x8000_201C;
const IOCTL_USBTMC_SEND_REQUEST: u32 = 0x8000_2080;
const IOCTL_USBTMC_GET_LAST_ERROR: u32 = 0x8000_2088;

/// USBTMC_PIPE_TYPE.
const PIPE_INTERRUPT_IN: i32 = 1;
const PIPE_READ_DATA: i32 = 2;
const PIPE_WRITE_DATA: i32 = 3;

/// USBD_STATUS values that mean the device stalled the request.
const USBD_STATUS_STALL_PID: u32 = 0xC000_0004;
const USBD_STATUS_ENDPOINT_HALTED: u32 = 0xC000_0030;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const ERROR_OPERATION_ABORTED: u32 = 995;

/// USBTMC_IO_BLOCK (IVI-6.2 section 3). `pbyData` and `fTransferDirectionIn` are there for
/// usbscan's layout; the data goes in the call's output buffer.
#[repr(C)]
#[allow(dead_code)] // its layout only: `io_block` writes the fields where it puts them
struct IoBlock {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    data: *mut u8,
    direction_in: u8,
}

/// The enabled USBTMC device interface of the device with this instance ID, as a path to open. None if it has none --
/// not on a USBTMC driver, or one whose interface is not enabled yet (a driver that just took the device over, until
/// the device is plugged in again).
pub(crate) fn interface_path(instance_id: &OsStr) -> Option<String> {
    let id: Vec<u16> = instance_id.encode_wide().chain(Some(0)).collect();
    let mut len = 0u32;
    // SAFETY: the GUID and the NUL-terminated ID outlive the call; `len` is written.
    if unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &USBTMC_GUID, id.as_ptr(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT) } != CR_SUCCESS || len <= 1 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units.
    if unsafe { CM_Get_Device_Interface_ListW(&USBTMC_GUID, id.as_ptr(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) } != CR_SUCCESS {
        return None;
    }
    // A list of NUL-terminated paths ending in an empty one: the first.
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (end > 0).then(|| String::from_utf16_lossy(&buf[..end]))
}

/// The open device, shared by the link and the interrupt thread (overlapped calls on one handle may
/// run at the same time, each with its own OVERLAPPED).
struct Handle(HANDLE);

// SAFETY: a file handle may be used from any thread; every call on it here is overlapped with its
// own event and completes before the call returns.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: ours, closed once; no call on it is outstanding (each one completes before returning).
        unsafe { CloseHandle(self.0) };
    }
}

fn win_error(e: u32) -> LinkError {
    match e {
        // ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_BAD_COMMAND, ERROR_NO_SUCH_DEVICE,
        // ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED: the device went.
        2 | 6 | 22 | 433 | 1167 | 1617 => LinkError::Gone,
        ERROR_OPERATION_ABORTED => LinkError::Timeout,
        e => LinkError::Other(format!("{} (Windows error {e})", std::io::Error::from_raw_os_error(e as i32))),
    }
}

impl Handle {
    /// One overlapped call, started by `start`, waited for at most `wait`: cancelled and waited out
    /// if it takes longer, so no buffer it was given is in use when this returns. Bytes done.
    fn call(&self, wait: Duration, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32) -> Result<u32, LinkError> {
        // SAFETY: an unnamed manual-reset event, closed below.
        let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if ev.is_null() {
            return Err(LinkError::Other("cannot create an event".into()));
        }
        // SAFETY: OVERLAPPED is plain data; zeroed is its initial state.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev;
        let mut n = 0u32;
        let ok = start(self.0, &mut ov);
        // SAFETY (the calls below): `ov` and every buffer `start` passed outlive the operation, which
        // has completed (or been cancelled and waited out) before this function returns.
        let r = if ok != 0 {
            unsafe { GetOverlappedResult(self.0, &ov, &mut n, 1) };
            Ok(n)
        } else {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                Err(win_error(e))
            } else {
                let ms = wait.as_millis().min(u32::MAX as u128 - 1) as u32;
                if unsafe { GetOverlappedResultEx(self.0, &ov, &mut n, ms, 0) } != 0 {
                    Ok(n)
                } else {
                    let e = unsafe { GetLastError() };
                    if e == WAIT_TIMEOUT {
                        unsafe { CancelIoEx(self.0, &ov) };
                        // Done or cancelled: either way it is over. What it may have moved is dropped.
                        unsafe { GetOverlappedResult(self.0, &ov, &mut n, 1) };
                        Err(LinkError::Timeout)
                    } else {
                        Err(win_error(e))
                    }
                }
            }
        };
        // SAFETY: the event is ours and nothing waits on it any more.
        unsafe { CloseHandle(ev) };
        r
    }

    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8], wait: Duration) -> Result<u32, LinkError> {
        self.call(wait, |h, ov| {
            let mut n = 0u32;
            // SAFETY: both buffers are the caller's and outlive the call (see `call`).
            unsafe {
                DeviceIoControl(
                    h,
                    code,
                    if input.is_empty() { std::ptr::null() } else { input.as_ptr().cast() },
                    input.len() as u32,
                    if output.is_empty() { std::ptr::null_mut() } else { output.as_mut_ptr().cast() },
                    output.len() as u32,
                    &mut n,
                    ov,
                )
            }
        })
    }

    /// A failed control request: a stall if the driver's last USB error says so.
    fn request_error(&self, e: LinkError) -> LinkError {
        let LinkError::Other(_) = e else { return e };
        let mut status = [0u8; 4];
        match self.ioctl(IOCTL_USBTMC_GET_LAST_ERROR, &[], &mut status, Duration::from_millis(500)) {
            Ok(4) if matches!(u32::from_le_bytes(status), USBD_STATUS_STALL_PID | USBD_STATUS_ENDPOINT_HALTED) => LinkError::Stall,
            _ => e,
        }
    }

    fn reset_pipe(&self, pipe: i32) {
        let _ = self.ioctl(IOCTL_USBTMC_RESET_PIPE, &pipe.to_le_bytes(), &mut [], Duration::from_millis(1000));
    }
}

/// An `IoBlock` as the call's input buffer: each field written at its place in the struct's layout, the
/// padding zero (reading the struct itself as bytes would read its padding, which is not initialised).
fn io_block(request_type: u8, request: u8, value: u16, index: u16, length: u16, data: *mut u8) -> Vec<u8> {
    use std::mem::offset_of;
    let mut b = vec![0u8; std::mem::size_of::<IoBlock>()];
    let mut put = |at: usize, bytes: &[u8]| b[at..at + bytes.len()].copy_from_slice(bytes);
    put(offset_of!(IoBlock, request_type), &[request_type]);
    put(offset_of!(IoBlock, request), &[request]);
    put(offset_of!(IoBlock, value), &value.to_ne_bytes());
    put(offset_of!(IoBlock, index), &index.to_ne_bytes());
    put(offset_of!(IoBlock, length), &length.to_ne_bytes());
    put(offset_of!(IoBlock, data), &(data as usize).to_ne_bytes());
    put(offset_of!(IoBlock, direction_in), &[(request_type >> 7) & 1]);
    b
}

pub(crate) struct IviLink {
    h: Arc<Handle>,
    intr: Option<usize>,
}

/// Open the device at `path` (an `interface_path`) and read its USBTMC interface's endpoints from
/// the configuration descriptor (GET_DESCRIPTOR through the driver).
pub(crate) fn open(path: &str, iface: u8) -> Result<(Box<dyn Link>, Endpoints), Error> {
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
    // Shared: the driver does not refuse other programs anyway. SAFETY: the path is NUL-terminated; the handle is
    // closed by `Handle`.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let e = std::io::Error::last_os_error();
        return Err(Error::Io(format!("cannot open it through its USBTMC driver: {e}")));
    }
    let link = IviLink { h: Arc::new(Handle(h)), intr: None };
    let wait = Duration::from_millis(1000);
    let head = link.control_in(0x80, 6, 0x0200, 0, 9, wait).map_err(|e| e.into_error("its configuration descriptor"))?;
    let total = if head.len() >= 4 { u16::from_le_bytes([head[2], head[3]]) } else { 0 };
    let config = link.control_in(0x80, 6, 0x0200, 0, total.max(9), wait).map_err(|e| e.into_error("its configuration descriptor"))?;
    let eps = endpoints_in(&config, iface).ok_or_else(|| Error::Io("its USBTMC interface has no bulk endpoint pair".into()))?;
    Ok((Box::new(IviLink { intr: eps.intr.map(|(_, max)| max), ..link }), eps))
}

impl Link for IviLink {
    fn bulk_out(&mut self, data: &[u8], wait: Duration) -> Result<(), LinkError> {
        let n = self.h.call(wait, |h, ov| {
            // SAFETY: `data` outlives the call (see `Handle::call`).
            unsafe { WriteFile(h, data.as_ptr(), data.len() as u32, std::ptr::null_mut(), ov) }
        })?;
        if (n as usize) < data.len() {
            return Err(LinkError::Other(format!("{n} of {} bytes sent", data.len())));
        }
        Ok(())
    }

    fn bulk_in(&mut self, max: usize, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let mut buf = vec![0u8; max];
        let n = self.h.call(wait, |h, ov| {
            // SAFETY: `buf` outlives the call (see `Handle::call`).
            unsafe { ReadFile(h, buf.as_mut_ptr(), max as u32, std::ptr::null_mut(), ov) }
        })?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn control_in(&self, request_type: u8, request: u8, value: u16, index: u16, length: u16, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let mut buf = vec![0u8; length as usize];
        let block = io_block(request_type, request, value, index, length, buf.as_mut_ptr());
        let n = self.h.ioctl(IOCTL_USBTMC_SEND_REQUEST, &block, &mut buf, wait).map_err(|e| self.h.request_error(e))?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn control_out(&self, request_type: u8, request: u8, value: u16, index: u16, data: &[u8], wait: Duration) -> Result<(), LinkError> {
        // The data goes in the output buffer, as IVI-6.2 2.4.5 lays it out for a write.
        let mut buf = data.to_vec();
        let block = io_block(request_type, request, value, index, data.len() as u16, buf.as_mut_ptr());
        self.h.ioctl(IOCTL_USBTMC_SEND_REQUEST, &block, &mut buf, wait).map_err(|e| self.h.request_error(e))?;
        Ok(())
    }

    fn clear_halt(&mut self, out: bool) {
        self.h.reset_pipe(if out { PIPE_WRITE_DATA } else { PIPE_READ_DATA });
    }

    fn interrupts(&mut self) -> Option<Box<dyn Interrupts>> {
        self.intr.take().map(|max| Box::new(IviInterrupts { h: self.h.clone(), max }) as Box<dyn Interrupts>)
    }
}

/// The interrupt endpoint through the driver, which hands each notification to every process
/// waiting on it (IVI-6.2 2.4.3).
struct IviInterrupts {
    h: Arc<Handle>,
    max: usize,
}

impl Interrupts for IviInterrupts {
    fn wait(&mut self, wait: Duration) -> Result<Vec<u8>, LinkError> {
        let mut buf = vec![0u8; self.max];
        let n = self.h.ioctl(IOCTL_USBTMC_WAIT_INTERRUPT, &[], &mut buf, wait)?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    fn clear_halt(&mut self) {
        self.h.reset_pipe(PIPE_INTERRUPT_IN);
    }
}
