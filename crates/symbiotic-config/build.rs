//! Tauri's build step (the page in ui/, the capabilities, and on Windows the resource with the icon
//! and version). The version is the MSI's, by tools/winres.rs's rule (0.1.<commit count>): Tauri
//! writes the Windows version resource itself, so it is given the number rather than a second
//! resource, which would clash with its own.

include!("../../tools/winres.rs");

fn main() {
    println!("cargo:rerun-if-changed=../../tools/winres.rs");
    let (major, minor, build) = winres_version();
    // A patch over tauri.conf.json, read by tauri-build here (the Windows resource) and by
    // generate_context! when the crate is compiled (what the program says of itself).
    let patch = format!(r#"{{"version":"{major}.{minor}.{build}"}}"#);
    println!("cargo:rustc-env=TAURI_CONFIG={patch}");
    std::env::set_var("TAURI_CONFIG", patch);
    tauri_build::build()
}
