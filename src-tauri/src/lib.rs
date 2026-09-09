use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_shell::ShellExt;

pub mod content;
pub mod exe_patch;
mod update;
mod update_auth;
mod update_payload;

const REALMLIST_VALUE: &str = "set realmlist projectrx.net";
const SERVER_HOST: &str = "projectrx.net";
const SERVER_PORT: u16 = 3724;
const REALM_STATUS_URL: &str = "https://projectrx.net/api/realm-status/public";
const REALM_STATUS_MAX_BYTES: usize = 16 * 1024;
const PATCH_DOWNLOAD_MAX_BYTES: u64 = 512 * 1024 * 1024;
const ADDONS_EXTRACT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const ADDONS_MAX_ENTRIES: usize = 10_000;
const CONTENT_CACHE_FILENAME: &str = "content-manifest.json";
const INSTALLED_CONTENT_FILENAME: &str = "installed_content.json";
const LEGACY_PATCH_PATHS: &[&str] = &[
    "Data/patch-7.MPQ",
    "Data/patch-A.MPQ",
    "Data/patch-B.MPQ",
    "Data/patch-D.MPQ",
];

struct HttpClient(reqwest::Client);

#[cfg(windows)]
struct AppInstanceMutex(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for AppInstanceMutex {}
#[cfg(windows)]
unsafe impl Sync for AppInstanceMutex {}

#[cfg(windows)]
impl AppInstanceMutex {
    fn acquire() -> Result<Option<Self>, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = std::ffi::OsStr::new("Local\\ProjectRxLauncher.Instance")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err("Could not create the launcher instance mutex".into());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

#[cfg(windows)]
impl Drop for AppInstanceMutex {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

// ── Input validation ────────────────────────────────────────

/// Validates game_path from the frontend: must be absolute, no traversal,
/// must exist as a directory. Returns the validated PathBuf or a generic error.
fn validate_game_path(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);

    if !path.is_absolute() {
        return Err("Invalid game directory".into());
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("Invalid game directory".into());
        }
    }

    if !path.is_dir() {
        return Err("Game directory not found".into());
    }

    Ok(path)
}

// ── Launcher config (constants exposed to frontend) ─────────

#[derive(Serialize)]
struct LauncherConfig {
    patch_version: String,
    realmlist: String,
    server_host: String,
    server_port: u16,
    launcher_version: String,
}

fn unavailable_patch_version() -> String {
    "Unavailable".into()
}

#[tauri::command]
async fn check_launcher_update(
    app: tauri::AppHandle,
) -> Result<Option<update::UpdateNotice>, String> {
    update::check_for_update(app).await
}

#[tauri::command]
async fn apply_launcher_update(app: tauri::AppHandle) -> Result<(), String> {
    update::check_for_update_and_apply(app).await
}

#[tauri::command]
fn get_launcher_config(app: tauri::AppHandle) -> LauncherConfig {
    LauncherConfig {
        patch_version: unavailable_patch_version(),
        realmlist: REALMLIST_VALUE.into(),
        server_host: SERVER_HOST.into(),
        server_port: SERVER_PORT,
        launcher_version: app.package_info().version.to_string(),
    }
}

#[tauri::command]
async fn get_patch_manifest(
    app: tauri::AppHandle,
    http: tauri::State<'_, HttpClient>,
) -> Result<content::ContentManifestInfo, String> {
    fetch_content_manifest(&app, &http.0)
        .await
        .map(|manifest| manifest.info())
}

#[derive(Serialize)]
struct GameDirectoryStatus {
    has_wow: bool,
    has_data: bool,
    has_addons: bool,
    has_rx_wow: bool,
}

#[tauri::command]
fn check_game_directory(game_path: String) -> Result<GameDirectoryStatus, String> {
    let dir = validate_game_path(&game_path)?;
    Ok(GameDirectoryStatus {
        has_wow: dir.join("Wow.exe").is_file(),
        has_data: dir.join("Data").is_dir(),
        has_addons: dir.join("Interface").join("AddOns").is_dir(),
        has_rx_wow: dir.join("rx-wow.exe").is_file(),
    })
}

// ── Server status ────────────────────────────────────────────

#[derive(Debug, PartialEq, Serialize)]
struct ServerStatus {
    online: bool,
    players: Option<u32>,
}

#[derive(Deserialize)]
struct RealmStatusResponse {
    online: bool,
    players: u32,
}

fn parse_realm_status(bytes: &[u8]) -> Result<ServerStatus, String> {
    if bytes.len() > REALM_STATUS_MAX_BYTES {
        return Err("Realm status response was too large".into());
    }
    let status: RealmStatusResponse =
        serde_json::from_slice(bytes).map_err(|_| "Invalid realm status response".to_string())?;
    Ok(ServerStatus {
        online: status.online,
        players: Some(status.players),
    })
}

fn tcp_probe_addr(addr: std::net::SocketAddr, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

fn tcp_probe_host(host: &str, port: u16, timeout: Duration) -> bool {
    format!("{host}:{port}")
        .to_socket_addrs()
        .ok()
        .is_some_and(|addrs| addrs.into_iter().any(|addr| tcp_probe_addr(addr, timeout)))
}

async fn fetch_realm_status(client: &reqwest::Client) -> Result<ServerStatus, String> {
    let response = client
        .get(REALM_STATUS_URL)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|_| "Could not retrieve realm status".to_string())?;
    if !response.status().is_success() {
        return Err("Realm status service returned an error".into());
    }
    let bytes = read_capped_body(response, REALM_STATUS_MAX_BYTES).await?;
    parse_realm_status(&bytes)
}

async fn read_capped_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|n| n > max_bytes as u64)
    {
        return Err("Project Rx response was too large".into());
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Could not read the Project Rx response".to_string())?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err("Project Rx response was too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[tauri::command]
async fn check_server_status(http: tauri::State<'_, HttpClient>) -> Result<ServerStatus, String> {
    if let Ok(status) = fetch_realm_status(&http.0).await {
        return Ok(status);
    }

    let online = tauri::async_runtime::spawn_blocking(|| {
        tcp_probe_host(SERVER_HOST, SERVER_PORT, Duration::from_secs(3))
    })
    .await
    .unwrap_or(false);
    Ok(ServerStatus {
        online,
        players: None,
    })
}

// ── JSON fetch helper (size-capped) ─────────────────────────

async fn fetch_json_vec<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<T>, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| "Could not connect to Project Rx".to_string())?;
    if !resp.status().is_success() {
        return Err(format!(
            "Project Rx returned HTTP {}",
            resp.status().as_u16()
        ));
    }
    let bytes = read_capped_body(resp, max_bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| "Project Rx returned invalid data".to_string())
}

fn content_manifest_cache_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(CONTENT_CACHE_FILENAME))
}

