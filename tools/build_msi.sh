#!/usr/bin/env bash
# build_msi.sh -- the MSI (for all users), from WSL: windows/symbiotic.wxs through wixl (apt: wixl), then the
# installer's pages (tools/msi_ui.py).
#   tools/build_msi.sh                -> target/symbiotic-machine-<version>.msi
# ("machine" is accepted as an argument.)
# Builds the 64-bit workspace, the 32-bit DLL and the VISA.NET assembly (tools/build_dotnet.sh) first, stages what the MSI holds, and never the
# Keysight 82357B firmware (firmware/ is Keysight's image: not ours to distribute).
# The version is the workspace's major.minor and the commit count (SYMBIOTIC_BUILD overrides it): every commit's MSI is
# newer than the last, so it upgrades in place.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
case "${1:-machine}" in machine) ;; *) echo "usage: $0 (builds the all-users MSI; it takes no other argument)"; exit 2 ;; esac
REL=target/x86_64-pc-windows-msvc/release
REL32=target/i686-pc-windows-msvc/release
mm=$(sed -n 's/^version = "\([0-9]*\.[0-9]*\)\..*/\1/p' Cargo.toml | head -1)
build=${SYMBIOTIC_BUILD:-$(git rev-list --count HEAD)}
version="$mm.$build"
# The files carry the same version in their resources (tools/winres.rs), from this same count.
export SYMBIOTIC_BUILD=$build
cargo xwin build -q --release --target x86_64-pc-windows-msvc
tools/build_visa32.sh >/dev/null
tools/build_dotnet.sh >/dev/null
stage=$(mktemp -d); trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/include" "$stage/windows" "$stage/dotnet"
cp "$REL/visa64.dll" "$REL/symbioticd.exe" "$REL/symbiotic-ctl.exe" "$REL/symbiotic-config.exe" "$stage/"
cp "$REL/visa64.dll.lib" "$stage/visa64.lib"
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
out="target/symbiotic-machine-$version.msi"
wixl -a x64 -D Version="$version" -D Src="$stage" -o "$out" windows/symbiotic.wxs
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
# The installer's pages, which wixl cannot write: nothing is installed until the user clicks Install.
python3 tools/msi_ui.py "$out"
echo "$out"
