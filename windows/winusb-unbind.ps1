<#
 winusb-unbind.ps1 -- undo winusb-bind.ps1 for one VID/PID: remove its package from the driver
 store, and Windows gives the device back to its vendor driver (which is still there). Run as
 Administrator.

   .\winusb-unbind.ps1 -VidPid F4EC:1103

 The package is found by its original file name through Get-WindowsDriver, not by reading
 pnputil's text, which is translated on a Windows in another language. The certificate stays
 trusted; see winusb-bind.ps1 for which one it is, and certmgr.msc to remove it if nothing else
 uses it.
#>
param([Parameter(Mandatory)][ValidatePattern('^[0-9A-Fa-f]{4}:[0-9A-Fa-f]{4}$')][string]$VidPid)
$ErrorActionPreference = 'Stop'

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Administrator rights are required. Run this from an elevated PowerShell."
}

$vid, $prod = $VidPid.ToUpper().Split(':')
$hwid = "USB\VID_$vid&PID_$prod"
$inf = "symbiotic_winusb_$($vid.ToLower())_$($prod.ToLower()).inf"

$pkgs = @(Get-WindowsDriver -Online | Where-Object { (Split-Path $_.OriginalFileName -Leaf) -ieq $inf })
if (-not $pkgs) { Write-Host "$inf is not in the driver store; nothing to undo."; exit 0 }
foreach ($p in $pkgs) {
    Write-Host "Removing $($p.Driver) ($inf)"
    pnputil /delete-driver $p.Driver /uninstall /force
}
Start-Sleep 2
pnputil /scan-devices | Out-Null
Start-Sleep 3

Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like "$hwid\*" -or $_.InstanceId -like "$hwid&*" } |
    ForEach-Object { Write-Host ("  {0}  {1}  service={2}  status={3}" -f $_.InstanceId, $_.FriendlyName, $_.Service, $_.Status) }
Write-Host "A service other than WINUSB means the vendor driver has it back."
