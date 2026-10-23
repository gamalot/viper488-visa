#!/usr/bin/env bash
# check.sh -- everything that can be checked without hardware, in one go.
#
#   tools/check.sh            host tests, workspace check with warnings as errors, the Windows
#                             build, and the VISA constants against pyvisa's (when pyvisa is here)
#
# None of it proves anything on hardware.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n== %s\n' "$*"; }
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

step "unit tests (symbiotic-core, symbiotic-visa, symbiotic-explorer)"
cargo test -q -p symbiotic-core
cargo test -q -p symbiotic-visa                    # the C API in-process: the I/O trace end to end
cargo test -q -p symbiotic-explorer                # Symbiotic Explorer's own logic (console escapes, labels, export)

step "workspace check, all targets, warnings are errors"
RUSTFLAGS="-D warnings" cargo check -q --workspace --all-targets

step "clippy, warnings are errors (the lints allowed for good are in Cargo.toml)"
if cargo clippy --version >/dev/null 2>&1; then
    cargo clippy -q --workspace --all-targets -- -D warnings
    # The Windows-only code too; not the examples, which are Linux and macOS tools.
    if cargo xwin --version >/dev/null 2>&1; then
        cargo xwin clippy -q --target x86_64-pc-windows-msvc --workspace --lib --bins --tests -- -D warnings
    fi
else
    echo "SKIPPED: clippy is not installed (rustup component add clippy)"
fi

step "Windows build (visa64.dll, symbioticd.exe, symbiotic-ctl.exe, symbiotic-probe.exe)"
if cargo xwin --version >/dev/null 2>&1; then
    RUSTFLAGS="-D warnings" cargo xwin build -q --release --target x86_64-pc-windows-msvc
    ls -l target/x86_64-pc-windows-msvc/release/visa64.dll
else
    echo "SKIPPED: cargo-xwin is not installed"
fi

step "formatted I/O (fmtio.c) against VPP-4.3's rules, on a fake device; again under ASan and UBSan; scanning against sscanf"
clang -std=c11 -Wall -Wextra -Werror -o "$tmp/fmtio_test" tests/fmtio_test.c crates/symbiotic-visa/src/fmtio.c -lpthread
"$tmp/fmtio_test" | tail -1
clang -std=c11 -g -O1 -fsanitize=address,undefined,float-cast-overflow -fno-sanitize-recover=all -o "$tmp/fmtio_asan" \
    tests/fmtio_test.c crates/symbiotic-visa/src/fmtio.c -lpthread
"$tmp/fmtio_asan" | tail -1
clang -std=c11 -g -O1 -fsanitize=address,undefined -fno-sanitize-recover=all -o "$tmp/fmtio_diff" \
    tests/fmtio_diff.c crates/symbiotic-visa/src/fmtio.c -lpthread
"$tmp/fmtio_diff" 200000 | tail -1                 # viSScanf against the C library's sscanf, random cases

step "C headers: current, compile as C and C++, and every declared function exported"
python3 tools/gen_visa_h.py "$tmp/include" >/dev/null && diff -u include "$tmp/include" \
    || { echo "include/ is stale: run python3 tools/gen_visa_h.py"; exit 1; }
printf '#include "visa.h"\nint main(void) { ViSession rm; return viOpenDefaultRM(&rm) != VI_SUCCESS; }\n' > "$tmp/h.c"
cp "$tmp/h.c" "$tmp/h.cpp"
for f in h.c h.cpp; do
    clang-cl --target=x86_64-pc-windows-msvc /nologo /W4 /WX /c /Iinclude /Fo"$tmp/$f.obj" "$tmp/$f"
done
if [ -f target/x86_64-pc-windows-msvc/release/visa64.dll ]; then
    python3 tools/check_exports.py target/x86_64-pc-windows-msvc/release/visa64.dll include/visa.h
    # A C program against the header and visa64.lib links (running it is on hardware: tests/c_api.c).
    tools/build_c_api.sh "$tmp/c_api.exe" >/dev/null && echo "tests/c_api.c builds and links against visa64.lib"
fi

