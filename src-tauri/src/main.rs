// Release Windows builds use the GUI subsystem so launching the application
// does not open a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(any(target_os = "linux", test))]
use std::fmt::Display;

#[cfg(any(target_os = "linux", test))]
fn ignore_path_fix_failure<E: Display>(result: Result<(), E>) {
    if let Err(error) = result {
        eprintln!("Could not refresh the GUI process PATH: {error}. Continuing startup.");
    }
}

#[cfg(target_os = "linux")]
fn fix_path_for_startup() {
    ignore_path_fix_failure(fix_path_env::fix());
}

#[cfg(not(target_os = "linux"))]
fn fix_path_for_startup() {}

fn main() {
    fix_path_for_startup();
    rx_launcher_lib::run()
}

#[cfg(test)]
mod tests {
    use super::ignore_path_fix_failure;

    #[test]
    fn path_fix_failure_is_non_fatal() {
        ignore_path_fix_failure(Err("simulated PATH refresh failure"));
    }
}
