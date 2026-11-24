//! Out-of-band display for the host embedded in this process.
use std::io;

pub(crate) fn display(app: &tauri::AppHandle, code: String) -> io::Result<()> {
    let title = rust_i18n::t!("password_recovery_title").into_owned();
    show(app, title, code)
}

#[cfg(windows)]
fn show(_app: &tauri::AppHandle, title: String, code: String) -> io::Result<()> {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MessageBoxW,
        },
        core::HSTRING,
    };
    std::thread::Builder::new()
        .name("password-reset-dialog".into())
        .spawn(move || unsafe {
            let _ = MessageBoxW(
                None,
                &HSTRING::from(code),
                &HSTRING::from(title),
                MB_OK | MB_SETFOREGROUND | MB_ICONINFORMATION,
            );
        })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn show(_app: &tauri::AppHandle, title: String, code: String) -> io::Result<()> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let (mut command, input) = {
        let mut cmd = Command::new("/usr/bin/osascript");
        cmd.arg("-");
        (
            cmd,
            format!(
                "display dialog {} with title {} buttons {{\"OK\"}} default button \"OK\" giving up after 300",
                serde_json::to_string(&code)?,
                serde_json::to_string(&title)?
            ),
        )
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = child
        .stdin
        .take()
        .expect("piped dialog input")
        .write_all(input.as_bytes());
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    result
}

#[cfg(target_os = "linux")]
fn show(app: &tauri::AppHandle, title: String, code: String) -> io::Result<()> {
    use gtk::prelude::*;
    app.run_on_main_thread(move || {
        let dialog = gtk::MessageDialog::builder()
            .message_type(gtk::MessageType::Info)
            .buttons(gtk::ButtonsType::Ok)
            .text(title)
            .secondary_text(code)
            .build();
        dialog.connect_response(|window, _| window.close());
        dialog.present();
        gtk::glib::timeout_add_seconds_local_once(300, move || dialog.close());
    })
    .map_err(io::Error::other)
}
