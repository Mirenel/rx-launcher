// Release-only binaries live under src/tools because Tauri auto-discovers
// binaries under src/bin for application bundles.
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::Signer;

#[path = "../update_payload.rs"]
mod update_payload;

fn value(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("Missing argument {name}"))
}

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--generate") {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        println!("private_key_hex={}", hex_encode(&signing_key.to_bytes()));
        println!(
            "public_key_b64={}",
            BASE64.encode(signing_key.verifying_key().to_bytes())
        );
        return Ok(());
    }
    let key_hex = std::env::var("RX_UPDATE_PRIVATE_KEY_HEX")
        .map_err(|_| "RX_UPDATE_PRIVATE_KEY_HEX is not set".to_string())?;
    let key_bytes = hex_decode(&key_hex)?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "RX_UPDATE_PRIVATE_KEY_HEX must contain 32 bytes".to_string())?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_bytes);
    if args.iter().any(|arg| arg == "--print-public-key") {
        println!("{}", BASE64.encode(signing_key.verifying_key().to_bytes()));
        return Ok(());
    }
    let version = value(&args, "--version")?;
    let published_at = value(&args, "--published-at")?;
    let platform = value(&args, "--platform")?;
    let arch = value(&args, "--arch")?;
    let url = value(&args, "--url")?;
    let sha256 = value(&args, "--sha256")?.to_ascii_lowercase();
    let payload =
        update_payload::signed_payload(&version, &published_at, &platform, &arch, &url, &sha256);
    let signature = signing_key.sign(payload.as_bytes());
    println!("{}", BASE64.encode(signature.to_bytes()));
    Ok(())
}

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("Private key hex has an odd length".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| "Private key hex is invalid".to_string())
        })
        .collect()
}

fn hex_encode(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
