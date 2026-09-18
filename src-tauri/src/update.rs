// The signed self-updater is currently Windows-only. Keep its implementation
// compiled for Windows while allowing Linux builds to expose only the public
// no-updater behavior until a Linux updater is published.
#![cfg_attr(not(windows), allow(dead_code, unused_imports, unused_variables))]

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;

use crate::update_auth::{ensure_newer, UpdateManifest};

include!(concat!(env!("OUT_DIR"), "/embedded_updater.rs"));
static UPDATER_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rx-updater.exe"));

const UPDATE_MANIFEST_URL: &str =
    "https://github.com/Mirenel/rx-launcher/releases/latest/download/latest.json";
const UPDATE_MAX_BYTES: u64 = 512 * 1024 * 1024;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateState {
    Idle,
    Checking,
    Downloading,
    Ready,
    Applying,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateNotice {
    pub version: String,
    pub notes: Option<String>,
}

pub async fn check_for_update(app: tauri::AppHandle) -> Result<Option<UpdateNotice>, String> {
    #[cfg(not(windows))]
    {
        let _ = app;
        log_update(
            "Linux launcher updates are disabled until a Linux artifact updater is published",
        );
        return Ok(None);
    }

    #[cfg(windows)]
    let result = check_for_update_inner(app).await;
    #[cfg(windows)]
    if let Err(error) = &result {
        log_update(&format!("update check failed: {error}"));
    }
    #[cfg(windows)]
    result
}

#[cfg(windows)]
async fn check_for_update_inner(app: tauri::AppHandle) -> Result<Option<UpdateNotice>, String> {
    if cfg!(debug_assertions) {
        log_update("debug build: launcher update check skipped");
        return Ok(None);
    }

    log_update(&format!(
        "checking for updates; current version {}",
        app.package_info().version
    ));
    cleanup_stale_update_dirs();

    let client = update_client()?;
    let (manifest, remote_version, current_version) = fetch_manifest(&app, &client).await?;
    if ensure_newer(&remote_version, &current_version).is_err() {
        log_update("no newer launcher update available");
        return Ok(None);
    }
    log_update("launcher update available; waiting for user confirmation");
    Ok(Some(UpdateNotice {
        version: manifest.version,
        notes: manifest.notes,
    }))
}

pub async fn check_for_update_and_apply(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = app;
        return Err("Launcher updates are not available for Linux builds yet".into());
    }

    #[cfg(windows)]
    let result = check_for_update_and_apply_inner(app).await;
    #[cfg(windows)]
    if let Err(error) = &result {
        log_update(&format!(
            "update apply failed before helper completion: {error}"
        ));
    }
    #[cfg(windows)]
    result
}