fn read_cached_content_manifest(app: &tauri::AppHandle) -> Option<content::ContentManifest> {
    let path = content_manifest_cache_path(app)?;
    let bytes = fs::read(path).ok()?;
    content::parse_and_validate(&bytes, content::CONTENT_PUBLIC_KEY_B64).ok()
}

fn cache_content_manifest(app: &tauri::AppHandle, bytes: &[u8]) {
    let Some(path) = content_manifest_cache_path(app) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = path.with_file_name(format!(
        "{CONTENT_CACHE_FILENAME}.part-{}",
        std::process::id()
    ));
    if fs::write(&temp, bytes).is_ok() {
        let _ = replace_verified_file(&temp, &path, "cached content manifest");
    } else {
        fs::remove_file(&temp).ok();
    }
}

async fn fetch_content_manifest(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
) -> Result<content::ContentManifest, String> {
    let cached = read_cached_content_manifest(app);
    let urls = [
        content::CONTENT_MANIFEST_URL,
        content::CONTENT_MANIFEST_FALLBACK_URL,
    ];
    let mut errors = Vec::new();
    for url in urls {
        let response = match client
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                errors.push(format!("{url} was unavailable"));
                continue;
            }
        };
        if !response.status().is_success() {
            errors.push(format!(
                "{url} returned HTTP {}",
                response.status().as_u16()
            ));
            continue;
        }
        let bytes = match read_capped_body(response, content::CONTENT_MANIFEST_MAX_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let manifest = match content::parse_and_validate(&bytes, content::CONTENT_PUBLIC_KEY_B64) {
            Ok(manifest) => manifest,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if cached
            .as_ref()
            .is_some_and(|old| old.release > manifest.release)
        {
            errors.push(
                "Project Rx content manifest attempted to roll back to an older release".into(),
            );
            continue;
        }
        cache_content_manifest(app, &bytes);
        return Ok(manifest);
    }
    cached.ok_or_else(|| {
        format!(
            "Could not retrieve a valid Project Rx content manifest. {}",
            errors.join("; ")
        )
    })
}

// ── News (fetched from projectrx.net) ───────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct NewsItem {
    date: String,
    title: String,
    body: String,
    tag: String,
    featured: bool,
    url: String,
    #[serde(default)]
    image: String,
}

#[tauri::command]
async fn get_news(http: tauri::State<'_, HttpClient>) -> Result<Vec<NewsItem>, String> {
    fetch_json_vec(&http.0, "https://projectrx.net/news.json", 1_048_576).await
}

// ── Changelog (fetched from projectrx.net) ──────────────────

#[derive(Serialize, Deserialize, Clone)]
struct ChangelogEntry {
    version: String,
    date: String,
    changes: Vec<String>,
}

#[tauri::command]
async fn get_changelog(http: tauri::State<'_, HttpClient>) -> Result<Vec<ChangelogEntry>, String> {
    fetch_json_vec(&http.0, "https://projectrx.net/changelog.json", 524_288).await
}

// ── Game launch (via shell plugin) ──────────────────────────

#[tauri::command]
fn launch_game(app: tauri::AppHandle, game_path: String) -> Result<(), String> {
    let dir = validate_game_path(&game_path)?;
    let wow_exe = dir.join("rx-wow.exe");

    if !wow_exe.exists() {
        return Err("rx-wow.exe not found. Patch the game before launching.".into());
    }

    let exe_str = wow_exe.to_str().ok_or("Invalid path encoding")?;
    let (_rx, _child) = app
        .shell()
        .command(exe_str)
        .spawn()
        .map_err(|_| "Failed to launch game".to_string())?;

    Ok(())
}

// ── Patch download ──────────────────────────────────────────

#[derive(Clone, Serialize)]
struct DownloadProgress {
    percent: u8,
    message: String,
    log: bool,
    bytes_downloaded: u64,
    bytes_total: u64,
}

/// Replace a verified download without deleting the existing game file until
/// the new file has been moved into place. This keeps a failed rename
/// recoverable on Windows, where renaming over an existing file is not
/// consistently supported.
fn replace_verified_file(part: &Path, destination: &Path, label: &str) -> Result<(), String> {
    if destination.exists() && !destination.is_file() {
        return Err(format!(
            "Cannot replace {label}: the destination is not a file"
        ));
    }
    let backup = destination.with_file_name(format!(
        ".{}.rx-old-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    if backup.exists() {
        return Err(format!(
            "Cannot replace {label}: a previous replacement is incomplete"
        ));
    }
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, &backup)
            .map_err(|_| format!("Could not prepare {label} for replacement"))?;
    }
    if let Err(error) = fs::rename(part, destination) {
        if had_destination {
            let _ = fs::rename(&backup, destination);
        }
        return Err(format!("Could not install {label}: {error}"));
    }
    if had_destination {
        // The replacement is already verified and in place. Failure to remove
        // this private rollback copy is harmless and avoids reporting a false
        // install failure after a successful replacement.
        fs::remove_file(&backup).ok();
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|_| "Failed to read file".to_string())?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| "Failed to read file".to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn classify_patch_file(path: &Path, name: &str, expected: &str) -> Option<String> {
    if !path.exists() {
        Some(format!("{name} (missing)"))
    } else {
        match sha256_file(path) {
            Ok(actual) if actual == expected => None,
            Ok(_) => Some(format!("{name} (corrupted)")),
            Err(_) => Some(format!("{name} (unreadable)")),
        }
    }
}

fn resolve_content_path(game_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    content::validate_relative_content_path(relative)?;
    let mut resolved = game_dir.to_path_buf();
    for component in relative.split('/') {
        resolved.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&resolved) {
            if metadata.file_type().is_symlink() {
                return Err(format!("Content path contains a symbolic link: {relative}"));
            }
        }
    }
    Ok(resolved)
}

fn file_hash_and_size(path: &Path) -> Result<(u64, String), String> {
    let metadata =
        fs::metadata(path).map_err(|_| "Could not read the game executable".to_string())?;
    if !metadata.is_file() {
        return Err("The game executable is not a regular file".into());
    }
    let hash = sha256_file(path)?;
    Ok((metadata.len(), hash))
}

fn progress_percent(completed: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((completed.saturating_mul(100) / total).min(99)) as u8
}

fn emit_progress(
    app: &tauri::AppHandle,
    completed: u64,
    total: u64,
    message: String,
    log: bool,
    bytes_downloaded: u64,
    bytes_total: u64,
) {
    app.emit(
        "download-progress",
        DownloadProgress {
            percent: progress_percent(completed, total),
            message,
            log,
            bytes_downloaded,
            bytes_total,
        },
    )
    .ok();
}

