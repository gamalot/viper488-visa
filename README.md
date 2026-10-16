# Viper488 Symbiotic VISA Runtime

> **Preview release.** Interfaces and file formats may still change.

A VISA library for GPIB instruments behind USB GPIB adapters (Viper488, NI GPIB-USB-HS, Keysight 82357B,
CONTEC GP-IB(USB)FL, and Prologix/AR488 controllers on a serial port), for USBTMC instruments, LAN (LXI)
instruments and serial instruments. It exports the VISA C API (`viOpenDefaultRM`, `viOpen`, `viWrite`,
`viRead`, ...) from `visa64.dll` (and a 32-bit `visa32.dll`; `libvisa64.so` on Linux), so pyvisa,
LabVIEW-style applications, C programs and anything else that loads a VISA library can use them:

    GPIB0::20::INSTR                        an instrument on the first adapter (one adapter = one board)
    GPIB0::9::2::INSTR                      secondary address 2 of primary address 9
    USB0::0x0957::0x1792::MY12345678::INSTR a USBTMC instrument
    ASRL3::INSTR                            a serial instrument on COM3 (`COM3` works as an alias)
    TCPIP0::192.168.1.20::inst0::INSTR      VXI-11 (the device name may be left out)
    TCPIP0::192.168.1.20::hislip0::INSTR    HiSLIP
    TCPIP0::192.168.1.20::5025::SOCKET      a raw socket
    DMM                                     an alias you defined (`symbiotic-ctl alias add DMM GPIB1::6::INSTR`)

It also provides VISA.NET and VISA COM implementations on top of the same C API, a configuration window
(`symbiotic-config`) and a command-line tool (`symbiotic-ctl`).

Platforms: Windows (MSI), Linux (`.deb`, amd64) and macOS (`.dmg`, Apple Silicon and Intel).

## Quick start

