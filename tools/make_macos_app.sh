#!/usr/bin/env bash
# make_macos_app.sh -- wrap the daemon in a minimal Symbiotic.app, on a Mac.
#   tools/make_macos_app.sh [BINDIR] [APPDIR]       defaults: target/debug  ~/Applications
# macOS keeps a program that has no application bundle out of the local network ("No route to host", EHOSTUNREACH) when
# launchd starts it, and shows the "allow Symbiotic to find and connect to devices on your local network" question only
# for a bundle with an identity; a daemon that serves a sharing group at login must reach the registry and the other
# computers. The question is answered once, at the screen of the Mac. The bundle is ad-hoc signed: a stable identity
# for that answer, not a developer's signature.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=${1:-target/debug}
dest=${2:-$HOME/Applications}
app="$dest/Symbiotic.app"
[ -x "$bin/symbioticd" ] || { echo "$bin/symbioticd is not built (cargo build -p symbiotic-core)"; exit 1; }
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$bin/symbioticd" "$app/Contents/MacOS/symbioticd"
# the rest of the install sits beside the daemon, where the clients look for it (client.rs: the DLL's own directory)
for f in symbiotic-config symbiotic-ctl libvisa64.dylib; do
  [ -e "$bin/$f" ] && cp "$bin/$f" "$app/Contents/MacOS/$f"
done
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>com.viper488.symbiotic</string>
	<key>CFBundleName</key><string>Symbiotic</string>
	<key>CFBundleDisplayName</key><string>Viper488 Symbiotic VISA</string>
	<key>CFBundleExecutable</key><string>symbioticd</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>0.1</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSUIElement</key><true/>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Symbiotic shares this computer's instruments with, and uses the instruments of, the other computers in your group on the local network.</string>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$app"
# the configuration window: its own bundle (so it has a Dock icon and opens by double click), running the program that
# sits beside the daemon
if [ -x "$app/Contents/MacOS/symbiotic-config" ]; then
  cfg="$dest/Symbiotic Config.app"
  rm -rf "$cfg"
  mkdir -p "$cfg/Contents/MacOS"
  # LaunchServices wants a Mach-O executable, not a script
  printf '#include <unistd.h>\nint main(int c, char **v) { v[0] = "%s/Contents/MacOS/symbiotic-config"; execv(v[0], v); return 1; }\n' "$app" \
    | clang -x c - -o "$cfg/Contents/MacOS/symbiotic-config"
  cat > "$cfg/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>com.viper488.symbiotic.config</string>
	<key>CFBundleName</key><string>Symbiotic Config</string>
	<key>CFBundleDisplayName</key><string>Viper488 Symbiotic VISA</string>
	<key>CFBundleExecutable</key><string>symbiotic-config</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>0.1</string>
	<key>CFBundleVersion</key><string>1</string>
</dict>
</plist>
PLIST
  codesign --force --deep --sign - "$cfg"
  echo "$cfg"
fi
echo "$app"
