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

pub fn status() -> GameRuntimeStatus {
    #[cfg(windows)]
    {
        return GameRuntimeStatus {
            runtime: "native".into(),
            ready: true,
            version: None,
            message: "Native Windows game runtime ready.".into(),
        };
    }

    #[cfg(target_os = "linux")]
    {
        return linux_status();
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    unavailable_status("Project Rx game launch is supported on Windows and Linux only")
}

#[cfg(target_os = "linux")]
fn linux_status() -> GameRuntimeStatus {
    let output = match Command::new("wine").arg("--version").output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GameRuntimeStatus {
                runtime: "wine".into(),
                ready: false,
                version: None,
                message: "Wine is required to launch the Windows game client on Linux. Install Wine and try again.".into(),
            };
        }
        Err(_) => {
            return GameRuntimeStatus {
                runtime: "wine".into(),
                ready: false,
                version: None,
                message: "Wine was found but could not be started. Check the Wine installation and try again.".into(),
            };
        }
    };

    classify_wine_probe(output.status.success(), &output.stdout, &output.stderr)
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

pub fn launch(game_exe: &Path, game_dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        return Command::new(game_exe)
            .current_dir(game_dir)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to launch game: {error}"));
    }

    #[cfg(target_os = "linux")]
    {
        let runtime = linux_status();
        if !runtime.ready {
            return Err(runtime.message);
        }

        return Command::new("wine")
            .arg(game_exe)
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
        let _ = (game_exe, game_dir);
        Err("Project Rx game launch is supported on Windows and Linux only".into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

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
}
