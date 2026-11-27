//! Fetches the prebuilt ere verifier library of the ere release that the workspace manifest pins,
//! and adds it to the link.

use std::{env, fs, process::Command};

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    let manifest = fs::read_to_string("../../Cargo.toml").unwrap();
    let tag = (manifest.lines())
        .find(|line| line.starts_with("ere-verifier-rust"))
        .and_then(|line| line.split('"').nth(3))
        .expect("the workspace manifest pins ere-verifier-rust to a tag");
    let (platform, sha256) = match env::var("TARGET").unwrap().as_str() {
        "x86_64-unknown-linux-gnu" => (
            "linux-amd64",
            "c51b01524c97d08bea7d2e4917adf643225b6d201b2d1e6b1b662a7c6355c7f5",
        ),
        "aarch64-unknown-linux-gnu" => (
            "linux-arm64",
            "aff72b8f477314a6c189d417407bc70670536f1f42c3c807d3d5c01c62b64806",
        ),
        "aarch64-apple-darwin" => (
            "darwin-arm64",
            "66f0b7a00c5b1439976058d5b6ecf28645db989635222e492ba10960da788b08",
        ),
        target => panic!("ere has no prebuilt verifier for {target}"),
    };
    let out_dir = env::var("OUT_DIR").unwrap();
    let name = format!("libere_verifier_c.{platform}.tar.gz");
    let archive = format!("{out_dir}/{name}");
    let url = format!("https://github.com/eth-act/ere/releases/download/{tag}/{name}");
    let fetched =
        || fs::read(&archive).is_ok_and(|bytes| format!("{:x}", Sha256::digest(bytes)) == sha256);
    if !fetched() {
        run(Command::new("curl").args(["-fsSL", "--retry", "3", "-o", &archive, &url]));
    }
    assert!(fetched(), "{url} does not have the SHA-256 {sha256}");
    run(Command::new("tar").args(["-xzf", &archive, "-C", &out_dir, "libere_verifier_c.a"]));
    println!("cargo:rustc-link-search=native={out_dir}");
}

fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(status.success(), "{command:?}: {status}");
}
