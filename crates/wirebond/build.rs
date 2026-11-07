//! Embeds Wintun 0.14.1 in a Windows build (SPEC 2.0 §5.3): wintun/bin/<arch>/wintun.dll, unpacked
//! from the zip that wintun.sum pins, which .github/scripts/wintun.sh downloads and checks. A release
//! build for Windows fails without it. Any other build (check, clippy, test) goes on without it and
//! warns; opening the TUN device then fails with a message naming the script.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(wintun)");
    println!("cargo::rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "amd64",
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "arm",
    };
    let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let dll = dir.join("wintun").join("bin").join(arch).join("wintun.dll");
    if dll.is_file() {
        println!("cargo::rerun-if-changed={}", dll.display());
        println!("cargo::rustc-cfg=wintun");
        println!("cargo::rustc-env=WIREBOND_WINTUN_DLL={}", dll.display());
        return;
    }
    // Until it appears: the package's directory, whose wintun/ is where it would.
    println!("cargo::rerun-if-changed={}", dir.display());
    let why = format!(
        "{} is missing: run .github/scripts/wintun.sh, which downloads Wintun 0.14.1 from \
         https://www.wintun.net/builds/wintun-0.14.1.zip and checks it against crates/wirebond/wintun.sum",
        dll.display()
    );
    if env::var("PROFILE").as_deref() == Ok("release") {
        panic!("{why}");
    }
    println!("cargo::warning={why}; this build cannot open the TUN device");
}
