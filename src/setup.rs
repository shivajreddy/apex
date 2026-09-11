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
    // Settle the task first, then decide about the Run key from what exists
    // rather than from what was asked for.
    let task = ensure_admin_task(config.general_flag("run_as_admin", false), &exe);
    ensure_run_at_login(
        want_run_key(config.general_flag("start_on_startup", true), task),
        &exe,
    );
}

/// Whether the `HKCU` `Run` value should be registered.
///
/// The Run key and the elevated logon task both start apex at sign-in, so at
/// most one should be active. The decision keys off whether the task really
/// exists, not off the config flag: if task creation failed - most likely
/// because the UAC prompt was declined - removing the Run key as well would
/// leave apex with no way to start at all.
fn want_run_key(start_on_startup: bool, task_exists: bool) -> bool {
    start_on_startup && !task_exists
}

const TASK_NAME: &str = "Apex Elevated Logon";

/// Create or remove the elevated logon task behind `run_as_admin`.
///
/// A scheduled task with highest privileges is the only way to start
/// elevated at sign-in without a UAC prompt every time. Creating one needs
/// admin, so enabling the setting prompts once; after that it is silent.
///
/// Takes effect at the next sign-in: this process is already running, and at
/// the wrong integrity level to fix that itself.
///
/// Returns whether the task exists *afterwards*, which is what the Run key
/// decision depends on. Always re-queried rather than assumed: creation can
/// fail, and declining the UAC prompt is a perfectly ordinary way for it to.
fn ensure_admin_task(enabled: bool, exe: &Path) -> bool {
    let exists = task_exists();
    if enabled == exists {
        return exists;
    }
    let args = if enabled {
        format!(
            "/create /tn \"{}\" /tr \"\\\"{}\\\"\" /sc onlogon /rl highest /f",
            TASK_NAME,
            exe.display()
        )
    } else {
        format!("/delete /tn \"{TASK_NAME}\" /f")
    };
    run_schtasks(&args);

    let now = task_exists();
    if enabled && !now {
        crate::dlog!("setup: elevated logon task not created; keeping the Run key");
    }
    now
}

fn task_exists() -> bool {
    // Querying needs no elevation, so this is cheap to check every launch.
    std::process::Command::new("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run `schtasks` elevated, and wait for it to finish.
///
/// Waiting is the point. `ShellExecuteW` returns as soon as the process is
/// launched - before the UAC prompt has even been answered - so checking the
/// task straight afterwards would race the user. `ShellExecuteExW` with
/// `SEE_MASK_NOCLOSEPROCESS` hands back a process handle to wait on.
fn run_schtasks(args: &str) {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// Generous, but bounded: this blocks the UI thread at startup, and a
    /// UAC prompt left unanswered must not hang apex forever.
    const TIMEOUT_MS: u32 = 60_000;

    let args_w: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: w!("runas"),
            lpFile: w!("schtasks.exe"),
            lpParameters: PCWSTR(args_w.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        if ShellExecuteExW(&mut info).is_err() {
            // Declining the UAC prompt lands here, which is ordinary.
            crate::dlog!("setup: schtasks {args} was declined or failed to start");
            return;
        }
        if !info.hProcess.is_invalid() {
            if WaitForSingleObject(info.hProcess, TIMEOUT_MS) != WAIT_OBJECT_0 {
                crate::dlog!("setup: schtasks did not finish within {TIMEOUT_MS}ms");
            }
            let _ = CloseHandle(info.hProcess);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Run key and the elevated task both start apex at sign-in, so at
    /// most one may be active - but never neither.
    #[test]
    fn run_key_and_logon_task_are_mutually_exclusive() {
        assert!(want_run_key(true, false), "no task: the Run key must start apex");
        assert!(!want_run_key(true, true), "task exists: the Run key would double-start");
    }

    /// The regression this guards: enabling run_as_admin used to drop the Run
    /// key on the strength of the config flag alone. If creating the task
    /// then failed - a declined UAC prompt is the obvious way - apex was left
    /// with no startup entry at all.
    #[test]
    fn a_failed_task_leaves_the_run_key_in_place() {
        let requested_admin = true;
        let task_created = false; // user declined the prompt
        let _ = requested_admin;
        assert!(
            want_run_key(true, task_created),
            "apex must still start at sign-in when the task was not created"
        );
    }

    #[test]
    fn start_on_startup_still_wins_over_everything() {
        assert!(!want_run_key(false, false));
        assert!(!want_run_key(false, true));
    }
}