step "Linux: libvisa64.so exports every declared function (fmtio.c's too), tests/c_api.c links against it"
cargo build -q --release -p symbiotic-visa
python3 tools/check_exports.py target/release/libvisa64.so include/visa.h
clang -std=c11 -Wall -Werror -o "$tmp/c_api" tests/c_api.c -Iinclude -Ltarget/release -lvisa64 \
    && echo "tests/c_api.c builds and links against libvisa64.so"

step "32-bit: visa32.dll and visa32.lib, every declared function exported, tests/c_api.c links against it"
if cargo xwin --version >/dev/null 2>&1; then
    RUSTFLAGS="-D warnings" tools/build_visa32.sh | tail -1
    python3 tools/check_exports.py target/i686-pc-windows-msvc/release/visa32.dll include/visa.h
    ARCH=x86 tools/build_c_api.sh "$tmp/c_api32.exe" >/dev/null && echo "tests/c_api.c builds and links against visa32.lib"
else
    echo "SKIPPED: cargo-xwin is not installed"
fi

step "the MSI (wixl, for all users), with what it must hold and never the Keysight firmware"
# The USBTMC package must lose every tie to a vendor driver: PnP takes the newer date.
grep -qxF 'DriverVer   = 01/01/2000,$Version' windows/usbtmc-winusb.ps1 \
    || { echo "windows/usbtmc-winusb.ps1: the INF's DriverVer is not 01/01/2000"; exit 1; }
