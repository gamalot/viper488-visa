#!/usr/bin/env bash
# build_c_api.sh -- build tests/c_api.exe from WSL: include/visa.h, linked against the visa64.lib
# the Windows build made, with the Windows SDK and CRT that cargo-xwin downloaded. Nothing else
# (no MSVC, no Windows machine) is needed to build it; running it is on Windows.
#   tools/build_c_api.sh [OUT.exe]      (default: target/x86_64-pc-windows-msvc/release/c_api.exe)
#   ARCH=x86 tools/build_c_api.sh       32-bit, against visa32.lib (tools/build_visa32.sh first)
#   ARCH=arm64 tools/build_c_api.sh     Windows on Arm, against the aarch64-pc-windows-msvc build's visa64.lib
# (SYMBIOTIC_PROFILE=quick: against the [profile.quick] build instead of the release one.)
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "${ARCH:-x64}" = x86 ]; then
    TRIPLE=i686-pc-windows-msvc; LIBARCH=x86; LIB=visa32.lib; MACHINE=/machine:x86
elif [ "${ARCH:-x64}" = arm64 ]; then
    TRIPLE=aarch64-pc-windows-msvc; LIBARCH=aarch64; LIB=visa64.lib; MACHINE=/machine:arm64
else
    TRIPLE=x86_64-pc-windows-msvc; LIBARCH=x86_64; LIB=visa64.lib; MACHINE=/machine:x64
fi
REL=target/$TRIPLE/${SYMBIOTIC_PROFILE:-release}
OUT=${1:-$REL/c_api.exe}
X=${XWIN_CACHE:-$HOME/.cache/cargo-xwin/xwin}
LLD=$(ls -d "$HOME"/.rustup/toolchains/*/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld | head -1)
if [ "$LIB" = visa64.lib ]; then
    [ -f "$REL/visa64.dll.lib" ] || { echo "no $REL/visa64.dll.lib: run the Windows build first"; exit 1; }
    cp "$REL/visa64.dll.lib" "$REL/visa64.lib"
fi
[ -f "$REL/$LIB" ] || { echo "no $REL/$LIB"; exit 1; }
obj=$(mktemp -d); trap 'rm -rf "$obj"' EXIT
clang-cl --target=$TRIPLE /nologo /W4 /WX /O2 /MD /c /Iinclude /D_CRT_SECURE_NO_WARNINGS \
    -imsvc "$X/crt/include" -imsvc "$X/sdk/include/ucrt" -imsvc "$X/sdk/include/um" -imsvc "$X/sdk/include/shared" \
    /Fo"$obj/c_api.obj" tests/c_api.c
"$LLD" -flavor link /nologo $MACHINE /out:"$OUT" "$obj/c_api.obj" "$REL/$LIB" \
    /libpath:"$X/crt/lib/$LIBARCH" /libpath:"$X/sdk/lib/um/$LIBARCH" /libpath:"$X/sdk/lib/ucrt/$LIBARCH" \
    msvcrt.lib ucrt.lib vcruntime.lib kernel32.lib legacy_stdio_definitions.lib
echo "$OUT"
