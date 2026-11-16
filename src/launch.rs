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
//!
//! That route carries a target and nothing else. A program with arguments -
//! a script's interpreter, a quicklink's `open_with` - goes to the desktop's
//! Explorer through its automation object instead ([`start`]), which also
//! takes the folder to start in and how to show the window.

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, IDispatch, IServiceProvider};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
    SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellExecuteW,
    ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{SHOW_WINDOW_CMD, SW_HIDE, SW_SHOWNORMAL};
use windows::core::{BSTR, Interface, PCWSTR, w};

/// How a started program's window appears.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Show {
    Normal,
    /// Not at all. A console program's window is created hidden rather than
    /// shown and then hidden, so not even a flash of it appears - Windows
    /// Terminal included, when it is the default terminal.
    Hidden,
}

impl Show {
    fn cmd(self) -> SHOW_WINDOW_CMD {
        match self {
            Self::Normal => SW_SHOWNORMAL,
            Self::Hidden => SW_HIDE,
        }
    }
}

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
/// goes through [`start`].
pub fn open(target: &str, program: Option<&str>) -> bool {
    match program {
        None if is_elevated() => via_explorer(target),
        None => direct(target, ""),
        Some(program) => start(program, target, "", Show::Normal),
    }
}

/// Start `program` with `args` in `dir` (empty for no particular folder), at
/// the user's own integrity.
///
/// While apex is elevated, the desktop's Explorer is asked to start it
/// ([`start_from_desktop`]). Failing that - no Explorer running as the shell
/// - it starts directly, and so elevated, rather than not at all.
pub fn start(program: &str, args: &str, dir: &str, show: Show) -> bool {
    if is_elevated() {
        match unsafe { start_from_desktop(program, args, dir, show) } {
            Ok(()) => return true,
            Err(e) => crate::dlog!(
                "launch: no desktop to start {program} from ({e}), starting it directly"
            ),
        }
    }
    execute(w!("open"), program, args, dir, show)
}

/// [`start`], as administrator - the one launch that must *not* shed
/// privileges.
///
/// While apex is elevated a plain `ShellExecuteW` hands the child apex's own
/// token, which is exactly "run as administrator", with nothing to prompt
/// for. Unelevated (a dev build), the shell's `runas` verb raises the UAC
/// prompt instead. Returns false when that prompt is declined - an ordinary
/// outcome, not an error.
pub fn start_as_admin(program: &str, args: &str, dir: &str, show: Show) -> bool {
    let verb = if is_elevated() {
        w!("open")
    } else {
        w!("runas")
    };
    execute(verb, program, args, dir, show)
}

/// Open `target` as administrator; see [`start_as_admin`].
pub fn open_as_admin(target: &str) -> bool {
    start_as_admin(target, "", "", Show::Normal)
}

/// Show the file at `path` selected in an Explorer window. An Explorer
/// started by an elevated apex hands the request to the desktop's own,
/// which runs as the user.
pub fn reveal(path: &str) -> bool {
    direct("explorer.exe", &format!("/select,\"{path}\""))
}

/// Hand the target to Explorer, which is running unelevated, so it does the
/// opening on our behalf.
fn via_explorer(target: &str) -> bool {
    // Quoted: targets are paths and shell: verbs, and both can contain spaces.
    direct("explorer.exe", &format!("\"{target}\""))
}

fn direct(file: &str, args: &str) -> bool {
    execute(w!("open"), file, args, "", Show::Normal)
}

/// Have the desktop's own Explorer start `program`. Whatever Explorer starts
/// runs as the signed-in user, so this is the documented way for an elevated
/// process to start an unelevated one
/// (<https://devblogs.microsoft.com/oldnewthing/20131118-00/?p=2643>): the
/// desktop's folder view hands out the shell's automation object, whose
/// `ShellExecute` then runs inside Explorer.
unsafe fn start_from_desktop(
    program: &str,
    args: &str,
    dir: &str,
    show: Show,
) -> windows::core::Result<()> {
    unsafe {
        let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let mut hwnd = 0;
        let desktop = shell_windows.FindWindowSW(
            &VARIANT::from(CSIDL_DESKTOP as i32),
            &VARIANT::default(),
            SWC_DESKTOP,
            &mut hwnd,
            SWFO_NEEDDISPATCH,
        )?;
        let browser: IShellBrowser = desktop
            .cast::<IServiceProvider>()?
            .QueryService(&SID_STopLevelBrowser)?;
        // The view's automation object is only handed out as IDispatch.
        let view: IShellFolderViewDual = browser
            .QueryActiveShellView()?
            .GetItemObject::<IDispatch>(SVGIO_BACKGROUND)?
            .cast()?;
        let shell: IShellDispatch2 = view.Application()?.cast()?;
        shell.ShellExecute(
            &BSTR::from(program),
            &VARIANT::from(args),
            &VARIANT::from(dir),
            &VARIANT::from("open"),
            &VARIANT::from(show.cmd().0),
        )
    }
}

