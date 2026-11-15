#!/usr/bin/env bash
# build_arm64x.sh -- visa64.dll for Windows on Arm as ONE ARM64X file, from WSL: native ARM64 code for ARM64 programs and
# ARM64EC code for programs emulated as x64, so either loads the same visa64.dll by name, and an x64 program's VISA calls
# run natively.
#   tools/build_arm64x.sh [quick|release]   -> target/arm64x/<profile>/visa64.dll, visa64.lib (the import library)
#
# Cargo makes no ARM64X file, so the crate is built as a static library twice (aarch64-pc-windows-msvc and
# arm64ec-pc-windows-msvc, in target/arm64x so as not to rebuild the usual cdylib each time) and rust-lld links the two
# with /machine:arm64x, exporting what the ARM64 cdylib exports (it is built first if it is not there) in both views.
# What that needs:
#   * llvm-lib takes the ARM64EC objects of fmtio.c only with /machine:arm64ec (ARFLAGS);
#   * the ARM64EC half needs the Universal CRT's import library with its ARM64EC symbols. The Windows SDK's
#     ucrt\arm64\ucrt.lib has them (it is a hybrid library), but the copy cargo-xwin keeps has lost them, so the SDK's
#     is fetched once, the way cargo-xwin fetches the SDK (Microsoft's Visual Studio manifest, the Windows 11 SDK's
#     "Universal CRT Headers Libraries and Sources" MSI and its cabinets), into target/arm64x/sdk. It is only linked
#     against, never shipped;
#   * the windows crates' import libraries come in both, the aarch64 one for the native half and the x86_64 one for the
#     EC half (an ARM64EC link takes x64 import libraries).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
profile=${1:-release}
case "$profile" in quick|release) ;; *) echo "usage: $0 [quick|release]"; exit 2 ;; esac
out=target/arm64x/$profile
sdk=target/arm64x/sdk
mkdir -p "$out" "$sdk"
X=${XWIN_CACHE:-$HOME/.cache/cargo-xwin/xwin}
export XWIN_ARCH=x86,x86_64,aarch64
export ARFLAGS_arm64ec_pc_windows_msvc=/machine:arm64ec

# The SDK's own ucrt.lib, with its ARM64EC symbols.
if [ ! -s "$sdk/ucrt.lib" ]; then
    work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
    python3 - "$work" <<'EOF'
import json, sys, urllib.request
work = sys.argv[1]
get = lambda url: urllib.request.urlopen(url).read()
channel = json.loads(get("https://aka.ms/vs/17/release/channel"))
manifest = next(i for i in channel["channelItems"] if i["type"] == "Manifest")["payloads"][0]["url"]
pkgs = json.loads(get(manifest))["packages"]
sdk = next(p for p in pkgs if p["id"] == "Win11SDK_10.0.26100" and p.get("payloads"))
files = {f["fileName"].replace("\\", "/").split("/")[-1]: f["url"] for f in sdk["payloads"]}
msi = "Universal CRT Headers Libraries and Sources-x86_en-us.msi"
open(f"{work}/ucrt.msi", "wb").write(get(files[msi]))
open(f"{work}/files.json", "w").write(json.dumps(files))
EOF
    for cab in $(msiinfo export "$work/ucrt.msi" Media | tail -n +4 | cut -f4 | tr -d '\r' | grep .); do
        python3 -c 'import json,sys,urllib.request; f=json.load(open(sys.argv[1])); open(sys.argv[2],"wb").write(urllib.request.urlopen(f[sys.argv[3]]).read())' \
            "$work/files.json" "$work/$cab" "$cab"
    done
    (cd "$work" && msiextract -C x ucrt.msi >/dev/null)
    lib=$(find "$work/x" -ipath '*/Lib/*/ucrt/arm64/ucrt.lib' | head -1)
    [ -n "$lib" ] || { echo "no ucrt/arm64/ucrt.lib in the SDK's UCRT MSI"; exit 1; }
    cp "$lib" "$sdk/ucrt.lib"
fi
# (Counted, not grep -q: under pipefail llvm-nm's SIGPIPE would fail the pipe once grep has stopped reading.)
[ "$(llvm-nm "$sdk/ucrt.lib" 2>/dev/null | grep -c ' #malloc$')" -gt 0 ] \
    || { echo "$sdk/ucrt.lib has no ARM64EC symbols (#malloc): not the SDK's hybrid library"; exit 1; }

