use serde::Serialize;
use std::path::Path;

#[cfg(any(windows, target_os = "linux"))]
use std::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct GameRuntimeStatus {
    pub runtime: String,
    pub ready: bool,
    pub version: Option<String>,
    pub message: String,
}

pub fn unavailable_status(message: impl Into<String>) -> GameRuntimeStatus {
    GameRuntimeStatus {
        runtime: "unsupported".into(),
        ready: false,
        version: None,
        message: message.into(),
    }
}

pub fn status(wine_prefix: Option<&str>) -> GameRuntimeStatus {
    #[cfg(windows)]
    {
        let _ = wine_prefix;
        return GameRuntimeStatus {
            runtime: "native".into(),
            ready: true,
            version: None,
            message: "Native Windows game runtime ready.".into(),
        };
    }

    #[cfg(target_os = "linux")]
    {
        return linux_status(wine_prefix);
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = wine_prefix;
        unavailable_status("Project Rx game launch is supported on Windows and Linux only")
    }
}

#[cfg(target_os = "linux")]
fn linux_status(wine_prefix: Option<&str>) -> GameRuntimeStatus {
    let mut command = match wine_command(wine_prefix) {
        Ok(command) => command,
        Err(message) => {
            return GameRuntimeStatus {
                runtime: "wine".into(),
                ready: false,
                version: None,
                message,
            };
        }
    };
    let output = match command.arg("--version").output() {
        Ok(output) => output,
        Err(error) => return classify_wine_start_error(&error),
    };

    classify_wine_probe(output.status.success(), &output.stdout, &output.stderr)
}

#[cfg(target_os = "linux")]
fn classify_wine_start_error(error: &std::io::Error) -> GameRuntimeStatus {
    let message = if error.kind() == std::io::ErrorKind::NotFound {
        "Wine is required to launch the Windows game client on Linux. Install Wine and try again."
    } else {
        "Wine was found but could not be started. Check the Wine installation and try again."
    };
    GameRuntimeStatus {
        runtime: "wine".into(),
        ready: false,
        version: None,
        message: message.into(),
    }
}

#[cfg(target_os = "linux")]
fn wine_command(wine_prefix: Option<&str>) -> Result<Command, String> {
    let mut command = Command::new("wine");
    let Some(wine_prefix) = wine_prefix.filter(|value| !value.is_empty()) else {
        return Ok(command);
    };
    let path = Path::new(wine_prefix);
    if !path.is_absolute() {
        return Err("Wine prefix must be an absolute directory".into());
    }
    if !path.is_dir() {
        return Err("Wine prefix directory not found".into());
    }
    command.env("WINEPREFIX", path);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn wine_launch_command(game_exe: &Path, wine_prefix: Option<&str>) -> Result<Command, String> {
    let mut command = wine_command(wine_prefix)?;
    command.arg(game_exe);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn classify_wine_probe(success: bool, stdout: &[u8], stderr: &[u8]) -> GameRuntimeStatus {
    let diagnostics = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    let lower = diagnostics.to_ascii_lowercase();
    let missing_32_bit = lower.contains("wine32 is missing")
        || lower.contains("multiarch needs to be enabled")
        || (lower.contains("32-bit")
            && (lower.contains("missing") || lower.contains("not installed")));

    if missing_32_bit {
        return GameRuntimeStatus {
            runtime: "wine".into(),
            ready: false,
            version: first_line(stdout),
            message: "Wine is installed, but 32-bit Wine support is missing. Install Wine support for 32-bit Windows applications and try again.".into(),
        };
    }

    if !success {
        return GameRuntimeStatus {
            runtime: "wine".into(),
            ready: false,
            version: first_line(stdout),
            message:
                "Wine could not verify its runtime. Check the Wine installation and try again."
                    .into(),
        };
    }

    let version = first_line(stdout).or_else(|| first_line(stderr));
    GameRuntimeStatus {
        runtime: "wine".into(),
        ready: true,
        message: version
            .as_deref()
            .map(|value| format!("{value} detected."))
            .unwrap_or_else(|| "Wine detected.".into()),
        version,
    }
}

#[cfg(target_os = "linux")]
fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(128).collect())
}

