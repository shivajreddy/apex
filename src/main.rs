// No console window in release; keep one in debug for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Debug-only logging to stderr; compiled out entirely in release
/// (arguments are still name-checked so builds stay warning-free).
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        #[cfg(debug_assertions)]
        eprintln!("[Apex] {}", format!($($arg)*));
        #[cfg(not(debug_assertions))]
        {
            let _ = format_args!($($arg)*);
        }
    }};
}

mod app;
mod appindex;
mod config;
mod editor;
mod frecency;
mod fuzzy;
mod icon;
mod plugin;
mod plugins;
mod render;
mod setup;
mod window;

/// Construct the set of enabled plugins. Disabled plugins are never built,
/// so they cost zero memory and zero startup time.
fn plugins(config: &config::Config) -> Vec<Box<dyn plugin::Plugin>> {
    let mut list: Vec<Box<dyn plugin::Plugin>> = Vec::new();
    if config.plugin_enabled(plugins::search::ID) {
        list.push(Box::new(plugins::search::Search::new(config.aliases_map())));
    }
    if config.plugin_enabled(plugins::quicklinks::ID) {
        list.push(Box::new(plugins::quicklinks::Quicklinks::new(
            config.subtables(plugins::quicklinks::ID),
        )));
    }
    if config.plugin_enabled(plugins::commands::ID) {
        list.push(Box::new(plugins::commands::Commands::new()));
    }
    list
}

fn main() {
    // The index helper is this same exe: it builds the application index,
    // writes it to disk and exits, keeping the shell's enumeration and
    // imaging DLLs out of the launcher. Decided before anything else so a
    // helper never touches the single-instance mutex, setup, or a window.
    if std::env::args().nth(1).as_deref() == Some(appindex::HELPER_FLAG) {
        appindex::helper_main();
        return;
    }
    let config = config::Config::load();
    if let Err(err) = window::run(&config) {
        fatal(&format!("Apex failed to start:\n{err}"));
    }
}

fn fatal(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::{HSTRING, w};
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("Apex"), MB_OK | MB_ICONERROR);
    }
}
