#!/usr/bin/env bash
# build_deb.sh -- the Linux package (.deb, amd64 or arm64: the machine's own, since the window links its
# WebKitGTK), from the workspace, linked against glibc 2.34 so
# it installs on Ubuntu 22.04 / Debian 12 / RHEL 9 era systems and later. Needs cargo-zigbuild and
# zig (~/.local/zig), dpkg-deb and fakeroot.
#
#   tools/build_deb.sh        -> target/symbiotic-visa-0.1.<commits>-linux-<arch>.deb
# (the package itself is named viper488-symbiotic-visa)
#
# Never with the Keysight 82357B firmware: it is Keysight's.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/zig:$PATH"
GLIBC=2.34
case "$(uname -m)" in
    x86_64) TARGET=x86_64-unknown-linux-gnu; ARCH=amd64 ;;
    aarch64) TARGET=aarch64-unknown-linux-gnu; ARCH=arm64 ;;   # a Raspberry Pi 3/4/5 on a 64-bit system
    *) echo "no package for $(uname -m)" >&2; exit 1 ;;
esac
VERSION="0.1.${SYMBIOTIC_BUILD:-$(git rev-list --count HEAD)}"
OUT="target/symbiotic-visa-${VERSION}-linux-${ARCH}.deb"

# Two runs: --bin would filter the other packages' targets out as well.
cargo zigbuild -q --release --target "$TARGET.$GLIBC" -p symbiotic-core --bin symbioticd --bin symbiotic-ctl
cargo zigbuild -q --release --target "$TARGET.$GLIBC" -p symbiotic-visa -p symbiotic-explorer
REL="target/$TARGET/release"

# What the binaries need of glibc: nothing past $GLIBC, or the package would not install where it says.
for f in "$REL"/{libvisa64.so,symbioticd,symbiotic-ctl,symbiotic-explorer}; do
    need=$(objdump -T "$f" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
    if [ "$(printf '%s\n%s\n' "$need" "$GLIBC" | sort -V | tail -1)" != "$GLIBC" ]; then
        echo "$f needs glibc $need, more than $GLIBC" >&2
        exit 1
    fi
done

root=$(mktemp -d); trap 'rm -rf "$root"' EXIT
chmod 0755 "$root"   # the package's /: mktemp makes it 0700
chmod 0755 "$root"
lib="$root/usr/lib/symbiotic"
install -d "$lib" "$root/usr/bin" "$root/usr/include/symbiotic" "$root/usr/lib/udev/rules.d" \
    "$root/usr/share/applications" "$root/usr/share/icons/hicolor/512x512/apps" \
    "$root/usr/share/doc/viper488-symbiotic-visa" "$root/DEBIAN"
install -m 0644 "$REL/libvisa64.so" "$lib/"
install -m 0755 "$REL/symbioticd" "$REL/symbiotic-ctl" "$REL/symbiotic-explorer" "$lib/"
strip --strip-unneeded "$lib"/*
ln -s ../lib/symbiotic/symbiotic-ctl "$root/usr/bin/symbiotic-ctl"
ln -s ../lib/symbiotic/symbiotic-explorer "$root/usr/bin/symbiotic-explorer"
install -m 0644 include/visa.h include/visatype.h "$root/usr/include/symbiotic/"
install -m 0644 linux/70-symbiotic.rules "$root/usr/lib/udev/rules.d/"
install -d "$root/usr/share/viper488-symbiotic-visa"
install -m 0644 linux/71-symbiotic-everyone.rules "$root/usr/share/viper488-symbiotic-visa/"   # postinst puts it in /etc where wanted
install -m 0644 linux/symbiotic-explorer.desktop "$root/usr/share/applications/"
install -m 0644 crates/symbiotic-explorer/icons/icon.png "$root/usr/share/icons/hicolor/512x512/apps/symbiotic-explorer.png"
install -m 0644 README.md "$root/usr/share/doc/viper488-symbiotic-visa/"
install -m 0644 LICENSE "$root/usr/share/doc/viper488-symbiotic-visa/copyright"
sed "s/@VERSION@/$VERSION/; s/@ARCH@/$ARCH/" linux/deb/control > "$root/DEBIAN/control"
echo "Installed-Size: $(du -sk --exclude=DEBIAN "$root" | cut -f1)" >> "$root/DEBIAN/control"
install -m 0755 linux/deb/postinst linux/deb/prerm linux/deb/postrm "$root/DEBIAN/"

fakeroot dpkg-deb -Zxz --build "$root" "$OUT" >/dev/null
echo "$OUT"