if command -v wixl >/dev/null && cargo xwin --version >/dev/null 2>&1 && command -v dotnet.exe >/dev/null; then
    want="MicrosoftEdgeWebview2Setup.exe Symbiotic.Visa.deps.json Symbiotic.Visa.dll Symbiotic.Visa.dll Symbiotic.VisaCom.dll register-visa.ps1 register-visacom.ps1 register-visanet.ps1 symbiotic-ctl.exe symbiotic-explorer.exe symbiotic-quiet.exe symbioticd.exe usbtmc-winusb.ps1 visa.h visa32.dll visa32.lib visa64.dll visa64.lib visatype.h winusb-bind.ps1 winusb-unbind.ps1 "
    msi=$(tools/build_msi.sh | tail -1)
    files=$(msiinfo export "$msi" File | tail -n +4 | cut -f3 | sort | tr '\n' ' ')
    [ "$files" = "$want" ] || { echo "$msi holds: $files"; exit 1; }
    # The binaries are versioned in the MSI as in their resources (tools/winres.rs), the rest not.
    vers=$(msiinfo export "$msi" File | tail -n +4 | awk -F'\t' '$3 ~ /\.(dll|exe)$/ && $3 !~ /Webview2/ { print $5 }' | sort -u)
    want_ver="$(basename "$msi" .msi | sed 's/^symbiotic-visa-//; s/-windows-x64$//').0"
    [ "$vers" = "$want_ver" ] || { echo "$msi's binaries are versioned '$vers', not $want_ver"; exit 1; }
    # For all users, and taken back out of the system's PATH on uninstall.
    msiinfo export "$msi" Property | grep -qP '^ALLUSERS\t1' || { echo "$msi is not for all users (ALLUSERS)"; exit 1; }
    msiinfo export "$msi" Environment | tail -n +4 | cut -f2 | grep -qxF -- '=-*PATH' \
        || { echo "$msi's PATH entry is not removed on uninstall (want =-*PATH)"; exit 1; }
    # WebView2's bootstrapper (in the file list above), run only where WebView2 is missing.
    msiinfo export "$msi" InstallExecuteSequence | grep -P '^InstallWebView2\t' | grep -q 'NOT WEBVIEW2_MACHINE AND NOT WEBVIEW2_USER' \
        || { echo "$msi does not run the WebView2 bootstrapper, or runs it unconditionally"; exit 1; }
    # The registrations are there but off unless asked for (Level 1000).
    regs=$(msiinfo export "$msi" Feature | tail -n +4 | awk -F'\t' '$6 == 1000 { print $1 }' | sort | tr '\n' ' ')
    [ "$regs" = "RegisterVisa RegisterVisaCom RegisterVisaNet " ] || { echo "$msi's optional registrations: '$regs'"; exit 1; }
    # USBTMC instruments without a driver onto WinUSB: on by default (Level 1), run when its feature is installed.
    msiinfo export "$msi" Feature | tail -n +4 | awk -F'\t' '$1 == "UsbtmcWinUsb" && $6 == 1' | grep -q . \
        || { echo "$msi has no UsbtmcWinUsb feature on by default"; exit 1; }
    msiinfo export "$msi" InstallExecuteSequence | grep -P '^InstallUsbtmc\t&UsbtmcWinUsb=3\t' | grep -q . \
        || { echo "$msi does not install the USBTMC package with its feature"; exit 1; }
    # What runs the installed scripts on the way out, and stops the daemons, must come before RemoveFiles (3500) and
    # after InstallInitialize (1500).
    early=$(msiinfo export "$msi" InstallExecuteSequence | tail -n +4 \
        | awk -F'\t' '$1 ~ /^(StopDaemons|StopDaemonsQuietly|UnregisterIvi|UnregisterNet|UnregisterCom|UninstallUsbtmc)$/ && ($3 <= 1500 || $3 >= 3500) { print $1 "=" $3 }')
    [ -z "$early" ] || { echo "$msi runs these outside InstallInitialize..RemoveFiles: $early"; exit 1; }
    # The pages (tools/msi_ui.py): nothing is installed before the user clicks Install. A double-click opens
    # Welcome (or, installed, Change/Repair/Remove); the progress page is modeless, or the UI sequence waits on it
    # and never reaches ExecuteAction.
    dlgs=$(msiinfo export "$msi" Dialog | tail -n +4 | cut -f1 | sort | tr '\n' ' ')
    [ "$dlgs" = "BrowseDlg CancelDlg CustomizeDlg ErrorDlg ExitDlg FatalErrorDlg FilesInUse MaintenanceDlg ProgressDlg UserExitDlg VerifyReadyDlg WelcomeDlg " ] \
        || { echo "$msi's dialogs: $dlgs"; exit 1; }
    ui=$(msiinfo export "$msi" InstallUISequence | tail -n +4)
    for want_ui in $'WelcomeDlg\tNOT Installed' $'MaintenanceDlg\tInstalled AND NOT RESUME AND NOT Preselected' $'ProgressDlg\t' $'ExitDlg\t\t-1' $'UserExitDlg\t\t-2' $'FatalErrorDlg\t\t-3'; do
        grep -qF "$want_ui" <<<"$ui" || { echo "$msi's InstallUISequence lacks: $want_ui"; exit 1; }
    done
    msiinfo export "$msi" Dialog | awk -F'\t' '$1 == "ProgressDlg" && $6 == 1' | grep -q . || { echo "$msi's ProgressDlg is not modeless"; exit 1; }
    msiinfo export "$msi" Property | grep -qP '^ErrorDialog\tErrorDlg' || { echo "$msi has no ErrorDialog"; exit 1; }
    # The location is the user's choice, and PATH follows it.
    msiinfo export "$msi" Environment | tail -n +4 | cut -f3 | grep -qxF '[~];[INSTALLDIR]' || { echo "$msi's PATH entry is not [INSTALLDIR]"; exit 1; }
    echo "$msi: $files(binaries $want_ver; off by default: $regs)"
else
    echo "SKIPPED: wixl (apt install wixl), cargo-xwin or dotnet.exe (the .NET SDK on Windows) is not installed"
fi