1. Install it: run the MSI on Windows, or install the `.deb` on Linux (see [Installing](#installing)).
2. For an NI, Keysight or CONTEC adapter on Windows, bind it to WinUSB first (see [GPIB adapters](#gpib-adapters-gpibnaddrinstr-gpibnintfc)).
   A Viper488 needs nothing.
3. From Python:

       import pyvisa
       rm = pyvisa.ResourceManager(r"C:\Program Files\Symbiotic\visa64.dll")   # or no path, once registered with the IVI router
       print(rm.visalib, rm.list_resources())
       print(rm.open_resource("GPIB0::20::INSTR").query("*IDN?"))

Any number of processes share an adapter: it is owned by a small daemon, `symbioticd`, which the library
starts on demand from its own directory and which exits when no client has been connected for 10 seconds,
so the adapter is free for other tools. Library and daemon talk over a local channel -- a named pipe on
Windows (remote clients refused), a Unix socket elsewhere -- never the network. `symbioticd` must sit next
to the library. It logs to `symbioticd.log`: `%LOCALAPPDATA%\Symbiotic\logs` on Windows,
`~/.local/state/symbiotic/` on Linux, `~/Library/Logs/Symbiotic/` on macOS. `SYMBIOTIC_TRACE=1` prints the
adapter-level error behind a VISA error status.

## Supported resources

### GPIB adapters (`GPIBn::addr::INSTR`, `GPIBn::INTFC`)

Four kinds of USB adapter: **Viper488** (WinUSB protocol v2), the **NI GPIB-USB-HS**, the **Keysight
82357B** and the **CONTEC GP-IB(USB)FL**. Boards are numbered Viper488 first, then NI, then Keysight, then
CONTEC, each by serial number, then the Prologix/AR488 bridges; after that a number stays with its adapter
while the daemon runs (unplugged, it keeps its number and comes back to it).

- **NI, Keysight and CONTEC** adapters must be bound to WinUSB on Windows first, which takes them away from
  their vendor's own software: from an elevated PowerShell, `windows\winusb-bind.ps1 -VidPid <VID:PID>`
  (`winusb-unbind.ps1` gives the device back).
- **The 82357B** boots blank and needs Keysight's firmware image, which this project may not distribute:
  put `measat_releaseX1.8.hex` in the `firmware` folder of the configuration directory (see
  `firmware/README.md`). The daemon loads it the first time the adapter is used after it is plugged in,
  which takes about 12 s.
- **The CONTEC** is slow by design of its firmware (about 80-100 KiB/s in bulk), and a message longer than
  2026 bytes left half-read while another instrument is used reports an error on its next read.

Service requests (SRQ) work by queue (`viWaitOnEvent`) or handler on all four adapters; the daemon watches
for them and hands the status byte back to `viReadSTB`. `GPIBn::INTFC` sessions work: `viGpibCommand` sends
interface messages and `viWrite`/`viRead` then move data addressing nobody (a Viper488 needs firmware with
the interface level). Secondary addresses (`GPIBn::pad::sad::INSTR`, sad 0..30) are built on the same
interface level; `viOpen` does not probe a secondary address and `viFindRsrc` lists primary addresses only.

### Prologix GPIB-USB and AR488 controllers (`GPIBn::addr::INSTR`)

A Prologix or AR488 controller on a serial port is a GPIB board too, once its port is configured
(`symbiotic-ctl bridge add COM5 [prologix|ar488]`): its instruments are `GPIBn::<address>::INSTR`, numbered
after the USB adapters, and its port is no longer an `ASRL` resource. Nothing is sent to a port that is not
configured. The dialect is detected with `++ver real`, and an AR488 is never sent `++savecfg` (it would
write its EEPROM).

### USBTMC instruments (`USB0::vid::pid::serial::INSTR`)

Through whatever driver the instrument has:

- **No driver** (a clean Windows has no USBTMC driver): bound to WinUSB by the installer's
  `UsbtmcWinUsb` feature (see [Installing](#installing)), and then owned by the daemon.
- **A vendor's USBTMC driver** (the one Keysight, NI and other VISAs install): left there and used through the
  interface IVI-6.2 specifies for such drivers. That driver lets two programs have an instrument open at
  once, so do not use one USBTMC instrument from Symbiotic and another VISA at the same time. To give an
  instrument to Symbiotic alone, bind it with `windows\winusb-bind.ps1 -VidPid <VID:PID>` (elevated).
- **Linux**: an instrument the kernel's `usbtmc` driver has is used through its `/dev/usbtmcN` (Linux 5.0 or
  later), with the same caution; one with no kernel driver is used directly. The udev rules give access to both.

Read, write, clear, trigger, status byte, REN/GTL/LLO (as the instrument supports them), locks, service
requests over the interrupt endpoint and `viUsbControlIn`/`viUsbControlOut`. `symbiotic-ctl usb` lists every
USBTMC device and says why one is not used.

### LAN instruments (`TCPIP`: VXI-11, HiSLIP, SOCKET)

Connected by the library itself, from the calling process: no daemon, and outbound connections only --
except that enabling SRQ events on a VXI-11 session opens a port for the instrument to call back on
(VXI-11's interrupt channel; the firewall may ask about the host program the first time). HiSLIP delivers
SRQ on the connection it already has. `host,port` names a VXI-11 core port directly instead of asking the
portmapper on port 111.

Read, write, clear, status byte, trigger, remote/local and the instrument's own locks work; service requests
work on VXI-11 and HiSLIP sessions. A SOCKET session ends a read only at the termination character, the count
or the timeout, so set `read_termination`; with `VI_ATTR_IO_PROT` set to `VI_PROT_4882_STRS` its
`viReadSTB` sends `*STB?`.

An instrument behind a LAN/GPIB gateway (such as an Agilent E5810A) is
`TCPIP0::<gateway>::gpib0,<address>::INSTR`: it gets its GPIB address as attributes, and REN, local lockout
and IFC through the gateway's interface link (VXI-11.2 `device_docmd`).

`viFindRsrc` lists the LAN instruments configured in `lan.conf` (`symbiotic-ctl lan add NAME`) that answer a
TCP connection, and -- only with `symbiotic-ctl lan discover on` -- the ones found by a VXI-11 portmapper
broadcast and mDNS (`_vxi-11`, `_hislip`, `_scpi-raw`); `symbiotic-ctl lan scan` finds them on demand.

### Serial instruments (`ASRLn::INSTR`)

On Windows `n` is the COM number. On Linux a USB serial port is named by its device,
`ASRL/dev/ttyACM0::INSTR` or `ASRL/dev/ttyUSB0::INSTR`, and `ASRL1::INSTR` is `/dev/ttyS0`;
`/dev/serial/by-id/...` links open the port they point to. On Linux the user must be in the group that owns
the tty (`dialout` on most distributions). On macOS a port is given a number the first time it is seen
(kept in `serialports.conf`).

Ports are opened by the daemon for the sessions that use them, so several processes share one serial
instrument, and closed when the last of those sessions closes. Listing opens no port. The port's settings
(baud rate, framing, flow control, DTR/RTS) are the port's and every session sees the same; how a read or
write ends (`VI_ATTR_ASRL_END_IN`/`END_OUT`) is each session's. With `VI_ATTR_IO_PROT` set to
`VI_PROT_4882_STRS`, `viReadSTB`, `viAssertTrigger` and `viClear` send `*STB?`, `*TRG` and `*CLS`;
otherwise `viClear` sends a break.

## The VISA API

- **Every VPP-4.3 function** is declared (`include/visa.h`) and exported. What this runtime cannot do --
  VXI and PXI register access, shared memory, trigger mapping, `viGpibControlATN`, `viGpibPassControl` --
  answers `VI_ERROR_NSUP_OPER`.
- **Attributes**: every attribute VPP-4.3 section 5 gives the resource classes above.
- **Formatted I/O** (`viPrintf`, `viScanf`, `viQueryf`, `viSPrintf`, `viSScanf` and their `viV...` forms) as
  VPP-4.3 has it: the C conversions and VISA's own -- `%b`/`%B` IEEE 488.2 blocks, `%y` raw binary,
  `%t`/`%T`, `,` arrays, `@1`..`@B` number formats, `#` sizes -- through per-session formatted I/O buffers.
  `viPrintf` output is sent when its format sends `"\n"` (LF with END), when the buffer fills, or on
  `viFlush(VI_WRITE_BUF)`, not at every call; what a `viScanf` does not parse is left for the next one.
  `viBufRead`/`viBufWrite`, `viSetBuf` and `viFlush` work on the same buffers.
- **Events**: service requests, I/O completion and exceptions, by queue or handler (exceptions by handler
  only). Handlers run last-installed first and `VI_SUCCESS_NCHAIN` stops the rest.
- **Asynchronous I/O**: `viReadAsync`/`viWriteAsync` complete before they return (`VI_SUCCESS_SYNC`) and
  post the I/O completion event. `viReadToFile`/`viWriteFromFile` work.
- **Locks** (`viLock`/`viUnlock`, exclusive and shared) work across processes.
- **Aliases** (`symbiotic-ctl alias add DMM GPIB1::6::INSTR`, kept in `aliases.conf`) open with `viOpen`,
  and `viParseRsrcEx` (pyvisa's `resource_info`) gives a resource's alias back. An alias cannot contain a
  colon or be a resource name itself, so a real name always means itself.

## C and C++

`include/visa.h` and `include/visatype.h` declare the whole VPP-4.3 API; `visa64.lib` and `visa32.lib`
(installed with them) link against the 64-bit and 32-bit DLLs. The 32-bit `visa32.dll` is the same code
built for x86 and starts the same 64-bit daemon. `tests/c_api.c` is an example.

## .NET (VISA.NET)

`Symbiotic.Visa.dll` implements the IVI VISA.NET interfaces (`Ivi.Visa`, VPP-4.3.6) over the C API: a
resource manager and the sessions for every resource above (`IGpibSession`, `IGpibInterfaceSession`,
`ITcpipSession`, `ITcpipSocketSession`, `IUsbSession`, `ISerialSession`, each also `INativeVisaSession`),
with events, locks, asynchronous I/O and the IVI shared components' formatted I/O. Two builds, one source:
`net462` for .NET Framework and `net6.0` for .NET 6 and later. It needs the IVI VISA.NET shared components
(`Ivi.Visa` 8.0; a .NET 6+ program must reference `IviFoundation.Visa` 8.0.2 or later), which it does not ship.

    var rm = new Symbiotic.Visa.ResourceManager();
    using var dmm = (IMessageBasedSession)rm.Open("GPIB0::6::INSTR");
    dmm.FormattedIO.WriteLine("*IDN?");
    Console.WriteLine(dmm.FormattedIO.ReadLine());

For programs that go through `GlobalResourceManager` it has to be registered (the installer's
`RegisterVisaNet` feature, or `windows\register-visanet.ps1` elevated). The vendor id `vy` (0x7679) is this
project's own, not a VPP-9 assignment.

## VISA COM

`Symbiotic.VisaCom.dll` implements VISA COM (VPP-4.3.4), the layer IVI-COM instrument drivers (and the IVI-C
drivers built around them), Excel VBA and older vendor tools talk through: a vendor-specific resource manager
plus one component per resource kind, over the same C API. The shared VISA COM components (`VISA.GlobalRM`,
`VISA.BasicFormattedIO`) come from the IVI VISA shared components. Register it with the installer's
`RegisterVisaCom` feature or `windows\register-visacom.ps1` (elevated). `SYMBIOTIC_VISACOM_TRACE` logs every
interface asked for and every error to `%TEMP%\symbiotic-visacom.log`.

## The configuration window: symbiotic-config

`symbiotic-config` (a Start menu entry on Windows, a menu entry on Linux) shows the GPIB boards with their
instruments, the USB instruments, the LAN instruments and the serial ports, each instrument by the model its
`*IDN?` answer names (an instrument that must not be sent `*IDN?` can be excluded, and any can be given a
display name). An instrument's page has a console that queries, writes, reads, reads the status byte and
clears the device through the VISA C API itself. The window edits what `symbiotic-ctl` edits: each board's T1
level, the LAN instrument list and discovery switch (with a network scan), which serial ports are
Prologix/AR488 controllers, and the aliases. English and Chinese, light and dark.

It is a web page in the system's webview (WebView2 on Windows, WebKitGTK on Linux, WKWebView on macOS); the
page talks to the program in-process, and nothing listens on a port. Some features are hidden unless
switched on in `features.conf` in the configuration directory (the window writes the file, every switch at
its default, the first time it starts). `--lang en|zh-CN`, `--theme system|light|dark` and
`--page settings|about|board:GPIB0|group:lan|group:usb|group:serial|instrument:<resource>` start it that way.

## Beyond VISA: symbiotic-ctl

Some settings have no place in the VISA API and are not presented as VISA attributes: the write-direction
settling time T1 (`low`, `normal`, `high`; a property of the whole adapter, remembered and set again whenever
the daemon opens that adapter), the LAN instrument list, the bridges and the aliases:

    symbiotic-ctl list
    symbiotic-ctl clients                                    # the processes using the daemon
    symbiotic-ctl [--board N] speed [low|normal|high]       # no level: only report it
    symbiotic-ctl lan [list | add NAME | remove NAME]       # the LAN instruments viFindRsrc lists
    symbiotic-ctl lan discover [on|off]                      # viFindRsrc also finds them (off by default)
    symbiotic-ctl lan scan                                   # find LAN instruments now
    symbiotic-ctl usb                                        # the USBTMC instruments, and which are ours
    symbiotic-ctl serial                                     # the serial ports (opens none)
    symbiotic-ctl bridge [list | add COMn [prologix|ar488] | remove COMn]   # Prologix/AR488 bridges
    symbiotic-ctl alias [list | add NAME RESOURCE | remove NAME]          # aliases: viOpen("DMM")
    symbiotic-ctl stop                                       # ask the daemon to exit

`symbiotic-ctl` with no arguments lists every command. The speed is also the exported (non-VISA) function
`ViStatus symbioticSpeed(ViUInt16 board, ViUInt32 level, ViUInt32 *out)` (level 0 = only read).

## Installing

### Windows

One MSI, for all users, which needs an administrator. Double-clicking it opens the installer's pages:
welcome, the features to install and the folder (`C:\Program Files\Symbiotic` by default), then a last page
where **Install** starts the work. Nothing is changed before that. Double-clicking it again when it is
installed offers Change, Repair or Remove. The install folder goes on the system PATH; the configuration
(`lan.conf`, `aliases.conf`, `bridges.conf`, `speed.conf`, the 82357B's `firmware\`) is shared in
`C:\ProgramData\Symbiotic`, which every user may change. Each user runs a daemon of their own. Upgrading
keeps the folder and the features chosen before.

Features:

- **Viper488 Symbiotic VISA** (required): the libraries, the daemon, the configuration window,
  `symbiotic-ctl`, the C headers and import libraries, the VISA.NET and VISA COM assemblies.
- **USBTMC instruments on WinUSB** (on by default): Windows has no USBTMC driver of its own, so an instrument
  with no driver cannot be used until something gives it one. This feature installs a driver package that
  gives such instruments Windows' own WinUSB driver (no kernel code of its own). The package is dated
  01/01/2000, so every vendor's USBTMC driver outranks it: an instrument that already has a vendor driver
  keeps it, and if a vendor VISA is installed later its driver takes its instruments back. To be installed
  the package has to be signed: the installer creates a certificate on this computer, adds it to the
  computer's **Trusted Root Certification Authorities** and **Trusted Publishers** stores, signs the package
  and then **deletes the certificate's private key**, so it cannot sign anything else. Uninstalling removes
  the package and the certificate. Untick it on the features page if you do not want it.
- **Register with the IVI VISA router**, **Register the VISA.NET implementation**, **Register the VISA COM
  implementation** (off by default): make programs that find VISA through the IVI shared components find
  this one. Think before turning them on: every program that scans the system's VISA then reaches your
  instruments. They are taken back when the feature is removed or the product uninstalled.

A silent install chooses the features by name:

    msiexec /i symbiotic-machine-<v>.msi /qn ADDLOCAL=Runtime,UsbtmcWinUsb,RegisterVisa,RegisterVisaNet,RegisterVisaCom

pyvisa needs no path once the router registration is on (its default library is the router); before that,
give it `C:\Program Files\Symbiotic\visa64.dll`. Never copy the DLL over `C:\Windows\System32\visa64.dll`:
that one belongs to the IVI shared components or another installed VISA.

The configuration window needs Microsoft WebView2, which Windows 11 has; where it is missing the installer
downloads and installs it.

The MSI is not code-signed yet, so Windows SmartScreen may warn when it is run from a download.

### Linux

A `.deb` (amd64, glibc 2.34 and later: Ubuntu 22.04, Debian 12 and newer):

    sudo apt install ./viper488-symbiotic-visa_<v>_amd64.deb

It installs `libvisa64.so`, the daemon, `symbiotic-ctl` and `symbiotic-config` in `/usr/lib/symbiotic`, links
the two programs into `/usr/bin`, the headers into `/usr/include/symbiotic`, a menu entry (the window needs
WebKitGTK 4.1) and the udev rules that let an ordinary user's daemon open the USB devices (the four adapters
and any USBTMC instrument; no kernel driver is unbound). It does not take the system's `libvisa.so` name, so
give pyvisa the path: `pyvisa.ResourceManager("/usr/lib/symbiotic/libvisa64.so")`. Serial ports need the
user in `dialout`. Each user's configuration is in `~/.config/symbiotic`; the 82357B's firmware goes in its
`firmware/`.

Without the package: build with `cargo build --release`, keep the library, the daemon and the tools in one
directory, and install the udev rules with `sudo linux/install-udev.sh` (`--remove` takes them out again).


### macOS

A `.dmg` (universal: Apple Silicon and Intel, macOS 11 and later) holding one `Symbiotic.app`: drag it onto
Applications. No administrator password; upgrading replaces it, removing it is dragging it to the Bin (turn
starting at login off in the window first, if you turned it on). Opening it shows the configuration window.
The library pyvisa is given is
`/Applications/Symbiotic.app/Contents/Frameworks/libvisa64.dylib`; `symbiotic-ctl` is in `Contents/MacOS`, the
headers in `Contents/Resources/include`, and the daemon is the main program of a bundle of its own inside
(`Contents/Helpers/SymbioticDaemon.app`), which macOS needs before it lets a daemon started at login on the
local network. It does not take `/Library/Frameworks/VISA.framework`, pyvisa's default there.

The package is not signed by an Apple developer yet. **Before using it**, run once:

    xattr -dr com.apple.quarantine /Applications/Symbiotic.app

Without that, macOS asks at the screen before anything in it may run, and a program loading the library
waits, without an error, until the question is answered. The first time the daemon uses the local network
(alias sharing), macOS asks whether it may: allow it. Each user's configuration is in `~/.config/symbiotic`.

## Building

Rust (stable), and for the Windows build from Linux, `cargo-xwin`, `clang-cl`, `llvm-lib` and `llvm-rc`
(`sudo apt install clang llvm`); the DLL has a C part (the variadic formatted I/O).

    cargo test -p symbiotic-core                                   # host tests, no hardware
    cargo xwin build --release --target x86_64-pc-windows-msvc     # visa64.dll, symbioticd.exe, symbiotic-ctl.exe
    tools/build_visa32.sh                                          # visa32.dll, visa32.lib
    tools/build_dotnet.sh                                          # Symbiotic.Visa.dll, Symbiotic.VisaCom.dll (dotnet.exe on Windows)
    tools/build_msi.sh                                             # the MSI (wixl and msitools; builds the above first)
    tools/build_deb.sh                                             # the Linux .deb (cargo-zigbuild)
    tools/build_macos.sh                                           # on a Mac: Symbiotic.app (universal) in a .dmg
    tools/check.sh                                                 # every check that needs no hardware

A Linux build of the whole workspace needs WebKitGTK's headers for the window (`libwebkit2gtk-4.1-dev`).

## Limits

- Not implemented: VXI, PXI, shared memory and trigger mapping (no such resources); TLS and SASL for
  HiSLIP; `viGpibControlATN` and `viGpibPassControl`.
- GPIB secondary addresses work on the USB adapters, not on a Prologix/AR488.
- Not yet verified with real hardware: Prologix controllers and LAN/GPIB gateways.
- **32-bit programs on a machine with NI-VISA**: NI installs its own VISA as `SysWOW64\visa32.dll` instead of
  the IVI router, so a 32-bit program that loads `visa32.dll` by name gets NI-VISA only. VISA COM is not
  affected. Copying our `visa32.dll` into such a program's own folder makes it find ours first.

## License

MIT (`LICENSE`). The Keysight 82357B firmware image is not part of this project and is not distributed with
it (`firmware/README.md`).
