<#
 register-visacom.ps1 -- register Symbiotic's VISA COM implementation (VPP-4.3.4) with the IVI VISA COM shared
 components, so VISA.GlobalRM finds it: IVI-COM and Nimbus-built IVI-C drivers (Keysight's among them), Excel
 VBA and other VISA COM programs. Needs an elevated 64-bit PowerShell and the VISA shared components.

   .\register-visacom.ps1 [-Dll C:\...\Symbiotic.VisaCom.dll]   # default: the installed one's
   .\register-visacom.ps1 -Uninstall

 What it does: RegAsm /codebase, 64-bit and 32-bit, which registers the
 resource manager (Symbiotic.ResourceManager.1) and one component per resource kind (Symbiotic.GpibInstr.1 ...),
 ThreadingModel Both; the assembly's own registration function adds the resource manager to
 HKCR\VISA.GlobalRM\VendorSpecificRMs and to the category of vendor-specific resource managers.

 After this every program that asks VISA.GlobalRM for resources finds ours too, instruments and all. The
 registration points at the DLL where it is (C:\Program Files\Symbiotic\dotnet\net462 for the MSI's).
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
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrators')) { throw 'run this from an elevated PowerShell' }
if (-not [Environment]::Is64BitProcess) { throw 'run this from a 64-bit PowerShell' }
$regasm = @(
    (Join-Path $env:windir 'Microsoft.NET\Framework64\v4.0.30319\RegAsm.exe'),
    (Join-Path $env:windir 'Microsoft.NET\Framework\v4.0.30319\RegAsm.exe')
)
# The C library's router id (register-visa.ps1): one id per vendor in the conflict table they share.
$clsid = '{6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64}'
# An earlier build's resource manager had a CLSID of its own; whatever is left of it goes, on every run.
$oldClsid = '{9E4D7559-0916-429B-98B2-8FEC98BB71A6}'
$routerKey = 'HKLM:\SOFTWARE\VXIPNP_Alliance\VISA_Installs\CurrentVersion\6B1D2A40-7E31-4C8A-9F52-3D8A0B7C1E64'
if (-not $Dll) { $Dll = Join-Path (Get-SymbioticDir) 'dotnet\net462\Symbiotic.VisaCom.dll' }
$Dll = [IO.Path]::GetFullPath($Dll)
if (-not (Test-Path $Dll)) { throw "no such file: $Dll (install the MSI first, or pass -Dll)" }

foreach ($r in $regasm) {
    if (-not (Test-Path $r)) { throw "no $r (.NET Framework 4)" }
    $a = if ($Uninstall) { @('/unregister', $Dll, '/nologo') } else { @('/codebase', $Dll, '/nologo') }
    $out = & $r @a 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$r failed ($LASTEXITCODE): $out" }
    "$(Split-Path (Split-Path $r) -Leaf) $(Split-Path $r -Leaf): $(($out | Where-Object { $_ -and $_ -notmatch 'RA0000' }) -join ' ')"
}

$views = @('Registry::HKEY_CLASSES_ROOT\CLSID', 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Classes\CLSID')
$srm = "Registry::HKEY_CLASSES_ROOT\VISA.GlobalRM\VendorSpecificRMs\$($clsid.Trim('{}'))"   # no braces: GlobMgr adds them
foreach ($k in @($views | ForEach-Object { "$_\$oldClsid" }) + "Registry::HKEY_CLASSES_ROOT\VISA.GlobalRM\VendorSpecificRMs\$($oldClsid.Trim('{}'))") {
    if (Test-Path $k) { Remove-Item $k -Recurse -Force; "removed the old $k" }
}
Remove-ConflictManagers @($oldClsid)
if ($Uninstall) {
    if (Test-Path $srm) { Remove-Item $srm -Force; "removed $srm" }
    # Our id stays in the conflict table while the C library is still registered under it.
    if (-not (Test-Path $routerKey)) { Remove-ConflictManagers @($clsid) }
    foreach ($v in $views) { if (Test-Path "$v\$clsid") { throw "still registered: $v\$clsid" } }
    'unregistered'
    return
}
foreach ($v in $views) {
    $p = "$v\$clsid\InprocServer32"
    if (-not (Test-Path $p)) { throw "not registered: $p" }
}
if (-not (Test-Path $srm)) { throw "not in VISA.GlobalRM's list: $srm" }
"registered $Dll (64-bit and 32-bit), and in $srm"
