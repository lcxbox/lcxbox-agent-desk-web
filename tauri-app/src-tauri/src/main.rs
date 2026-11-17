#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    // Before anything else: the sandbox child's stdout carries only its frame.
    // Same dispatch as the server binary: sandbox children and helpers are
    // re-launched from `current_exe`, which is this binary in the desktop app.
    if let Some(code) = desk_utils::process_sandbox::run_child_if_requested(&[
        desk_document_conversion::sandbox::SERVICE,
    ]) {
        return ExitCode::from(code as u8);
    }
    // The Windows command-sandbox launcher must keep the full 32-bit exit
    // code, which `ExitCode::from(u8)` would truncate.
    #[cfg(windows)]
    if let Some(code) = lcxl_remote_desk_server::exec_sandbox_launcher::run_if_requested() {
        std::process::exit(code);
    }
    #[cfg(windows)]
    if let Some(code) = lcxl_remote_desk_server::windows_application_host::run_if_requested() {
        return ExitCode::from(code as u8);
    }
    #[cfg(windows)]
    {
        let mut args = std::env::args_os();
        let _ = args.next();
        if args.next().as_deref()
            == Some(std::ffi::OsStr::new(
                lcxl_remote_desk_server::windows_office_helper::MODE,
            ))
        {
            if args.next().is_some() {
                return ExitCode::from(2);
            }
            return ExitCode::from(
                lcxl_remote_desk_server::windows_office_helper::run_stdio() as u8
            );
        }
    }
    let run_result = lcxl_remote_desk_tauri::run();
    match run_result {
        Ok(_) => {
            log::info!("Server exit normally");
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error!("Server exit with error: {}", e);
            // log may not be initialized, so print to stderr
            eprintln!("Server exit with error: {}", e);
            ExitCode::FAILURE
        }
    }
}
