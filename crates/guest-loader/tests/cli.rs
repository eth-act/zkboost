//! Integration tests for the guest-loader CLI.

use std::{fs, io::Cursor};

use anyhow::Result;
use assert_cmd::Command;
use minisign::KeyPair;
use tempfile::tempdir;

#[test]
fn test_cli_verify_and_save() -> Result<()> {
    let temp_dir = tempdir()?;
    let program_path = temp_dir.path().join("program.elf");
    let signature_path = temp_dir.path().join("program.sig");
    let pk_path = temp_dir.path().join("minisign.pub");
    let output_path = temp_dir.path().join("verified.elf");

    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let pk_str = keypair.pk.to_base64();
    let program_data = b"cli test program data".to_vec();

    let reader = Cursor::new(program_data.clone());
    let signature_box = minisign::sign(None, &keypair.sk, reader, None, None).unwrap();
    let sig_str = signature_box.to_string();

    fs::write(&program_path, &program_data)?;
    fs::write(&signature_path, &sig_str)?;
    fs::write(&pk_path, &pk_str)?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_guest-loader"));
    cmd.arg("--program")
        .arg(program_path.to_str().unwrap())
        .arg("--signature")
        .arg(signature_path.to_str().unwrap())
        .arg("--public-key")
        .arg(pk_path.to_str().unwrap())
        .arg("--output")
        .arg(output_path.to_str().unwrap());

    cmd.assert().success();

    let verified_data = fs::read(&output_path)?;
    assert_eq!(verified_data, program_data);

    Ok(())
}
