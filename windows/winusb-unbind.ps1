<#
 winusb-unbind.ps1 -- undo winusb-bind.ps1 for one VID/PID: Windows chooses the device's driver again (the vendor's, if
 it is still there). Run as Administrator.

   .\winusb-unbind.ps1 -VidPid F4EC:1103

 It is `symbiotic-bind unbind VVVV:PPPP`. With no vendor driver to go back to, a USBTMC instrument or a GPIB adapter is left with none, and the service
 SymbioticBind puts it on WinUSB again. A package that winusb-bind.ps1 installed before 0.1.307 (an INF of ours for
 this VID/PID, symbiotic_winusb_vvvv_pppp.inf) is taken out of the driver store too, found by its original file name
 through Get-WindowsDriver (not pnputil's text, which is translated); the certificate it was signed with stays trusted:
 certmgr.msc removes it ('Symbiotic device driver') if nothing else uses it.
#>
param([Parameter(Mandatory)][ValidatePattern('^[0-9A-Fa-f]{4}:[0-9A-Fa-f]{4}$')][string]$VidPid)
$ErrorActionPreference = 'Stop'

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Administrator rights are required. Run this from an elevated PowerShell."
}

$vid, $prod = $VidPid.ToUpper().Split(':')
$inf = "symbiotic_winusb_$($vid.ToLower())_$($prod.ToLower()).inf"
foreach ($p in @(Get-WindowsDriver -Online | Where-Object { (Split-Path $_.OriginalFileName -Leaf) -ieq $inf })) {
    Write-Host "Removing $($p.Driver) ($inf, from winusb-bind.ps1 before 0.1.307)"
    pnputil /delete-driver $p.Driver /uninstall /force
}

$exe = Join-Path (Split-Path $PSScriptRoot) 'symbiotic-bind.exe'
if (-not (Test-Path $exe)) {
    $dir = (Get-ItemProperty 'HKLM:\SOFTWARE\Viper488\Symbiotic' -Name InstallDir -ErrorAction SilentlyContinue).InstallDir
    if ($dir) { $exe = Join-Path $dir 'symbiotic-bind.exe' }
}
if (-not (Test-Path $exe)) { throw "symbiotic-bind.exe is not installed: install Viper488 Symbiotic VISA" }
& $exe unbind $VidPid
exit $LASTEXITCODE
