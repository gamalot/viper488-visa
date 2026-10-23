#!/usr/bin/env bash
# build_macos.sh -- the macOS package, on a Mac: Symbiotic.app, universal (arm64 + x86_64), in a .dmg.
# Needs the Command Line Tools and rustup; adds the x86_64 target if it is missing.
#   tools/build_macos.sh               -> target/symbiotic-visa-<version>-macos-universal.dmg
#   tools/build_macos.sh --app DIR     -> Symbiotic.app put in DIR (~/Applications while developing), no .dmg
#
#   Symbiotic.app/Contents/MacOS/symbiotic-explorer       the main program: Symbiotic Explorer
#                         MacOS/symbiotic-ctl
#                         Frameworks/libvisa64.dylib      the library pyvisa is given
#                         Helpers/SymbioticDaemon.app     the daemon, main program of a bundle of its own: launchd lets
#                                                         only such a program on the local network
#                         Resources/include/              the C headers
#
# Ad-hoc signed (no Developer ID yet): a stable identity within one build, not a developer's signature.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(uname)" = Darwin ] || { echo "build_macos.sh runs on a Mac"; exit 1; }
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

dest=
case "${1:-}" in
  "") ;;
  --app) dest=${2:?--app needs a directory} ;;
  *) echo "usage: $0 [--app DIR]"; exit 2 ;;
esac

mm=$(sed -n 's/^version = "\([0-9]*\.[0-9]*\)\..*/\1/p' Cargo.toml | head -1)
if [ -z "${SYMBIOTIC_BUILD:-}" ]; then
  SYMBIOTIC_BUILD=$(git rev-list --count HEAD 2>/dev/null) || { echo "not a git checkout: set SYMBIOTIC_BUILD"; exit 1; }
fi
export SYMBIOTIC_BUILD
version="$mm.$SYMBIOTIC_BUILD"
export MACOSX_DEPLOYMENT_TARGET=11.0

targets="aarch64-apple-darwin x86_64-apple-darwin"
for t in $targets; do
  rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  # one package per build: built together, the library's copy at the top of target/ is not refreshed
  cargo build --release --target "$t" -p symbiotic-core --bin symbioticd --bin symbiotic-ctl
  cargo build --release --target "$t" -p symbiotic-visa
  cargo build --release --target "$t" -p symbiotic-explorer
done

stage=target/macos
app="$stage/Symbiotic.app"
helper="$app/Contents/Helpers/SymbioticDaemon.app"
rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources/include" "$helper/Contents/MacOS"

universal() {  # universal OUT FILE: both targets' FILE in one
  lipo -create -output "$1" target/aarch64-apple-darwin/release/"$2" target/x86_64-apple-darwin/release/"$2"
  for a in arm64 x86_64; do lipo "$1" -verify_arch "$a" || { echo "$1 has no $a"; exit 1; }; done
}
universal "$app/Contents/MacOS/symbiotic-explorer" symbiotic-explorer
universal "$app/Contents/MacOS/symbiotic-ctl" symbiotic-ctl
universal "$app/Contents/Frameworks/libvisa64.dylib" libvisa64.dylib
universal "$helper/Contents/MacOS/symbioticd" symbioticd
cp include/visa.h include/visatype.h "$app/Contents/Resources/include/"

# the icon, from the window's PNG
iconset="$stage/Symbiotic.iconset"
mkdir -p "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s crates/symbiotic-explorer/icons/icon.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2)); [ $d -le 512 ] && sips -z $d $d crates/symbiotic-explorer/icons/icon.png --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Symbiotic.icns"
rm -rf "$iconset"

lan="Symbiotic talks to the instruments on your local network, and shares instruments with the other computers in your group."
plist() {  # plist FILE ID NAME EXECUTABLE EXTRA
  cat > "$1" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>$2</string>
	<key>CFBundleName</key><string>$3</string>
	<key>CFBundleDisplayName</key><string>Viper488 Symbiotic VISA</string>
	<key>CFBundleExecutable</key><string>$4</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
	<key>NSLocalNetworkUsageDescription</key><string>$lan</string>
$5
</dict>
</plist>
PLIST
}
plist "$app/Contents/Info.plist" com.viper488.symbiotic Symbiotic symbiotic-explorer \
  "	<key>CFBundleIconFile</key><string>Symbiotic</string>
	<key>NSHighResolutionCapable</key><true/>"
plist "$helper/Contents/Info.plist" com.viper488.symbiotic.daemon SymbioticDaemon symbioticd \
  "	<key>LSUIElement</key><true/>"

# inside out: the library, the second program and the helper first, then the bundle around them
codesign --force --sign - "$app/Contents/Frameworks/libvisa64.dylib"
codesign --force --sign - "$app/Contents/MacOS/symbiotic-ctl"
codesign --force --sign - "$helper"
codesign --force --sign - "$app"
codesign --verify --strict "$app"

if [ -n "$dest" ]; then
  mkdir -p "$dest"
  rm -rf "$dest/Symbiotic.app"
  ditto "$app" "$dest/Symbiotic.app"
  echo "$dest/Symbiotic.app ($version)"
  exit 0
fi

dmgdir="$stage/dmg"
mkdir -p "$dmgdir"
ditto "$app" "$dmgdir/Symbiotic.app"
ln -s /Applications "$dmgdir/Applications"
cat > "$dmgdir/READ ME FIRST.txt" <<TXT
Viper488 Symbiotic VISA Runtime $version for macOS (Apple Silicon and Intel, macOS 11 and later)

Install: drag Symbiotic.app onto Applications.

BEFORE USING IT, run this once in Terminal:

    xattr -dr com.apple.quarantine /Applications/Symbiotic.app

This preview is not signed by an Apple developer. Without that command, macOS asks at the screen before
it lets anything in the app run -- and a program loading the library (pyvisa) waits, without an error,
until that question is answered. If that has happened, answer it ("Done"), run the command, and restart
the Mac if the library still hangs.

The first time the daemon uses the local network (alias sharing), macOS asks whether "Viper488 Symbiotic
VISA" may: allow it, or sharing will not work. LAN instruments opened from your own program do not need it.

From Python:

    import pyvisa
    rm = pyvisa.ResourceManager("/Applications/Symbiotic.app/Contents/Frameworks/libvisa64.dylib")
    print(rm.visalib, rm.list_resources())

Opening Symbiotic.app shows the configuration window. The command-line tool is
/Applications/Symbiotic.app/Contents/MacOS/symbiotic-ctl; the C headers are in Contents/Resources/include.
The configuration is in ~/.config/symbiotic, logs in ~/Library/Logs/Symbiotic.

Uninstall: if you turned on starting at login (alias sharing), turn it off in the window first; then drag
Symbiotic.app to the Bin.

https://github.com/gamalot/viper488-visa
TXT
out="target/symbiotic-visa-$version-macos-universal.dmg"
rm -f "$out"
hdiutil create -quiet -volname "Viper488 Symbiotic VISA $version" -srcfolder "$dmgdir" -format UDZO "$out"
echo "$out"
