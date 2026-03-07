use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::path::{Component, PathBuf};
use std::time::Duration;
use tauri::Emitter;
use tauri_plugin_shell::ShellExt;

const PATCH_VERSION: &str = "v1.00";
const REALMLIST_VALUE: &str = "set realmlist projectrx.net";
const SERVER_HOST: &str = "projectrx.net";
const SERVER_PORT: u16 = 3724;
const GITHUB_RELEASE_BASE: &str =
    "https://github.com/Mirenel/rx-patches/releases/download/v1.0";

struct HttpClient(reqwest::Client);

// Expected SHA256 hashes for patch files (single source of truth)
const PATCH_FILE_HASHES: [(&str, &str); 4] = [
    ("patch-7.MPQ", "6985968c22da35c6256e5c7ee5b58db1e8a1319354f52cb852721e95b8b2acf1"),
    ("patch-A.MPQ", "c5eb713d28f80c311aa94607800c1b197d3cc939fe52ad78246fa10399464458"),
    ("patch-B.MPQ", "d611c3bf911d4c88eb9c33e28d4ffb0d92a2209f3500eae68c6d031803cbfacb"),
    ("patch-D.MPQ", "3dea625c3014c1db6b9b03072dcbe3de936356d47945255a1df3a958ff7c63fb"),
];

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
}

#[tauri::command]
fn get_launcher_config() -> LauncherConfig {
    LauncherConfig {
        patch_version: PATCH_VERSION.into(),
        realmlist: REALMLIST_VALUE.into(),
        server_host: SERVER_HOST.into(),
        server_port: SERVER_PORT,
    }
}

// ── Server status (TCP check — not HTTP) ────────────────────

#[derive(Serialize)]
struct ServerStatus {
    online: bool,
    players: u32,
}

#[tauri::command]
async fn check_server_status() -> ServerStatus {
    let online = tauri::async_runtime::spawn_blocking(|| {
        let addr = format!("{}:{}", SERVER_HOST, SERVER_PORT)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next());

        match addr {
            Some(a) => TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok(),
            None => false,
        }
    })
    .await
    .unwrap_or(false);

    ServerStatus { online, players: 0 }
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
async fn get_news(http: tauri::State<'_, HttpClient>) -> Result<Vec<NewsItem>, ()> {
    Ok(match http.0.get("https://projectrx.net/news.json").send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<Vec<NewsItem>>().await.unwrap_or_default()
        }
        _ => Vec::new(),
    })
}

// ── Changelog (fetched from projectrx.net) ──────────────────

#[derive(Serialize, Deserialize, Clone)]
struct ChangelogEntry {
    version: String,
    date: String,
    changes: Vec<String>,
}

#[tauri::command]
async fn get_changelog(http: tauri::State<'_, HttpClient>) -> Result<Vec<ChangelogEntry>, ()> {
    Ok(match http.0.get("https://projectrx.net/changelog.json").send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json::<Vec<ChangelogEntry>>().await.unwrap_or_default()
        }
        _ => Vec::new(),
    })
}

// ── Game launch (via shell plugin) ──────────────────────────