async fn download_verified_asset(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    url: &str,
    part_path: &Path,
    label: &str,
    expected_size: u64,
    expected_hash: &str,
    completed: u64,
    total: u64,
) -> Result<u64, String> {
    if expected_size == 0 || expected_size > PATCH_DOWNLOAD_MAX_BYTES {
        return Err(format!("{label} has an invalid permitted size"));
    }
    if part_path.exists() {
        fs::remove_file(part_path)
            .map_err(|_| format!("Could not remove stale temporary file for {label}"))?;
    }

    emit_progress(
        app,
        completed,
        total,
        format!("Downloading {label}..."),
        true,
        completed,
        total,
    );
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| format!("Could not connect to the download server for {label}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Download server returned HTTP {} for {label}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size > PATCH_DOWNLOAD_MAX_BYTES || size != expected_size)
    {
        return Err(format!(
            "Downloaded size for {label} does not match the manifest"
        ));
    }

    let mut file = fs::File::create(part_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            format!("Permission denied writing {label}. Check the game directory permissions.")
        } else {
            format!("Could not create the temporary file for {label}")
        }
    })?;
    let mut downloaded = 0u64;
    let mut last_emit = 0u64;
    let mut stream = response.bytes_stream();
    let result: Result<(), String> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| format!("Connection lost while downloading {label}"))?;
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| format!("Downloaded size for {label} overflowed"))?;
            if downloaded > expected_size || downloaded > PATCH_DOWNLOAD_MAX_BYTES {
                return Err(format!("Downloaded size for {label} exceeds the manifest"));
            }
            file.write_all(&chunk)
                .map_err(|_| format!("Could not write the temporary file for {label}"))?;
            if downloaded.saturating_sub(last_emit) >= 524_288 || downloaded == expected_size {
                last_emit = downloaded;
                emit_progress(
                    app,
                    completed.saturating_add(downloaded),
                    total,
                    format!(
                        "Downloading {label}... ({:.1} / {:.1} MB)",
                        downloaded as f64 / 1_048_576.0,
                        expected_size as f64 / 1_048_576.0
                    ),
                    false,
                    completed.saturating_add(downloaded),
                    total,
                );
            }
        }
        file.flush()
            .map_err(|_| format!("Could not save {label}"))?;
        file.sync_all()
            .map_err(|_| format!("Could not finalize {label}"))?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(error) = result {
        fs::remove_file(part_path).ok();
        return Err(error);
    }

    let path = part_path.to_path_buf();
    let actual = tauri::async_runtime::spawn_blocking(move || file_hash_and_size(&path))
        .await
        .map_err(|_| format!("Could not verify {label}"))?
        .map_err(|_| format!("Could not verify {label}"))?;
    if actual.0 != expected_size || actual.1 != expected_hash.to_ascii_lowercase() {
        fs::remove_file(part_path).ok();
        return Err(format!("{label} failed manifest integrity verification"));
    }
    Ok(downloaded)
}

async fn prepare_rx_wow(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    game_dir: &Path,
    executable: &content::ExecutablePatch,
    repair: bool,
    completed: &mut u64,
    total: u64,
) -> Result<(), String> {
    let source = game_dir.join("Wow.exe");
    let output = game_dir.join("rx-wow.exe");
    let source_for_hash = source.clone();
    let source_info =
        tauri::async_runtime::spawn_blocking(move || file_hash_and_size(&source_for_hash))
            .await
            .map_err(|_| "Could not inspect Wow.exe".to_string())??;
    if source_info.0 != executable.source_size
        || source_info.1 != executable.source_sha256.to_ascii_lowercase()
    {
        return Err("Wow.exe is not the supported clean Project Rx client".into());
    }

    if !repair && output.is_file() {
        let output_for_hash = output.clone();
        let output_info =
            tauri::async_runtime::spawn_blocking(move || file_hash_and_size(&output_for_hash))
                .await
                .map_err(|_| "Could not verify rx-wow.exe".to_string())??;
        if output_info.0 == executable.output_size
            && output_info.1 == executable.output_sha256.to_ascii_lowercase()
        {
            *completed = completed.saturating_add(executable.patch_size.max(1));
            emit_progress(
                app,
                *completed,
                total,
                "rx-wow.exe is already up to date.".into(),
                true,
                *completed,
                total,
            );
            return Ok(());
        }
    }

    let patch_path = game_dir.join(format!(".rx-wow.rxpatch.part-{}", std::process::id()));
    let downloaded = download_verified_asset(
        app,
        client,
        &executable.patch_url,
        &patch_path,
        "executable patch",
        executable.patch_size,
        &executable.patch_sha256,
        *completed,
        total,
    )
    .await?;
    let patch_bytes = fs::read(&patch_path)
        .map_err(|_| "Could not read the downloaded executable patch".to_string())?;
    fs::remove_file(&patch_path).ok();
    if patch_bytes.len() as u64 != executable.patch_size {
        return Err("Executable patch size changed before application".into());
    }

    emit_progress(
        app,
        completed.saturating_add(downloaded),
        total,
        "Applying Project Rx executable patch...".into(),
        true,
        completed.saturating_add(downloaded),
        total,
    );
    let build_path = game_dir.join(format!(".rx-wow.exe.rx-build-{}", std::process::id()));
    if build_path.exists() {
        fs::remove_file(&build_path)
            .map_err(|_| "Could not clear the previous executable build".to_string())?;
    }
    let source_for_build = source.clone();
    let build_for_patch = build_path.clone();
    let patch_for_apply = patch_bytes;
    let source_size = executable.source_size;
    tauri::async_runtime::spawn_blocking(move || {
        exe_patch::apply_patch_to_copy(
            &source_for_build,
            &build_for_patch,
            &patch_for_apply,
            source_size,
        )
    })
    .await
    .map_err(|_| "Could not apply the executable patch".to_string())??;

    let output_for_hash = build_path.clone();
    let output_info =
        tauri::async_runtime::spawn_blocking(move || file_hash_and_size(&output_for_hash))
            .await
            .map_err(|_| "Could not verify generated rx-wow.exe".to_string())??;
    if output_info.0 != executable.output_size
        || output_info.1 != executable.output_sha256.to_ascii_lowercase()
    {
        fs::remove_file(&build_path).ok();
        return Err("Generated rx-wow.exe failed manifest integrity verification".into());
    }
    replace_verified_file(&build_path, &output, "rx-wow.exe")?;
    *completed = completed.saturating_add(executable.patch_size.max(1));
    emit_progress(
        app,
        *completed,
        total,
        "rx-wow.exe generated and verified.".into(),
        true,
        *completed,
        total,
    );
    Ok(())
}

