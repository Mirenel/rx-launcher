use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

pub const CONTENT_RELEASE_OWNER: &str = "Mirenel";
pub const CONTENT_RELEASE_REPOSITORY: &str = "rx-launcher-content";
pub const CONTENT_MANIFEST_URL: &str =
    "https://github.com/Mirenel/rx-launcher-content/releases/latest/download/content-manifest.json";
pub const CONTENT_MANIFEST_FALLBACK_URL: &str = "https://projectrx.net/launcher/manifest.json";
pub const CONTENT_MANIFEST_MAX_BYTES: usize = 512 * 1024;
pub const CONTENT_MAX_FILES: usize = 512;
pub const CONTENT_MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const CONTENT_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const CONTENT_MAX_PATCH_BYTES: u64 = 16 * 1024 * 1024;

// The checked-in public key is safe to distribute with every launcher. The
// environment override is useful for deliberate key rotation and tests.
pub const CONTENT_PUBLIC_KEY_B64: &str = match option_env!("RX_CONTENT_PUBLIC_KEY_B64") {
    Some(value) if !value.is_empty() => value,
    _ => include_str!("../public-keys/content-public-key.b64"),
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentFileKind {
    File,
    AddonsZip,
}

impl Default for ContentFileKind {
    fn default() -> Self {
        Self::File
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFile {
    pub path: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub kind: ContentFileKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutablePatch {
    pub source_sha256: String,
    pub source_size: u64,
    pub output_sha256: String,
    pub output_size: u64,
    pub patch_url: String,
    pub patch_size: u64,
    pub patch_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentManifest {
    pub schema: u32,
    pub release: u64,
    pub version: String,
    pub files: Vec<ContentFile>,
    pub executable_patch: ExecutablePatch,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentManifestInfo {
    pub release: u64,
    pub version: String,
}

impl ContentManifest {
    pub fn info(&self) -> ContentManifestInfo {
        ContentManifestInfo {
            release: self.release,
            version: self.version.clone(),
        }
    }

    /// This is the exact canonical byte sequence authenticated by the
    /// Ed25519 signature. JSON formatting and object key order are irrelevant.
    pub fn signed_payload(&self) -> String {
        use std::fmt::Write;

        let mut payload = String::from("project-rx-content-v1\n");
        writeln!(payload, "schema={}", self.schema).unwrap();
        writeln!(payload, "release={}", self.release).unwrap();
        writeln!(payload, "version={}", self.version).unwrap();
        writeln!(
            payload,
            "executable.source_sha256={}",
            self.executable_patch.source_sha256.to_ascii_lowercase()
        )
        .unwrap();
        writeln!(
            payload,
            "executable.source_size={}",
            self.executable_patch.source_size
        )
        .unwrap();
        writeln!(
            payload,
            "executable.output_sha256={}",
            self.executable_patch.output_sha256.to_ascii_lowercase()
        )
        .unwrap();
        writeln!(
            payload,
            "executable.output_size={}",
            self.executable_patch.output_size
        )
        .unwrap();
        writeln!(
            payload,
            "executable.patch_url={}",
            self.executable_patch.patch_url
        )
        .unwrap();
        writeln!(
            payload,
            "executable.patch_size={}",
            self.executable_patch.patch_size
        )
        .unwrap();
        writeln!(
            payload,
            "executable.patch_sha256={}",
            self.executable_patch.patch_sha256.to_ascii_lowercase()
        )
        .unwrap();

        for file in &self.files {
            writeln!(
                payload,
                "file={}|{}|{}|{}|{}",
                file.path,
                file.size,
                file.sha256.to_ascii_lowercase(),
                file.url,
                match file.kind {
                    ContentFileKind::File => "file",
                    ContentFileKind::AddonsZip => "addons_zip",
                }
            )
            .unwrap();
        }

        payload
    }

    pub fn validate_with_key(&self, public_key_b64: &str) -> Result<(), String> {
        if self.schema != 1 {
            return Err("Project Rx content manifest has an unsupported schema".into());
        }
        if self.release == 0 {
            return Err("Project Rx content manifest has an invalid release number".into());
        }
        semver::Version::parse(&self.version)
            .map_err(|_| "Project Rx content manifest has an invalid version".to_string())?;
        if self.files.len() > CONTENT_MAX_FILES {
            return Err("Project Rx content manifest contains too many files".into());
        }

        let mut total_size = 0u64;
        let mut seen_paths = Vec::with_capacity(self.files.len());
        for file in &self.files {
            validate_relative_content_path(&file.path)?;
            validate_sha256(&file.sha256, "content file")?;
            validate_release_asset_url(&file.url, &self.version, false)?;
            if file.size == 0 {
                return Err(format!(
                    "Content file {} has an invalid file size",
                    file.path
                ));
            }
            if file.size > CONTENT_MAX_FILE_BYTES {
                return Err(format!("Content file {} is too large", file.path));
            }
            total_size = total_size
                .checked_add(file.size)
                .ok_or_else(|| "Project Rx content manifest size overflowed".to_string())?;
            if seen_paths
                .iter()
                .any(|path: &String| path.eq_ignore_ascii_case(&file.path))
            {
                return Err(format!("Project Rx content manifest repeats {}", file.path));
            }
            seen_paths.push(file.path.clone());
        }
        if total_size > CONTENT_MAX_TOTAL_BYTES {
            return Err("Project Rx content manifest is too large".into());
        }

        let executable = &self.executable_patch;
        validate_sha256(&executable.source_sha256, "executable source")?;
        validate_sha256(&executable.output_sha256, "executable output")?;
        validate_sha256(&executable.patch_sha256, "executable patch")?;
        if executable.source_size == 0 || executable.output_size == 0 {
            return Err("Project Rx executable patch has an invalid file size".into());
        }
        if executable.patch_size == 0 || executable.patch_size > CONTENT_MAX_PATCH_BYTES {
            return Err("Project Rx executable patch has an invalid patch size".into());
        }
        validate_release_asset_url(&executable.patch_url, &self.version, true)?;

        let signature = BASE64
            .decode(&self.signature)
            .map_err(|_| "Project Rx content signature is not valid base64".to_string())?;
        let signature = Signature::from_slice(&signature)
            .map_err(|_| "Project Rx content signature has an invalid length".to_string())?;
        let public_key = BASE64
            .decode(public_key_b64.trim())
            .map_err(|_| "Embedded Project Rx content key is invalid".to_string())?;
        let public_key: [u8; 32] = public_key
            .try_into()
            .map_err(|_| "Embedded Project Rx content key has an invalid length".to_string())?;
        let public_key = VerifyingKey::from_bytes(&public_key)
            .map_err(|_| "Embedded Project Rx content key is invalid".to_string())?;
        public_key
            .verify(self.signed_payload().as_bytes(), &signature)
            .map_err(|_| "Project Rx content manifest signature verification failed".to_string())
    }
}

pub fn parse_and_validate(bytes: &[u8], public_key_b64: &str) -> Result<ContentManifest, String> {
    if bytes.len() > CONTENT_MANIFEST_MAX_BYTES {
        return Err("Project Rx content manifest is too large".into());
    }
    let manifest: ContentManifest = serde_json::from_slice(bytes)
        .map_err(|_| "Project Rx content manifest is malformed".to_string())?;
    manifest.validate_with_key(public_key_b64)?;
    Ok(manifest)
}

pub fn validate_relative_content_path(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains('\\')
        || value.starts_with('/')
        || value.contains(':')
        || value.split('/').any(|part| part.is_empty())
    {
        return Err(format!("Content path is not safe: {value}"));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(format!("Content path is not relative: {value}"));
    }
    for component in path.components() {
        match component {
            Component::Normal(value) => validate_windows_component(&value.to_string_lossy())?,
            _ => return Err(format!("Content path is not safe: {value}")),
        }
    }
    if value.rsplit('/').next().is_some_and(|name| {
        name.eq_ignore_ascii_case("Wow.exe")
            || name.eq_ignore_ascii_case("rx-wow.exe")
            || name.to_ascii_lowercase().ends_with(".exe")
    }) {
        return Err("Content manifests cannot target an executable".into());
    }
    Ok(())
}

/// Windows strips trailing dots and spaces from ordinary file names and
/// reserves several device names even when an extension is present. Reject
/// these spellings on every platform so a signed manifest has one unambiguous
/// destination on Windows and Linux.
pub fn validate_windows_component(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value.ends_with('.')
        || value.ends_with(' ')
    {
        return Err(format!("Path component is not safe: {value}"));
    }

    let device_name = value.split('.').next().unwrap_or(value);
    let is_reserved = matches!(
        device_name.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if is_reserved {
        return Err(format!(
            "Path component uses a reserved Windows name: {value}"
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("Invalid SHA-256 hash for {label}"));
    }
    Ok(())
}

fn validate_release_asset_url(value: &str, version: &str, patch: bool) -> Result<(), String> {
    let parsed = url::Url::parse(value)
        .map_err(|_| "Project Rx content asset URL is invalid".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Project Rx content asset URL is not an allowed HTTPS URL".into());
    }
    let segments = parsed
        .path_segments()
        .map(|parts| parts.collect::<Vec<_>>())
        .unwrap_or_default();
    let expected_tag = format!("v{version}");
    if segments.len() != 6
        || segments[0] != CONTENT_RELEASE_OWNER
        || segments[1] != CONTENT_RELEASE_REPOSITORY
        || segments[2] != "releases"
        || segments[3] != "download"
        || segments[4] != expected_tag
        || segments[5].is_empty()
        || segments[5].contains('\\')
        || segments[5].contains('/')
        || (patch && !segments[5].ends_with(".rxpatch"))
        || (!patch && segments[5].to_ascii_lowercase().ends_with(".exe"))
    {
        return Err("Project Rx content asset URL is not an immutable release asset".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_manifest() -> (ContentManifest, SigningKey) {
        let key = SigningKey::from_bytes(&[
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c,
            0xae, 0x7f, 0x60, 0x0b,
        ]);
        let mut manifest = ContentManifest {
            schema: 1,
            release: 1,
            version: "1.1.0".into(),
            files: vec![ContentFile {
                path: "Data/new.MPQ".into(),
                url: "https://github.com/Mirenel/rx-launcher-content/releases/download/v1.1.0/new.MPQ"
                    .into(),
                size: 4,
                sha256: "00".repeat(32),
                kind: ContentFileKind::File,
            }],
            executable_patch: ExecutablePatch {
                source_sha256: "11".repeat(32),
                source_size: 10,
                output_sha256: "22".repeat(32),
                output_size: 10,
                patch_url:
                    "https://github.com/Mirenel/rx-launcher-content/releases/download/v1.1.0/wow.rxpatch"
                        .into(),
                patch_size: 3,
                patch_sha256: "33".repeat(32),
            },
            signature: String::new(),
        };
        manifest.signature =
            BASE64.encode(key.sign(manifest.signed_payload().as_bytes()).to_bytes());
        (manifest, key)
    }

    #[test]
    fn validates_signed_manifest_and_rejects_tampering() {
        let (manifest, key) = signed_manifest();
        let public = BASE64.encode(key.verifying_key().to_bytes());
        assert!(manifest.validate_with_key(&public).is_ok());

        let mut tampered = manifest.clone();
        tampered.files[0].path = "new.MPQ".into();
        assert!(tampered.validate_with_key(&public).is_err());
    }

    #[test]
    fn rejects_unsafe_paths_and_executable_assets() {
        for path in [
            "../file",
            "/file",
            "C:file",
            "Wow.exe",
            "Wow.exe.",
            "Wow.exe ",
            "rx-wow.exe",
            "Data/helper.exe",
            "Data/CON.txt",
        ] {
            assert!(validate_relative_content_path(path).is_err(), "{path}");
        }
        assert!(validate_release_asset_url(
            "https://github.com/Mirenel/rx-launcher-content/releases/download/v1.1.0/file.exe",
            "1.1.0",
            false
        )
        .is_err());
        assert!(validate_release_asset_url(
            "https://github.com/Mirenel/rx-patches/releases/download/v1.1.0/file.MPQ",
            "1.1.0",
            false
        )
        .is_err());
        assert!(validate_release_asset_url(
            "https://github.com/Mirenel/rx-launcher-content/releases/download/v1.1.0/file.MPQ",
            "1.1.0",
            false
        )
        .is_ok());
        assert!(validate_relative_content_path("Data/file.MPQ").is_ok());
    }
}
