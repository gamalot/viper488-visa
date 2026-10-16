#!/usr/bin/env bash
# build_dotnet.sh -- the VISA.NET and VISA COM assemblies (dotnet/), from WSL through Windows' dotnet.exe.
#   tools/build_dotnet.sh            -> target/dotnet/net462/Symbiotic.Visa.dll (VISA.NET, for the GAC)
#                                       target/dotnet/net6.0/Symbiotic.Visa.dll + .deps.json (VendorAssemblies)
#                                       target/dotnet/net462/Symbiotic.VisaCom.dll (VISA COM, for RegAsm)
# Versioned 0.1.<commit count>.0 like the DLLs and the MSI. Needs the .NET SDK on Windows (dotnet.exe
# on the PATH); the IVI package VISA.NET compiles against comes from NuGet, and VISA COM's interop
# assembly is the installed VISA shared components' (embedded, never shipped).
set -euo pipefail
cd "$(dirname "$0")/.."
command -v dotnet.exe >/dev/null || { echo "dotnet.exe (the .NET SDK on Windows) is not on the PATH"; exit 1; }
mm=$(sed -n 's/^version = "\([0-9]*\.[0-9]*\)\..*/\1/p' Cargo.toml | head -1)
version="$mm.${SYMBIOTIC_BUILD:-$(git rev-list --count HEAD)}.0"
proj=dotnet/Symbiotic.Visa
for p in "$proj/Symbiotic.Visa.csproj" dotnet/Symbiotic.VisaCom/Symbiotic.VisaCom.csproj; do
    if ! log=$(dotnet.exe build "$(wslpath -w "$p")" -c Release -nologo -v q -p:Version="$version" 2>&1); then
        echo "$log" >&2
        exit 1
    fi
done
out=target/dotnet
rm -rf "$out"
mkdir -p "$out/net462" "$out/net6.0"
cp "$proj/bin/Release/net462/Symbiotic.Visa.dll" dotnet/Symbiotic.VisaCom/bin/Release/net462/Symbiotic.VisaCom.dll "$out/net462/"
cp "$proj/bin/Release/net6.0/Symbiotic.Visa.dll" "$proj/bin/Release/net6.0/Symbiotic.Visa.deps.json" "$out/net6.0/"
# Ivi.Visa must never sit beside ours: the GRM would load a second one.
if find "$out" -iname 'Ivi.Visa*.dll' | grep -q .; then echo "refusing: an IVI interop assembly is in $out"; exit 1; fi
echo "$out ($version)"