#[tauri::command]
async fn download_patch(
    app: tauri::AppHandle,
    http: tauri::State<'_, HttpClient>,
    game_path: String,
    repair: bool,
) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let data_dir = dir.join("Data");
    let addons_dir = dir.join("Interface").join("AddOns");

    if !data_dir.is_dir() {
        return Err("Data folder not found. Make sure you selected a valid game directory.".into());
    }
    if !addons_dir.is_dir() {
        return Err(
            "Interface/AddOns folder not found. Make sure you selected a valid game directory."
                .into(),
        );
    }

    let client = &http.0;
    let manifest = fetch_content_manifest(&app, client).await?;
    let previous_content = read_installed_content(&app, &dir)?;
    let mut items: Vec<(&str, PathBuf, &str, bool, u64, &str)> = Vec::new();
    for file in &manifest.files {
        let final_path = resolve_content_path(&dir, &file.path)?;
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).map_err(|_| {
                format!(
                    "Could not create the destination directory for {}",
                    file.path
                )
            })?;
        }
        resolve_content_path(&dir, &file.path)?;
        items.push((
            file.path.as_str(),
            final_path,
            file.url.as_str(),
            file.kind == content::ContentFileKind::AddonsZip,
            file.size.max(1),
            file.sha256.as_str(),
        ));
    }

    let total_bytes = manifest
        .executable_patch
        .patch_size
        .max(1)
        .saturating_add(items.iter().map(|item| item.4).sum::<u64>());
    let mut completed_bytes = 0u64;
    prepare_rx_wow(
        &app,
        client,
        &dir,
        &manifest.executable_patch,
        repair,
        &mut completed_bytes,
        total_bytes,
    )
    .await?;

    for (filename, final_path, base_url, extract, weight, expected_hash) in &items {
        let url = (*base_url).to_string();
        let part_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("content");
        let part_path =
            final_path.with_file_name(format!(".{part_name}.rx-part-{}", std::process::id()));

        // Check if we can skip this file
        let skip = if *extract {
            let installed = read_installed_addons(&app, &dir)?;
            !repair
                && previous_content.as_ref().is_some_and(|content| {
                    content.release == manifest.release && content.patch_version == manifest.version
                })
                && installed.as_ref().is_some_and(|manifest| {
                    !manifest.addons.is_empty()
                        && manifest
                            .addons
                            .iter()
                            .all(|addon| addons_dir.join(&addon.name).is_dir())
                })
        } else {
            if final_path.exists() {
                let path_clone = final_path.clone();
                let hash_owned = (*expected_hash).to_string();
                tauri::async_runtime::spawn_blocking(move || {
                    sha256_file(&path_clone)
                        .map(|a| a == hash_owned)
                        .unwrap_or(false)
                })
                .await
                .unwrap_or(false)
            } else {
                false
            }
        };
        if skip {
            completed_bytes = completed_bytes.saturating_add(*weight);
            emit_progress(
                &app,
                completed_bytes,
                total_bytes,
                format!("{} already up to date.", filename),
                true,
                completed_bytes,
                total_bytes,
            );
            continue;
        }

        let downloaded = download_verified_asset(
            &app,
            client,
            &url,
            &part_path,
            filename,
            *weight,
            expected_hash,
            completed_bytes,
            total_bytes,
        )
        .await?;

        if *extract {
            app.emit(
                "download-progress",
                DownloadProgress {
                    percent: progress_percent(
                        completed_bytes.saturating_add(downloaded),
                        total_bytes,
                    ),
                    message: format!("Extracting {}...", filename),
                    log: true,
                    bytes_downloaded: completed_bytes.saturating_add(downloaded),
                    bytes_total: total_bytes,
                },
            )
            .ok();

            let stage = addons_dir.join(format!(".rx-staging-{}", std::process::id()));
            if stage.exists() {
                fs::remove_dir_all(&stage)
                    .map_err(|_| "Could not clear addon staging directory".to_string())
                    .inspect_err(|_| {
                        fs::remove_file(&part_path).ok();
                    })?;
            }
            if let Err(error) = fs::create_dir(&stage) {
                fs::remove_file(&part_path).ok();
                return Err(format!("Could not create addon staging directory: {error}"));
            }
            let install_result = (|| {
                let addon_folders = extract_zip_file(&part_path, &stage)?;
                install_staged_addons(&app, &dir, &addons_dir, &stage, &addon_folders)
            })();
            fs::remove_dir_all(&stage).ok();
            fs::remove_file(&part_path).ok();
            install_result?;
        } else {
            if let Err(error) = replace_verified_file(&part_path, final_path, filename) {
                fs::remove_file(&part_path).ok();
                return Err(error);
            }
        }

        completed_bytes = completed_bytes.saturating_add(*weight);
        app.emit(
            "download-progress",
            DownloadProgress {
                percent: progress_percent(completed_bytes, total_bytes),
                message: format!("{} installed.", filename),
                log: true,
                bytes_downloaded: completed_bytes,
                bytes_total: total_bytes,
            },
        )
        .ok();
    }

    let installed_content = InstalledContentManifest {
        version: 1,
        game_path: canonical_game_path(&dir)?,
        release: manifest.release,
        patch_version: manifest.version.clone(),
        files: items
            .iter()
            .filter(|item| !item.3)
            .map(|item| InstalledContentFile {
                path: item.0.to_string(),
                sha256: item.5.to_ascii_lowercase(),
            })
            .collect(),
    };
    let preserved = remove_obsolete_content(&dir, previous_content.as_ref(), &installed_content)?;
    save_installed_content(&app, &installed_content)?;
    for path in preserved {
        emit_progress(
            &app,
            completed_bytes,
            total_bytes,
            format!("Preserved user-modified file: {path}"),
            true,
            completed_bytes,
            total_bytes,
        );
    }

    Ok(manifest.version)
}

fn extract_zip_file(zip_path: &Path, dest_dir: &Path) -> Result<Vec<String>, String> {
    let file = fs::File::open(zip_path).map_err(|_| {
        "Could not open Addons.zip for extraction. Try downloading again.".to_string()
    })?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|_| "Addons.zip appears to be corrupted. Try downloading again.".to_string())?;
    if archive.len() > ADDONS_MAX_ENTRIES {
        return Err("Addons.zip contains too many entries".into());
    }

    let mut roots = std::collections::BTreeSet::new();
    let mut extracted_bytes = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| "Failed to read a file inside Addons.zip. The download may be corrupted — try again.".to_string())?;

        let name = entry
            .enclosed_name()
            .ok_or_else(|| "Addons.zip contains an unsafe path".to_string())?
            .to_owned();
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err("Addons.zip contains an unsupported symbolic link".into());
        }
        extracted_bytes = extracted_bytes
            .checked_add(entry.size())
            .ok_or_else(|| "Addons.zip is too large to extract".to_string())?;
        if extracted_bytes > ADDONS_EXTRACT_MAX_BYTES {
            return Err("Addons.zip is too large to extract".into());
        }
        let top = match name.components().next() {
            Some(Component::Normal(value)) => value.to_string_lossy().into_owned(),
            _ => return Err("Addons.zip contains an invalid addon path".into()),
        };
        if top.is_empty() || top == "." || top == ".." {
            return Err("Addons.zip contains an invalid addon folder".into());
        }
        if name.components().count() == 1 && !entry.is_dir() {
            return Err("Addons.zip may contain files only inside addon folders".into());
        }
        roots.insert(top);

        let out_path = dest_dir.join(&name);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|_| "Could not create addon folder".to_string())?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|_| "Could not create addon folder".to_string())?;
            }
            let mut outfile =
                fs::File::create(&out_path).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        "Permission denied extracting addons. Try running the launcher as administrator.".to_string()
                    } else {
                        "Could not extract addon files. Check available disk space and permissions.".to_string()
                    }
                })?;
            std::io::copy(&mut entry, &mut outfile).map_err(|_| {
                "Failed to extract addon files. Check available disk space.".to_string()
            })?;
        }
    }

    if roots.is_empty() {
        return Err("Addons.zip did not contain any addon folders".into());
    }
    Ok(roots.into_iter().collect())
}

