<#
 winusb-bind.ps1 -- put ONE USB device, by its VID and PID, on Windows' own WinUSB driver, so that symbioticd can open
 it (a USBTMC instrument or a GPIB adapter taken off its vendor driver), now and whenever it is plugged in again. Run as
 Administrator.
   .\winusb-bind.ps1 -VidPid F4EC:1103                    # Siglent SDG1022X Plus
   .\winusb-unbind.ps1 -VidPid F4EC:1103                  # and back

 It is `symbiotic-bind bind VVVV:PPPP` (the installed symbiotic-bind.exe, which the service
 SymbioticBind also is): the in-box, Microsoft-signed winusb.inf's "WinUsb Device" is chosen for the device, as Device
 Manager's "Let me pick" does, and the VID/PID is remembered (HKLM\SOFTWARE\Viper488\Symbiotic\WinUsb) so that the
 service puts it back on WinUSB if Windows gives it another driver. No INF of ours and no certificate: Windows on Arm
 refuses a package signed by a certificate made on the machine, which this script made before 0.1.307. Devices without a driver need none of this: the service
 binds them by itself. Afterwards the vendor's software and every other VISA lose this device until winusb-unbind.ps1.
 -Name is still accepted and no longer used.
#>
param(
    [Parameter(Mandatory)][ValidatePattern('^[0-9A-Fa-f]{4}:[0-9A-Fa-f]{4}$')][string]$VidPid,
    [string]$Name
)
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Administrator rights are required. Run this from an elevated PowerShell."
}
# Installed beside this script's folder (windows\ under the install directory); else the install's, from the registry.
$exe = Join-Path (Split-Path $PSScriptRoot) 'symbiotic-bind.exe'
if (-not (Test-Path $exe)) {
    $dir = (Get-ItemProperty 'HKLM:\SOFTWARE\Viper488\Symbiotic' -Name InstallDir -ErrorAction SilentlyContinue).InstallDir
    if ($dir) { $exe = Join-Path $dir 'symbiotic-bind.exe' }
}
if (-not (Test-Path $exe)) { throw "symbiotic-bind.exe is not installed: install Viper488 Symbiotic VISA" }
& $exe bind $VidPid
exit $LASTEXITCODE