step "the Linux package (.deb): what it must hold, glibc 2.34 at most, never the Keysight firmware"
if cargo zigbuild --help >/dev/null 2>&1 && [ -x "$HOME/.local/zig/zig" ] && command -v dpkg-deb >/dev/null; then
    deb=$(tools/build_deb.sh | tail -1)   # it fails by itself on a binary that needs a newer glibc
    got=$(dpkg-deb -c "$deb" | awk '{print $6}' | grep -v '/$' | sort | tr '\n' ' ')
    want="./usr/bin/symbiotic-ctl ./usr/bin/symbiotic-explorer ./usr/include/symbiotic/visa.h ./usr/include/symbiotic/visatype.h ./usr/lib/symbiotic/libvisa64.so ./usr/lib/symbiotic/symbiotic-ctl ./usr/lib/symbiotic/symbiotic-explorer ./usr/lib/symbiotic/symbioticd ./usr/lib/udev/rules.d/70-symbiotic.rules ./usr/share/applications/symbiotic-explorer.desktop ./usr/share/doc/viper488-symbiotic-visa/README.md ./usr/share/doc/viper488-symbiotic-visa/copyright ./usr/share/icons/hicolor/512x512/apps/symbiotic-explorer.png ./usr/share/viper488-symbiotic-visa/71-symbiotic-everyone.rules "
    [ "$got" = "$want" ] || { echo "$deb holds: $got"; exit 1; }
    dpkg-deb -x "$deb" "$tmp/deb"
    python3 tools/check_exports.py "$tmp/deb/usr/lib/symbiotic/libvisa64.so" include/visa.h
    echo "$deb: $(echo "$got" | wc -w) files"
else
    echo "SKIPPED: cargo-zigbuild, zig in ~/.local/zig or dpkg-deb is not installed"
fi

step "the configuration window's strings: the same keys and placeholders in every language"
python3 tools/check_locales.py

step "no Chinese in code (code, comments and log strings are English)"
if { git ls-files; git ls-files --others --exclude-standard; } | grep -E '\.(rs|py|ps1|c|h|sh|toml|inf|cs|csproj|wxs|js|html|css)$' \
    | xargs -r grep -nP '[\x{3000}-\x{303F}\x{4E00}-\x{9FFF}\x{FF00}-\x{FFEF}]'; then
    echo "Chinese in the code above: write it in English"
    exit 1
fi
echo "none"

step "dependencies against the RustSec advisories (cargo-audit): no vulnerability, and no warning unless named here"
# Named, with why: both come with Tauri's Linux webview (gtk-rs 0.18), only into symbiotic-explorer, and go when Tauri
# moves to a newer gtk-rs; our code does not use glib itself.
#   RUSTSEC-2024-0370  proc-macro-error is unmaintained (a build-time macro of glib-macros)
#   RUSTSEC-2024-0429  glib's VariantStrIter is unsound
if cargo audit --version >/dev/null 2>&1; then
    known=(--ignore RUSTSEC-2024-0370 --ignore RUSTSEC-2024-0429)
    if ! cargo audit -q --deny warnings "${known[@]}" 2>"$tmp/audit.err"; then
        if grep -q -i "fetch\|network\|resolve\|could not\|couldn't" "$tmp/audit.err" && [ -d "$HOME/.cargo/advisory-db" ]; then
            # Offline: the copy of the advisories fetched last time, said as such.
            echo "the advisory database could not be fetched; checking against the copy from the last fetch"
            cargo audit -q --no-fetch --stale --deny warnings "${known[@]}"
        else
            cat "$tmp/audit.err"; exit 1
        fi
    fi
    echo "none beyond the two named"
else
    echo "SKIPPED: cargo-audit is not installed (cargo install cargo-audit --locked)"
fi

step "VISA constants against pyvisa's; the attribute tables made from its (VISA COM's, viFindRsrc's) current"
if python3 -c "import pyvisa" 2>/dev/null; then
    python3 tests/check_consts.py | tail -1
    python3 tools/gen_visacom_attrs.py > "$tmp/AttributeTypes.cs"
    python3 tools/gen_visacom_attrs.py rust > "$tmp/attrs.rs"
    diff -q "$tmp/AttributeTypes.cs" dotnet/Symbiotic.VisaCom/AttributeTypes.cs >/dev/null \
        && diff -q "$tmp/attrs.rs" crates/symbiotic-core/src/attrs.rs >/dev/null \
        || { echo "an attribute table is stale: run tools/gen_visacom_attrs.py (its docstring says how)"; exit 1; }
    echo "the attribute tables are current"
else
    echo "SKIPPED: pyvisa is not installed"
fi

printf '\nall checks passed\n'
