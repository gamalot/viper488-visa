//! The Windows version resource of each executable (tools/winres.rs).

include!("../../tools/winres.rs");

fn main() {
    println!("cargo:rerun-if-changed=../../tools/winres.rs");
    // The build's version on every platform, as the Windows resource and the MSI have it: what the daemon says it is
    // and how a client tells an older daemon from a newer one (ipc::BUILD).
    let (major, minor, build) = winres_version();
    println!("cargo:rustc-env=SYMBIOTIC_BUILD_VERSION={major}.{minor}.{build}");
    for (bin, description) in [
        ("symbioticd", "Viper488 Symbiotic VISA daemon"),
        ("symbiotic-ctl", "Viper488 Symbiotic VISA control tool"),
        ("symbiotic-probe", "Viper488 adapter probe"),
        ("symbiotic-quiet", "Viper488 Symbiotic VISA installer helper"),
    ] {
        winres_embed(&format!("rustc-link-arg-bin={bin}"), bin, &format!("{bin}.exe"), description, VFT_APP);
    }
}
