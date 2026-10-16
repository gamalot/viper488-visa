<#
 register-visanet.ps1 -- register Symbiotic's VISA.NET implementation with the IVI VISA.NET shared components, so
 programs that use Ivi.Visa's GlobalResourceManager (C# test programs, vendor tools, IVI.NET drivers) find it.
 Needs an elevated 64-bit Windows PowerShell (5.1: the GAC is .NET Framework's) and the VISA.NET shared components.

   .\register-visanet.ps1 [-From C:\...\Symbiotic\dotnet]   # default: the installed one's
   .\register-visanet.ps1 -Uninstall

 What VPP-4.3.6 (section 18) asks, and so what this does:
   - .NET Framework: the assembly (net462) into the Global Assembly Cache.
   - .NET 6+: the net6.0 build into <VXIPNPPATH>\Microsoft.NET\VendorAssemblies\vy\8.0\Symbiotic.Visa\ ("vy" is
     our vendor id 0x7679 as two letters, 8.0 the Ivi.Visa it was compiled against). Never Ivi.Visa.dll beside it.
   - Both: the key HKLM\SOFTWARE\IVI\VISA.NET\<GUID>\<version> in the 32-bit view (WOW6432Node), with
       Location      REG_SZ     "Symbiotic.Visa.ResourceManager, <the assembly's full name>"
       FriendlyName  REG_SZ     "Viper488 Symbiotic VISA.NET Resource Manager"
       VendorID      REG_DWORD  0x7679 (the resource manager's ManufacturerId)
       Comments      REG_SZ
     one version key per installed version: older ones of ours are removed with their GAC copies.

 After this every program that asks the GlobalResourceManager for resources finds ours too, instruments and all.
 The assembly finds the C library through the install (HKLM\SOFTWARE\Viper488\Symbiotic\InstallDir).
#>
param([string]$From, [switch]$Uninstall)
$ErrorActionPreference = 'Stop'

# The all-users install.
function Get-SymbioticDir {
    $m = (Get-ItemProperty 'HKLM:\SOFTWARE\Viper488\Symbiotic' -Name InstallDir -ErrorAction SilentlyContinue).InstallDir
    if ($m) { return $m.TrimEnd('\') }
    throw "Viper488 Symbiotic VISA is not installed (no HKLM\SOFTWARE\Viper488\Symbiotic\InstallDir); install the MSI, or name the files with a parameter"
}
$guid = '71ADCC3F-7D29-42A2-873E-1F1418FCA388'   # this product's, for good (VPP-4.3.6 RULE 18.2.7)
$key = "HKLM:\SOFTWARE\WOW6432Node\IVI\VISA.NET\$guid"
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrators')) { throw 'run this from an elevated PowerShell' }
if (-not [Environment]::Is64BitProcess) { throw 'run this from a 64-bit PowerShell' }
if ($PSVersionTable.PSEdition -eq 'Core') { throw 'run this from Windows PowerShell 5.1 (powershell.exe): the GAC belongs to .NET Framework' }

Add-Type -AssemblyName System.EnterpriseServices
$publish = New-Object System.EnterpriseServices.Internal.Publish
$gacDir = Join-Path $env:windir 'Microsoft.NET\assembly\GAC_MSIL\Symbiotic.Visa'
$vxipnp = (Get-ItemProperty 'HKLM:\SOFTWARE\VXIPNP_Alliance\VXIPNP\CurrentVersion' -ErrorAction SilentlyContinue).VXIPNPPATH
if (-not $vxipnp -or -not (Test-Path $vxipnp)) { $vxipnp = Join-Path $env:ProgramFiles 'IVI Foundation\VISA' }
$vendorDir = Join-Path $vxipnp 'Microsoft.NET\VendorAssemblies\vy'

function Remove-Ours {
    if (Test-Path $key) { Remove-Item $key -Recurse -Force; "removed $key" }
    if (Test-Path $gacDir) {
        foreach ($d in Get-ChildItem $gacDir -Directory) {
            $dll = Join-Path $d.FullName 'Symbiotic.Visa.dll'
            if (Test-Path $dll) { $publish.GacRemove($dll); "removed from the GAC: $($d.Name)" }
        }
    }
    if (Test-Path $vendorDir) { Remove-Item $vendorDir -Recurse -Force; "removed $vendorDir" }
}

if ($Uninstall) {
    Remove-Ours
    if (-not (Test-Path $gacDir) -or -not (Get-ChildItem $gacDir -Recurse -Filter Symbiotic.Visa.dll)) { 'nothing of ours left in the GAC' }
    return
}

if (-not $From) { $From = Join-Path (Get-SymbioticDir) 'dotnet' }
$fw = Join-Path $From 'net462\Symbiotic.Visa.dll'
$core = Join-Path $From 'net6.0'
foreach ($f in $fw, (Join-Path $core 'Symbiotic.Visa.dll'), (Join-Path $core 'Symbiotic.Visa.deps.json')) {
    if (-not (Test-Path $f)) { throw "no such file: $f (install the MSI first, or pass -From)" }
}
if (Get-ChildItem $From -Recurse -Filter 'Ivi.Visa*.dll') { throw "Ivi.Visa.dll is under ${From}: it must not be installed with ours" }
if (-not (Get-ChildItem (Join-Path $env:windir 'Microsoft.NET\assembly\GAC_MSIL\Ivi.Visa') -Recurse -Filter Ivi.Visa.dll -ErrorAction SilentlyContinue)) {
    throw 'the IVI VISA.NET shared components are not installed (no Ivi.Visa in the GAC)'
}
$name = [Reflection.AssemblyName]::GetAssemblyName($fw)
$version = '{0}.{1}.{2}' -f $name.Version.Major, $name.Version.Minor, $name.Version.Build   # the installer version (RULE 18.2.2)

Remove-Ours
$publish.GacInstall($fw)
$inGac = Get-ChildItem $gacDir -Recurse -Filter Symbiotic.Visa.dll -ErrorAction SilentlyContinue |
    Where-Object { [Reflection.AssemblyName]::GetAssemblyName($_.FullName).FullName -eq $name.FullName }
if (-not $inGac) { throw "GacInstall did not put $($name.FullName) into the GAC" }
"GAC: $($name.FullName)"

$dest = Join-Path $vendorDir '8.0\Symbiotic.Visa'
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item (Join-Path $core 'Symbiotic.Visa.dll'), (Join-Path $core 'Symbiotic.Visa.deps.json') $dest -Force
"VendorAssemblies: $dest"

$k = Join-Path $key $version
New-Item $k -Force | Out-Null
Set-ItemProperty $k -Name Location     -Value "Symbiotic.Visa.ResourceManager, $($name.FullName)" -Type String
Set-ItemProperty $k -Name FriendlyName -Value 'Viper488 Symbiotic VISA.NET Resource Manager' -Type String
Set-ItemProperty $k -Name VendorID     -Value 0x7679 -Type DWord
Set-ItemProperty $k -Name Comments     -Value 'GPIB (Viper488, NI GPIB-USB-HS, Keysight 82357B, CONTEC GP-IB(USB)FL, Prologix/AR488), USBTMC, serial and LAN instruments' -Type String
"registered $k"
