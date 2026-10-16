<#
 usbtmc-winusb.ps1 -- bind every USBTMC instrument that has NO driver to Windows' own WinUSB driver, so that
 symbioticd can open it; one on a vendor's USBTMC driver stays there. Run by the all-users MSI as SYSTEM (its
 UsbtmcWinUsb feature); by hand, from an elevated PowerShell:

   .\usbtmc-winusb.ps1 -Version 0.1.300.0     # install (an older package of ours is replaced)
   .\usbtmc-winusb.ps1 -Uninstall             # the package and its certificate go

 In short:
   * the INF matches the interface class, USB\Class_FE&SubClass_03&Prot_00 / Prot_01 (the level IVI-6.2 table
     2-3 allows a VISA library vendor), and includes winusb.inf: no kernel code of its own.
   * its DriverVer date is 01/01/2000, always. PnP gives our self-signed package and a vendor's WHQL one the same
     rank and then takes the newer date, so ours loses to every vendor driver and wins only where nothing else
     matches.
   * it is signed with a certificate made here, on this machine, whose private key is deleted as soon as the
     catalog is signed: the machine trusts it as a root, and with no key it can sign nothing else.
   * windows\winusb-bind.ps1 still moves ONE instrument off a vendor driver; its VID/PID package outranks this.

 Logs to %SystemRoot%\Logs\Symbiotic\usbtmc-winusb.log (only administrators can write there: this runs as
 SYSTEM, and a folder users can write would let one of them point the log somewhere else).
#>
param([string]$Version, [switch]$Uninstall)
$ErrorActionPreference = 'Stop'

$inf = 'symbiotic_usbtmc.inf'
$subject = "CN=Symbiotic USBTMC driver ($env:COMPUTERNAME)"
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

if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Administrator rights are required."
}

# pnputil's exit codes that are not failures: 3010 a reboot completes it; 259 (ERROR_NO_MORE_ITEMS) the package
# is in the store but no device took it -- none plugged in, or each on a better-ranked driver.
function Invoke-Pnputil([string[]]$argv) {
    $out = & "$env:SystemRoot\System32\pnputil.exe" @argv 2>&1 | Out-String
    $code = $LASTEXITCODE
    Log "pnputil $($argv -join ' ') -> $code"
    $out.Trim().Split("`n") | ForEach-Object { if ($_.Trim()) { Log "  $($_.TrimEnd())" } }
    if ($code -notin 0, 259, 3010) { throw "pnputil $($argv -join ' ') failed ($code)" }
}

# Our packages in the driver store, by their original name. Not from pnputil's text, which is in the system's
# language; Get-WindowsDriver lists the third-party packages with language-neutral properties.
function Get-OurPackages {
    @(Get-WindowsDriver -Online | Where-Object { (Split-Path $_.OriginalFileName -Leaf) -ieq $inf } | ForEach-Object Driver)
}

function Remove-Ours {
    foreach ($p in Get-OurPackages) {
        # /uninstall: off the devices that use it (they go to the next best driver, or to none), then out of the store.
        Invoke-Pnputil @('/delete-driver', $p, '/uninstall')
    }
    foreach ($store in 'Root', 'TrustedPublisher', 'My') {
        Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object { $_.Subject -like "$subjectPrefix*" } | ForEach-Object {
            Log "removing certificate $($_.Thumbprint) ($($_.Subject)) from LocalMachine\$store"
            if ($store -eq 'My') { Remove-Item $_.PSPath -DeleteKey } else { Remove-Item $_.PSPath }
        }
    }
    Remove-Item $regKey -Recurse -ErrorAction SilentlyContinue
}