#[tauri::command]
fn launch_game(app: tauri::AppHandle, game_path: String) -> Result<(), String> {
    let dir = validate_game_path(&game_path)?;
    let wow_exe = dir.join("Wow.exe");

    if !wow_exe.exists() {
        return Err("Wow.exe not found".into());
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
}

#[tauri::command]
async fn download_patch(app: tauri::AppHandle, http: tauri::State<'_, HttpClient>, game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let data_dir = dir.join("Data");
    let addons_dir = dir.join("Interface").join("AddOns");

    if !data_dir.is_dir() {
        return Err("Data folder not found. Make sure you selected a valid game directory.".into());
    }
    if !addons_dir.is_dir() {
        return Err("Interface/AddOns folder not found. Make sure you selected a valid game directory.".into());
    }

    let client = &http.0;

    // (filename, dest_dir, extract_zip, weight_mb, expected_sha256)
    fn hash_for(name: &str) -> Option<&'static str> {
        PATCH_FILE_HASHES.iter().find(|&&(n, _)| n == name).map(|&(_, h)| h)
    }
    let items: [(&str, &std::path::Path, bool, u32, Option<&str>); 5] = [
        ("patch-7.MPQ", data_dir.as_path(), false, 220, hash_for("patch-7.MPQ")),
        ("patch-A.MPQ", data_dir.as_path(), false, 131, hash_for("patch-A.MPQ")),
        ("patch-B.MPQ", data_dir.as_path(), false, 2,   hash_for("patch-B.MPQ")),
        ("patch-D.MPQ", data_dir.as_path(), false, 1,   hash_for("patch-D.MPQ")),
        ("Addons.zip", addons_dir.as_path(), true, 2, None),
    ];

    let total_weight: u32 = items.iter().map(|i| i.3).sum();
    let mut completed_weight: u32 = 0;

    for &(filename, dest, extract, weight, expected_hash) in &items {
        let url = format!("{}/{}", GITHUB_RELEASE_BASE, filename);
        let file_path = if extract {
            dir.join(filename)
        } else {
            dest.join(filename)
        };

        // Skip download if file already matches expected hash
        if let Some(hash) = expected_hash {
            if file_path.exists() {
                let path_clone = file_path.clone();
                let hash_owned = hash.to_string();
                let matches = tauri::async_runtime::spawn_blocking(move || {
                    sha256_file(&path_clone).map(|a| a == hash_owned).unwrap_or(false)
                }).await.unwrap_or(false);
                if matches {
                    completed_weight += weight;
                    app.emit(
                        "download-progress",
                        DownloadProgress {
                            percent: (completed_weight as f64 / total_weight as f64 * 100.0)
                                .min(100.0) as u8,
                            message: format!("{} already up to date.", filename),
                            log: true,
                        },
                    )
                    .ok();
                    continue;
                }
            }
        }

        let base_pct = (completed_weight as f64 / total_weight as f64 * 100.0) as u8;
        app.emit(
            "download-progress",
            DownloadProgress {
                percent: base_pct,
                message: format!("Downloading {}...", filename),
                log: true,
            },
        )
        .ok();

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|_| format!("Failed to connect for {}", filename))?;

        if !response.status().is_success() {
            return Err(format!("Download failed for {}", filename));
        }

        let total_size = response.content_length().unwrap_or(0);
        let mut file =
            fs::File::create(&file_path).map_err(|_| format!("Failed to create {}", filename))?;

        let mut downloaded: u64 = 0;
        let mut last_emit: u64 = 0;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| format!("Network error downloading {}", filename))?;
            file.write_all(&chunk)
                .map_err(|_| format!("Failed to write {}", filename))?;
            downloaded += chunk.len() as u64;

            if total_size > 0 && (downloaded - last_emit >= 524_288 || downloaded >= total_size) {
                last_emit = downloaded;
                let file_pct = (downloaded as f64 / total_size as f64).min(1.0);
                let overall_pct = ((completed_weight as f64 + weight as f64 * file_pct)
                    / total_weight as f64
                    * 100.0) as u8;
                app.emit(
                    "download-progress",
                    DownloadProgress {
                        percent: overall_pct.min(99),
                        message: format!(
                            "Downloading {}... ({:.1} / {:.1} MB)",
                            filename,
                            downloaded as f64 / 1_048_576.0,
                            total_size as f64 / 1_048_576.0
                        ),
                        log: false,
                    },
                )
                .ok();
            }
        }

        file.flush()
            .map_err(|_| format!("Failed to flush {}", filename))?;
        drop(file);

        // Verify downloaded file matches expected hash
        if let Some(hash) = expected_hash {
            let path_clone = file_path.clone();
            let actual = tauri::async_runtime::spawn_blocking(move || {
                sha256_file(&path_clone)
            }).await
                .map_err(|_| format!("Failed to verify {}", filename))?
                .map_err(|_| format!("Failed to verify {}", filename))?;
            if actual != hash {
                fs::remove_file(&file_path).ok();
                return Err(format!("{} failed integrity check", filename));
            }
        }

        if extract {
            app.emit(
                "download-progress",
                DownloadProgress {
                    percent: base_pct,
                    message: format!("Extracting {}...", filename),
                    log: true,
                },
            )
            .ok();

            extract_zip_file(&file_path, dest)?;
            fs::remove_file(&file_path).ok();
        }

        completed_weight += weight;
        app.emit(
            "download-progress",
            DownloadProgress {
                percent: (completed_weight as f64 / total_weight as f64 * 100.0).min(100.0) as u8,
                message: format!("{} installed.", filename),
                log: true,
            },
        )
        .ok();
    }

    Ok(PATCH_VERSION.into())
}

