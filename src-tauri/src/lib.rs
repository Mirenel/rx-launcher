use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Emitter;
use tauri::Manager;

pub mod content;
pub mod exe_patch;
mod runtime;
mod update;
#[cfg(any(windows, test))]
mod update_auth;
#[cfg(any(windows, test))]
mod update_payload;

const REALMLIST_VALUE: &str = "set realmlist projectrx.net";
const SERVER_HOST: &str = "projectrx.net";
const SERVER_PORT: u16 = 3724;
const REALM_STATUS_URL: &str = "https://projectrx.net/api/realm-status/public";
const REALM_STATUS_MAX_BYTES: usize = 16 * 1024;
const HTTP_ALLOWED_DOMAINS: &[&str] = &[
    "projectrx.net",
    "www.projectrx.net",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const PATCH_DOWNLOAD_MAX_BYTES: u64 = 512 * 1024 * 1024;
const ADDONS_EXTRACT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const ADDONS_MAX_ENTRIES: usize = 10_000;
const CONTENT_CACHE_FILENAME: &str = "content-manifest.json";
const INSTALLED_CONTENT_FILENAME: &str = "installed_content.json";

struct HttpClient(reqwest::Client);

#[derive(Clone)]
struct OperationLock {
    lock: Arc<tokio::sync::Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
    exit_permitted: Arc<AtomicBool>,
}

impl Default for OperationLock {
    fn default() -> Self {
        Self {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            exit_permitted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl OperationLock {
    async fn acquire(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.lock.clone().lock_owned().await
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    fn permit_exit(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.exit_permitted.store(true, Ordering::SeqCst);
    }

    fn exit_is_permitted(&self) -> bool {
        self.exit_permitted.load(Ordering::SeqCst)
    }

    async fn try_acquire(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("Launcher shutdown is already in progress".into());
        }
        let operation = self
            .lock
            .clone()
            .try_lock_owned()
            .map_err(|_| "Another game operation is already in progress".to_string())?;
        if self.shutting_down.load(Ordering::SeqCst) {
            drop(operation);
            return Err("Launcher shutdown is already in progress".into());
        }
        Ok(operation)
    }
}

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
        if let Component::Normal(value) = component {
            content::validate_windows_component(&value.to_string_lossy())
                .map_err(|_| "Game directory path contains an unsafe Windows name".to_string())?;
        }
    }

    if !path.is_dir() {
        return Err("Game directory not found".into());
    }

    if path_contains_link_or_reparse_point(&path)? {
        return Err("Game directory cannot contain symbolic links or reparse points".into());
    }

    Ok(path)
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    let is_link = metadata.file_type().is_symlink();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        return is_link || metadata.file_attributes() & 0x400 != 0;
    }
    #[cfg(not(windows))]
    {
        is_link
    }
}

fn path_contains_link_or_reparse_point(path: &Path) -> Result<bool, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_or_reparse_point(&metadata) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err("Could not inspect game directory".into()),
        }
    }
    Ok(false)
}

fn ensure_safe_path_components(path: &Path) -> Result<(), String> {
    if path_contains_link_or_reparse_point(path)? {
        return Err("Game file path contains a symbolic link or reparse point".into());
    }
    Ok(())
}

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
async fn apply_launcher_update(
    app: tauri::AppHandle,
    operations: tauri::State<'_, OperationLock>,
) -> Result<(), String> {
    let _operation = operations.try_acquire().await?;
    let on_exit = {
        let operations = operations.inner().clone();
        move || operations.permit_exit()
    };
    update::check_for_update_and_apply(app, on_exit).await
}

#[tauri::command]
async fn request_exit(
    app: tauri::AppHandle,
    operations: tauri::State<'_, OperationLock>,
) -> Result<(), String> {
    operations.begin_shutdown();
    let _operation = operations.acquire().await;
    operations.permit_exit();
    app.exit(0);
    Ok(())
}

fn schedule_exit(app: &tauri::AppHandle) {
    let app = app.clone();
    let operations = app.state::<OperationLock>().inner().clone();
    operations.begin_shutdown();
    tauri::async_runtime::spawn(async move {
        let _operation = operations.acquire().await;
        operations.permit_exit();
        app.exit(0);
    });
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
async fn check_game_runtime(wine_prefix: Option<String>) -> runtime::GameRuntimeStatus {
    tauri::async_runtime::spawn_blocking(move || runtime::status(wine_prefix.as_deref()))
        .await
        .unwrap_or_else(|_| runtime::unavailable_status("Could not check the game runtime"))
}

#[tauri::command]
fn check_game_directory(game_path: String) -> Result<GameDirectoryStatus, String> {
    let dir = validate_game_path(&game_path)?;
    let wow = dir.join("Wow.exe");
    let data = dir.join("Data");
    let addons = dir.join("Interface").join("AddOns");
    let rx_wow = dir.join("rx-wow.exe");
    Ok(GameDirectoryStatus {
        has_wow: wow.is_file() && ensure_safe_path_components(&wow).is_ok(),
        has_data: data.is_dir() && ensure_safe_path_components(&data).is_ok(),
        has_addons: addons.is_dir() && ensure_safe_path_components(&addons).is_ok(),
        has_rx_wow: rx_wow.is_file() && ensure_safe_path_components(&rx_wow).is_ok(),
    })
}

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

#[tauri::command]
async fn launch_game(
    app: tauri::AppHandle,
    http: tauri::State<'_, HttpClient>,
    operations: tauri::State<'_, OperationLock>,
    game_path: String,
    wine_prefix: Option<String>,
) -> Result<(), String> {
    let _operation = operations.try_acquire().await?;
    let dir = validate_game_path(&game_path)?;
    let wow_exe = dir.join("rx-wow.exe");
    ensure_safe_path_components(&wow_exe)?;

    let metadata = fs::symlink_metadata(&wow_exe).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "rx-wow.exe not found. Patch the game before launching.".to_string()
        } else {
            "Could not inspect rx-wow.exe.".to_string()
        }
    })?;
    if !metadata.is_file() {
        return Err("rx-wow.exe is not a regular file.".into());
    }

    let manifest = fetch_content_manifest(&app, &http.0).await?;
    let installed_addons = read_installed_addons(&app, &dir)?;
    let verify_dir = dir.clone();
    let bad = tauri::async_runtime::spawn_blocking(move || {
        verify_patch_files(&verify_dir, &manifest, installed_addons.as_ref())
    })
    .await
    .map_err(|_| "Could not verify the game before launching".to_string())?;
    if !bad.is_empty() {
        return Err(format!(
            "Game integrity verification failed: {}",
            bad.join(", ")
        ));
    }

    runtime::launch(&wow_exe, &dir, wine_prefix.as_deref())
}

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
    ensure_safe_path_components(destination)?;
    ensure_safe_path_components(part)?;
    let destination_metadata = fs::symlink_metadata(destination).ok();
    if destination_metadata
        .as_ref()
        .is_some_and(is_link_or_reparse_point)
    {
        return Err(format!(
            "Cannot replace {label}: the destination is a symbolic link or reparse point"
        ));
    }
    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_file())
    {
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
    if fs::symlink_metadata(&backup).is_ok() {
        return Err(format!(
            "Cannot replace {label}: a previous replacement is incomplete"
        ));
    }
    let had_destination = destination_metadata.is_some();
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

/// Copy a file through a newly-created inode. In particular, never let a
/// copy operation truncate an existing destination inode: an addon backup can
/// contain a hard link to a protected game executable.
fn copy_file_atomically(source: &Path, destination: &Path, label: &str) -> Result<(), String> {
    ensure_safe_path_components(source)?;
    ensure_safe_path_components(destination)?;
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|_| format!("Could not inspect the source for {label}"))?;
    if is_link_or_reparse_point(&source_metadata) || !source_metadata.is_file() {
        return Err(format!(
            "Could not copy {label}: the source is not a regular file"
        ));
    }
    if let Ok(destination_metadata) = fs::symlink_metadata(destination) {
        if is_link_or_reparse_point(&destination_metadata) || !destination_metadata.is_file() {
            return Err(format!(
                "Could not copy {label}: the destination is not a regular file"
            ));
        }
    }
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let temp = destination.with_file_name(format!(".{name}.rx-copy-{}", std::process::id()));
    if fs::symlink_metadata(&temp).is_ok() {
        return Err(format!(
            "Could not copy {label}: a previous copy is incomplete"
        ));
    }
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent).map_err(|_| format!("Could not prepare {label}"))?;
    }
    let mut source_file = fs::File::open(source)
        .map_err(|error| format!("Could not open the source for {label}: {error}"))?;
    let mut temp_file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
    {
        Ok(file) => file,
        Err(error) => {
            return Err(format!(
                "Could not create the temporary copy for {label}: {error}"
            ));
        }
    };
    if let Err(error) =
        std::io::copy(&mut source_file, &mut temp_file).and_then(|_| temp_file.sync_all())
    {
        drop(temp_file);
        fs::remove_file(&temp).ok();
        return Err(format!("Could not copy {label}: {error}"));
    }
    drop(temp_file);
    if let Err(error) = replace_verified_file(&temp, destination, label) {
        fs::remove_file(&temp).ok();
        return Err(error);
    }
    Ok(())
}