# What visa64.dll exports: the ARM64 cdylib's list (Rust's exports and fmtio.c's /EXPORTs alike).
cdylib=target/aarch64-pc-windows-msvc/$profile/visa64.dll
[ -f "$cdylib" ] || cargo xwin build -q --profile "$profile" --target aarch64-pc-windows-msvc -p symbiotic-visa
{ echo 'LIBRARY visa64.dll'; echo 'EXPORTS'; llvm-readobj --coff-exports "$cdylib" | awk '$1 == "Name:" { print "    " $2 }' | sort; } > "$out/visa64.def"
n=$(grep -c '^    ' "$out/visa64.def")

for t in aarch64 arm64ec; do
    RUSTFLAGS="-L $sdk -L $X/sdk/lib/um/aarch64 -L $X/crt/lib/aarch64" \
        cargo xwin rustc -q --profile "$profile" --target $t-pc-windows-msvc -p symbiotic-visa --lib --crate-type staticlib --target-dir target/arm64x
done

# The windows crates' import libraries each half uses: the aarch64 one in the native build's dependencies, the x86_64 one in
# the ARM64EC build's (windows-targets picks it for arm64ec), and only those.
winlibs=$(for t in aarch64 arm64ec; do cargo tree -q -p symbiotic-visa --target $t-pc-windows-msvc -e normal --prefix none; done \
    | grep -o '^windows_\(aarch64\|x86_64\)_msvc v[0-9.]*' | sort -u | while read -r name ver; do
        cargo metadata -q --format-version 1 | python3 -c '
import json, os, sys
name, ver = sys.argv[1], sys.argv[2].lstrip("v")
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == name and p["version"] == ver:
        d = os.path.join(os.path.dirname(p["manifest_path"]), "lib")
        print(" ".join(os.path.join(d, f) for f in sorted(os.listdir(d)) if f.endswith(".lib")))' "$name" "$ver"
    done)
[ -n "$winlibs" ] || { echo "no windows_*_msvc import library found for either half"; exit 1; }
# The version resource build.rs made for this build: the newest, under either of Cargo's layouts of the build directory
# (build/symbiotic-visa-<hash>/out until Rust 1.99, build/symbiotic-visa/<hash>/out since 1.100).
res=$(find target/arm64x/aarch64-pc-windows-msvc/$profile/build -path '*symbiotic-visa*/out/visa64.res' -printf '%T@ %p\n' \
    | sort -rn | head -1 | cut -d' ' -f2-)
[ -n "$res" ] || { echo "no visa64.res under target/arm64x/aarch64-pc-windows-msvc/$profile/build"; exit 1; }
# LLD 23 or newer (the rust-lld of Rust 1.99 and later): with LLD 22 the DLL's ARM64EC half fails in x64 programs
# ("cannot access a Thread Local Storage value during or after destruction").
lld="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"
lldv=$("$lld" -flavor link --version 2>/dev/null | sed -n 's/^LLD \([0-9]*\)\..*/\1/p')
[ "${lldv:-0}" -ge 23 ] || { echo "ARM64X needs LLD 23 or newer, the rust-lld of Rust 1.99+ (this toolchain's: LLD ${lldv:-?}): rustup update stable"; exit 1; }
"$lld" -flavor link /nologo /dll /machine:arm64x /out:"$out/visa64.dll" /implib:"$out/visa64.lib" \
    /def:"$out/visa64.def" /defarm64native:"$out/visa64.def" \
    target/arm64x/aarch64-pc-windows-msvc/$profile/visa64.lib target/arm64x/arm64ec-pc-windows-msvc/$profile/visa64.lib \
    /libpath:"$sdk" /libpath:"$X/sdk/lib/um/aarch64" /libpath:"$X/crt/lib/aarch64" \
    kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib msvcrt.lib vcruntime.lib ucrt.lib softintrin.lib \
    $winlibs "$res"

# One file, ARM64X, the same exports in both views.
[ "$(llvm-readobj --file-headers "$out/visa64.dll" | grep -c 'IMAGE_FILE_MACHINE_ARM64X')" -gt 0 ] || { echo "$out/visa64.dll is not ARM64X"; exit 1; }
got=$(llvm-readobj --coff-exports "$out/visa64.dll" | grep -c 'Name:')
[ "$got" = $((2 * n)) ] || { echo "$out/visa64.dll exports $got names, not $n in each view"; exit 1; }
echo "$out/visa64.dll ($n exports in each view)"