fn extract_zip_file(zip_path: &std::path::Path, dest_dir: &std::path::Path) -> Result<(), String> {
    let file = fs::File::open(zip_path).map_err(|_| "Failed to open archive".to_string())?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|_| "Invalid archive".to_string())?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| "Failed to read archive entry".to_string())?;

        let name = match entry.enclosed_name() {
            Some(name) => name.to_owned(),
            None => continue,
        };

        let out_path = dest_dir.join(&name);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).ok();
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            let mut outfile =
                fs::File::create(&out_path).map_err(|_| "Failed to extract file".to_string())?;
            std::io::copy(&mut entry, &mut outfile)
                .map_err(|_| "Failed to write extracted file".to_string())?;
        }
    }

    Ok(())
}

// ── Patch uninstall ─────────────────────────────────────────

const PATCH_FILENAMES: &[&str] = &["patch-7.MPQ", "patch-A.MPQ", "patch-B.MPQ", "patch-D.MPQ"];
const ADDON_FOLDERS: &[&str] = &[
    "MogIt", "MogIt_Accessories", "MogIt_Cata", "MogIt_Cloth", "MogIt_Leather",
    "MogIt_Mail", "MogIt_OneHanded", "MogIt_Other", "MogIt_Plate", "MogIt_Ranged",
    "MogIt_TwoHanded", "ProjectRx",
];

#[tauri::command]
fn uninstall_patch(game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;

    // Restore original Wow.exe from backup
    let wow_exe = dir.join("Wow.exe");
    let backup = dir.join("Wow_original.exe");
    if backup.exists() {
        fs::copy(&backup, &wow_exe).map_err(|_| "Failed to restore Wow.exe".to_string())?;
        fs::remove_file(&backup).ok();
    }

    // Remove exactly our 4 MPQ patch files
    let data_dir = dir.join("Data");
    for filename in PATCH_FILENAMES {
        let path = data_dir.join(filename);
        if path.exists() {
            fs::remove_file(&path).ok();
        }
    }

    // Remove exactly our 2 addon folders
    let addons_dir = dir.join("Interface").join("AddOns");
    for folder in ADDON_FOLDERS {
        let path = addons_dir.join(folder);
        if path.is_dir() {
            fs::remove_dir_all(&path).ok();
        }
    }

    Ok("Project Rx content removed successfully".into())
}

// ── Patch integrity check ────────────────────────────────────

#[tauri::command]
async fn verify_patch(game_path: String) -> Result<Vec<String>, String> {
    let dir = validate_game_path(&game_path)?;
    let data_dir = dir.join("Data");

    // Run hashing on a blocking thread to avoid stalling the async runtime
    tauri::async_runtime::spawn_blocking(move || {
        let mut bad: Vec<String> = Vec::new();
        for &(name, expected) in &PATCH_FILE_HASHES {
            let path = data_dir.join(name);
            if !path.exists() {
                bad.push(format!("{} (missing)", name));
            } else if let Ok(actual) = sha256_file(&path) {
                if actual != expected {
                    bad.push(format!("{} (corrupted)", name));
                }
            } else {
                bad.push(format!("{} (unreadable)", name));
            }
        }
        Ok(bad)
    })
    .await
    .unwrap_or_else(|_| Err("Verification failed".into()))
}