#[cfg(windows)]
async fn check_for_update_and_apply_inner(app: tauri::AppHandle) -> Result<(), String> {
    if cfg!(debug_assertions) {
        log_update("debug build: launcher update skipped");
        return Ok(());
    }

    log_update(&format!(
        "applying confirmed update; current version {}",
        app.package_info().version
    ));
    cleanup_stale_update_dirs();

    let target = std::env::current_exe()
        .map_err(|_| "Could not determine the launcher executable".to_string())?
        .canonicalize()
        .map_err(|_| "Could not validate the launcher executable".to_string())?;
    ensure_target_writable(&target)?;

    let client = update_client()?;
    let (manifest, remote_version, current_version) = fetch_manifest(&app, &client).await?;
    if ensure_newer(&remote_version, &current_version).is_err() {
        log_update("confirmed update is no longer newer; nothing to apply");
        return Ok(());
    }

    let update_dir = create_update_dir()?;
    let staged = update_dir.join("rx-launcher.exe.ready");
    let partial = update_dir.join("rx-launcher.exe.part");
    log_update("update available; download started");
    download_update(&client, &manifest, &partial).await?;
    verify_file_hash(&partial, &manifest.sha256)?;
    fs::rename(&partial, &staged)
        .map_err(|_| "Could not stage the verified launcher update".to_string())?;
    log_update("download completed; SHA-256 and release signature verified");

    let helper = extract_embedded_helper(&update_dir)?;
    verify_file_hash(&helper, EMBEDDED_UPDATER_SHA256)?;
    verify_file_hash(&staged, &manifest.sha256)?;
    manifest.validate()?;
    log_update("release signature reverified before starting the helper");

    let pid = std::process::id();
    #[cfg(windows)]
    let ready_event = ReadyEvent::create(pid)?;
    #[cfg(windows)]
    let ready_event_name = ready_event.name.clone();
    #[cfg(not(windows))]
    let ready_event_name = String::new();
    let mut command = Command::new(&helper);
    command
        .arg("apply")
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--source")
        .arg(&staged)
        .arg("--target")
        .arg(&target)
        .arg("--sha256")
        .arg(manifest.sha256.to_ascii_lowercase())
        .arg("--version")
        .arg(&manifest.version)
        .arg("--published-at")
        .arg(&manifest.published_at)
        .arg("--platform")
        .arg(&manifest.platform)
        .arg("--arch")
        .arg(&manifest.arch)
        .arg("--url")
        .arg(&manifest.url)
        .arg("--signature")
        .arg(&manifest.signature)
        .arg("--ready-event")
        .arg(&ready_event_name)
        .arg("--current-version")
        .arg(app.package_info().version.to_string())
        .current_dir(&update_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start the embedded launcher updater".to_string())?;
    #[cfg(windows)]
    if let Err(error) = ready_event.wait() {
        let _ = child.kill();
        return Err(error);
    }
    log_update("embedded helper started; launcher exiting for replacement");
    #[cfg(windows)]
    log_update("helper validated the launcher process image; exiting for replacement");
    app.exit(0);
    Ok(())
}

#[cfg(windows)]
struct ReadyEvent {
    handle: windows_sys::Win32::Foundation::HANDLE,
    name: String,
}

#[cfg(windows)]
impl ReadyEvent {
    fn create(pid: u32) -> Result<Self, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Threading::CreateEventW;

        let name = format!("Local\\ProjectRxLauncher.Ready.{pid}.{}", unique_suffix());
        let wide_name = std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err("Could not create the launcher updater readiness event".into());
        }
        Ok(Self { handle, name })
    }

    fn wait(&self) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let result = unsafe { WaitForSingleObject(self.handle, 5_000) };
        match result {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => {
                Err("The updater did not validate the launcher process before the timeout".into())
            }
            _ => Err("Could not wait for updater readiness".into()),
        }
    }
}

#[cfg(windows)]
impl Drop for ReadyEvent {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

fn update_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 || !is_allowed_github_url(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "Could not configure the launcher update client".to_string())
}

async fn fetch_manifest(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
) -> Result<(UpdateManifest, semver::Version, semver::Version), String> {
    let response = client
        .get(UPDATE_MANIFEST_URL)
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| "Could not retrieve the launcher update manifest".to_string())?;
    if !is_allowed_github_url(response.url()) {
        return Err("Launcher update manifest redirect was not secure".into());
    }
    if !response.status().is_success() {
        return Err("Launcher update manifest returned an HTTP error".into());
    }
    let manifest_bytes = read_capped_body(response, 64 * 1024).await?;
    let manifest: UpdateManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| "Launcher update manifest is malformed".to_string())?;
    let remote_version = manifest.validate()?;
    if let Some(notes) = &manifest.notes {
        log_update(&format!("update notes received ({} bytes)", notes.len()));
    }
    let current_version = semver::Version::parse(&app.package_info().version.to_string())
        .map_err(|_| "Installed launcher version is invalid".to_string())?;
    log_update(&format!(
        "remote version {}; current version {}",
        remote_version, current_version
    ));
    Ok((manifest, remote_version, current_version))
}

async fn read_capped_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err("Launcher update manifest is too large".into());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Could not read the launcher update manifest".to_string())?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err("Launcher update manifest is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn download_update(
    client: &reqwest::Client,
    manifest: &UpdateManifest,
    partial: &Path,
) -> Result<(), String> {
    let response = client
        .get(&manifest.url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|_| "Could not download the launcher update".to_string())?;
    if !is_allowed_github_url(response.url()) || !response.status().is_success() {
        return Err("Launcher update download was not a successful HTTPS response".into());
    }
    if response
        .content_length()
        .is_some_and(|length| length > UPDATE_MAX_BYTES)
    {
        return Err("Launcher update is larger than the allowed limit".into());
    }
    let mut file = tokio::fs::File::create(partial)
        .await
        .map_err(|_| "Could not create the launcher update staging file".to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Launcher update download was interrupted".to_string())?;
        downloaded = downloaded
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "Launcher update size overflowed".to_string())?;
        if downloaded > UPDATE_MAX_BYTES {
            return Err("Launcher update is larger than the allowed limit".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|_| "Could not write the launcher update to disk".to_string())?;
    }
    file.flush()
        .await
        .map_err(|_| "Could not flush the launcher update to disk".to_string())?;
    file.sync_all()
        .await
        .map_err(|_| "Could not sync the launcher update to disk".to_string())?;
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != manifest.sha256.to_ascii_lowercase() {
        return Err("Launcher update SHA-256 verification failed".into());
    }
    Ok(())
}