fn write_file_atomically(destination: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    ensure_safe_path_components(destination)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| format!("Could not create the directory for {label}"))?;
    }
    ensure_safe_path_components(destination)?;

    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let temp = destination.with_file_name(format!(".{name}.rx-write-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| format!("Could not create the temporary file for {label}"))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        fs::remove_file(&temp).ok();
        return Err(format!("Could not save {label}: {error}"));
    }
    drop(file);
    if let Err(error) = replace_verified_file(&temp, destination, label) {
        fs::remove_file(&temp).ok();
        return Err(error);
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
        ensure_safe_path_components(&resolved)
            .map_err(|_| format!("Content path contains a link or reparse point: {relative}"))?;
    }
    Ok(resolved)
}

fn file_hash_and_size(path: &Path) -> Result<(u64, String), String> {
    let link_metadata =
        fs::symlink_metadata(path).map_err(|_| "Could not read the game executable".to_string())?;
    if is_link_or_reparse_point(&link_metadata) {
        return Err("The game file cannot be a symbolic link or reparse point".into());
    }
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
    ensure_safe_path_components(part_path)?;
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

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part_path)
        .map_err(|error| {
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
    ensure_safe_path_components(&source)?;
    ensure_safe_path_components(&output)?;
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
    ensure_safe_path_components(&build_path)?;
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
    operations: tauri::State<'_, OperationLock>,
    game_path: String,
    repair: bool,
) -> Result<String, String> {
    let _operation = operations.try_acquire().await?;
    let dir = validate_game_path(&game_path)?;
    let data_dir = dir.join("Data");
    let addons_dir = dir.join("Interface").join("AddOns");

    if !data_dir.is_dir() || ensure_safe_path_components(&data_dir).is_err() {
        return Err("Data folder not found. Make sure you selected a valid game directory.".into());
    }
    if !addons_dir.is_dir() || ensure_safe_path_components(&addons_dir).is_err() {
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

        let skip = if *extract {
            let installed = read_installed_addons(&app, &dir)?;
            !repair
                && previous_content.as_ref().is_some_and(|content| {
                    content.release == manifest.release && content.patch_version == manifest.version
                })
                && installed.as_ref().is_some_and(|installed_manifest| {
                    !installed_manifest.addons.is_empty()
                        && installed_manifest
                            .addons
                            .iter()
                            .all(|addon| addon_files_match(&addons_dir, addon))
                        && installed_manifest.release == manifest.release
                        && installed_manifest.patch_version == manifest.version
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
                install_staged_addons(
                    &app,
                    &dir,
                    &addons_dir,
                    &stage,
                    &addon_folders,
                    manifest.release,
                    &manifest.version,
                )
            })();
            fs::remove_dir_all(&stage).ok();
            fs::remove_file(&part_path).ok();
            if install_result? {
                emit_progress(
                    &app,
                    completed_bytes,
                    total_bytes,
                    "Migrated legacy addon tracking; legacy folders and addon folders not in the current signed release were preserved as unmanaged content.".into(),
                    true,
                    completed_bytes,
                    total_bytes,
                );
            }
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
        if name.to_string_lossy().contains('\\') {
            return Err("Addons.zip contains an invalid path separator".into());
        }
        for component in name.components() {
            if let Component::Normal(value) = component {
                content::validate_windows_component(&value.to_string_lossy())?;
            }
        }
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

const INSTALLED_ADDONS_MANIFEST_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledAddonFile {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledAddon {
    name: String,
    backup_name: Option<String>,
    files: Vec<InstalledAddonFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstalledAddonsManifest {
    version: u32,
    game_path: String,
    release: u64,
    patch_version: String,
    addons: Vec<InstalledAddon>,
}

fn state_file_path(app: &tauri::AppHandle, prefix: &str, canonical_path: &str) -> Option<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(canonical_path.as_bytes());
    let key = format!("{:x}", hasher.finalize());
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(format!("{prefix}-{key}.json")))
}

fn installed_addons_path(app: &tauri::AppHandle, canonical_path: &str) -> Option<PathBuf> {
    state_file_path(app, "installed-addons", canonical_path)
}

fn legacy_state_path(app: &tauri::AppHandle, filename: &str) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(filename))
}

fn legacy_installed_addons_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    legacy_state_path(app, "installed_addons.json")
}

#[derive(Deserialize)]
struct LegacyInstalledAddon {
    name: String,
    backup_name: Option<String>,
}

#[derive(Deserialize)]
struct LegacyInstalledAddonsManifest {
    version: u32,
    game_path: String,
    addons: Vec<LegacyInstalledAddon>,
}

fn legacy_addons_match_game(
    manifest: &LegacyInstalledAddonsManifest,
    canonical_path: &str,
) -> bool {
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

fn legacy_addons_for_game(
    app: &tauri::AppHandle,
    game_dir: &Path,
) -> Result<Option<LegacyInstalledAddonsManifest>, String> {
    let canonical_path = canonical_game_path(game_dir)?;
    let Some(path) = legacy_installed_addons_path(app) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Could not read legacy addon tracking data".into()),
    };
    let manifest: LegacyInstalledAddonsManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if legacy_addons_match_game(&manifest, &canonical_path) {
        Ok(Some(manifest))
    } else {
        Ok(None)
    }
}

fn retire_legacy_addons(app: &tauri::AppHandle, game_dir: &Path) {
    let Ok(canonical_path) = canonical_game_path(game_dir) else {
        return;
    };
    let Some(path) = legacy_installed_addons_path(app) else {
        return;
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(manifest) = serde_json::from_str::<LegacyInstalledAddonsManifest>(&text) else {
        return;
    };
    if legacy_addons_match_game(&manifest, &canonical_path) {
        fs::remove_file(path).ok();
    }
}

fn canonical_game_path(game_dir: &Path) -> Result<String, String> {
    fs::canonicalize(game_dir)
        .map_err(|_| "Could not resolve game directory".to_string())?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Invalid game directory encoding".to_string())
}

fn validate_addon_name(name: &str) -> bool {
    content::validate_windows_component(name).is_ok()
        && !name
            .chars()
            .any(|character| character == '/' || character == '\\')
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

fn validate_addon_file_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|component| content::validate_windows_component(component).is_ok())
}

fn addon_manifest_files_valid(files: &[InstalledAddonFile]) -> bool {
    !files.is_empty()
        && files.iter().all(|file| {
            validate_addon_file_path(&file.path)
                && file.sha256.len() == 64
                && file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        && files
            .iter()
            .enumerate()
            .all(|(index, file)| files[..index].iter().all(|old| old.path != file.path))
}

fn manifest_matches_game(manifest: &InstalledAddonsManifest, canonical_path: &str) -> bool {
    let same_path = if cfg!(windows) {
        manifest.game_path.eq_ignore_ascii_case(canonical_path)
    } else {
        manifest.game_path == canonical_path
    };
    manifest.version == INSTALLED_ADDONS_MANIFEST_VERSION
        && same_path
        && manifest.release > 0
        && !manifest.patch_version.is_empty()
        && manifest.addons.iter().all(|addon| {
            validate_addon_name(&addon.name)
                && addon.backup_name.as_deref().is_none_or(validate_addon_name)
                && addon_manifest_files_valid(&addon.files)
        })
}

fn read_installed_addons(
    app: &tauri::AppHandle,
    game_dir: &Path,
) -> Result<Option<InstalledAddonsManifest>, String> {
    let canonical_path = canonical_game_path(game_dir)?;
    let Some(path) = installed_addons_path(app, &canonical_path) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Could not read addon tracking data".into()),
    };
    let manifest: InstalledAddonsManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => {
            // Legacy or malformed tracking is non-authoritative, so file
            // operations never rely on it.
            return Ok(None);
        }
    };
    if !manifest_matches_game(&manifest, &canonical_path) {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn save_installed_addons(
    app: &tauri::AppHandle,
    manifest: &InstalledAddonsManifest,
) -> Result<(), String> {
    let path = installed_addons_path(app, &manifest.game_path)
        .ok_or("Could not locate launcher data directory")?;
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

fn delete_installed_addons(app: &tauri::AppHandle, game_dir: &Path) -> Result<(), String> {
    let canonical_path = canonical_game_path(game_dir)?;
    if let Some(path) = installed_addons_path(app, &canonical_path) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Could not remove addon tracking data".into()),
        }
    }
    Ok(())
}

fn collect_addon_files(root: &Path) -> Result<Vec<InstalledAddonFile>, String> {
    fn visit(
        current: &Path,
        prefix: &str,
        files: &mut Vec<InstalledAddonFile>,
    ) -> Result<(), String> {
        let metadata = fs::symlink_metadata(current)
            .map_err(|_| "Could not inspect installed addon files".to_string())?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
            return Err("Installed addon contains an unsafe path".into());
        }
        for entry in fs::read_dir(current)
            .map_err(|_| "Could not inspect installed addon files".to_string())?
        {
            let entry = entry.map_err(|_| "Could not inspect installed addon files".to_string())?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| "Installed addon contains an invalid file name".to_string())?
                .to_owned();
            content::validate_windows_component(&name)?;
            let path = entry.path();
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "Could not inspect installed addon files".to_string())?;
            if is_link_or_reparse_point(&metadata) {
                return Err("Installed addon contains a symbolic link or reparse point".into());
            }
            if metadata.is_dir() {
                visit(&path, &relative, files)?;
            } else if metadata.is_file() {
                let (size, sha256) = file_hash_and_size(&path)?;
                files.push(InstalledAddonFile {
                    path: relative,
                    size,
                    sha256,
                });
            } else {
                return Err("Installed addon contains an unsupported file type".into());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, "", &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn addon_files_match(addons_dir: &Path, addon: &InstalledAddon) -> bool {
    let installed = addons_dir.join(&addon.name);
    if ensure_safe_path_components(&installed).is_err() || !installed.is_dir() {
        return false;
    }
    let Ok(actual) = collect_addon_files(&installed) else {
        return false;
    };
    actual.len() == addon.files.len()
        && actual.iter().zip(&addon.files).all(|(actual, expected)| {
            actual.path == expected.path
                && actual.size == expected.size
                && actual.sha256.eq_ignore_ascii_case(&expected.sha256)
        })
}

fn addon_file_path(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |mut path, part| {
            path.push(part);
            path
        })
}

/// Preserve only files that a user changed or added to a launcher-owned addon.
/// The backup is an overlay: the original user addon, when present, remains
/// intact and these files replace or extend it when the addon is uninstalled.
fn preserve_modified_addon_files(
    addons_dir: &Path,
    current: &Path,
    addon: &InstalledAddon,
) -> Result<(Option<String>, bool), String> {
    ensure_safe_path_components(current)?;
    let actual = collect_addon_files(current)?;
    let modified: Vec<InstalledAddonFile> = actual
        .into_iter()
        .filter(|file| {
            !addon.files.iter().any(|expected| {
                expected.path == file.path
                    && expected.size == file.size
                    && expected.sha256.eq_ignore_ascii_case(&file.sha256)
            })
        })
        .collect();
    if modified.is_empty() {
        return Ok((addon.backup_name.clone(), false));
    }

    let backup_name = addon
        .backup_name
        .clone()
        .unwrap_or_else(|| format!(".rx-user-backup-{}", addon.name));
    let backup_root = addons_dir.join(&backup_name);
    let created_backup = match fs::symlink_metadata(&backup_root) {
        Ok(metadata) => {
            if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                return Err(format!("Could not prepare addon {} backup", addon.name));
            }
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_safe_path_components(&backup_root)?;
            fs::create_dir(&backup_root)
                .map_err(|_| format!("Could not create addon {} backup", addon.name))?;
            true
        }
        Err(_) => return Err(format!("Could not inspect addon {} backup", addon.name)),
    };

    let copy_result = (|| {
        for file in &modified {
            let source = addon_file_path(current, &file.path);
            let destination = addon_file_path(&backup_root, &file.path);
            ensure_safe_path_components(&source)?;
            if let Some(parent) = destination.parent() {
                ensure_safe_path_components(parent)?;
                fs::create_dir_all(parent)
                    .map_err(|_| format!("Could not preserve addon file {}", file.path))?;
            }
            ensure_safe_path_components(&destination)?;
            if let Ok(metadata) = fs::symlink_metadata(&destination) {
                if is_link_or_reparse_point(&metadata) || metadata.is_dir() {
                    return Err(format!("Could not preserve addon file {}", file.path));
                }
            }
            copy_file_atomically(&source, &destination, &format!("addon file {}", file.path))?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = copy_result {
        if created_backup {
            fs::remove_dir_all(&backup_root).ok();
        }
        return Err(error);
    }

    Ok((Some(backup_name), created_backup))
}

/// Legacy tracking has no authenticated file inventory, so its active addon
/// directory cannot safely be compared with the new release. Preserve it in
/// a separate durable directory instead of modifying the original user
/// backup or treating old launcher files as user edits.
fn preserve_legacy_addon_directory(
    addons_dir: &Path,
    current: &Path,
    addon_name: &str,
) -> Result<PathBuf, String> {
    ensure_safe_path_components(current)?;
    collect_addon_files(current)?;
    let backup = addons_dir.join(format!(".rx-legacy-{addon_name}"));
    ensure_safe_path_components(&backup)?;
    if fs::symlink_metadata(&backup).is_ok() {
        return Err(format!(
            "Could not preserve legacy addon {addon_name}: its migration backup already exists"
        ));
    }
    fs::rename(current, &backup)
        .map_err(|_| format!("Could not preserve legacy addon {addon_name}"))?;
    Ok(backup)
}

fn addon_files_owned_or_missing(addons_dir: &Path, addon: &InstalledAddon) -> bool {
    let installed = addons_dir.join(&addon.name);
    if ensure_safe_path_components(&installed).is_err() || !installed.is_dir() {
        return false;
    }
    let Ok(actual) = collect_addon_files(&installed) else {
        return false;
    };
    actual.iter().all(|file| {
        addon.files.iter().any(|expected| {
            expected.path == file.path
                && expected.size == file.size
                && expected.sha256.eq_ignore_ascii_case(&file.sha256)
        })
    })
}

fn remove_empty_addon_directories(installed: &Path, addon: &InstalledAddon) -> Result<(), String> {
    let mut directories = Vec::new();
    for file in &addon.files {
        let file_path = addon_file_path(installed, &file.path);
        let mut directory = file_path.parent();
        while let Some(path) = directory {
            directories.push(path.to_path_buf());
            if path == installed {
                break;
            }
            directory = path.parent();
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        match fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(_) => {
                return Err(format!(
                    "Could not remove addon directory {}",
                    directory.display()
                ));
            }
        }
    }
    Ok(())
}

fn remove_tracked_addon_files(addons_dir: &Path, addon: &InstalledAddon) -> Result<bool, String> {
    let installed = addons_dir.join(&addon.name);
    match fs::symlink_metadata(&installed) {
        Ok(metadata) if is_link_or_reparse_point(&metadata) || !metadata.is_dir() => {
            return Ok(false);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(format!("Could not inspect addon {}", addon.name)),
    }
    if !addon_files_owned_or_missing(addons_dir, addon) {
        return Ok(false);
    }
    for file in &addon.files {
        let path = addon_file_path(&installed, &file.path);
        ensure_safe_path_components(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(format!("Could not remove addon file {}", file.path)),
        }
    }
    remove_empty_addon_directories(&installed, addon)?;
    Ok(true)
}

fn merge_addon_backup(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(|_| "Could not inspect addon backup".to_string())? {
        let entry = entry.map_err(|_| "Could not inspect addon backup".to_string())?;
        let source_path = entry.path();
        let name = entry.file_name();
        let destination_path = destination.join(&name);
        let source_metadata = fs::symlink_metadata(&source_path)
            .map_err(|_| "Could not inspect addon backup".to_string())?;
        if is_link_or_reparse_point(&source_metadata) {
            return Err("Addon backup contains a symbolic link or reparse point".into());
        }
        if source_metadata.is_dir() {
            match fs::symlink_metadata(&destination_path) {
                Ok(destination_metadata) => {
                    if is_link_or_reparse_point(&destination_metadata)
                        || !destination_metadata.is_dir()
                    {
                        return Err("Could not restore addon backup".into());
                    }
                    merge_addon_backup(&source_path, &destination_path)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::rename(&source_path, &destination_path)
                        .map_err(|_| "Could not restore addon backup".to_string())?;
                }
                Err(_) => return Err("Could not inspect addon backup".into()),
            }
            continue;
        }
        if !source_metadata.is_file() {
            return Err("Addon backup contains an unsupported file type".into());
        }
        if fs::symlink_metadata(&destination_path).is_ok() {
            return Err("Could not restore addon backup without overwriting user files".into());
        }
        fs::rename(&source_path, &destination_path)
            .map_err(|_| "Could not restore addon backup".to_string())?;
    }
    fs::remove_dir(source).map_err(|_| "Could not clean addon backup".to_string())?;
    Ok(())
}

fn restore_addon_backup(backup: &Path, installed: &Path) -> Result<(), String> {
    ensure_safe_path_components(backup)?;
    let backup_metadata =
        fs::symlink_metadata(backup).map_err(|_| "Could not inspect addon backup".to_string())?;
    if is_link_or_reparse_point(&backup_metadata) || !backup_metadata.is_dir() {
        return Err("Could not restore addon backup".into());
    }
    match fs::symlink_metadata(installed) {
        Ok(metadata) => {
            if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                return Err("Could not restore addon backup".into());
            }
            merge_addon_backup(backup, installed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(backup, installed).map_err(|_| "Could not restore addon backup".to_string())
        }
        Err(_) => Err("Could not inspect addon installation".into()),
    }
}

fn restore_tracked_addon(addons_dir: &Path, addon: &InstalledAddon) -> Result<bool, String> {
    if !remove_tracked_addon_files(addons_dir, addon)? {
        return Ok(false);
    }
    let installed = addons_dir.join(&addon.name);
    if let Some(backup_name) = &addon.backup_name {
        let backup = addons_dir.join(backup_name);
        if fs::symlink_metadata(&backup).is_ok() {
            restore_addon_backup(&backup, &installed)
                .map_err(|_| format!("Could not restore backup for {}", addon.name))?;
        }
    }
    Ok(true)
}

fn restore_moved_addons(
    addons_dir: &Path,
    rollback: &Path,
    moved_addons: &[(String, PathBuf)],
) -> Result<(), String> {
    let mut errors = Vec::new();
    for (name, source) in moved_addons.iter().rev() {
        let destination = addons_dir.join(name);
        match (source.exists(), destination.exists()) {
            (false, true) => {}
            (false, false) => errors.push(format!("moved addon {name} is missing")),
            (true, true) => errors.push(format!(
                "cannot restore moved addon {name}: destination already exists"
            )),
            (true, false) => {
                if let Err(error) = fs::rename(source, &destination) {
                    errors.push(format!("could not restore moved addon {name}: {error}"));
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    match fs::remove_dir_all(rollback) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove addon rollback: {error}")),
    }
}

fn install_staged_addons(
    app: &tauri::AppHandle,
    game_dir: &Path,
    addons_dir: &Path,
    stage: &Path,
    folders: &[String],
    release: u64,
    patch_version: &str,
) -> Result<bool, String> {
    if folders.iter().any(|name| !validate_addon_name(name)) {
        return Err("Addons.zip contains an invalid addon folder".into());
    }
    let old = read_installed_addons(app, game_dir)?.unwrap_or(InstalledAddonsManifest {
        version: INSTALLED_ADDONS_MANIFEST_VERSION,
        game_path: canonical_game_path(game_dir)?,
        release,
        patch_version: patch_version.into(),
        addons: Vec::new(),
    });
    let legacy_addons = legacy_addons_for_game(app, game_dir)?;

    let rollback = addons_dir.join(format!(".rx-rollback-{}", std::process::id()));
    if rollback.exists() {
        return Err(
            "A previous addon rollback is still present; repair it before trying again".into(),
        );
    }
    fs::create_dir(&rollback)
        .map_err(|_| "Could not create addon rollback directory".to_string())?;
    let mut manifest_addons = Vec::new();
    let mut installed_names: Vec<String> = Vec::new();
    let mut restored_backups: Vec<(String, String)> = Vec::new();
    let mut created_backups: Vec<(String, String)> = Vec::new();
    let mut created_preserved_backups: Vec<String> = Vec::new();
    let mut prepared_backups: Vec<(String, Option<String>)> = Vec::new();
    let mut moved_addons: Vec<(String, PathBuf)> = Vec::new();

    let result = (|| {
        // Launcher-owned directories are staged before mutation so failures
        // remain recoverable.
        for addon in &old.addons {
            let current = addons_dir.join(&addon.name);
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                        return Err(format!(
                            "Could not prepare addon {} for replacement",
                            addon.name
                        ));
                    }
                    let (backup_name, created) =
                        preserve_modified_addon_files(addons_dir, &current, addon)?;
                    if created {
                        if let Some(name) = &backup_name {
                            created_preserved_backups.push(name.clone());
                        }
                    }
                    prepared_backups.push((addon.name.clone(), backup_name));
                    let rollback_path = rollback.join(&addon.name);
                    fs::rename(&current, &rollback_path).map_err(|_| {
                        format!("Could not prepare addon {} for replacement", addon.name)
                    })?;
                    moved_addons.push((addon.name.clone(), rollback_path));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    prepared_backups.push((addon.name.clone(), addon.backup_name.clone()));
                }
                Err(_) => {
                    return Err(format!(
                        "Could not prepare addon {} for replacement",
                        addon.name
                    ));
                }
            }
        }

        // The legacy manifest did not contain file hashes. Active legacy
        // folders remain separate; comparing them to the new release would
        // mistake ordinary release changes for user edits and overwrite the
        // original user backup.
        if let Some(legacy) = &legacy_addons {
            for addon in legacy
                .addons
                .iter()
                .filter(|addon| folders.contains(&addon.name))
                .filter(|addon| !old.addons.iter().any(|old| old.name == addon.name))
            {
                let current = addons_dir.join(&addon.name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                            return Err(format!(
                                "Could not prepare addon {} for replacement",
                                addon.name
                            ));
                        }
                        let legacy_backup =
                            preserve_legacy_addon_directory(addons_dir, &current, &addon.name)?;
                        prepared_backups.push((addon.name.clone(), addon.backup_name.clone()));
                        moved_addons.push((addon.name.clone(), legacy_backup));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        prepared_backups.push((addon.name.clone(), addon.backup_name.clone()));
                    }
                    Err(_) => {
                        return Err(format!(
                            "Could not prepare addon {} for replacement",
                            addon.name
                        ));
                    }
                }
            }
        }

        // Backups for addons no longer shipped restore user content during
        // patch removal.
        for addon in old
            .addons
            .iter()
            .filter(|addon| !folders.contains(&addon.name))
        {
            let backup_name = prepared_backups
                .iter()
                .find(|(name, _)| name == &addon.name)
                .and_then(|(_, backup_name)| backup_name.as_ref());
            if let Some(backup_name) = backup_name {
                let backup = addons_dir.join(backup_name);
                if fs::symlink_metadata(&backup).is_ok() {
                    restore_addon_backup(&backup, &addons_dir.join(&addon.name))
                        .map_err(|_| format!("Could not restore backup for {}", addon.name))?;
                    restored_backups.push((addon.name.clone(), backup_name.clone()));
                }
            }
        }

        for name in folders {
            let current = addons_dir.join(name);
            let old_entry = old.addons.iter().find(|addon| addon.name == *name);
            let prepared_backup = prepared_backups
                .iter()
                .find(|(prepared_name, _)| prepared_name == name)
                .and_then(|(_, backup_name)| backup_name.clone());
            let current_metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return Err(format!("Could not inspect addon {name}")),
            };
            let backup_name = if prepared_backup.is_some() {
                prepared_backup
            } else if let Some(entry) = old_entry {
                entry.backup_name.clone()
            } else if let Some(metadata) = current_metadata {
                // A pre-existing user addon folder survives installation and
                // uninstall; its backup is recorded only after the signed
                // addon has been installed successfully.
                if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
                    return Err(format!("Could not prepare addon {name} for replacement"));
                }
                ensure_safe_path_components(&current)
                    .map_err(|_| format!("Could not prepare addon {name} for replacement"))?;
                let backup_name = format!(".rx-user-backup-{name}");
                let backup = addons_dir.join(&backup_name);
                if fs::symlink_metadata(&backup).is_ok() {
                    return Err(format!(
                        "Could not prepare addon {name}: its user backup already exists"
                    ));
                }
                fs::rename(&current, &backup)
                    .map_err(|_| format!("Could not prepare addon {name} for replacement"))?;
                created_backups.push((name.clone(), backup_name.clone()));
                Some(backup_name)
            } else {
                None
            };

            fs::rename(stage.join(name), &current)
                .map_err(|_| format!("Could not install addon {name}"))?;
            installed_names.push(name.clone());
            let files = collect_addon_files(&current)?;
            manifest_addons.push(InstalledAddon {
                name: name.clone(),
                backup_name,
                files,
            });
        }
        let manifest = InstalledAddonsManifest {
            version: INSTALLED_ADDONS_MANIFEST_VERSION,
            game_path: canonical_game_path(game_dir)?,
            release,
            patch_version: patch_version.into(),
            addons: manifest_addons,
        };
        save_installed_addons(app, &manifest)?;
        if legacy_addons.is_some() {
            retire_legacy_addons(app, game_dir);
        }
        Ok::<bool, String>(legacy_addons.is_some())
    })();

    let legacy_migrated = match result {
        Ok(value) => value,
        Err(error) => {
            let mut rollback_errors = Vec::new();
            for name in installed_names.iter().rev() {
                let current = addons_dir.join(name);
                if current.exists() {
                    if let Err(cleanup_error) = fs::remove_dir_all(&current) {
                        rollback_errors.push(format!(
                            "could not remove staged addon {}: {cleanup_error}",
                            name
                        ));
                    }
                }
            }
            for (name, backup_name) in restored_backups.iter().rev() {
                let restored = addons_dir.join(name);
                if restored.exists() {
                    if let Err(cleanup_error) = fs::rename(restored, addons_dir.join(backup_name)) {
                        rollback_errors.push(format!(
                            "could not restore addon backup {}: {cleanup_error}",
                            name
                        ));
                    }
                }
            }
            for (name, backup_name) in created_backups.iter().rev() {
                let backup = addons_dir.join(backup_name);
                if backup.exists() && !addons_dir.join(name).exists() {
                    if let Err(cleanup_error) = fs::rename(backup, addons_dir.join(name)) {
                        rollback_errors.push(format!(
                            "could not restore pre-existing addon {}: {cleanup_error}",
                            name
                        ));
                    }
                }
            }
            for backup_name in created_preserved_backups.iter().rev() {
                let backup = addons_dir.join(backup_name);
                if fs::symlink_metadata(&backup).is_ok() {
                    if let Err(cleanup_error) = fs::remove_dir_all(backup) {
                        rollback_errors.push(format!(
                            "could not remove temporary addon backup {}: {cleanup_error}",
                            backup_name
                        ));
                    }
                }
            }
            if let Err(cleanup_error) = restore_moved_addons(addons_dir, &rollback, &moved_addons) {
                rollback_errors.push(cleanup_error);
            }
            if rollback_errors.is_empty() {
                return Err(error);
            }
            return Err(format!(
                "{error}; addon recovery could not be completed; recovery data was retained where possible: {}",
                rollback_errors.join("; ")
            ));
        }
    };
    fs::remove_dir_all(&rollback)
        .map_err(|_| "Could not clean addon rollback directory".to_string())?;
    Ok(legacy_migrated)
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

fn installed_content_path(app: &tauri::AppHandle, canonical_path: &str) -> Option<PathBuf> {
    state_file_path(app, "installed-content", canonical_path)
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
    let canonical_path = canonical_game_path(game_dir)?;
    let Some(path) = installed_content_path(app, &canonical_path) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(legacy_path) = legacy_state_path(app, INSTALLED_CONTENT_FILENAME) else {
                return Ok(None);
            };
            let legacy_text = match fs::read_to_string(&legacy_path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("Could not read legacy content tracking data".into()),
            };
            let legacy: InstalledContentManifest = match serde_json::from_str(&legacy_text) {
                Ok(manifest) => manifest,
                Err(_) => return Ok(None),
            };
            if !installed_content_matches_game(&legacy, &canonical_path) {
                return Ok(None);
            }
            save_installed_content(app, &legacy)?;
            fs::remove_file(legacy_path).ok();
            return Ok(Some(legacy));
        }
        Err(_) => return Err("Could not read content tracking data".into()),
    };
    let manifest: InstalledContentManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(None),
    };
    if !installed_content_matches_game(&manifest, &canonical_path) {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn save_installed_content(
    app: &tauri::AppHandle,
    manifest: &InstalledContentManifest,
) -> Result<(), String> {
    let path = installed_content_path(app, &manifest.game_path)
        .ok_or("Could not locate launcher data directory")?;
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

fn delete_installed_content(app: &tauri::AppHandle, game_dir: &Path) -> Result<(), String> {
    let canonical_path = canonical_game_path(game_dir)?;
    if let Some(path) = installed_content_path(app, &canonical_path) {
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
async fn uninstall_patch(
    app: tauri::AppHandle,
    operations: tauri::State<'_, OperationLock>,
    game_path: String,
) -> Result<String, String> {
    let _operation = operations.try_acquire().await?;
    let dir = validate_game_path(&game_path)?;
    let manifest = read_installed_addons(&app, &dir)?;
    let content_manifest = read_installed_content(&app, &dir)?;

    let rx_wow = dir.join("rx-wow.exe");
    if fs::symlink_metadata(&rx_wow).is_ok() {
        ensure_safe_path_components(&rx_wow)?;
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
            delete_installed_content(&app, &dir)?;
        } else {
            content_manifest.files = remaining;
            save_installed_content(&app, &content_manifest)?;
        }
    } else {
        // Without authenticated per-installation tracking, every content file
        // is preserved rather than assigning ownership from a fixed filename.
    }

    let addons_dir = dir.join("Interface").join("AddOns");
    if let Some(mut manifest) = manifest {
        let mut remaining = Vec::new();
        let mut preserved_addons = Vec::new();
        let mut first_error = None;
        for addon in &manifest.addons {
            match restore_tracked_addon(&addons_dir, addon) {
                Ok(true) => {}
                Ok(false) => {
                    preserved_addons.push(addon.name.clone());
                    remaining.push(addon.clone());
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    remaining.push(addon.clone());
                }
            }
        }
        if remaining.is_empty() {
            delete_installed_addons(&app, &dir)?;
        } else {
            manifest.addons = remaining;
            save_installed_addons(&app, &manifest)?;
        }
        if let Some(error) = first_error {
            return Err(format!(
                "Uninstall was incomplete: {error}. Tracking data was retained for retry."
            ));
        }
        if !preserved_addons.is_empty() {
            preserved_content.extend(
                preserved_addons
                    .into_iter()
                    .map(|name| format!("Interface/AddOns/{name}")),
            );
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

fn verify_patch_files(
    dir: &Path,
    manifest: &content::ContentManifest,
    installed_addons: Option<&InstalledAddonsManifest>,
) -> Vec<String> {
    let mut bad = Vec::new();
    let source = dir.join("Wow.exe");
    match file_hash_and_size(&source) {
        Ok((size, hash))
            if size == manifest.executable_patch.source_size
                && hash == manifest.executable_patch.source_sha256.to_ascii_lowercase() => {}
        _ => bad.push("Wow.exe (unsupported or corrupted source)".into()),
    }
    let output = dir.join("rx-wow.exe");
    match file_hash_and_size(&output) {
        Ok((size, hash))
            if size == manifest.executable_patch.output_size
                && hash == manifest.executable_patch.output_sha256.to_ascii_lowercase() => {}
        _ => bad.push("rx-wow.exe (missing or corrupted)".into()),
    }
    for file in &manifest.files {
        if file.kind == content::ContentFileKind::AddonsZip {
            let addons_dir = dir.join("Interface").join("AddOns");
            let ready = installed_addons.is_some_and(|installed| {
                installed.release == manifest.release
                    && installed.patch_version == manifest.version
                    && !installed.addons.is_empty()
                    && installed
                        .addons
                        .iter()
                        .all(|addon| addon_files_match(&addons_dir, addon))
            });
            if !ready {
                bad.push(format!("{} (missing or incomplete)", file.path));
            }
            continue;
        }
        let path = match resolve_content_path(dir, &file.path) {
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
    bad
}

#[tauri::command]
async fn verify_patch(
    app: tauri::AppHandle,
    http: tauri::State<'_, HttpClient>,
    operations: tauri::State<'_, OperationLock>,
    game_path: String,
) -> Result<Vec<String>, String> {
    let _operation = operations.try_acquire().await?;
    let dir = validate_game_path(&game_path)?;
    let manifest = fetch_content_manifest(&app, &http.0).await?;
    let installed_addons = read_installed_addons(&app, &dir)?;

    // Hashing runs on a blocking thread so the async runtime remains responsive.
    tauri::async_runtime::spawn_blocking(move || {
        verify_patch_files(&dir, &manifest, installed_addons.as_ref())
    })
    .await
    .map_err(|_| "Verification failed".into())
}

#[tauri::command]
fn check_realmlist(game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let base = dir.join("Data");
    ensure_safe_path_components(&base)?;
    let locales = ["enUS", "enGB"];

    for locale in &locales {
        let realmlist_path = base.join(locale).join("realmlist.wtf");
        if fs::symlink_metadata(&realmlist_path).is_ok() {
            ensure_safe_path_components(&realmlist_path)?;
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
async fn patch_realmlist(
    operations: tauri::State<'_, OperationLock>,
    game_path: String,
) -> Result<String, String> {
    let _operation = operations.try_acquire().await?;
    patch_realmlist_inner(game_path)
}

fn patch_realmlist_inner(game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let base = dir.join("Data");
    if !base.is_dir() {
        return Err("Data folder not found. Make sure you selected a valid game directory.".into());
    }
    ensure_safe_path_components(&base)?;

    let locales = ["enUS", "enGB"];
    let mut patched = false;
    let mut create_in: Option<PathBuf> = None;

    for locale in &locales {
        let locale_dir = base.join(locale);
        let realmlist_path = locale_dir.join("realmlist.wtf");
        if fs::symlink_metadata(&realmlist_path).is_ok() {
            ensure_safe_path_components(&realmlist_path)?;
            write_file_atomically(&realmlist_path, REALMLIST_VALUE.as_bytes(), "realmlist")?;
            patched = true;
        } else if create_in.is_none() && locale_dir.is_dir() {
            // An existing locale directory takes precedence when the file is
            // missing, matching the normal WoW installation layout.
            ensure_safe_path_components(&locale_dir)?;
            create_in = Some(locale_dir);
        }
    }

    if patched {
        Ok("Realmlist set successfully".into())
    } else {
        // A valid WoW installation can have the locale directory without a
        // realmlist file. The file is created in the first supported locale,
        // defaulting to enUS when neither locale directory exists.
        let locale_dir = create_in.unwrap_or_else(|| base.join("enUS"));
        fs::create_dir_all(&locale_dir)
            .map_err(|_| "Failed to create realmlist directory".to_string())?;
        let realmlist_path = locale_dir.join("realmlist.wtf");
        write_file_atomically(&realmlist_path, REALMLIST_VALUE.as_bytes(), "realmlist")?;
        Ok("Realmlist created and set successfully".into())
    }
}

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

fn is_allowed_http_redirect(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|host| HTTP_ALLOWED_DOMAINS.contains(&host))
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_string())?;
    if !is_allowed_url(parsed.as_str()) {
        return Err("Only allowlisted Project Rx HTTPS URLs may be opened".into());
    }

    // The `open` crate uses ShellExecuteW without cmd.exe shell parsing, so
    // URL path, query, and fragment data cannot inject commands.
    open::that(parsed.as_str()).map_err(|_| "Failed to open URL".to_string())?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let http_client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_allowed_http_redirect(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("Failed to create HTTP client");

    let mut builder = tauri::Builder::default();

    #[cfg(any(windows, target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }));
    }

    builder
        .manage(HttpClient(http_client))
        .manage(OperationLock::default())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if app.state::<OperationLock>().exit_is_permitted() {
                    return;
                }
                api.prevent_close();
                schedule_exit(&app);
            }
        })
        .setup(|app| {
            // The scoped filesystem plugin requires the AppConfig directory
            // to exist before it can authorize the first settings write.
            let app_config_dir = app.path().app_config_dir()?;
            fs::create_dir_all(&app_config_dir)?;

            #[cfg(windows)]
            let window = app.get_webview_window("main").unwrap();

            #[cfg(target_os = "windows")]
            window_vibrancy::apply_acrylic(&window, Some((9, 7, 5, 220)))
                .expect("Failed to apply acrylic");

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
                        schedule_exit(app);
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
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_launcher_config,
            get_patch_manifest,
            check_game_directory,
            check_game_runtime,
            check_server_status,
            check_launcher_update,
            apply_launcher_update,
            request_exit,
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
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                let operations = app_handle.state::<OperationLock>();
                if !operations.exit_is_permitted() {
                    api.prevent_exit();
                    schedule_exit(app_handle);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
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
    fn shutdown_request_blocks_new_game_operations() {
        let operations = OperationLock::default();
        tauri::async_runtime::block_on(async {
            let active = operations.try_acquire().await.unwrap();
            // The updater invokes this transition while holding its operation
            // guard; shutdown admission closes before that guard is released.
            operations.permit_exit();
            assert!(operations.try_acquire().await.is_err());
            drop(active);
            assert!(operations.exit_is_permitted());
            assert!(operations.try_acquire().await.is_err());
        });
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
    fn atomic_realmlist_replacement_does_not_truncate_in_place() {
        let temp = TestDir::new();
        let data = temp.0.join("Data").join("enUS");
        fs::create_dir_all(&data).unwrap();
        let realmlist = data.join("realmlist.wtf");
        fs::write(&realmlist, b"set realmlist old.example").unwrap();

        patch_realmlist_inner(temp.0.to_string_lossy().into_owned()).unwrap();

        assert_eq!(fs::read_to_string(realmlist).unwrap(), REALMLIST_VALUE);
    }

    #[cfg(unix)]
    #[test]
    fn realmlist_rejects_symbolic_links() {
        let temp = TestDir::new();
        let data = temp.0.join("Data").join("enUS");
        fs::create_dir_all(&data).unwrap();
        let outside = temp.0.join("outside.txt");
        fs::write(&outside, b"protected").unwrap();
        symlink(&outside, data.join("realmlist.wtf")).unwrap();

        assert!(patch_realmlist_inner(temp.0.to_string_lossy().into_owned()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"protected");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_realmlist_replacement_does_not_modify_hard_link_source() {
        let temp = TestDir::new();
        let data = temp.0.join("Data").join("enUS");
        fs::create_dir_all(&data).unwrap();
        let wow = temp.0.join("Wow.exe");
        fs::write(&wow, b"protected executable").unwrap();
        fs::hard_link(&wow, data.join("realmlist.wtf")).unwrap();

        patch_realmlist_inner(temp.0.to_string_lossy().into_owned()).unwrap();

        assert_eq!(fs::read(&wow).unwrap(), b"protected executable");
        assert_eq!(
            fs::read_to_string(data.join("realmlist.wtf")).unwrap(),
            REALMLIST_VALUE
        );
    }

    #[cfg(unix)]
    #[test]
    fn game_path_rejects_symbolic_link_directories() {
        let temp = TestDir::new();
        let target = temp.0.join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.0.join("link");
        symlink(&target, &link).unwrap();

        assert!(validate_game_path(link.to_str().unwrap()).is_err());
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
            version: INSTALLED_ADDONS_MANIFEST_VERSION,
            game_path: "C:\\Game".into(),
            release: 1,
            patch_version: "1.0.0".into(),
            addons: vec![InstalledAddon {
                name: "Addon".into(),
                backup_name: Some("Addon_rx_backup".into()),
                files: vec![InstalledAddonFile {
                    path: "main.lua".into(),
                    size: 1,
                    sha256: "00".repeat(32),
                }],
            }],
        };
        assert!(manifest_matches_game(&manifest, "C:\\Game"));
        assert!(!manifest_matches_game(&manifest, "D:\\Other"));
        manifest.addons[0].name = "../Addon".into();
        assert!(!manifest_matches_game(&manifest, "C:\\Game"));
        assert!(!validate_addon_name("Addon\\Nested"));
        assert!(!validate_addon_name("Addon/Nested"));
    }

    #[test]
    fn addon_verification_rejects_modified_files() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let addon_dir = addons_dir.join("Addon");
        fs::create_dir_all(&addon_dir).unwrap();
        let file = addon_dir.join("main.lua");
        fs::write(&file, b"one").unwrap();
        let (size, sha256) = file_hash_and_size(&file).unwrap();
        let addon = InstalledAddon {
            name: "Addon".into(),
            backup_name: None,
            files: vec![InstalledAddonFile {
                path: "main.lua".into(),
                size,
                sha256,
            }],
        };

        assert!(addon_files_match(&addons_dir, &addon));
        fs::write(&file, b"modified").unwrap();
        assert!(!addon_files_match(&addons_dir, &addon));
    }

    #[test]
    fn addon_update_preserves_modified_and_added_files() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let addon_dir = addons_dir.join("Addon");
        fs::create_dir_all(&addon_dir).unwrap();
        fs::write(addon_dir.join("main.lua"), b"modified").unwrap();
        fs::write(addon_dir.join("user.lua"), b"added by user").unwrap();
        let addon = InstalledAddon {
            name: "Addon".into(),
            backup_name: None,
            files: vec![InstalledAddonFile {
                path: "main.lua".into(),
                size: 8,
                sha256: format!("{:x}", Sha256::digest(b"original")),
            }],
        };

        let (backup_name, created) =
            preserve_modified_addon_files(&addons_dir, &addon_dir, &addon).unwrap();
        let backup = addons_dir.join(backup_name.unwrap());
        assert!(created);
        assert_eq!(fs::read(backup.join("main.lua")).unwrap(), b"modified");
        assert_eq!(fs::read(backup.join("user.lua")).unwrap(), b"added by user");
    }

    #[cfg(unix)]
    #[test]
    fn addon_backup_copy_does_not_modify_hard_link_source() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let addon_dir = addons_dir.join("Addon");
        let backup_dir = addons_dir.join(".rx-user-backup-Addon");
        fs::create_dir_all(&addon_dir).unwrap();
        fs::create_dir_all(&backup_dir).unwrap();
        let wow = temp.0.join("Wow.exe");
        fs::write(&wow, b"protected executable").unwrap();
        fs::write(addon_dir.join("main.lua"), b"modified").unwrap();
        fs::hard_link(&wow, backup_dir.join("main.lua")).unwrap();
        let addon = InstalledAddon {
            name: "Addon".into(),
            backup_name: Some(".rx-user-backup-Addon".into()),
            files: vec![InstalledAddonFile {
                path: "main.lua".into(),
                size: 8,
                sha256: format!("{:x}", Sha256::digest(b"original")),
            }],
        };

        preserve_modified_addon_files(&addons_dir, &addon_dir, &addon).unwrap();

        assert_eq!(fs::read(&wow).unwrap(), b"protected executable");
        assert_eq!(fs::read(backup_dir.join("main.lua")).unwrap(), b"modified");
    }

    #[test]
    fn legacy_addon_preservation_keeps_original_user_backup() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let current = addons_dir.join("Addon");
        let original_backup = addons_dir.join(".rx-user-backup-Addon");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&original_backup).unwrap();
        fs::write(current.join("main.lua"), b"old launcher release").unwrap();
        fs::write(original_backup.join("main.lua"), b"user version").unwrap();

        let legacy_backup =
            preserve_legacy_addon_directory(&addons_dir, &current, "Addon").unwrap();

        assert!(!current.exists());
        assert_eq!(
            fs::read(original_backup.join("main.lua")).unwrap(),
            b"user version"
        );
        assert_eq!(
            fs::read(legacy_backup.join("main.lua")).unwrap(),
            b"old launcher release"
        );
        fs::rename(legacy_backup, current).unwrap();
    }

    #[test]
    fn moved_addon_rollback_restores_all_journaled_directories() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let rollback = addons_dir.join(".rx-rollback-test");
        let first = rollback.join("First");
        let second = addons_dir.join(".rx-legacy-Second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("main.lua"), b"first").unwrap();
        fs::write(second.join("main.lua"), b"second").unwrap();

        restore_moved_addons(
            &addons_dir,
            &rollback,
            &[("First".into(), first), ("Second".into(), second)],
        )
        .unwrap();

        assert_eq!(
            fs::read(addons_dir.join("First/main.lua")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(addons_dir.join("Second/main.lua")).unwrap(),
            b"second"
        );
        assert!(!rollback.exists());
    }

    #[test]
    fn nested_addon_backup_restores_after_tracked_removal() {
        let temp = TestDir::new();
        let addons_dir = temp.0.join("AddOns");
        let addon_dir = addons_dir.join("Addon");
        let backup_dir = addons_dir.join(".rx-user-backup-Addon");
        fs::create_dir_all(addon_dir.join("Libs")).unwrap();
        fs::create_dir_all(backup_dir.join("Libs")).unwrap();
        fs::write(addon_dir.join("Libs/main.lua"), b"installed").unwrap();
        fs::write(backup_dir.join("Libs/main.lua"), b"user addon").unwrap();
        let (size, sha256) = file_hash_and_size(&addon_dir.join("Libs/main.lua")).unwrap();
        let addon = InstalledAddon {
            name: "Addon".into(),
            backup_name: Some(".rx-user-backup-Addon".into()),
            files: vec![InstalledAddonFile {
                path: "Libs/main.lua".into(),
                size,
                sha256,
            }],
        };

        assert!(restore_tracked_addon(&addons_dir, &addon).unwrap());
        assert_eq!(
            fs::read(addon_dir.join("Libs/main.lua")).unwrap(),
            b"user addon"
        );
        assert!(!backup_dir.exists());
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
    fn http_redirect_policy_requires_https_and_approved_hosts() {
        assert!(is_allowed_http_redirect(
            &url::Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
        ));
        assert!(is_allowed_http_redirect(
            &url::Url::parse("https://projectrx.net/news").unwrap()
        ));
        assert!(!is_allowed_http_redirect(
            &url::Url::parse("http://projectrx.net/news").unwrap()
        ));
        assert!(!is_allowed_http_redirect(
            &url::Url::parse("https://evil.example/file").unwrap()
        ));
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

        let result = patch_realmlist_inner(temp.0.to_string_lossy().into_owned()).unwrap();

        assert_eq!(result, "Realmlist created and set successfully");
        assert_eq!(
            fs::read_to_string(temp.0.join("Data").join("enUS").join("realmlist.wtf")).unwrap(),
            REALMLIST_VALUE
        );
    }
}
