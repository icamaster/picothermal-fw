//! Puts `memory.x` on the linker search path and injects the linker scripts,
//! so builds need no RUSTFLAGS and work the same from any directory.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo::rustc-link-search={}", out.display());
    println!("cargo::rerun-if-changed=memory.x");

    println!("cargo::rustc-link-arg-bins=--nmagic");
    println!("cargo::rustc-link-arg-bins=-Tlink.x");
    println!("cargo::rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo::rustc-link-arg-bins=-Tdefmt.x");
}
