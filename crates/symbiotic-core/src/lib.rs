//! VISA-independent half of Symbiotic: the GPIB adapters (the Viper488 WinUSB protocol v2, the NI
//! GPIB-USB-HS, the Keysight 82357B, the CONTEC GP-IB(USB)FL), USBTMC instruments bound to WinUSB,
//! the resource-name logic, the LAN instrument clients (`lan`), and the daemon that owns every
//! WinUSB device and the IPC to it. Nothing here knows about the VISA C API.

pub mod adapter;
pub mod alias;
pub mod attrs;
pub mod autostart;
pub mod backend;
pub mod boards;
pub mod client;
pub mod conffile;
pub mod diag;
#[cfg(test)]
mod fuzz;
pub mod contec;
pub mod daemonconf;
pub mod features;
pub mod instruments;
pub mod iotrace;
pub mod identity;
pub mod ipc;
pub mod ks;
pub mod lan;
pub mod lanunits;
pub mod neighbor;
pub mod local;
pub mod locks;
pub mod ni;
pub mod owed;
pub mod paths;
pub mod pictures;
pub mod prologix;
pub mod proto;
pub mod renconf;
pub mod resource;
pub mod serial;
pub mod server;
pub mod share;
pub mod speedconf;
pub mod usbtmc;
#[cfg(windows)]
mod usbtmc_ivi;
#[cfg(target_os = "linux")]
mod usbtmc_linux;