pub fn launch(game_exe: &Path, game_dir: &Path, wine_prefix: Option<&str>) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = wine_prefix;
        return Command::new(game_exe)
            .current_dir(game_dir)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to launch game: {error}"));
    }

    #[cfg(target_os = "linux")]
    {
        let runtime = linux_status(wine_prefix);
        if !runtime.ready {
            return Err(runtime.message);
        }

        let mut command = wine_launch_command(game_exe, wine_prefix)?;
        return command
            .current_dir(game_dir)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "Wine is required to launch the Windows game client on Linux. Install Wine and try again.".into()
                } else {
                    format!("Failed to launch game through Wine: {error}")
                }
            });
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (game_exe, game_dir, wine_prefix);
        Err("Project Rx game launch is supported on Windows and Linux only".into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temporary_prefix(label: &str) -> std::path::PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("project-rx-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temporary Wine prefix");
        path
    }

    #[test]
    fn missing_wine32_diagnostic_blocks_runtime() {
        let status = classify_wine_probe(
            true,
            b"wine-10.0\n",
            b"it looks like wine32 is missing, you should install it.\n",
        );
        assert!(!status.ready);
        assert!(status.message.contains("32-bit"));
    }

    #[test]
    fn successful_wine_probe_reports_version() {
        let status = classify_wine_probe(true, b"wine-10.0\n", b"");
        assert!(status.ready);
        assert_eq!(status.version.as_deref(), Some("wine-10.0"));
    }

    #[test]
    fn failed_wine_probe_blocks_runtime() {
        let status = classify_wine_probe(false, b"", b"Wine failed\n");
        assert!(!status.ready);
        assert!(status.message.contains("could not verify"));
    }

    #[test]
    fn missing_wine_reports_install_diagnostic() {
        let status = classify_wine_start_error(&std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!status.ready);
        assert!(status.message.contains("Install Wine"));
    }

    #[test]
    fn missing_wine_prefix_blocks_runtime() {
        let status = linux_status(Some("/definitely/missing/project-rx-wine-prefix"));
        assert!(!status.ready);
        assert!(status.message.contains("Wine prefix directory not found"));
    }

    #[test]
    fn relative_wine_prefix_blocks_runtime() {
        let status = linux_status(Some("project-rx-wine-prefix"));
        assert!(!status.ready);
        assert!(status.message.contains("absolute directory"));
    }

    #[test]
    fn explicit_prefix_with_spaces_is_preserved_as_one_environment_value() {
        let prefix = temporary_prefix("prefix with spaces");
        let command = wine_command(prefix.to_str());
        let command = command.expect("construct Wine command");
        let configured_prefix = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("WINEPREFIX"))
            .and_then(|(_, value)| value)
            .expect("explicit WINEPREFIX");
        assert_eq!(configured_prefix, prefix.as_os_str());
        fs::remove_dir_all(prefix).expect("remove temporary Wine prefix");
    }

    #[test]
    fn explicit_prefix_overrides_inherited_wineprefix() {
        let prefix = temporary_prefix("explicit-prefix");
        let command = wine_command(prefix.to_str()).expect("construct Wine command");
        let configured_prefix = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("WINEPREFIX"))
            .and_then(|(_, value)| value)
            .expect("explicit WINEPREFIX");
        assert_ne!(configured_prefix, OsStr::new("/inherited/prefix"));
        assert_eq!(configured_prefix, prefix.as_os_str());
        fs::remove_dir_all(prefix).expect("remove temporary Wine prefix");
    }

    #[test]
    fn empty_prefix_inherits_wineprefix() {
        let command = wine_command(Some("")).expect("construct Wine command");
        assert!(!command
            .get_envs()
            .any(|(key, _)| key == OsStr::new("WINEPREFIX")));
    }

    #[test]
    fn wine_launch_uses_direct_command_arguments_without_shell_splitting() {
        let game_exe = Path::new("/tmp/Project Rx/Game Files/rx-wow.exe");
        let command = wine_launch_command(game_exe, None).expect("construct Wine launch");
        assert_eq!(command.get_program(), OsStr::new("wine"));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![game_exe.as_os_str()]);
    }
}
