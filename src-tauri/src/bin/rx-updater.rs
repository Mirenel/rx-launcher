#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "../update_auth.rs"]
mod update_auth;
#[path = "../update_payload.rs"]
mod update_payload;

#[cfg(windows)]
mod windows_updater {
    use sha2::{Digest, Sha256};
    use std::ffi::OsStr;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, OpenEventW, OpenProcess, QueryFullProcessImageNameW, SetEvent,
        WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };

    use crate::update_auth::{ensure_newer, UpdateManifest};

    const MUTEX_NAME: &str = "Local\\ProjectRxLauncher.Update";

    pub fn run(args: &[String]) -> Result<(), String> {
        let options = parse_args(args)?;
        let result = run_inner(&options);
        if let Err(error) = &result {
            log(&options.work_dir, &format!("update failed: {error}"));
        }
        result
    }

    fn run_inner(options: &Options) -> Result<(), String> {
        log(
            &options.work_dir,
            &format!(
                "current version: {}; remote version: {}",
                options.current_version, options.version
            ),
        );
        let _mutex = UpdateMutex::acquire()?;
        validate_paths(&options)?;
        wait_for_exact_process(
            options.pid,
            &options.target,
            &options.ready_event,
            &options.work_dir,
        )?;
        log(&options.work_dir, "waiting for old process completed");
        let remote_version = options.manifest.validate()?;
        let current_version = semver::Version::parse(&options.current_version)
            .map_err(|_| "Installed launcher version is invalid".to_string())?;
        ensure_newer(&remote_version, &current_version)?;
        log(
            &options.work_dir,
            "release signature verified immediately before replacement",
        );
        log(&options.work_dir, "verifying staged executable hash");
        verify_hash(&options.source, &options.sha256)?;
        log(
            &options.work_dir,
            "SHA-256 verified immediately before replacement",
        );

        log(
            &options.work_dir,
            "staging verified executable beside launcher",
        );
        let ready = prepare_ready(&options.source, &options.target, &options.sha256)?;
        log(&options.work_dir, "verified update staged beside launcher");
        let backup = backup_path(&options.target)?;
        if backup.exists() {
            fs::remove_file(&backup)
                .map_err(|_| "Could not remove the stale launcher backup".to_string())?;
        }
        move_file(&options.target, &backup)?;
        log(&options.work_dir, "backup created");

        if let Err(error) = move_file(&ready, &options.target) {
            log(&options.work_dir, "replacement failed; rollback started");
            restore(&options.target, &backup, &options.work_dir);
            return Err(error);
        }
        log(&options.work_dir, "replacement completed");

        match launch(&options.target) {
            Ok(_) => {
                log(&options.work_dir, "new process launched");
                let _ = fs::remove_file(&backup);
                Ok(())
            }
            Err(error) => {
                log(
                    &options.work_dir,
                    "new process launch failed; rollback started",
                );
                restore(&options.target, &backup, &options.work_dir);
                Err(error)
            }
        }
    }

    struct Options {
        pid: u32,
        source: PathBuf,
        target: PathBuf,
        sha256: String,
        version: String,
        work_dir: PathBuf,
        current_version: String,
        manifest: UpdateManifest,
        ready_event: String,
    }

    fn parse_args(args: &[String]) -> Result<Options, String> {
        if args.first().map(String::as_str) != Some("apply") {
            return Err("Usage: rx-updater.exe apply --pid PID --source FILE --target FILE --sha256 HASH --version VERSION --ready-event NAME".into());
        }
        let value = |name: &str| {
            args.windows(2)
                .find(|pair| pair[0] == name)
                .map(|pair| pair[1].clone())
                .ok_or_else(|| format!("Missing updater argument {name}"))
        };
        let source = PathBuf::from(value("--source")?);
        let target = PathBuf::from(value("--target")?);
        let work_dir = std::env::current_dir()
            .map_err(|_| "Could not determine updater directory".to_string())?;
        let sha256 = value("--sha256")?.to_ascii_lowercase();
        let version = value("--version")?;
        let manifest = UpdateManifest {
            version: version.clone(),
            published_at: value("--published-at")?,
            platform: value("--platform")?,
            arch: value("--arch")?,
            url: value("--url")?,
            sha256: sha256.clone(),
            signature: value("--signature")?,
            notes: None,
        };
        Ok(Options {
            pid: value("--pid")?
                .parse()
                .map_err(|_| "Invalid updater process ID".to_string())?,
            sha256,
            version,
            source,
            target,
            work_dir,
            current_version: args
                .windows(2)
                .find(|pair| pair[0] == "--current-version")
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| "unknown".into()),
            manifest,
            ready_event: value("--ready-event")?,
        })
    }

    fn validate_paths(options: &Options) -> Result<(), String> {
        if !options.source.is_absolute() || !options.target.is_absolute() {
            return Err("Updater paths must be absolute".into());
        }
        if !options.source.is_file() || !options.target.is_file() {
            return Err("Updater source or target is missing".into());
        }
        if options.target.extension().and_then(|value| value.to_str()) != Some("exe") {
            return Err("Updater target is not an executable".into());
        }
        if options.sha256.len() != 64
            || !options.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Updater hash is invalid".into());
        }
        Ok(())
    }

    struct UpdateMutex(HANDLE);

    impl UpdateMutex {
        fn acquire() -> Result<Self, String> {
            let name = wide(MUTEX_NAME);
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle == std::ptr::null_mut() {
                return Err("Could not create the launcher update mutex".into());
            }
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(handle) };
                return Err("Another launcher update is already running".into());
            }
            Ok(Self(handle))
        }
    }

    impl Drop for UpdateMutex {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wait_for_exact_process(
        pid: u32,
        target: &Path,
        ready_event_name: &str,
        work_dir: &Path,
    ) -> Result<(), String> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle == std::ptr::null_mut() {
            return Err("Could not open the launcher process for waiting".into());
        }
        let result = (|| {
            let process_path = process_image_path(handle)?;
            let expected = target
                .canonicalize()
                .map_err(|_| "Could not canonicalize the launcher target".to_string())?;
            if process_path != expected {
                return Err("The process ID does not belong to the launcher target".into());
            }
            signal_ready_event(ready_event_name)?;
            log(
                work_dir,
                "launcher process image validated; readiness signaled",
            );
            let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
            if wait != WAIT_OBJECT_0 {
                return Err("Could not wait for the launcher process to exit".into());
            }
            Ok(())
        })();
        unsafe { CloseHandle(handle) };
        result
    }

    fn signal_ready_event(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("Updater readiness event name is missing".into());
        }
        let name = wide(name);
        let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err("Could not open the launcher updater readiness event".into());
        }
        let result = if unsafe { SetEvent(handle) } == 0 {
            Err("Could not signal launcher updater readiness".into())
        } else {
            Ok(())
        };
        unsafe { CloseHandle(handle) };
        result
    }

    fn process_image_path(handle: HANDLE) -> Result<PathBuf, String> {
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
        if ok == 0 {
            return Err("Could not validate the launcher process image".into());
        }
        PathBuf::from(
            String::from_utf16(&buffer[..length as usize])
                .map_err(|_| "Invalid process image path".to_string())?,
        )
        .canonicalize()
        .map_err(|_| "Could not canonicalize the launcher process image".to_string())
    }

    fn verify_hash(path: &Path, expected: &str) -> Result<(), String> {
        let mut file =
            fs::File::open(path).map_err(|_| "Could not open the staged executable".to_string())?;
        let mut hasher = Sha256::new();
        // Keep the large hashing buffer on the heap. The standalone helper
        // has a small Windows thread stack, and a 1 MiB stack array can cause
        // an abrupt stack-overflow termination before the error logger runs.
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| "Could not read the staged executable".to_string())?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let actual = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual != expected {
            return Err("Staged executable hash verification failed".into());
        }
        Ok(())
    }

    fn backup_path(target: &Path) -> Result<PathBuf, String> {
        let stem = target
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "Invalid launcher target name".to_string())?;
        Ok(target.with_file_name(format!("{stem}.previous.exe")))
    }

    fn ready_path(target: &Path) -> Result<PathBuf, String> {
        let stem = target
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "Invalid launcher target name".to_string())?;
        Ok(target.with_file_name(format!("{stem}.ready.exe")))
    }

    fn prepare_ready(source: &Path, target: &Path, expected_hash: &str) -> Result<PathBuf, String> {
        let ready = ready_path(target)?;
        if ready.exists() {
            fs::remove_file(&ready)
                .map_err(|_| "Could not remove a stale adjacent update file".to_string())?;
        }
        let mut input = fs::File::open(source)
            .map_err(|_| "Could not open the staged executable".to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&ready)
            .map_err(|_| "Could not create the adjacent update file".to_string())?;
        std::io::copy(&mut input, &mut output)
            .map_err(|_| "Could not copy the update beside the launcher".to_string())?;
        output
            .sync_all()
            .map_err(|_| "Could not sync the adjacent update file".to_string())?;
        drop(output);
        if let Err(error) = verify_hash(&ready, expected_hash) {
            let _ = fs::remove_file(&ready);
            return Err(error);
        }
        Ok(ready)
    }

    fn move_file(source: &Path, destination: &Path) -> Result<(), String> {
        let source = wide(source.as_os_str());
        let destination = wide(destination.as_os_str());
        let ok = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            Err("Could not move the launcher executable".into())
        } else {
            Ok(())
        }
    }

    fn restore(target: &Path, backup: &Path, work_dir: &Path) {
        if target.exists() {
            let _ = fs::remove_file(target);
        }
        if move_file(backup, target).is_ok() {
            log(work_dir, "rollback completed");
        } else {
            log(
                work_dir,
                "rollback failed; the previous executable remains as the backup",
            );
        }
    }

    fn launch(target: &Path) -> Result<(), String> {
        let mut command = Command::new(target);
        command.current_dir(
            target
                .parent()
                .ok_or_else(|| "Invalid launcher directory".to_string())?,
        );
        command
            .spawn()
            .map_err(|_| String::from("Could not launch the replaced launcher"))?;
        Ok(())
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn log(work_dir: &Path, message: &str) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(work_dir.join("updater.log"))
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or(0);
            let _ = writeln!(file, "{now} {message}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn backup_is_next_to_target_and_deterministic() {
            assert_eq!(
                backup_path(Path::new(r"C:\Apps\rx-launcher.exe")).unwrap(),
                PathBuf::from(r"C:\Apps\rx-launcher.previous.exe")
            );
        }

        #[test]
        fn ready_is_next_to_target_and_deterministic() {
            assert_eq!(
                ready_path(Path::new(r"C:\Apps\rx-launcher.exe")).unwrap(),
                PathBuf::from(r"C:\Apps\rx-launcher.ready.exe")
            );
        }

        #[test]
        fn update_mutex_allows_only_one_helper() {
            let first = UpdateMutex::acquire().unwrap();
            assert!(UpdateMutex::acquire().is_err());
            drop(first);
        }

        #[test]
        fn rollback_restores_the_previous_executable() {
            let directory =
                std::env::temp_dir().join(format!("rx-updater-test-{}", std::process::id()));
            let _ = fs::remove_dir_all(&directory);
            fs::create_dir_all(&directory).unwrap();
            let target = directory.join("rx-launcher.exe");
            let backup = directory.join("rx-launcher.previous.exe");
            fs::write(&target, b"new").unwrap();
            fs::write(&backup, b"old").unwrap();
            restore(&target, &backup, &directory);
            assert_eq!(fs::read(&target).unwrap(), b"old");
            assert!(!backup.exists());
            let _ = fs::remove_dir_all(directory);
        }
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_updater::run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The Project Rx updater is Windows-only");
    std::process::exit(1);
}