// ── Dynamic addon tracking ─────────────────────────────────

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledAddon {
    name: String,
    backup_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledAddonsManifest {
    version: u32,
    game_path: String,
    addons: Vec<InstalledAddon>,
}

fn installed_addons_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("installed_addons.json"))
}

fn canonical_game_path(game_dir: &Path) -> Result<String, String> {
    fs::canonicalize(game_dir)
        .map_err(|_| "Could not resolve game directory".to_string())?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Invalid game directory encoding".to_string())
}

fn validate_addon_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

fn manifest_matches_game(manifest: &InstalledAddonsManifest, canonical_path: &str) -> bool {
    let same_path = if cfg!(windows) {
        manifest.game_path.eq_ignore_ascii_case(canonical_path)
    } else {
        manifest.game_path == canonical_path
    };
    manifest.version == 1
        && same_path
        && manifest.addons.iter().all(|addon| {
            validate_addon_name(&addon.name)
                && addon.backup_name.as_deref().is_none_or(validate_addon_name)
        })
}

fn read_installed_addons(
    app: &tauri::AppHandle,
    game_dir: &Path,
) -> Result<Option<InstalledAddonsManifest>, String> {
    let Some(path) = installed_addons_path(app) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Could not read addon tracking data".into()),
    };
    let manifest: InstalledAddonsManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None), // Legacy or malformed data is non-authoritative.
    };
    if !manifest_matches_game(&manifest, &canonical_game_path(game_dir)?) {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn save_installed_addons(
    app: &tauri::AppHandle,
    manifest: &InstalledAddonsManifest,
) -> Result<(), String> {
    let path = installed_addons_path(app).ok_or("Could not locate launcher data directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "Could not create launcher data directory".to_string())?;
    }
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|_| "Could not serialize addon tracking data".to_string())?;
    let temp = path.with_file_name(format!("installed_addons.json.part-{}", std::process::id()));
    if temp.exists() {
        return Err("An addon tracking update is already in progress".into());
    }
    if let Err(error) = fs::write(&temp, json) {
        fs::remove_file(&temp).ok();
        return Err(format!("Could not save addon tracking data: {error}"));
    }
    // Windows requires a writable handle for FlushFileBuffers/sync_all.
    if let Err(error) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp)
        .and_then(|file| file.sync_all())
    {
        fs::remove_file(&temp).ok();
        return Err(format!("Could not save addon tracking data: {error}"));
    }

    let old = path.with_file_name(format!(
        ".installed_addons.json.rx-old-{}",
        std::process::id()
    ));
    let had_manifest = path.exists();
    if had_manifest {
        if old.exists() {
            fs::remove_file(&temp).ok();
            return Err("A previous addon tracking replacement is incomplete".into());
        }
        if let Err(error) = fs::rename(&path, &old) {
            fs::remove_file(&temp).ok();
            return Err(format!("Could not replace addon tracking data: {error}"));
        }
    }
    if let Err(error) = fs::rename(&temp, &path) {
        if had_manifest {
            let _ = fs::rename(&old, &path);
        }
        fs::remove_file(&temp).ok();
        return Err(format!("Could not finalize addon tracking data: {error}"));
    }
    if had_manifest {
        fs::remove_file(&old).ok();
    }
    Ok(())
}

fn delete_installed_addons(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(path) = installed_addons_path(app) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Could not remove addon tracking data".into()),
        }
    }
    Ok(())
}

fn restore_tracked_addon(addons_dir: &Path, addon: &InstalledAddon) -> Result<(), String> {
    let installed = addons_dir.join(&addon.name);
    if installed.exists() {
        fs::remove_dir_all(&installed)
            .map_err(|_| format!("Could not remove addon {}", addon.name))?;
    }
    if let Some(backup_name) = &addon.backup_name {
        let backup = addons_dir.join(backup_name);
        if backup.exists() {
            fs::rename(&backup, &installed)
                .map_err(|_| format!("Could not restore backup for {}", addon.name))?;
        }
    }
    Ok(())
}

