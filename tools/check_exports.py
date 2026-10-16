"""check_exports.py -- every function include/visa.h declares is exported by visa64.dll (or the Linux
libvisa64.so), and nothing else starting with `vi` is.
    python tools/check_exports.py path/to/visa64.dll|libvisa64.so [include/visa.h]
Reads the DLL's export table with llvm-readobj, the shared object's defined dynamic symbols with nm. A declared function that is missing would make a C
program that calls it fail to link against visa64.lib; one exported but not declared is a name
the header does not tell anybody about.
"""
import re, subprocess, sys

dll = sys.argv[1]
header = sys.argv[2] if len(sys.argv) > 2 else "include/visa.h"
declared = set(re.findall(r"^\s*(?:ViStatus|void)\s+_VI_FUNCC?\s+(vi\w+)\s*\(", open(header).read(), re.M))
if dll.lower().endswith(".dll"):
    out = subprocess.run(["llvm-readobj", "--coff-exports", dll], capture_output=True, text=True, check=True).stdout
    exported = set(re.findall(r"Name: (vi\w+)", out))
else:
    out = subprocess.run(["nm", "-D", "--defined-only", dll], capture_output=True, text=True, check=True).stdout
    exported = set(re.findall(r"^\S+ T (vi\w+)$", out, re.M))
missing, extra = sorted(declared - exported), sorted(exported - declared)
print("%d declared, %d exported" % (len(declared), len(exported)))
for n in missing:
    print("  declared but not exported:", n)
for n in extra:
    print("  exported but not declared:", n)
sys.exit(1 if missing or extra else 0)
