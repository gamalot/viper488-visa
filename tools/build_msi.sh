#!/usr/bin/env bash
# build_msi.sh -- the MSI (for all users), from WSL: windows/symbiotic.wxs through wixl (apt: wixl), then the
# installer's pages (tools/msi_ui.py).
#   tools/build_msi.sh                -> target/symbiotic-visa-<version>-windows-x64.msi
#   tools/build_msi.sh quick          -> the same, built with Cargo.toml's [profile.quick] (no LTO, incremental): for
#                                        a quick local install after a change, never for a release
#   tools/build_msi.sh [quick] arm64  -> target/symbiotic-visa-<version>-windows-arm64.msi, for Windows on Arm: ARM64
#                                        daemon and programs, an ARM64X visa64.dll (tools/build_arm64x.sh) that ARM64
#                                        programs and x64 ones alike load, no registrations offered
# ("machine" is accepted as an argument.)
# Builds the 64-bit workspace, the 32-bit DLL and the VISA.NET assembly (tools/build_dotnet.sh) first, stages what the MSI holds, and never the
# Keysight 82357B firmware (firmware/ is Keysight's image: not ours to distribute).
# The version is the workspace's major.minor and the commit count (SYMBIOTIC_BUILD overrides it): every commit's MSI is
# newer than the last, so it upgrades in place.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
profile=release; arch=x64
for a in "$@"; do
    case "$a" in
        machine) ;;
        quick) profile=quick ;;
        arm64) arch=arm64 ;;
        *) echo "usage: $0 [quick] [arm64] (builds the all-users MSI; quick: without LTO, for this machine only; arm64: for Windows on Arm)"; exit 2 ;;
    esac
done
REL64=target/x86_64-pc-windows-msvc/$profile
REL32=target/i686-pc-windows-msvc/$profile
# What runs (the daemon, the programs, symbiotic-quiet) is the machine's own architecture.
if [ "$arch" = arm64 ]; then REL=target/aarch64-pc-windows-msvc/$profile; else REL=$REL64; fi
mm=$(sed -n 's/^version = "\([0-9]*\.[0-9]*\)\..*/\1/p' Cargo.toml | head -1)
build=${SYMBIOTIC_BUILD:-$(git rev-list --count HEAD)}
version="$mm.$build"
# The files carry the same version in their resources (tools/winres.rs), from this same count.
export SYMBIOTIC_BUILD=$build
if [ "$arch" = arm64 ]; then
    XWIN_ARCH=x86,x86_64,aarch64 cargo xwin build -q --profile "$profile" --target aarch64-pc-windows-msvc
    tools/build_arm64x.sh "$profile" >/dev/null
else
    cargo xwin build -q --profile "$profile" --target x86_64-pc-windows-msvc
fi
SYMBIOTIC_PROFILE=$profile tools/build_visa32.sh >/dev/null
tools/build_dotnet.sh >/dev/null
stage=$(mktemp -d); trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/include" "$stage/windows" "$stage/dotnet"
cp "$REL/symbioticd.exe" "$REL/symbiotic-ctl.exe" "$REL/symbiotic-explorer.exe" "$REL/symbiotic-bind.exe" "$stage/"
# On Windows on Arm one ARM64X visa64.dll serves programs emulated as x64 (its ARM64EC half) and ARM64 ones alike.
if [ "$arch" = arm64 ]; then
    cp "target/arm64x/$profile/visa64.dll" "target/arm64x/$profile/visa64.lib" "$stage/"
else
    cp "$REL64/visa64.dll" "$stage/"
    cp "$REL64/visa64.dll.lib" "$stage/visa64.lib"
fi
# Microsoft's WebView2 Evergreen bootstrapper, installed in windows\ and run where WebView2 is missing (symbiotic.wxs): fetched once into
# target/webview2 (redistributable under the WebView2 runtime's terms; not kept in git).
wv2=target/webview2/MicrosoftEdgeWebview2Setup.exe
if [ ! -s "$wv2" ]; then
    mkdir -p "$(dirname "$wv2")"
    curl -fsSL -o "$wv2.part" 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' && mv "$wv2.part" "$wv2"
