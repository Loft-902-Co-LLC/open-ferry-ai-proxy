//! Not upstream's: gives the crate the target triple it is built for, so it
//! picks that target's archive from a release (`OPEN_FERRY_TARGET`).

fn main() {
    // Cargo always sets TARGET for a build script; empty means no archive
    // matches, and the updater says so.
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=OPEN_FERRY_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
