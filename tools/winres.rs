// winres.rs -- a Windows version resource for a build script: include!("../../tools/winres.rs").
// Explorer's Details tab reads it, and Windows Installer compares file versions by it instead of
// treating every file as unversioned. The version is the MSI's: the workspace's major.minor and
// the commit count (SYMBIOTIC_BUILD overrides the count; tools/build_msi.sh uses the same rule).
// The .rc file is written with numeric constants and no #include, so llvm-rc needs no Windows SDK
// headers; the .res it makes is given to the linker as an input (link.exe and lld-link take it).

/// VS_FIXEDFILEINFO's file types.
#[allow(dead_code)]
const VFT_APP: u32 = 1;
#[allow(dead_code)]
const VFT_DLL: u32 = 2;

/// The build's version: (major, minor, build).
fn winres_version() -> (u32, u32, u32) {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut it = pkg.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    println!("cargo:rerun-if-env-changed=SYMBIOTIC_BUILD");
    let build = match std::env::var("SYMBIOTIC_BUILD") {
        Ok(b) => b.trim().parse().unwrap_or_else(|_| panic!("SYMBIOTIC_BUILD is not a number: {b:?}")),
        Err(_) => {
            let root = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
            // Every commit and checkout appends to the reflog, so a new commit rebuilds the resource.
            let log = root.join(".git/logs/HEAD");
            if log.exists() {
                println!("cargo:rerun-if-changed={}", log.display());
            }
            std::process::Command::new("git")
                .args(["rev-list", "--count", "HEAD"])
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)
        }
    };
    (major, minor, build)
}

/// Writes, compiles and links the version resource of one binary. `link_arg` is the cargo
/// directive that reaches it ("rustc-cdylib-link-arg" for the DLL, "rustc-link-arg-bin=NAME" for an
/// executable). Only for a Windows target; a missing llvm-rc is an error, as a missing clang-cl is.
#[allow(dead_code)]
fn winres_embed(link_arg: &str, stem: &str, file_name: &str, description: &str, file_type: u32) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let (major, minor, build) = winres_version();
    let version = format!("{major}.{minor}.{build}.0");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc = out.join(format!("{stem}.rc"));
    let res = out.join(format!("{stem}.res"));
    let text = format!(
        r#"1 VERSIONINFO
FILEVERSION {major},{minor},{build},0
PRODUCTVERSION {major},{minor},{build},0
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE {file_type}
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Viper488"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{stem}"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "{file_name}"
      VALUE "ProductName", "Viper488 Symbiotic VISA"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    std::fs::write(&rc, text).unwrap();
    let rc_tool = std::env::var("SYMBIOTIC_RC").unwrap_or_else(|_| "llvm-rc".into());
    let status = std::process::Command::new(&rc_tool)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg(&rc)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {rc_tool} for the version resource ({e}); apt install llvm, or set SYMBIOTIC_RC"));
    assert!(status.success(), "{rc_tool} failed on {}", rc.display());
    println!("cargo:{link_arg}={}", res.display());
}
