use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=RX_UPDATE_PUBLIC_KEY_B64");
    println!("cargo:rerun-if-env-changed=RX_CONTENT_PUBLIC_KEY_B64");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let helper_output = out_dir.join("rx-updater.exe");
    let helper_hash_output = out_dir.join("embedded_updater.rs");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let helper_source = PathBuf::from("resources/rx-updater.exe");

    if target_os == "windows" {
        println!("cargo:rerun-if-changed={}", helper_source.display());
    }

    // The self-replacing helper is a Windows-only release component. Linux
    // builds still generate the included files so ordinary source checkouts
    // do not need a private, generated PE binary just to compile.
    let helper_bytes = if target_os == "windows" {
        fs::read(&helper_source).unwrap_or_default()
    } else {
        Vec::new()
    };
    fs::write(&helper_output, &helper_bytes).expect("write embedded updater staging file");

    let hash = Sha256::digest(&helper_bytes);
    let hash = hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(
        helper_hash_output,
        format!("pub const EMBEDDED_UPDATER_SHA256: &str = \"{hash}\";\n"),
    )
    .expect("write embedded updater metadata");

    tauri_build::build()
}
