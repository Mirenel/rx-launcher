use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use rx_launcher_lib::content::ContentManifest;
use std::env;
use std::fs;
use std::path::PathBuf;

fn argument(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn private_key(args: &[String]) -> Result<SigningKey, String> {
    let value = argument(args, "--private-key-hex")
        .or_else(|| env::var("RX_CONTENT_PRIVATE_KEY_HEX").ok())
        .ok_or_else(|| "Set --private-key-hex or RX_CONTENT_PRIVATE_KEY_HEX".to_string())?;
    let bytes = decode_hex(&value)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "Content private key must contain 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn public_key(args: &[String]) -> Result<String, String> {
    argument(args, "--public-key-b64")
        .or_else(|| env::var("RX_CONTENT_PUBLIC_KEY_B64").ok())
        .ok_or_else(|| "Set --public-key-b64 or RX_CONTENT_PUBLIC_KEY_B64".to_string())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("Hex value has an odd length".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| "Hex value contains an invalid character".to_string())
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn main() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--generate") {
        let key = SigningKey::generate(&mut OsRng);
        println!("private_key_hex={}", encode_hex(&key.to_bytes()));
        println!(
            "public_key_b64={}",
            BASE64.encode(key.verifying_key().to_bytes())
        );
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--verify") {
        let manifest_path = PathBuf::from(
            argument(&args, "--manifest").ok_or_else(|| "Missing --manifest".to_string())?,
        );
        let bytes = fs::read(&manifest_path)
            .map_err(|error| format!("Could not read manifest: {error}"))?;
        let manifest: ContentManifest =
            serde_json::from_slice(&bytes).map_err(|_| "Manifest JSON is malformed".to_string())?;
        manifest.validate_with_key(&public_key(&args)?)?;
        println!("Verified {}", manifest_path.display());
        return Ok(());
    }

    let key = private_key(&args)?;
    if args.iter().any(|arg| arg == "--print-public-key") {
        println!("{}", BASE64.encode(key.verifying_key().to_bytes()));
        return Ok(());
    }

    if !args.iter().any(|arg| arg == "--sign") {
        return Err(
            "Use --generate, --verify --manifest PATH, --print-public-key, or --sign --manifest PATH"
                .into(),
        );
    }
    let manifest_path = PathBuf::from(
        argument(&args, "--manifest").ok_or_else(|| "Missing --manifest".to_string())?,
    );
    let output_path = argument(&args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_path.clone());
    let bytes =
        fs::read(&manifest_path).map_err(|error| format!("Could not read manifest: {error}"))?;
    let mut manifest: ContentManifest =
        serde_json::from_slice(&bytes).map_err(|_| "Manifest JSON is malformed".to_string())?;
    manifest.signature = BASE64.encode(key.sign(manifest.signed_payload().as_bytes()).to_bytes());
    let mut output = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "Could not serialize signed manifest".to_string())?;
    output.push(b'\n');
    fs::write(&output_path, output)
        .map_err(|error| format!("Could not write manifest: {error}"))?;
    println!("Signed {}", output_path.display());
    Ok(())
}
