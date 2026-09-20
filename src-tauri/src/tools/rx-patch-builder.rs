// Release-only binaries live under src/tools because Tauri auto-discovers
// binaries under src/bin for application bundles.
use rx_launcher_lib::exe_patch::{apply_patch_bytes, build_patch, parse_patch};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;

fn argument(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("Missing {name}"))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn main() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    let source_path = PathBuf::from(argument(&args, "--source")?);
    let target_path = PathBuf::from(argument(&args, "--target")?);
    let output_path = PathBuf::from(argument(&args, "--output")?);
    let source =
        fs::read(&source_path).map_err(|error| format!("Could not read source: {error}"))?;
    let target =
        fs::read(&target_path).map_err(|error| format!("Could not read target: {error}"))?;
    let patch = build_patch(&source, &target)?;
    let reconstructed = apply_patch_bytes(&source, &patch)?;
    if reconstructed != target {
        return Err("Generated executable patch does not reconstruct the target".into());
    }
    let hunk_count = parse_patch(&patch, source.len() as u64)?.len();
    fs::write(&output_path, &patch).map_err(|error| format!("Could not write patch: {error}"))?;

    println!("source={}", source_path.display());
    println!("target={}", target_path.display());
    println!("source_size={}", source.len());
    println!("source_sha256={}", sha256(&source));
    println!("output_size={}", target.len());
    println!("output_sha256={}", sha256(&target));
    println!("patch_size={}", patch.len());
    println!("patch_sha256={}", sha256(&patch));
    println!("hunks={hunk_count}");
    println!("patch={}", output_path.display());
    Ok(())
}
