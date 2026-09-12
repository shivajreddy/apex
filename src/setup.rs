//! Self-registration driven by `[general]` config flags.
//!
//! Ensured on every launch (idempotent, ~1ms):
//! - `start_menu`: Start menu shortcut so apex is searchable/pinnable.
//! - `start_on_startup`: an elevated logon task so apex starts at sign-in.
//!
//! Apex requires administrator (its manifest), so start-at-login cannot use
//! the `HKCU` `Run` key - Windows will not launch an elevation-required exe
//! from it. Instead a scheduled task with highest privileges, triggered at
//! logon, starts apex elevated with no UAC prompt at sign-in. Because apex is
//! already elevated when this runs, creating the task needs no prompt either.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR, w};

use crate::config::Config;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Apex");
const TASK_NAME: &str = "Apex Elevated Logon";

pub fn ensure(config: &Config) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    ensure_start_menu(config.general_flag("start_menu", true), &exe);
    ensure_logon_task(config.general_flag("start_on_startup", true), &exe);
    // Apex used to start via the Run key; a requireAdministrator exe cannot,
    // so remove any leftover value from an older install - the task is the
    // only startup path now.
    remove_run_key();
}

fn start_menu_lnk() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Apex.lnk"))
}

fn ensure_start_menu(enabled: bool, exe: &Path) {
    let Some(lnk) = start_menu_lnk() else { return };
    if enabled {
        // Delete-then-write: the filesystem is case-insensitive, so this also
        // migrates an old "apex.lnk" to the properly-cased "Apex.lnk".
        let _ = std::fs::remove_file(&lnk);
        if let Err(e) = write_shortcut(&lnk, exe) {
            crate::dlog!("setup: start menu shortcut failed: {e}");
        }
    } else {
        let _ = std::fs::remove_file(&lnk);
    }
}

fn write_shortcut(lnk: &Path, exe: &Path) -> windows::core::Result<()> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let exe_w = wide(exe.as_os_str());
        link.SetPath(PCWSTR(exe_w.as_ptr()))?;
        link.SetDescription(w!("Apex - ultra-fast launcher"))?;
        let persist: IPersistFile = link.cast()?;
        let lnk_w = wide(lnk.as_os_str());
        persist.Save(PCWSTR(lnk_w.as_ptr()), true)?;
        Ok(())
    }
}

/// Create or remove the elevated logon task that starts apex at sign-in.
///
/// `schtasks` is run as a plain child: apex is already elevated, so `/create
/// /rl highest` needs no separate prompt. When enabled it is re-created each
/// launch (`/f`) so the task always points at the exe's current location, the
/// same way the Start-menu shortcut is re-written each launch.
fn ensure_logon_task(enabled: bool, exe: &Path) {
    if enabled {
        let ok = run_schtasks(&[
            "/create",
            "/tn",
            TASK_NAME,
            "/tr",
            &format!("\"{}\"", exe.display()),
            "/sc",
            "onlogon",
            "/rl",
            "highest",
            "/f",
        ]);
        if !ok {
            crate::dlog!("setup: schtasks create failed");
        }
    } else if task_exists() {
        if !run_schtasks(&["/delete", "/tn", TASK_NAME, "/f"]) {
            crate::dlog!("setup: schtasks delete failed");
        }
    }
}

/// Whether the logon task is registered. Querying needs no elevation.
fn task_exists() -> bool {
    std::process::Command::new("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run_schtasks(args: &[&str]) -> bool {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Remove the legacy `HKCU\...\Run` value if present. Harmless when absent.
fn remove_run_key() {
    unsafe {
        let status = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        if status != windows::Win32::Foundation::ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            crate::dlog!("setup: run-key cleanup failed: {status:?}");
        }
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}