fn install_staged_addons(
    app: &tauri::AppHandle,
    game_dir: &Path,
    addons_dir: &Path,
    stage: &Path,
    folders: &[String],
) -> Result<(), String> {
    if folders.iter().any(|name| !validate_addon_name(name)) {
        return Err("Addons.zip contains an invalid addon folder".into());
    }
    let old = read_installed_addons(app, game_dir)?.unwrap_or(InstalledAddonsManifest {
        version: 1,
        game_path: canonical_game_path(game_dir)?,
        addons: Vec::new(),
    });

    let rollback = addons_dir.join(format!(".rx-rollback-{}", std::process::id()));
    if rollback.exists() {
        fs::remove_dir_all(&rollback)
            .map_err(|_| "Could not clear addon rollback directory".to_string())?;
    }
    fs::create_dir(&rollback)
        .map_err(|_| "Could not create addon rollback directory".to_string())?;
    let mut manifest_addons = Vec::new();
    let mut installed_names: Vec<String> = Vec::new();
    let mut restored_backups: Vec<(String, String)> = Vec::new();
    let mut replaced_existing: Vec<String> = Vec::new();

    let result = (|| {
        // Move every launcher-owned directory aside so all later changes can roll back.
        for addon in &old.addons {
            let current = addons_dir.join(&addon.name);
            if current.exists() {
                fs::rename(&current, rollback.join(&addon.name)).map_err(|_| {
                    format!("Could not prepare addon {} for replacement", addon.name)
                })?;
            }
        }

        // Restore user content for addons no longer shipped by the new patch.
        for addon in old
            .addons
            .iter()
            .filter(|addon| !folders.contains(&addon.name))
        {
            if let Some(backup_name) = &addon.backup_name {
                let backup = addons_dir.join(backup_name);
                if backup.exists() {
                    fs::rename(&backup, addons_dir.join(&addon.name))
                        .map_err(|_| format!("Could not restore backup for {}", addon.name))?;
                    restored_backups.push((addon.name.clone(), backup_name.clone()));
                }
            }
        }

        for name in folders {
            let current = addons_dir.join(name);
            let old_entry = old.addons.iter().find(|addon| addon.name == *name);
            let backup_name = if let Some(entry) = old_entry {
                entry.backup_name.clone()
            } else if current.exists() {
                // Project Rx addon names are reserved. Replace an existing
                // colliding folder without creating a persistent user backup;
                // the temporary rollback copy still protects this transaction
                // if a later install step fails.
                fs::rename(&current, rollback.join(name))
                    .map_err(|_| format!("Could not prepare addon {name} for replacement"))?;
                replaced_existing.push(name.clone());
                None
            } else {
                None
            };

            fs::rename(stage.join(name), &current)
                .map_err(|_| format!("Could not install addon {name}"))?;
            installed_names.push(name.clone());
            manifest_addons.push(InstalledAddon {
                name: name.clone(),
                backup_name,
            });
        }
        let manifest = InstalledAddonsManifest {
            version: 1,
            game_path: canonical_game_path(game_dir)?,
            addons: manifest_addons,
        };
        save_installed_addons(app, &manifest)
    })();

    if let Err(error) = result {
        for name in installed_names.iter().rev() {
            let current = addons_dir.join(name);
            if current.exists() {
                fs::remove_dir_all(&current).ok();
            }
        }
        for (name, backup_name) in restored_backups.iter().rev() {
            let restored = addons_dir.join(name);
            if restored.exists() {
                fs::rename(restored, addons_dir.join(backup_name)).ok();
            }
        }
        for name in replaced_existing.iter().rev() {
            let replaced = rollback.join(name);
            if replaced.exists() && !addons_dir.join(name).exists() {
                fs::rename(replaced, addons_dir.join(name)).ok();
            }
        }
        for addon in &old.addons {
            let prior = rollback.join(&addon.name);
            if prior.exists() && !addons_dir.join(&addon.name).exists() {
                fs::rename(prior, addons_dir.join(&addon.name)).ok();
            }
        }
        fs::remove_dir_all(&rollback).ok();
        return Err(error);
    }
    fs::remove_dir_all(&rollback)
        .map_err(|_| "Could not clean addon rollback directory".to_string())?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledContentFile {
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledContentManifest {
    version: u32,
    game_path: String,
    release: u64,
    patch_version: String,
    files: Vec<InstalledContentFile>,
}

fn installed_content_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(INSTALLED_CONTENT_FILENAME))
}

fn installed_content_matches_game(
    manifest: &InstalledContentManifest,
    canonical_path: &str,
) -> bool {
    let same_path = if cfg!(windows) {
        manifest.game_path.eq_ignore_ascii_case(canonical_path)
    } else {
        manifest.game_path == canonical_path
    };
    manifest.version == 1
        && manifest.release > 0
        && same_path
        && manifest.files.iter().all(|file| {
            content::validate_relative_content_path(&file.path).is_ok()
                && file.sha256.len() == 64
                && file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn read_installed_content(
    app: &tauri::AppHandle,
    game_dir: &Path,
) -> Result<Option<InstalledContentManifest>, String> {
    let Some(path) = installed_content_path(app) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Could not read content tracking data".into()),
    };
    let manifest: InstalledContentManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if !installed_content_matches_game(&manifest, &canonical_game_path(game_dir)?) {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn save_installed_content(
    app: &tauri::AppHandle,
    manifest: &InstalledContentManifest,
) -> Result<(), String> {
    let path = installed_content_path(app).ok_or("Could not locate launcher data directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "Could not create launcher data directory".to_string())?;
    }
    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|_| "Could not serialize content tracking data".to_string())?;
    let temp = path.with_file_name(format!(
        "{INSTALLED_CONTENT_FILENAME}.part-{}",
        std::process::id()
    ));
    if fs::write(&temp, json).is_err() {
        fs::remove_file(&temp).ok();
        return Err("Could not save content tracking data".into());
    }
    let old = path.with_file_name(format!(
        ".{INSTALLED_CONTENT_FILENAME}.rx-old-{}",
        std::process::id()
    ));
    let had_manifest = path.exists();
    if had_manifest {
        if old.exists() {
            fs::remove_file(&temp).ok();
            return Err("A previous content tracking replacement is incomplete".into());
        }
        if let Err(error) = fs::rename(&path, &old) {
            fs::remove_file(&temp).ok();
            return Err(format!("Could not replace content tracking data: {error}"));
        }
    }
    if let Err(error) = fs::rename(&temp, &path) {
        if had_manifest {
            let _ = fs::rename(&old, &path);
        }
        fs::remove_file(&temp).ok();
        return Err(format!("Could not finalize content tracking data: {error}"));
    }
    if had_manifest {
        fs::remove_file(&old).ok();
    }
    Ok(())
}

fn delete_installed_content(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(path) = installed_content_path(app) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Could not remove content tracking data".into()),
        }
    }
    Ok(())
}

fn remove_obsolete_content(
    game_dir: &Path,
    old: Option<&InstalledContentManifest>,
    current: &InstalledContentManifest,
) -> Result<Vec<String>, String> {
    let Some(old) = old else {
        return Ok(Vec::new());
    };
    let mut preserved = Vec::new();
    for file in &old.files {
        if current
            .files
            .iter()
            .any(|current_file| current_file.path.eq_ignore_ascii_case(&file.path))
        {
            continue;
        }
        let path = resolve_content_path(game_dir, &file.path)?;
        if !path.exists() {
            continue;
        }
        let actual = match file_hash_and_size(&path) {
            Ok((_, hash)) => hash,
            Err(_) => {
                preserved.push(file.path.clone());
                continue;
            }
        };
        if actual == file.sha256.to_ascii_lowercase() {
            fs::remove_file(&path)
                .map_err(|_| format!("Could not remove obsolete Project Rx file {}", file.path))?;
        } else {
            preserved.push(file.path.clone());
        }
    }
    Ok(preserved)
}

#[tauri::command]
fn uninstall_patch(app: tauri::AppHandle, game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let manifest = read_installed_addons(&app, &dir)?;
    let content_manifest = read_installed_content(&app, &dir)?;

    let rx_wow = dir.join("rx-wow.exe");
    if rx_wow.exists() {
        if !rx_wow.is_file() {
            return Err("rx-wow.exe is not a regular file".into());
        }
        fs::remove_file(&rx_wow).map_err(|_| {
            "Could not remove rx-wow.exe. Make sure the game is closed.".to_string()
        })?;
    }

    let mut preserved_content = Vec::new();
    if let Some(mut content_manifest) = content_manifest {
        let mut remaining = Vec::new();
        for file in &content_manifest.files {
            let path = resolve_content_path(&dir, &file.path)?;
            if !path.exists() {
                continue;
            }
            let current_hash = file_hash_and_size(&path).ok().map(|(_, hash)| hash);
            if current_hash.is_some_and(|hash| hash == file.sha256.to_ascii_lowercase()) {
                fs::remove_file(&path).map_err(|_| format!("Failed to remove {}", file.path))?;
            } else {
                preserved_content.push(file.path.clone());
                remaining.push(file.clone());
            }
        }
        if remaining.is_empty() {
            delete_installed_content(&app)?;
        } else {
            content_manifest.files = remaining;
            save_installed_content(&app, &content_manifest)?;
        }
    } else {
        // Preserve uninstall compatibility with launcher versions that used
        // the former fixed MPQ list before installed_content.json existed.
        for relative in LEGACY_PATCH_PATHS {
            let path = resolve_content_path(&dir, relative)?;
            if path.exists() {
                fs::remove_file(&path).map_err(|_| format!("Failed to remove {relative}"))?;
            }
        }
    }

    // Remove installed addon folders; restore backups if they exist
    let addons_dir = dir.join("Interface").join("AddOns");
    if let Some(mut manifest) = manifest {
        let mut remaining = Vec::new();
        let mut first_error = None;
        for addon in &manifest.addons {
            if let Err(error) = restore_tracked_addon(&addons_dir, addon) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                remaining.push(addon.clone());
            }
        }
        if remaining.is_empty() {
            delete_installed_addons(&app)?;
        } else {
            manifest.addons = remaining;
            save_installed_addons(&app, &manifest)?;
        }
        if let Some(error) = first_error {
            return Err(format!(
                "Uninstall was incomplete: {error}. Tracking data was retained for retry."
            ));
        }
    }

    if preserved_content.is_empty() {
        Ok("Project Rx content removed".into())
    } else {
        Ok(format!(
            "Project Rx content removed; preserved user-modified files: {}",
            preserved_content.join(", ")
        ))
    }
}

