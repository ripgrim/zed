#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
#![cfg_attr(not(target_os = "macos"), allow(unused))]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(gles)");
}
