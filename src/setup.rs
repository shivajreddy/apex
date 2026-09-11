//! Self-registration driven by `[general]` config flags.
//!
//! Ensured on every launch (idempotent, ~1ms):
//! - `start_menu`: Start menu shortcut so apex is searchable/pinnable.
//! - `start_on_startup`: HKCU Run registry value so apex starts at sign-in.
//!
//! Disabling a flag removes the corresponding registration.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR, w};

use crate::config::Config;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Apex");

pub fn ensure(config: &Config) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    ensure_start_menu(config.general_flag("start_menu", true), &exe);
    ensure_run_at_login(config.general_flag("start_on_startup", true), &exe);
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

fn ensure_run_at_login(enabled: bool, exe: &Path) {
    unsafe {
        if enabled {
            // Registry value names are case-insensitive; delete-then-set
            // migrates an old lowercase "apex" value to "Apex".
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            let exe_w = wide(exe.as_os_str());
            let status = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(exe_w.as_ptr() as *const core::ffi::c_void),
                (exe_w.len() * 2) as u32,
            );
            if status != ERROR_SUCCESS {
                crate::dlog!("setup: run-at-login registration failed: {status:?}");
            }
        } else {
            let status = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                crate::dlog!("setup: run-at-login removal failed: {status:?}");
            }
        }
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}