// ── Wow.exe patching ─────────────────────────────────────────

const PATCHED_WOW_SHA256: &str = "c6f9209840a8a5941538bcda0e63c4bf445a154b6c1c9c7397be31cebabc289c";

#[derive(Serialize)]
struct WowExeStatus {
    status: String,
}

fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|_| "Failed to read file".to_string())?;
    let mut reader = BufReader::with_capacity(1 << 16, file); // 64KB chunks
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf).map_err(|_| "Failed to read file".to_string())?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[tauri::command]
fn check_wow_exe(game_path: String) -> Result<WowExeStatus, String> {
    let dir = validate_game_path(&game_path)?;
    let wow_exe = dir.join("Wow.exe");

    if !wow_exe.exists() {
        return Ok(WowExeStatus { status: "missing".into() });
    }

    let hash = sha256_file(&wow_exe)?;
    if hash == PATCHED_WOW_SHA256 {
        Ok(WowExeStatus { status: "patched".into() })
    } else {
        Ok(WowExeStatus { status: "original".into() })
    }
}

#[tauri::command]
fn patch_wow_exe(app: tauri::AppHandle, game_path: String) -> Result<String, String> {
    let dir = validate_game_path(&game_path)?;
    let wow_exe = dir.join("Wow.exe");
    let backup = dir.join("Wow_original.exe");

    // Check if already patched
    if wow_exe.exists() {
        let hash = sha256_file(&wow_exe)?;
        if hash == PATCHED_WOW_SHA256 {
            return Ok("Already patched".into());
        }
        // Backup existing Wow.exe
        fs::copy(&wow_exe, &backup).map_err(|_| "Failed to backup Wow.exe".to_string())?;
    }

    // Resolve bundled resource path
    let resource_path = app
        .path()
        .resource_dir()
        .map_err(|_| "Failed to locate resources".to_string())?
        .join("resources")
        .join("Wow.exe");

    if !resource_path.exists() {
        return Err("Bundled Wow.exe not found".into());
    }

    fs::copy(&resource_path, &wow_exe).map_err(|_| "Failed to install patched Wow.exe".to_string())?;

    Ok("Wow.exe patched successfully".into())
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
    let locales = ["enUS", "enGB"];
    let mut patched = false;

    for locale in &locales {
        let realmlist_path = base.join(locale).join("realmlist.wtf");
        if realmlist_path.exists() {
            fs::write(&realmlist_path, REALMLIST_VALUE)
                .map_err(|_| "Failed to write realmlist".to_string())?;
            patched = true;
        }
    }

    if patched {
        Ok("Realmlist set successfully".into())
    } else {
        Err("realmlist.wtf not found".into())
    }
}

// ── Open URL (allowlisted domains only) ─────────────────────

const ALLOWED_DOMAINS: &[&str] = &["projectrx.net", "www.projectrx.net"];

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_string())?;

    if parsed.scheme() != "https" {
        return Err("Only HTTPS URLs are allowed".into());
    }

    let host = parsed.host_str().ok_or("Invalid URL")?;
    if !ALLOWED_DOMAINS.contains(&host) {
        return Err("Domain not allowed".into());
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

            #[cfg(desktop)]
            app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;

            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_launcher_config,
            check_server_status,
            get_news,
            launch_game,
            download_patch,
            uninstall_patch,
            check_wow_exe,
            patch_wow_exe,
            check_realmlist,
            patch_realmlist,
            open_url,
            get_changelog,
            verify_patch,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
