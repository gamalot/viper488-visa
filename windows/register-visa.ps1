<#
 register-visa.ps1 -- register Symbiotic with the IVI VISA router, so applications that load "visa64.dll" (or,
 32-bit, "visa32.dll") by name (LabVIEW, vendor tools, C programs) find it. Needs an elevated 64-bit PowerShell.

   .\register-visa.ps1 [-Dll C:\...\visa64.dll]      # default: the installed one's
   .\register-visa.ps1 -Uninstall                     # both views

 The router (System32\visa64.dll) asks the conflict manager (System32\visaConfMgr.dll) for the installed
 VISAs and loads each enabled one. An installed VISA is one subkey of
     HKLM\SOFTWARE\VXIPNP_Alliance\VISA_Installs\CurrentVersion
 whose NAME is a GUID without braces (checked with UuidFromStringA; anything else is never enabled), with
     Location      REG_SZ     full path of the library the router LoadLibraryA's
     FriendlyName  REG_SZ
     Comments      REG_SZ
     VendorID      REG_DWORD
 A VISA the conflict table (ConflictTbl.xml) does not mention counts as enabled.

 If visa32.dll sits beside visa64.dll, the same entry goes into the 32-bit view too,
     HKLM\SOFTWARE\WOW6432Node\VXIPNP_Alliance\VISA_Installs\CurrentVersion
 with Location naming visa32.dll: the 32-bit router (SysWOW64\visa32.dll) and its conflict manager run as
 32-bit code, and Windows redirects their HKLM\SOFTWARE to that view.

 This adds one key of its own and changes nothing else; the key is exported to windows\regbackup first
 if it already exists. System32\visa64.dll stays the IVI router: never copy Symbiotic over it.
 The daemon starts from the DLL's directory; a DLL under one user's profile serves only processes that can
 read that directory, so for more than one user install it somewhere shared first.
#>
param([string]$Dll, [switch]$Uninstall)
$ErrorActionPreference = 'Stop'

# The IVI VISA router's conflict table names a VISA by its id; one that names a VISA no longer installed can make the
# router crash. Only Manager entries naming one of our ids go; a Resource left with none goes too; a copy is kept
# beside.
function Remove-ConflictManagers([string[]]$ids) {
    $path = Join-Path $env:ProgramData 'IVI Foundation\VISA\ConflictTbl.xml'
    if (-not (Test-Path $path)) { return }
    $ids = $ids | ForEach-Object { $_.Trim('{}').ToUpperInvariant() }
    $x = New-Object Xml.XmlDocument; $x.PreserveWhitespace = $false; $x.Load($path)
    $gone = @($x.SelectNodes('//Manager') | Where-Object { $ids -contains $_.GetAttribute('clsid').Trim('{}').ToUpperInvariant() })
    # The preferred VISA too, if it is ours: the same shape, a VISA the router cannot find.
    $pref = @($x.SelectNodes('//Api[@preferredVisa]') | Where-Object { $ids -contains $_.GetAttribute('preferredVisa').Trim('{}').ToUpperInvariant() })
    if (-not $gone -and -not $pref) { return }
    Copy-Item $path "$path.symbiotic-backup" -Force
    foreach ($m in $gone) { $r = $m.ParentNode; [void]$r.RemoveChild($m); if (-not $r.SelectSingleNode('Manager')) { [void]$r.ParentNode.RemoveChild($r) } }
    foreach ($a in $pref) { $a.RemoveAttribute('preferredVisa') }
    $s = New-Object Xml.XmlWriterSettings; $s.Indent = $true; $s.IndentChars = '    '; $s.OmitXmlDeclaration = $true
    $s.Encoding = New-Object Text.UTF8Encoding $false
    $w = [Xml.XmlWriter]::Create($path, $s); $x.Save($w); $w.Close()
    "removed $($gone.Count) of our entries$(if ($pref) { ' and our preferred-VISA mark' }) from $path (the old table is $path.symbiotic-backup)"
}

# The all-users install.
function Get-SymbioticDir {
    $m = (Get-ItemProperty 'HKLM:\SOFTWARE\Viper488\Symbiotic' -Name InstallDir -ErrorAction SilentlyContinue).InstallDir
    if ($m) { return $m.TrimEnd('\') }
    throw "Viper488 Symbiotic VISA is not installed (no HKLM\SOFTWARE\Viper488\Symbiotic\InstallDir); install the MSI, or name the files with a parameter"
}
$id  = '6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64'
$key = "HKLM:\SOFTWARE\VXIPNP_Alliance\VISA_Installs\CurrentVersion\$id"
$key32 = "HKLM:\SOFTWARE\WOW6432Node\VXIPNP_Alliance\VISA_Installs\CurrentVersion\$id"
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrators')) { throw 'run this from an elevated PowerShell' }
if (-not [Environment]::Is64BitProcess) { throw 'run this from a 64-bit PowerShell (a 32-bit one writes the WOW6432Node view)' }

if ($Uninstall) {
    foreach ($k in $key, $key32) { if (Test-Path $k) { Remove-Item $k -Recurse -Force; "removed $k" } else { "not registered: $k" } }
    # Our id stays in the conflict table while VISA COM (register-visacom.ps1) is still registered under it.
    if (-not (Test-Path "Registry::HKEY_CLASSES_ROOT\VISA.GlobalRM\VendorSpecificRMs\$id")) { Remove-ConflictManagers @($id) }
    return
}
if (-not $Dll) { $Dll = Join-Path (Get-SymbioticDir) 'visa64.dll' }
$Dll = [IO.Path]::GetFullPath($Dll)
if (-not (Test-Path $Dll)) { throw "no such file: $Dll (install the MSI, or pass -Dll)" }
if ($Dll -like "$env:SystemRoot\*") { throw "refusing a DLL under $env:SystemRoot: Symbiotic must not live in System32" }
if (-not (Test-Path (Join-Path (Split-Path $Dll) 'symbioticd.exe'))) { throw "symbioticd.exe must sit beside $Dll (the DLL starts it from there)" }

function Register([string]$k, [string]$lib, [string]$backup) {
    if (Test-Path $k) {
        $d = Join-Path $PSScriptRoot 'regbackup'; New-Item -ItemType Directory -Force $d | Out-Null
        reg export ($k -replace '^HKLM:', 'HKLM') "$d\$backup" /y | Out-Null
    }
    New-Item $k -Force | Out-Null
    Set-ItemProperty $k -Name Location     -Value $lib -Type String
    Set-ItemProperty $k -Name FriendlyName -Value 'Viper488 Symbiotic VISA' -Type String
    Set-ItemProperty $k -Name Comments     -Value 'GPIB (Viper488, NI GPIB-USB-HS, Keysight 82357B, CONTEC GP-IB(USB)FL, Prologix/AR488), USBTMC, serial and LAN instruments' -Type String
    Set-ItemProperty $k -Name VendorID     -Value 0 -Type DWord   # no VXIplug&play manufacturer id assigned
    "registered $lib ($k)"
}
Register $key $Dll 'symbiotic-visa-install.before.reg'
$Dll32 = Join-Path (Split-Path $Dll) 'visa32.dll'
if (Test-Path $Dll32) { Register $key32 $Dll32 'symbiotic-visa-install32.before.reg' }
"check: python tests\cm_probe.py; python tests\router_probe.py --idn"
