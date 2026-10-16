//! The variadic half of the VISA API (viPrintf and friends, src/fmtio.c) is C: stable Rust can
//! neither define a variadic function nor take a va_list. It is compiled into the DLL here, with
//! the formatted I/O buffers that viBufWrite and viBufRead share with it. The DLL's Windows
//! version resource is made here too (tools/winres.rs).

include!("../../tools/winres.rs");

/// What fmtio.c defines for the DLL to export. An object pulled in from a static library is not
/// exported by itself, so each is named to the linker.
const EXPORTS: &[&str] = &[
    "viPrintf", "viVPrintf", "viSPrintf", "viVSPrintf", "viScanf", "viVScanf", "viSScanf", "viVSScanf",
    "viQueryf", "viVQueryf", "viBufWrite", "viBufRead",
];

fn main() {
    println!("cargo:rerun-if-changed=src/fmtio.c");
    println!("cargo:rerun-if-changed=../../tools/winres.rs");
    // The same crate is visa64.dll and, built for i686, visa32.dll (tools/build_visa32.sh).
    let bits = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    let name = if bits == "32" { "visa32" } else { "visa64" };
    winres_embed("rustc-cdylib-link-arg", name, &format!("{name}.dll"),
                 &format!("Viper488 Symbiotic VISA library ({bits}-bit)"), VFT_DLL);
    cc::Build::new()
        .file("src/fmtio.c")
        .warnings(true)
        .extra_warnings(true)
        // viSPrintf takes a buffer with no size -- that is VISA's signature, not a choice -- so it
        // has to be vsprintf, which the MSVC runtime headers mark deprecated.
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .compile("fmtio");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for name in EXPORTS {
            println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}");
        }
    } else if os == "macos" {
        for name in EXPORTS {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_{name}");
        }
        // Its install name, which a program linked against it records and loads by (otherwise the build tree's path).
        // The program names where to look with -rpath.
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libvisa64.dylib");
    } else {
        // ELF: rustc's version script lists only Rust's own exports and makes the rest local, so
        // the C functions need a script of their own; -u pulls their object out of the archive.
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let script = out.join("fmtio.ver");
        let names: String = EXPORTS.iter().map(|n| format!("{n}; ")).collect();
        std::fs::write(&script, format!("{{ global: {names}}};\n")).unwrap();
        for name in EXPORTS {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-u,{name}");
        }
        println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={}", script.display());
        // Two anonymous version scripts are more than GNU ld takes; lld merges them. rustc links
        // x86_64 Linux with its own lld already; elsewhere (aarch64) the system cc would use GNU ld,
        // so it is given rustc's lld too -- unless the build names a linker of its own
        // (cargo-zigbuild's zig is lld).
        let target = std::env::var("TARGET").unwrap();
        let named = format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace('-', "_"));
        if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") && std::env::var_os(&named).is_none() {
            let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
            let sysroot = std::process::Command::new(rustc).args(["--print", "sysroot"]).output().unwrap();
            let gcc_ld = std::path::PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim())
                .join("lib/rustlib").join(std::env::var("HOST").unwrap()).join("bin/gcc-ld");
            if gcc_ld.join("ld.lld").exists() {
                println!("cargo:rustc-cdylib-link-arg=-fuse-ld=lld");
                println!("cargo:rustc-cdylib-link-arg=-B{}", gcc_ld.display());
            }
        }
    }
}
