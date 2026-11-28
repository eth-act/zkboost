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
            "536d37fe209194841cc7cdaa5a84f6ae64b9b3b66f2e34fba9ac18ec7b6c8d57",
        ),
        "aarch64-unknown-linux-gnu" => (
            "linux-arm64",
            "5dea125f40c6af755deef9ede374fc9deb4412e738fc9120d3a64b1b66dfa9cd",
        ),
        "aarch64-apple-darwin" => (
            "darwin-arm64",
            "18daea2770a4615cdaed539a4e5395315cd2c5440063f6ba2c8744d739301383",
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
