//! Starting things, at the right privilege level.
//!
//! Apex runs elevated (its manifest requires administrator), so the global
//! hotkey works even while an elevated window has focus. But `ShellExecuteW`
//! hands a child its parent's token, and Windows does not consult the
//! target's manifest - a manifest only decides whether an *unelevated* parent
//! must escalate. So an elevated apex would start every app as administrator:
//! Chrome, editors, everything, with all the fallout (admin-owned files,
//! drag-and-drop from normal Explorer blocked by UIPI).
//!
//! So the target is handed to `explorer.exe` instead. A second Explorer
//! process notices the desktop Explorer already running, forwards the request
//! to it and exits - and that one runs as the normal user, so the target comes
//! back to medium integrity. Opening an app from apex then behaves exactly
//! like opening it from the Start menu.

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

/// Whether this process holds an elevated token.
///
/// Cached: it cannot change for the lifetime of the process, and the launch
/// path consults it on every activation.
pub fn is_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open `target`, optionally through `program`, at the user's own integrity.
///
/// `program` is for quicklinks that name an application to open the link
/// with. Explorer cannot forward a program-plus-argument pair, so that case
/// launches directly and, while apex is elevated, inherits elevation - the
/// one gap in the de-elevation, and a deliberate, documented one.
pub fn open(target: &str, program: Option<&str>) -> bool {
    match program {
        None if is_elevated() => via_explorer(target),
        _ => direct(program.unwrap_or(target), program.map(|_| target)),
    }
}

/// Hand the target to Explorer, which is running unelevated, so it does the
/// opening on our behalf.
fn via_explorer(target: &str) -> bool {
    // Quoted: targets are paths and shell: verbs, and both can contain spaces.
    direct("explorer.exe", Some(&format!("\"{target}\"")))
}

fn direct(file: &str, args: Option<&str>) -> bool {
    let file_w = wide(file);
    let args_w = args.map(wide);
    unsafe {
        let inst = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(file_w.as_ptr()),
            args_w
                .as_ref()
                .map(|a| PCWSTR(a.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            None,
            SW_SHOWNORMAL,
        );
        // ShellExecuteW returns a fake HINSTANCE; > 32 means success.
        let ok = inst.0 as isize > 32;
        if !ok {
            crate::dlog!("launch: failed to open {file}");
        }
        ok
    }
}