fn execute(verb: PCWSTR, file: &str, args: &str, dir: &str, show: Show) -> bool {
    let file_w = wide(file);
    let args_w = wide(args);
    let dir_w = wide(dir);
    // Empty means none: no arguments, no particular folder.
    let optional = |s: &str, w: &[u16]| {
        if s.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(w.as_ptr())
        }
    };
    unsafe {
        let inst = ShellExecuteW(
            None,
            verb,
            PCWSTR(file_w.as_ptr()),
            optional(args, &args_w),
            optional(dir, &dir_w),
            show.cmd(),
        );
        // ShellExecuteW returns a fake HINSTANCE; > 32 means success.
        let ok = inst.0 as isize > 32;
        if !ok {
            crate::dlog!("launch: failed to open {file}");
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetShellWindow, GetWindowThreadProcessId, IsWindowVisible,
    };
    use windows::core::{BOOL, PWSTR};

    /// Visible top-level windows, by handle.
    fn visible_windows() -> Vec<isize> {
        unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    (*(list.0 as *mut Vec<isize>)).push(hwnd.0 as isize);
                }
            }
            BOOL(1)
        }
        let mut list: Vec<isize> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut _ as isize));
        }
        list
    }

    /// The file name of the program that owns a window, lowercased.
    fn owner(hwnd: isize) -> String {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid));
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return format!("pid {pid}");
            };
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(process);
            let path = if ok {
                String::from_utf16_lossy(&buf[..len as usize])
            } else {
                format!("pid {pid}")
            };
            path.rsplit('\\').next().unwrap_or_default().to_lowercase()
        }
    }

    /// The desktop route, end to end: Explorer starts a hidden PowerShell,
    /// which reports its parent, and no console or terminal window appears
    /// while it runs. Needs a signed-in desktop; run by hand:
    ///
    /// `cargo test -- --ignored started_from_the_desktop --nocapture`
    #[test]
    #[ignore]
    fn started_from_the_desktop_unseen() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("launch probe");
        std::fs::create_dir_all(&dir).unwrap();
        let report = dir.join("parent.txt");
        let _ = std::fs::remove_file(&report);
        std::fs::write(
            dir.join("parent.ps1"),
            "$parent = (Get-CimInstance Win32_Process -Filter \"ProcessId=$PID\").ParentProcessId\r\n\
             Start-Sleep -Seconds 2\r\n\
             Set-Content -LiteralPath (Join-Path $PSScriptRoot 'parent.txt') -Value $parent\r\n",
        )
        .unwrap();
        let dir = dir.to_str().unwrap();
        let powershell = format!(
            r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
            std::env::var("SystemRoot").unwrap()
        );
        let args = format!(
            r#"-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{dir}\parent.ps1""#
        );

        let before = visible_windows();
        unsafe { start_from_desktop(&powershell, &args, dir, Show::Hidden) }
            .expect("the desktop's Explorer takes the request");
        // Mid-run: the script sleeps for two seconds.
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let new: Vec<String> = visible_windows()
            .into_iter()
            .filter(|w| !before.contains(w))
            .map(owner)
            .collect();
        eprintln!("new visible windows while it ran: {new:?}");
        let consoles = [
            "windowsterminal.exe",
            "openconsole.exe",
            "conhost.exe",
            "powershell.exe",
        ];
        assert!(
            !new.iter().any(|name| consoles.contains(&name.as_str())),
            "a console window showed: {new:?}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let parent = loop {
            if let Ok(text) = std::fs::read_to_string(&report)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "the script never ran");
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let mut explorer = 0;
        unsafe {
            GetWindowThreadProcessId(GetShellWindow(), Some(&mut explorer));
        }
        assert_eq!(parent, explorer, "started by the desktop's Explorer");
    }
}
