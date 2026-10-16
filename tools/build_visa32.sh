#!/usr/bin/env bash
# build_visa32.sh -- the 32-bit VISA library, visa32.dll, and its import library visa32.lib, from WSL.
#   tools/build_visa32.sh          -> target/i686-pc-windows-msvc/release/visa32.dll, visa32.lib
#
# The crate is the 64-bit one built for i686 (lib.rs picks ViAttrState's and ViBusAddress's width
# by pointer width; fmtio.c marks its non-variadic functions __stdcall, as VPP-4.3's _VI_FUNC is on
# 32-bit Windows). rustc names the output visa64.dll; it is copied to visa32.dll, the name 32-bit
# programs and the IVI router's 32-bit view look for. The daemon stays 64-bit: visa32.dll starts
# the symbioticd.exe beside it and talks to it over the same local pipe.
#
# The import library: a 32-bit C program calling a __stdcall function refers to it decorated
# (_viOpen@20), so the library has to map each decorated name to the DLL's undecorated export.
# The decorations are the compiler's own: a file that takes the address of every function
# include/visa.h declares is compiled for i686, its unresolved symbols are the names, and
# llvm-dlltool --kill-at makes the library from them.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
REL=target/i686-pc-windows-msvc/release
XWIN_ARCH=x86,x86_64,aarch64 RUSTFLAGS="${RUSTFLAGS:-}" cargo xwin build -q --release --target i686-pc-windows-msvc -p symbiotic-visa
cp "$REL/visa64.dll" "$REL/visa32.dll"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
{
    echo '#include "visa.h"'
    echo 'void *symbiotic_every_function[] = {'
    grep -oP '^\s*(?:ViStatus|void)\s+_VI_FUNCC?\s+\Kvi\w+(?=\s*\()' include/visa.h | sed 's/^ *//; s/.*/    (void *)&,/'
    echo '};'
} > "$work/refs.c"
clang-cl --target=i686-pc-windows-msvc /nologo /c /Iinclude /Fo"$work/refs.obj" "$work/refs.c"
{
    echo 'LIBRARY visa32.dll'
    echo 'EXPORTS'
    llvm-nm --undefined-only "$work/refs.obj" | awk '{print $2}' | grep -E '^_vi' | sed 's/^_//' | sort
} > "$work/visa32.def"
n=$(grep -c '^vi' "$work/visa32.def")
llvm-dlltool -m i386 -D visa32.dll -d "$work/visa32.def" -l "$REL/visa32.lib" -k
echo "$REL/visa32.dll, $REL/visa32.lib ($n functions)"
