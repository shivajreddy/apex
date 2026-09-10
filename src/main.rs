// No console window in release; keep one in debug for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Debug-only logging to stderr; compiled out entirely in release.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        eprintln!("[apex] {}", format!($($arg)*))
    };
}

mod window;

fn main() {
    if let Err(err) = window::run() {
        fatal(&format!("apex failed to start:\n{err}"));
    }
}

fn fatal(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{HSTRING, w};
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("apex"), MB_OK | MB_ICONERROR);
    }
}