fn is_allowed_github_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some("github.com")
                | Some("release-assets.githubusercontent.com")
                | Some("objects.githubusercontent.com")
        )
}

fn verify_file_hash(path: &Path, expected: &str) -> Result<(), String> {
    let mut file =
        fs::File::open(path).map_err(|_| "Could not open a staged update file".to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "Could not read a staged update file".to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != expected.to_ascii_lowercase() {
        return Err("Staged launcher update hash verification failed".into());
    }
    Ok(())
}

fn create_update_dir() -> Result<PathBuf, String> {
    let root = update_root_dir();
    fs::create_dir_all(&root)
        .map_err(|_| "Could not create the launcher update directory".to_string())?;
    let path = root.join(format!("update-{}-{}", std::process::id(), unique_suffix()));
    fs::create_dir(&path)
        .map_err(|_| "Could not create a unique launcher update directory".to_string())?;
    Ok(path)
}

fn extract_embedded_helper(update_dir: &Path) -> Result<PathBuf, String> {
    if UPDATER_EXE.is_empty() {
        return Err("This build does not contain an embedded updater".into());
    }
    let partial = update_dir.join("rx-updater.exe.part");
    let helper = update_dir.join("rx-updater.exe");
    let mut file = fs::File::create(&partial)
        .map_err(|_| "Could not create the embedded updater file".to_string())?;
    file.write_all(UPDATER_EXE)
        .map_err(|_| "Could not extract the embedded updater".to_string())?;
    file.sync_all()
        .map_err(|_| "Could not sync the embedded updater".to_string())?;
    fs::rename(&partial, &helper)
        .map_err(|_| "Could not finalize the embedded updater".to_string())?;
    Ok(helper)
}

fn ensure_target_writable(target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| "Launcher executable has no parent directory".to_string())?;
    let probe = parent.join(format!(".rx-launcher-update-check-{}", std::process::id()));
    let result = OpenOptions::new().write(true).create_new(true).open(&probe);
    match result {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            Ok(())
        }
        Err(_) => Err(
            "Launcher installation is not user-writable; install it for the current user outside Program Files".into(),
        ),
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn cleanup_stale_update_dirs() {
    let root = update_root_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(7 * 24 * 60 * 60));
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("update-") {
            continue;
        }
        let stale = cutoff
            .zip(
                fs::metadata(&path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok()),
            )
            .is_some_and(|(cutoff, modified)| modified < cutoff);
        if stale {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn log_update(message: &str) {
    let root = update_root_dir();
    let _ = fs::create_dir_all(&root);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("update.log"))
    {
        let _ = writeln!(file, "{} {message}", chrono_like_timestamp());
    }
}

fn update_root_dir() -> PathBuf {
    std::env::temp_dir()
        .join("Project.Rx.Launcher")
        .join("updates")
}

fn chrono_like_timestamp() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_state_has_transaction_phases() {
        assert_ne!(UpdateState::Downloading, UpdateState::Applying);
        assert_eq!(UpdateState::Idle, UpdateState::Idle);
    }

    #[cfg(not(windows))]
    #[test]
    fn update_root_uses_native_linux_separators() {
        assert!(!update_root_dir().to_string_lossy().contains('\\'));
    }

    #[test]
    fn file_hash_verification_rejects_mismatches() {
        let path = std::env::temp_dir().join(format!(
            "rx-launcher-hash-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::write(&path, b"verified update bytes").unwrap();
        assert!(verify_file_hash(&path, &"00".repeat(32)).is_err());
        fs::remove_file(path).unwrap();
    }
}
