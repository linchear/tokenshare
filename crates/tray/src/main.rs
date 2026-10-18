//! Menubar (macOS) / taskbar (Windows) app. It runs a delegate in-process and
//! shows its health as a colored dot. Linux uses the headless `tokenshare`.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod app;
mod icon;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod settings;

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() {
    app::run();
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("tokenshare-tray supports macOS and Windows. On Linux run the headless `tokenshare` daemon.");
    std::process::exit(2);
}