try {
    if ($Uninstall) {
        Log "uninstall"
        Remove-Ours
        Log "done"
        exit 0
    }

    if ($Version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$') { throw "-Version a.b.c[.d] is required (got '$Version')" }
    if ($Version -match '^\d+\.\d+\.\d+$') { $Version += '.0' }
    Log "install $Version"

    # An older package of ours first, so that no device ever has two of ours to choose between; its certificate too.
    Remove-Ours

    # A directory no one else can have prepared: SYSTEM writes the INF and its catalog here.
    $work = Join-Path ([IO.Path]::GetTempPath()) ("symbiotic-usbtmc-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $work | Out-Null
    try {
        $infPath = Join-Path $work $inf
        $cat = [IO.Path]::ChangeExtension($inf, '.cat')
        @"
; Bind every USBTMC / USB488 function that has no other driver to the in-box WinUSB driver, for Symbiotic's
; symbioticd. Installed by the all-users MSI (windows\usbtmc-winusb.ps1). No kernel binary of its own.
; The date is 01/01/2000 on purpose: PnP ranks this and a vendor's USBTMC driver alike and then takes the
; newer date, so a vendor driver always wins.

[Version]
Signature   = "`$Windows NT`$"
Class       = USBDevice
ClassGuid   = {88BAE032-5A81-49f0-BC3D-A4FF138216D6}
Provider    = %Provider%
CatalogFile = $cat
DriverVer   = 01/01/2000,$Version
PnpLockdown = 1

[Manufacturer]
%Provider% = Devices, NTamd64

[Devices.NTamd64]
%DeviceName% = USB_Install, USB\Class_FE&SubClass_03&Prot_00
%DeviceName% = USB_Install, USB\Class_FE&SubClass_03&Prot_01

[USB_Install]
Include = winusb.inf
Needs   = WINUSB.NT

[USB_Install.Services]
Include = winusb.inf
Needs   = WINUSB.NT.Services

[USB_Install.HW]
AddReg = Dev_AddReg

[Dev_AddReg]
; The device interface a WinUSB client opens it through; the same GUID winusb-bind.ps1 gives.
HKR,,DeviceInterfaceGUIDs,0x10000,"{64F9CE3B-42E2-489B-AED7-DEE3AD038EC4}"

[Strings]
Provider   = "Symbiotic"
DeviceName = "USB Test and Measurement Device (WinUSB, Symbiotic)"
"@ | Set-Content -Path $infPath -Encoding ASCII

        $catPath = Join-Path $work $cat
        # The INF alone, not the directory: a directory catalog hashes whatever else is in it.
        New-FileCatalog -Path $infPath -CatalogFilePath $catPath -CatalogVersion 2 | Out-Null

        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject -CertStoreLocation Cert:\LocalMachine\My `
                -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(50)
        Log "made certificate $($cert.Thumbprint) ($subject)"
        try {
            # Trusted first, then signed: Set-AuthenticodeSignature checks the chain as it signs, and reports a
            # signature by a certificate not yet trusted as UnknownError.
            $cer = Join-Path $work 'signer.cer'
            Export-Certificate -Cert $cert -FilePath $cer | Out-Null
            # certutil, not Import-Certificate, which is refused on TrustedPublisher (E_ACCESSDENIED).
            foreach ($store in 'Root', 'TrustedPublisher') {
                $o = & "$env:SystemRoot\System32\certutil.exe" -f -addstore $store $cer 2>&1 | Out-String
                if ($LASTEXITCODE -ne 0) { throw "certutil -addstore $store failed ($LASTEXITCODE): $o" }
                Log "trusted in LocalMachine\$store"
            }
            $sig = Set-AuthenticodeSignature -FilePath $catPath -Certificate $cert -HashAlgorithm SHA256
            if ($sig.Status -ne 'Valid') { throw "signing the catalog failed: $($sig.Status) $($sig.StatusMessage)" }
            Log "signed $cat"
        } finally {
            # The private key goes whatever happened: with it gone the certificate signs nothing else.
            Remove-Item $cert.PSPath -DeleteKey
            if (Test-Path $cert.PSPath) { throw "the signing certificate's private key could not be deleted" }
            Log "deleted the private key"
        }

        Invoke-Pnputil @('/add-driver', $infPath, '/install')
        $pkgs = Get-OurPackages
        if (-not $pkgs) { throw "the package is not in the driver store after pnputil /add-driver" }
        New-Item -Force $regKey | Out-Null
        Set-ItemProperty $regKey -Name Package -Value ($pkgs -join ' ')
        Set-ItemProperty $regKey -Name Certificate -Value $cert.Thumbprint
        Set-ItemProperty $regKey -Name Version -Value $Version
        Log "installed as $($pkgs -join ' ')"
    } finally {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
    Log "done"
    exit 0
} catch {
    Log "FAILED: $($_.Exception.Message)"
    if (-not $Uninstall) {
        # Nothing half done stays behind: above all no certificate the machine trusts.
        try { Remove-Ours } catch { Log "cleaning up failed too: $($_.Exception.Message)" }
    }
    exit 1
}