// ── Patch integrity check ────────────────────────────────────

#[tauri::command]
async fn verify_patch(
    app: tauri::AppHandle,
    http: tauri::State<'_, HttpClient>,
    game_path: String,
) -> Result<Vec<String>, String> {
    let dir = validate_game_path(&game_path)?;
    let manifest = fetch_content_manifest(&app, &http.0).await?;
    let installed_addons = read_installed_addons(&app, &dir)?;
    let expected_source = manifest.executable_patch.source_sha256.to_ascii_lowercase();
    let expected_source_size = manifest.executable_patch.source_size;
    let expected_output = manifest.executable_patch.output_sha256.to_ascii_lowercase();
    let expected_output_size = manifest.executable_patch.output_size;

    // Run hashing on a blocking thread to avoid stalling the async runtime
    tauri::async_runtime::spawn_blocking(move || {
        let mut bad: Vec<String> = Vec::new();
        let source = dir.join("Wow.exe");
        match file_hash_and_size(&source) {
            Ok((size, hash)) if size == expected_source_size && hash == expected_source => {}
            _ => bad.push("Wow.exe (unsupported or corrupted source)".into()),
        }
        let output = dir.join("rx-wow.exe");
        match file_hash_and_size(&output) {
            Ok((size, hash)) if size == expected_output_size && hash == expected_output => {}
            _ => bad.push("rx-wow.exe (missing or corrupted)".into()),
        }
        for file in &manifest.files {
            if file.kind == content::ContentFileKind::AddonsZip {
                let ready = installed_addons.as_ref().is_some_and(|installed| {
                    !installed.addons.is_empty()
                        && installed.addons.iter().all(|addon| {
                            dir.join("Interface")
                                .join("AddOns")
                                .join(&addon.name)
                                .is_dir()
                        })
                });
                if !ready {
                    bad.push(format!("{} (missing or incomplete)", file.path));
                }
                continue;
            }
            let path = match resolve_content_path(&dir, &file.path) {
                Ok(path) => path,
                Err(_) => {
                    bad.push(format!("{} (unsafe path)", file.path));
                    continue;
                }
            };
            if let Some(problem) = classify_patch_file(&path, &file.path, &file.sha256) {
                bad.push(problem);
            }
        }
        Ok(bad)
    })
    .await
    .unwrap_or_else(|_| Err("Verification failed".into()))
}

// ── Realmlist (needs arbitrary path access — stays in Rust) ─

#[tauri::command]
fn check_realmlist(game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let base = dir.join("Data");
    let locales = ["enUS", "enGB"];

    for locale in &locales {
        let realmlist_path = base.join(locale).join("realmlist.wtf");
        if realmlist_path.exists() {
            let content = fs::read_to_string(&realmlist_path)
                .map_err(|_| "Failed to read realmlist".to_string())?;
            return if content.contains("projectrx.net") {
                Ok("OK — realmlist is correct".into())
            } else {
                Err("Realmlist points elsewhere".into())
            };
        }
    }

    Err("realmlist.wtf not found".into())
}

#[tauri::command]
fn patch_realmlist(game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let base = dir.join("Data");
    if !base.is_dir() {
        return Err("Data folder not found. Make sure you selected a valid game directory.".into());
    }

    let locales = ["enUS", "enGB"];
    let mut patched = false;
    let mut create_in: Option<PathBuf> = None;

    for locale in &locales {
        let locale_dir = base.join(locale);
        let realmlist_path = locale_dir.join("realmlist.wtf");
        if realmlist_path.exists() {
            fs::write(&realmlist_path, REALMLIST_VALUE)
                .map_err(|_| "Failed to write realmlist".to_string())?;
            patched = true;
        } else if create_in.is_none() && locale_dir.is_dir() {
            // Prefer an existing locale directory when the file itself is
            // missing; this matches the normal WoW installation layout.
            create_in = Some(locale_dir);
        }
    }

    if patched {
        Ok("Realmlist set successfully".into())
    } else {
        // A valid WoW installation can have the locale directory without a
        // realmlist file. Create it in the first supported locale, defaulting
        // to enUS when neither locale directory exists yet.
        let locale_dir = create_in.unwrap_or_else(|| base.join("enUS"));
        fs::create_dir_all(&locale_dir)
            .map_err(|_| "Failed to create realmlist directory".to_string())?;
        fs::write(locale_dir.join("realmlist.wtf"), REALMLIST_VALUE)
            .map_err(|_| "Failed to write realmlist".to_string())?;
        Ok("Realmlist created and set successfully".into())
    }
}

// ── Open URL (allowlisted domains only) ─────────────────────

const ALLOWED_DOMAINS: &[&str] = &[
    "projectrx.net",
    "www.projectrx.net",
    "discord.gg",
    "discord.com",
];

