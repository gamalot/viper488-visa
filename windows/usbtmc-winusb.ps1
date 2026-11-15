<#
 usbtmc-winusb.ps1 -- take away the USBTMC WinUSB package that versions before 0.1.307 installed, and its certificate.
 Run by the all-users MSI as SYSTEM whenever its UsbtmcWinUsb feature is installed or removed; by hand, from an
 elevated PowerShell:

   .\usbtmc-winusb.ps1 -Uninstall

 That package was a class-matched INF (USB\Class_FE&SubClass_03), dated 01/01/2000 so that every vendor driver
 outranked it, signed by a certificate made on the machine whose private key was then deleted. Windows on Arm refuses
 such a package, and from 0.1.307 the service SymbioticBind (symbiotic-bind.exe) gives devices without a driver
 Windows' own winusb.inf instead, on x64 too. Instruments that were on the package are left with no driver when it goes, and the service binds them again.

 Logs to %SystemRoot%\Logs\Symbiotic\usbtmc-winusb.log (only administrators can write there: this runs as SYSTEM, and a
 folder users can write would let one of them point the log somewhere else).
#>
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

$inf = 'symbiotic_usbtmc.inf'
$subjectPrefix = 'CN=Symbiotic USBTMC driver'
$regKey = 'HKLM:\SOFTWARE\Viper488\Symbiotic\UsbtmcWinUsb'

$logDir = Join-Path $env:SystemRoot 'Logs\Symbiotic'
New-Item -ItemType Directory -Force $logDir | Out-Null
$log = Join-Path $logDir 'usbtmc-winusb.log'
function Log([string]$m) {
    $line = "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $m"
    Add-Content -Path $log -Value $line -Encoding UTF8
    $line
}

if (-not $Uninstall) { throw "only -Uninstall is left: devices are bound by the service SymbioticBind now" }
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Administrator rights are required."
}

try {
    Log "uninstall"
    # Our packages in the driver store, by their original name. Not from pnputil's text, which is in the system's
    # language; Get-WindowsDriver lists the third-party packages with language-neutral properties.
    $pkgs = @(Get-WindowsDriver -Online | Where-Object { (Split-Path $_.OriginalFileName -Leaf) -ieq $inf } | ForEach-Object Driver)
    foreach ($p in $pkgs) {
        # /uninstall: off the devices that use it (they go to the next best driver, or to none), then out of the store.
        $out = & "$env:SystemRoot\System32\pnputil.exe" /delete-driver $p /uninstall 2>&1 | Out-String
        Log "pnputil /delete-driver $p /uninstall -> $LASTEXITCODE"
        $out.Trim().Split("`n") | ForEach-Object { if ($_.Trim()) { Log "  $($_.TrimEnd())" } }
    }
    foreach ($store in 'Root', 'TrustedPublisher', 'My') {
        Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object { $_.Subject -like "$subjectPrefix*" } | ForEach-Object {
            Log "removing certificate $($_.Thumbprint) ($($_.Subject)) from LocalMachine\$store"
            if ($store -eq 'My') { Remove-Item $_.PSPath -DeleteKey } else { Remove-Item $_.PSPath }
        }
    }
    Remove-Item $regKey -Recurse -ErrorAction SilentlyContinue
    Log "done"
    exit 0
} catch {
    Log "FAILED: $($_.Exception.Message)"
    exit 1
}