fi
file "$wv2" | grep -q 'PE32 executable' || { echo "$wv2 is not a Windows executable"; exit 1; }
cp "$REL32/visa32.dll" "$REL32/visa32.lib" "$stage/"
cp include/visa.h include/visatype.h "$stage/include/"
cp windows/winusb-bind.ps1 windows/winusb-unbind.ps1 windows/usbtmc-winusb.ps1 windows/register-visa.ps1 windows/register-visanet.ps1 windows/register-visacom.ps1 "$wv2" "$stage/windows/"
# What the installer's console commands are started through, so that none opens a window (symbiotic.wxs).
cp "$REL/symbiotic-quiet.exe" "$stage/windows/"
cp -r target/dotnet/net462 target/dotnet/net6.0 "$stage/dotnet/"
if find "$stage" -iname '*.hex' -o -iname 'measat*' | grep -q .; then
    echo "refusing: a firmware image is in the staging directory"; exit 1
fi
out="target/symbiotic-visa-$version-windows-$arch.msi"
wixl -a x64 -D Version="$version" -D Src="$stage" -D Arch="$arch" -o "$out" windows/symbiotic.wxs
# wixl leaves the File table's Version column empty, which Windows Installer reads as "unversioned"
# whatever the file holds; the DLLs and executables carry $version.0 (tools/winres.rs), so it is
# written in here and the table put back.
idt=$(mktemp -d); trap 'rm -rf "$stage" "$idt"' EXIT
msiinfo export "$out" File > "$idt/File.idt"
awk -F'\t' -v OFS='\t' -v ver="$version.0" 'NR > 3 && $3 ~ /\.(dll|exe)$/ && $3 !~ /Webview2/ { $5 = ver } { print }' \
    "$idt/File.idt" > "$idt/File.new" && mv "$idt/File.new" "$idt/File.idt"
msibuild "$out" -i "$idt/File.idt"
# wixl writes the PATH entry as "=PATH" whatever Permanent says; without the "-" Windows Installer
# never takes our folder back out of PATH on uninstall (or on an upgrade's removal of the old one).
msiinfo export "$out" Environment > "$idt/Environment.idt"
awk -F'\t' -v OFS='\t' 'NR > 3 && $2 == "=PATH" { $2 = "=-PATH" } NR > 3 && $2 == "=*PATH" { $2 = "=-*PATH" } { print }' "$idt/Environment.idt" > "$idt/Environment.new" \
    && mv "$idt/Environment.new" "$idt/Environment.idt"
msibuild "$out" -i "$idt/Environment.idt"
# wixl writes no RemoveFile element: the registry backups register-visa.ps1 kept in windows\regbackup before 0.1.311, and
# that folder, are taken away on uninstall here (REGBACKUPDIR is set by symbiotic.wxs's SetRegBackupDir; 2 = on uninstall).
msiinfo export "$out" RemoveFile > "$idt/RemoveFile.idt"
printf 'RegBackupFiles\tScripts\t*.reg\tREGBACKUPDIR\t2\r\nRegBackupDir\tScripts\t\tREGBACKUPDIR\t2\r\n' >> "$idt/RemoveFile.idt"
msibuild "$out" -i "$idt/RemoveFile.idt"
# The installer's pages, which wixl cannot write: nothing is installed until the user clicks Install.
python3 tools/msi_ui.py "$out"
# wixl knows no arm64 (-a x64 above, whose components are the 64-bit ones Windows on Arm installs too); the summary's
# platform says Arm64, so that Windows Installer refuses it on an x64 machine. msibuild -s rewrites the subject, the
# author, the platform and the package code, and keeps the rest.
if [ "$arch" = arm64 ]; then
    msibuild "$out" -s "Viper488 Symbiotic VISA runtime (all users)" "Viper488" "Arm64;1033" "{$(cat /proc/sys/kernel/random/uuid | tr a-z A-Z)}"
    [ "$(msiinfo suminfo "$out" | grep -c '^Template: Arm64;1033$')" = 1 ] || { echo "the platform of $out is not Arm64"; exit 1; }
fi
echo "$out"
