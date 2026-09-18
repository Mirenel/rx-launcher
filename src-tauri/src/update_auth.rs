use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::update_payload::signed_payload;

pub const UPDATE_PLATFORM: &str = "windows";
pub const UPDATE_ARCH: &str = "x86_64";

// The checked-in public key is safe to distribute with every launcher. The
// environment override is useful for deliberate key rotation and tests.
#[cfg(windows)]
pub const UPDATE_PUBLIC_KEY_B64: &str = match option_env!("RX_UPDATE_PUBLIC_KEY_B64") {
    Some(value) if !value.is_empty() => value,
    _ => include_str!("../public-keys/update-public-key.b64"),
};

pub fn ensure_newer(remote: &semver::Version, current: &semver::Version) -> Result<(), String> {
    if remote <= current {
        Err("The launcher update is not newer than the installed version".into())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub version: String,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    pub platform: String,
    pub arch: String,
    pub url: String,
    pub sha256: String,
    pub signature: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub notes: Option<String>,
}

impl UpdateManifest {
    pub fn signed_payload(&self) -> String {
        signed_payload(
            &self.version,
            &self.published_at,
            &self.platform,
            &self.arch,
            &self.url,
            &self.sha256,
        )
    }

    #[cfg(windows)]
    pub fn validate(&self) -> Result<semver::Version, String> {
        self.validate_with_key(UPDATE_PUBLIC_KEY_B64)
    }

    fn validate_with_key(&self, public_key_b64: &str) -> Result<semver::Version, String> {
        let version = semver::Version::parse(&self.version)
            .map_err(|_| "Launcher update manifest has an invalid version".to_string())?;
        OffsetDateTime::parse(&self.published_at, &Rfc3339)
            .map_err(|_| "Launcher update manifest has invalid publication metadata".to_string())?;
        if self.platform != UPDATE_PLATFORM || self.arch != UPDATE_ARCH {
            return Err("Launcher update is for an unsupported platform or architecture".into());
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Launcher update manifest has an invalid SHA-256 hash".into());
        }
        validate_artifact_url(&self.url, &version)?;

        let signature = BASE64
            .decode(&self.signature)
            .map_err(|_| "Launcher update signature is not valid base64".to_string())?;
        let signature = Signature::from_slice(&signature)
            .map_err(|_| "Launcher update signature has an invalid length".to_string())?;
        let public_key = BASE64
            .decode(public_key_b64.trim())
            .map_err(|_| "Embedded update public key is invalid".to_string())?;
        let public_key: [u8; 32] = public_key
            .try_into()
            .map_err(|_| "Embedded update public key has an invalid length".to_string())?;
        let public_key = VerifyingKey::from_bytes(&public_key)
            .map_err(|_| "Embedded update public key is invalid".to_string())?;
        public_key
            .verify(self.signed_payload().as_bytes(), &signature)
            .map_err(|_| "Launcher update signature verification failed".to_string())?;
        Ok(version)
    }
}

fn validate_artifact_url(value: &str, version: &semver::Version) -> Result<(), String> {
    let url = url::Url::parse(value)
        .map_err(|_| "Launcher update manifest has an invalid URL".to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Launcher update URL is not an allowed HTTPS release URL".into());
    }
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let expected_tag_lower = format!("v{version}");
    let expected_tag_upper = format!("V{version}");
    if segments.len() != 6
        || segments[0] != "Mirenel"
        || segments[1] != "rx-launcher"
        || segments[2] != "releases"
        || segments[3] != "download"
        || (segments[4] != expected_tag_lower && segments[4] != expected_tag_upper)
        || segments[5].is_empty()
        || !segments[5].to_ascii_lowercase().ends_with(".exe")
    {
        return Err("Launcher update URL is not an immutable Project Rx release artifact".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn manifest() -> (UpdateManifest, ed25519_dalek::SigningKey) {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c,
            0xae, 0x7f, 0x60, 0x0b,
        ]);
        let mut manifest = UpdateManifest {
            version: "1.0.1".into(),
            published_at: "2026-09-05T00:00:00Z".into(),
            platform: UPDATE_PLATFORM.into(),
            arch: UPDATE_ARCH.into(),
            url: "https://github.com/Mirenel/rx-launcher/releases/download/v1.0.1/Project.Rx.Launcher_1.0.1_x64.exe".into(),
            sha256: "00".repeat(32),
            signature: String::new(),
            notes: None,
        };
        manifest.signature = BASE64.encode(
            signing_key
                .sign(manifest.signed_payload().as_bytes())
                .to_bytes(),
        );
        (manifest, signing_key)
    }

    fn public_key(signing_key: &ed25519_dalek::SigningKey) -> String {
        BASE64.encode(signing_key.verifying_key().to_bytes())
    }

    #[test]
    fn accepts_a_valid_signed_manifest() {
        let (manifest, key) = manifest();
        assert_eq!(
            manifest.validate_with_key(&public_key(&key)).unwrap(),
            semver::Version::parse("1.0.1").unwrap()
        );
    }

    #[test]
    fn rejects_tampering_and_invalid_metadata() {
        let (valid, key) = manifest();
        let key = public_key(&key);

        let mut tampered = valid.clone();
        tampered.sha256 = "11".repeat(32);
        assert!(tampered.validate_with_key(&key).is_err());

        let mut invalid_signature = valid.clone();
        invalid_signature.signature = BASE64.encode([0u8; 64]);
        assert!(invalid_signature.validate_with_key(&key).is_err());

        let mut wrong_arch = valid.clone();
        wrong_arch.arch = "aarch64".into();
        assert!(wrong_arch.validate_with_key(&key).is_err());

        let mut mutable_url = valid;
        mutable_url.url =
            "https://github.com/Mirenel/rx-launcher/releases/latest/download/rx-launcher.exe"
                .into();
        assert!(mutable_url.validate_with_key(&key).is_err());
    }

    #[test]
    fn rejects_malformed_manifests_and_non_newer_versions() {
        assert!(serde_json::from_str::<UpdateManifest>(r#"{"version":"1.0.1"}"#).is_err());
        assert!(serde_json::from_str::<UpdateManifest>(
            r#"{"version":"1.0.1","publishedAt":"2026-09-05T00:00:00Z","platform":"windows","arch":"x86_64","url":"https://example.com/a.exe","sha256":"00","signature":"AA==","unexpected":true}"#
        )
        .is_err());

        let current = semver::Version::parse("1.0.1").unwrap();
        assert!(ensure_newer(&semver::Version::parse("1.0.1").unwrap(), &current).is_err());
        assert!(ensure_newer(&semver::Version::parse("1.0.0").unwrap(), &current).is_err());
        assert!(ensure_newer(&semver::Version::parse("1.0.2").unwrap(), &current).is_ok());
    }
}