fn is_allowed_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|parsed| {
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port().is_none()
            && parsed
                .host_str()
                .is_some_and(|host| ALLOWED_DOMAINS.contains(&host))
    })
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_string())?;
    if !is_allowed_url(parsed.as_str()) {
        return Err("Only allowlisted Project Rx HTTPS URLs may be opened".into());
    }

    // Use ShellExecuteW via the `open` crate — no cmd.exe shell parsing,
    // so URL path/query/fragment cannot inject commands.
    open::that(parsed.as_str()).map_err(|_| "Failed to open URL".to_string())?;

    Ok(())
}

// ── App entry ───────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("Failed to create HTTP client");

    tauri::Builder::default()
        .manage(HttpClient(http_client))
        .setup(|app| {
            #[cfg(windows)]
            match AppInstanceMutex::acquire()? {
                Some(mutex) => app.manage(mutex),
                None => {
                    app.handle().exit(0);
                    return Ok(());
                }
            };

            let window = app.get_webview_window("main").unwrap();

            #[cfg(target_os = "windows")]
            window_vibrancy::apply_acrylic(&window, Some((9, 7, 5, 220)))
                .expect("Failed to apply acrylic");

            // System tray
            let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &quit_i])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Project Rx Launcher")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app: &tauri::AppHandle, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray: &tauri::tray::TrayIcon, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_launcher_config,
            get_patch_manifest,
            check_game_directory,
            check_server_status,
            check_launcher_update,
            apply_launcher_update,
            get_news,
            launch_game,
            download_patch,
            uninstall_patch,
            check_realmlist,
            patch_realmlist,
            open_url,
            get_changelog,
            verify_patch,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rx-launcher-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn realm_status_parses_zero_and_positive_counts() {
        assert_eq!(
            parse_realm_status(br#"{"online":true,"players":0}"#).unwrap(),
            ServerStatus {
                online: true,
                players: Some(0)
            }
        );
        assert_eq!(
            parse_realm_status(br#"{"online":true,"players":42}"#).unwrap(),
            ServerStatus {
                online: true,
                players: Some(42)
            }
        );
    }

    #[test]
    fn realm_status_rejects_malformed_and_oversized_data() {
        assert!(parse_realm_status(b"not json").is_err());
        assert!(parse_realm_status(&vec![b' '; REALM_STATUS_MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn verified_file_replacement_is_atomic_for_regular_files() {
        let temp = TestDir::new();
        let destination = temp.0.join("patch.MPQ");
        let part = temp.0.join("patch.MPQ.part");
        fs::write(&destination, b"old").unwrap();
        fs::write(&part, b"new").unwrap();
        replace_verified_file(&part, &destination, "patch.MPQ").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!part.exists());
        assert!(!temp
            .0
            .join(format!(".patch.MPQ.rx-old-{}", std::process::id()))
            .exists());
    }

    #[test]
    fn tcp_probe_handles_reachable_and_unreachable_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let reachable = listener.local_addr().unwrap();
        assert!(tcp_probe_addr(reachable, Duration::from_secs(1)));

        let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable_addr = unavailable.local_addr().unwrap();
        drop(unavailable);
        assert!(!tcp_probe_addr(
            unavailable_addr,
            Duration::from_millis(100)
        ));
    }

    #[test]
    fn zip_extraction_rejects_traversal_and_absolute_paths() {
        for unsafe_name in ["../outside.txt", "/absolute.txt"] {
            let temp = TestDir::new();
            let zip_path = temp.0.join("addons.zip");
            let stage = temp.0.join("stage");
            fs::create_dir(&stage).unwrap();
            make_zip(&zip_path, &[(unsafe_name, b"bad")]);
            assert!(extract_zip_file(&zip_path, &stage).is_err());
        }
    }

    #[test]
    fn zip_extraction_accepts_nested_files_and_deduplicates_roots() {
        let temp = TestDir::new();
        let zip_path = temp.0.join("addons.zip");
        let stage = temp.0.join("stage");
        fs::create_dir(&stage).unwrap();
        make_zip(
            &zip_path,
            &[
                ("Alpha/one.lua", b"one"),
                ("Alpha/two.lua", b"two"),
                ("Beta/main.lua", b"main"),
            ],
        );
        let roots = extract_zip_file(&zip_path, &stage).unwrap();
        assert_eq!(roots, vec!["Alpha", "Beta"]);
        assert_eq!(fs::read(stage.join("Alpha/one.lua")).unwrap(), b"one");
    }

    #[test]
    fn path_bound_manifest_rejects_other_paths_and_unsafe_names() {
        let mut manifest = InstalledAddonsManifest {
            version: 1,
            game_path: "C:\\Game".into(),
            addons: vec![InstalledAddon {
                name: "Addon".into(),
                backup_name: Some("Addon_rx_backup".into()),
            }],
        };
        assert!(manifest_matches_game(&manifest, "C:\\Game"));
        assert!(!manifest_matches_game(&manifest, "D:\\Other"));
        manifest.addons[0].name = "../Addon".into();
        assert!(!manifest_matches_game(&manifest, "C:\\Game"));
    }

    #[test]
    fn url_allowlist_requires_https_and_exact_host() {
        assert!(is_allowed_url("https://projectrx.net/news"));
        assert!(is_allowed_url("https://www.projectrx.net/news"));
        assert!(is_allowed_url("https://discord.gg/VTWnWbqJYE"));
        assert!(!is_allowed_url("http://projectrx.net/news"));
        assert!(!is_allowed_url("https://projectrx.net.evil.example/news"));
        assert!(!is_allowed_url("https://user:pass@projectrx.net/news"));
        assert!(!is_allowed_url("https://projectrx.net:444/news"));
    }

    #[test]
    fn validates_game_paths_and_classifies_patch_files() {
        assert!(validate_game_path("relative/path").is_err());
        let temp = TestDir::new();
        assert!(validate_game_path(temp.0.to_str().unwrap()).is_ok());

        let patch = temp.0.join("patch.MPQ");
        assert_eq!(
            classify_patch_file(&patch, "patch.MPQ", "x").unwrap(),
            "patch.MPQ (missing)"
        );
        fs::write(&patch, b"contents").unwrap();
        assert!(classify_patch_file(&patch, "patch.MPQ", "wrong")
            .unwrap()
            .contains("corrupted"));
        let actual = sha256_file(&patch).unwrap();
        assert!(classify_patch_file(&patch, "patch.MPQ", &actual).is_none());
    }

    #[test]
    fn patch_realmlist_creates_missing_file_in_default_locale() {
        let temp = TestDir::new();
        fs::create_dir(temp.0.join("Data")).unwrap();

        let result = patch_realmlist(temp.0.to_string_lossy().into_owned()).unwrap();

        assert_eq!(result, "Realmlist created and set successfully");
        assert_eq!(
            fs::read_to_string(temp.0.join("Data").join("enUS").join("realmlist.wtf")).unwrap(),
            REALMLIST_VALUE
        );
    }
}
