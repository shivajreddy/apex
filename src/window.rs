//! Window creation, global hotkey, input routing, and the message loop.
//!
//! The global hotkey uses a low-level keyboard hook (WH_KEYBOARD_LL) rather
//! than RegisterHotKey: the hook sees the combo before the shell does, which
//! lets Apex claim system-reserved combos like Ctrl+Esc (Start menu) exactly
//! the way Raycast does - and it can never fail with "hotkey already
//! registered".

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::Relaxed};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::app::{App, Mode, UiOutcome};
use crate::plugin::ShellCommand;
use crate::render;

const CARET_TIMER_ID: usize = 1;
const CARET_BLINK_MS: u32 = 530;
const HOTKEY_ID: i32 = 1;

/// Posted by the keyboard hook when the hotkey combo fires.
const WM_APP_TOGGLE: u32 = WM_APP + 1;
/// Posted by either hook when the user interacts outside the window.
const WM_APP_DISMISS: u32 = WM_APP + 2;
/// Tray icon callback; the mouse event arrives in lparam.
const WM_APP_TRAY: u32 = WM_APP + 3;

const TRAY_UID: u32 = 1;
// Tray menu command ids.
const IDM_OPEN: usize = 1;
const IDM_RELOAD: usize = 2;
const IDM_CONFIG: usize = 3;
const IDM_QUIT: usize = 4;

// Hook state (the hook procs are free functions, so this lives in statics).
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);
static HOOK_MODS: AtomicU32 = AtomicU32::new(0);
static HOOK_VK: AtomicU32 = AtomicU32::new(0);
/// Suppresses autorepeat while the hotkey chord is held.
static HOOK_HELD: AtomicBool = AtomicBool::new(false);
/// Window is currently shown.
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// Mouse hook handle; installed only while the window is visible.
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
/// Single-instance mutex, kept so Restart can release it before relaunching.
static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);
/// Embedded app icon, reused for the tray.
static APP_ICON: AtomicIsize = AtomicIsize::new(0);
/// Whether the tray icon is currently registered.
static TRAY_SHOWN: AtomicBool = AtomicBool::new(false);

pub fn run(config: &crate::config::Config) -> Result<()> {
    unsafe {
        // Single instance: bail silently if apex is already running.
        let mutex = CreateMutexW(None, true, w!("Local\\apex-launcher-mutex"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        INSTANCE_MUTEX.store(mutex.0 as isize, Relaxed);

        // Shell launches (shell:AppsFolder) want COM on the calling thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);

        // Start menu entry + run-at-login, per config.
        crate::setup::ensure(config);

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class_name = w!("ApexWindow");

        // Embedded app icon (id 1, from build.rs / assets/apex.ico).
        // MAKEINTRESOURCE(1): the integer *is* the "pointer".
        let icon = LoadImageW(
            Some(instance),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        APP_ICON.store(icon.0 as isize, Relaxed);

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_thread());
        }

        // WS_EX_TOOLWINDOW keeps apex out of the taskbar and Alt+Tab.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name,
            w!("Apex"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;

        // Windows 11 rounded corners; harmless no-op on Windows 10.
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const core::ffi::c_void,
            size_of_val(&corner) as u32,
        );
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const core::ffi::c_void,
            size_of_val(&dark) as u32,
        );

        // Attach application state to the window.
        let app = Box::new(App::new(crate::plugins(config)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        if config.general_flag("tray_icon", true) {
            add_tray(hwnd);
        }

        // Global hotkey via low-level keyboard hook (see module docs).
        HOOK_HWND.store(hwnd.0 as isize, Relaxed);
        HOOK_MODS.store(config.hotkey_mods, Relaxed);
        HOOK_VK.store(config.hotkey_vk, Relaxed);
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)?;

        // Belt-and-suspenders: UIPI skips our hook while an elevated window
        // (e.g. Task Manager) has focus, but registered hotkeys still fire.
        // In the normal case the hook swallows the chord before hotkey
        // matching runs, so both never fire together. Non-fatal if taken.
        let mods = HOT_KEY_MODIFIERS(config.hotkey_mods) | MOD_NOREPEAT;
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, mods, config.hotkey_vk).is_err() {
            crate::dlog!("RegisterHotKey fallback unavailable (combo in use elsewhere)");
        }
        crate::dlog!("hooks installed, entering message loop");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
        let _ = UnhookWindowsHookEx(hook);
        let mouse = MOUSE_HOOK.swap(0, Relaxed);
        if mouse != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(mouse as *mut core::ffi::c_void));
        }
        Ok(())
    }
}

fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        VIRTUAL_KEY(vk as u16),
        VK_CONTROL
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_SHIFT
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_MENU
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
    )
}

/// Exact-match the chord: every configured modifier down, every other
/// modifier up. Exactness matters - with Ctrl+Esc configured, Ctrl+Shift+Esc
/// (Task Manager) must pass through untouched.
fn chord_matches(mods: u32) -> bool {
    let down = |vk: VIRTUAL_KEY| unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };
    let win = down(VK_LWIN) || down(VK_RWIN);
    down(VK_MENU) == (mods & 0x1 != 0)
        && down(VK_CONTROL) == (mods & 0x2 != 0)
        && down(VK_SHIFT) == (mods & 0x4 != 0)
        && win == (mods & 0x8 != 0)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let msg = wparam.0 as u32;
            let is_down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let hwnd = HWND(HOOK_HWND.load(Relaxed) as *mut core::ffi::c_void);

            if info.vkCode == HOOK_VK.load(Relaxed) {
                if is_down {
                    if chord_matches(HOOK_MODS.load(Relaxed)) {
                        if !HOOK_HELD.swap(true, Relaxed) {
                            let _ = PostMessageW(Some(hwnd), WM_APP_TOGGLE, WPARAM(0), LPARAM(0));
                        }
                        // Swallow so the shell never sees the combo
                        // (e.g. Ctrl+Esc won't open the Start menu).
                        return LRESULT(1);
                    }
                } else {
                    HOOK_HELD.store(false, Relaxed);
                }
            }

            // Typing that lands elsewhere while we're shown means the user is
            // interacting outside Apex (activation can be denied over an
            // elevated window): dismiss. Foreground is checked live rather
            // than cached - activation state updates asynchronously, and a
            // stale flag here silently dismissed the window mid-typing.
            // Modifier keys are ignored so the toggle chord doesn't
            // dismiss-then-retoggle.
            if is_down
                && VISIBLE.load(Relaxed)
                && !is_modifier_vk(info.vkCode)
                && GetForegroundWindow() != hwnd
            {
                crate::dlog!("dismiss: key {:#x} went elsewhere", info.vkCode);
                let _ = PostMessageW(Some(hwnd), WM_APP_DISMISS, WPARAM(1), LPARAM(0));
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Installed only while the window is visible: any click outside the window
/// rect dismisses, independent of Win32 activation (which we may never get
/// when shown over an elevated window).
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            match wparam.0 as u32 {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                    let hwnd = HWND(HOOK_HWND.load(Relaxed) as *mut core::ffi::c_void);
                    let mut rc = RECT::default();
                    if GetWindowRect(hwnd, &mut rc).is_ok() {
                        let p = info.pt;
                        let outside =
                            p.x < rc.left || p.x >= rc.right || p.y < rc.top || p.y >= rc.bottom;
                        if outside {
                            crate::dlog!("dismiss: click outside at ({},{})", p.x, p.y);
                            let _ = PostMessageW(Some(hwnd), WM_APP_DISMISS, WPARAM(2), LPARAM(0));
                        }
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Borrow the App attached to the window. Callers must not overlap two
/// mutable borrows; wndproc arms fetch it locally and drop it before any
/// call that fetches it again (show/hide re-fetch internally).
unsafe fn app_mut(hwnd: HWND) -> Option<&'static mut App> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        ptr.as_mut()
    }
}

unsafe fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_TOGGLE => {
                toggle(hwnd);
                LRESULT(0)
            }
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
                // Fallback path: fires when UIPI bypassed the keyboard hook
                // (elevated window had focus); the hook swallows the chord
                // otherwise, so this can't double-fire.
                toggle(hwnd);
                LRESULT(0)
            }
            WM_APP_TRAY => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP => toggle(hwnd),
                    WM_RBUTTONUP => tray_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match (wparam.0 & 0xFFFF) as usize {
                    IDM_OPEN => show(hwnd),
                    IDM_RELOAD => reload_plugins(hwnd),
                    IDM_CONFIG => crate::plugins::commands::open_config_file(),
                    IDM_QUIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_DISMISS => {
                crate::dlog!("WM_APP_DISMISS (visible={})", IsWindowVisible(hwnd).as_bool());
                if IsWindowVisible(hwnd).as_bool() {
                    hide(hwnd);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                on_keydown(hwnd, VIRTUAL_KEY(wparam.0 as u16));
                LRESULT(0)
            }
            WM_CHAR => {
                on_char(hwnd, wparam.0 as u16);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == CARET_TIMER_ID => {
                if let Some(app) = app_mut(hwnd) {
                    app.caret_visible = !app.caret_visible;
                }
                invalidate(hwnd);
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(app) = app_mut(hwnd) {
                    if let Some(r) = app.renderer.as_mut() {
                        let w = (lparam.0 & 0xFFFF) as u32;
                        let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
                        r.resize(w, h);
                    }
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                if let Some(app) = app_mut(hwnd) {
                    if let Some(r) = app.renderer.as_mut() {
                        r.update_dpi((wparam.0 & 0xFFFF) as f32);
                    }
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                if let Some(app) = app_mut(hwnd) {
                    if app.ensure_renderer().is_some() {
                        let panel = match &app.mode {
                            Mode::Search => None,
                            Mode::Actions { actions, selected } => Some(render::PanelView::Actions {
                                actions,
                                selected: *selected,
                            }),
                            Mode::TextInput { prompt, buffer, .. } => {
                                Some(render::PanelView::TextInput { prompt, buffer })
                            }
                            Mode::Form {
                                title,
                                fields,
                                focused,
                                ..
                            } => Some(render::PanelView::Form {
                                title,
                                fields,
                                focused: *focused,
                            }),
                        };
                        let frame = render::Frame {
                            query: &app.query,
                            caret_visible: app.caret_visible,
                            results: &app.results,
                            selected: app.selected,
                            panel,
                        };
                        if let Some(r) = app.renderer.as_mut() {
                            r.draw(hwnd, &frame);
                        }
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Dismiss when the window loses focus (click elsewhere), Raycast-style.
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    hide(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                remove_tray(hwnd);
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut App;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Which mode the app is in, without holding a borrow.
#[derive(PartialEq, Clone, Copy)]
enum ModeKind {
    Search,
    Actions,
    TextInput,
    Form,
}

unsafe fn mode_kind(hwnd: HWND) -> ModeKind {
    unsafe {
        match app_mut(hwnd).map(|a| &a.mode) {
            Some(Mode::Actions { .. }) => ModeKind::Actions,
            Some(Mode::TextInput { .. }) => ModeKind::TextInput,
            Some(Mode::Form { .. }) => ModeKind::Form,
            _ => ModeKind::Search,
        }
    }
}

/// Apply a mode-level outcome: dismiss, refit and repaint, or run something
/// only the window can do.
unsafe fn settle(hwnd: HWND, outcome: UiOutcome) {
    unsafe {
        match outcome {
            UiOutcome::Hide => hide(hwnd),
            UiOutcome::Stay => {
                resize_to_content(hwnd);
                invalidate(hwnd);
            }
            UiOutcome::Shell(cmd) => run_shell_command(hwnd, cmd),
        }
    }
}

unsafe fn run_shell_command(hwnd: HWND, cmd: ShellCommand) {
    unsafe {
        match cmd {
            ShellCommand::Quit => {
                let _ = DestroyWindow(hwnd);
            }
            ShellCommand::Restart => restart(hwnd),
            ShellCommand::ToggleTray => {
                toggle_tray(hwnd);
                hide(hwnd);
            }
            // Handled by App, which owns the history.
            ShellCommand::ClearHistory => {
                resize_to_content(hwnd);
                invalidate(hwnd);
            }
        }
    }
}

/// Relaunch apex and exit.
///
/// The single-instance mutex has to go first: the replacement checks it
/// immediately on startup and would otherwise see this process still holding
/// it and quit silently. Closing our only handle destroys the named object.
unsafe fn restart(hwnd: HWND) {
    unsafe {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mutex = INSTANCE_MUTEX.swap(0, Relaxed);
        if mutex != 0 {
            let _ = CloseHandle(HANDLE(mutex as *mut core::ffi::c_void));
        }
        if let Err(e) = std::process::Command::new(&exe).spawn() {
            crate::dlog!("restart: failed to spawn {}: {e}", exe.display());
        }
        let _ = DestroyWindow(hwnd);
    }
}

// ---- tray icon --------------------------------------------------------

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    }
}

unsafe fn add_tray(hwnd: HWND) {
    unsafe {
        if TRAY_SHOWN.load(Relaxed) {
            return;
        }
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = HICON(APP_ICON.load(Relaxed) as *mut core::ffi::c_void);
        for (i, c) in "Apex".encode_utf16().enumerate() {
            nid.szTip[i] = c;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            TRAY_SHOWN.store(true, Relaxed);
        }
    }
}

unsafe fn remove_tray(hwnd: HWND) {
    unsafe {
        if !TRAY_SHOWN.swap(false, Relaxed) {
            return;
        }
        let nid = tray_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

unsafe fn toggle_tray(hwnd: HWND) {
    unsafe {
        let shown = TRAY_SHOWN.load(Relaxed);
        if shown {
            remove_tray(hwnd);
        } else {
            add_tray(hwnd);
        }
        crate::config::set_general_flag_file("tray_icon", !shown);
    }
}

unsafe fn tray_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN, w!("Open Apex"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_RELOAD, w!("Reload"));
        let _ = AppendMenuW(menu, MF_STRING, IDM_CONFIG, w!("Open Config"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, w!("Quit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required foreground dance: without it the menu refuses to close
        // when you click elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

unsafe fn reload_plugins(hwnd: HWND) {
    unsafe {
        if let Some(app) = app_mut(hwnd) {
            for p in &mut app.plugins {
                p.refresh();
            }
        }
    }
}

/// Clipboard text as a single line.
///
/// Control characters are dropped rather than replaced with spaces: the
/// common case is a URL copied with a trailing newline, where a space would
/// corrupt it.
unsafe fn clipboard_text(hwnd: HWND) -> Option<String> {
    // CF_UNICODETEXT, spelled out to avoid pulling in the Ole feature.
    const CF_UNICODETEXT: u32 = 13;
    /// Cap: these are all one-line fields, not a document editor.
    const MAX_CHARS: usize = 4096;

    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return None;
        }
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while len < MAX_CHARS && *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hglobal);
            Some(s)
        })();
        // Always close, even if reading failed: leaving the clipboard open
        // blocks every other process from using it.
        let _ = CloseClipboard();
        text.map(|s| s.chars().filter(|c| !c.is_control()).collect())
    }
}

unsafe fn on_keydown(hwnd: HWND, key: VIRTUAL_KEY) {
    unsafe {
        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;

        // Ctrl+Q quits entirely (dev convenience until a tray icon exists).
        if key == VK_Q && ctrl {
            let _ = DestroyWindow(hwnd);
            return;
        }

        // Paste, in whichever text field has focus. Handled here rather than
        // in on_char because Ctrl+V arrives as WM_CHAR 0x16, a control code
        // the text handlers correctly ignore.
        if (key == VK_V && ctrl) || (key == VK_INSERT && shift) {
            if let Some(text) = clipboard_text(hwnd)
                && app_mut(hwnd).map(|a| a.paste(&text)).unwrap_or(false)
            {
                resize_to_content(hwnd);
                invalidate(hwnd);
            }
            return;
        }

        match mode_kind(hwnd) {
            ModeKind::Search => match key {
                VK_ESCAPE => hide(hwnd),
                VK_K if ctrl => {
                    if app_mut(hwnd).map(|a| a.open_actions()).unwrap_or(false) {
                        invalidate(hwnd);
                    }
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.move_selection(1);
                    }
                    invalidate(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.move_selection(-1);
                    }
                    invalidate(hwnd);
                }
                VK_RETURN => {
                    let outcome = app_mut(hwnd)
                        .map(|a| a.activate_selected())
                        .unwrap_or(UiOutcome::Stay);
                    settle(hwnd, outcome);
                }
                _ => {}
            },
            ModeKind::Actions => match key {
                VK_ESCAPE => {
                    if let Some(app) = app_mut(hwnd) {
                        app.close_panel();
                    }
                    invalidate(hwnd);
                }
                VK_K if ctrl => {
                    if let Some(app) = app_mut(hwnd) {
                        app.close_panel();
                    }
                    invalidate(hwnd);
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.panel_move(1);
                    }
                    invalidate(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.panel_move(-1);
                    }
                    invalidate(hwnd);
                }
                VK_RETURN => {
                    let outcome = app_mut(hwnd)
                        .map(|a| a.run_panel_action())
                        .unwrap_or(UiOutcome::Stay);
                    settle(hwnd, outcome);
                }
                _ => {}
            },
            ModeKind::TextInput => match key {
                VK_ESCAPE => {
                    if let Some(app) = app_mut(hwnd) {
                        app.close_panel();
                    }
                    resize_to_content(hwnd);
                    invalidate(hwnd);
                }
                VK_RETURN => {
                    let outcome = app_mut(hwnd)
                        .map(|a| a.submit_text_input())
                        .unwrap_or(UiOutcome::Stay);
                    settle(hwnd, outcome);
                }
                _ => {}
            },
            ModeKind::Form => match key {
                VK_ESCAPE => {
                    if let Some(app) = app_mut(hwnd) {
                        app.close_panel();
                    }
                    resize_to_content(hwnd);
                    invalidate(hwnd);
                }
                // Tab and the arrows both move between fields; Shift+Tab and
                // Up go back.
                VK_TAB => {
                    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(if shift { -1 } else { 1 });
                    }
                    invalidate(hwnd);
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(1);
                    }
                    invalidate(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(-1);
                    }
                    invalidate(hwnd);
                }
                VK_RETURN => {
                    let outcome = app_mut(hwnd).map(|a| a.submit_form()).unwrap_or(UiOutcome::Stay);
                    settle(hwnd, outcome);
                }
                _ => {}
            },
        }
    }
}

unsafe fn on_char(hwnd: HWND, unit: u16) {
    unsafe {
        match mode_kind(hwnd) {
            ModeKind::Search => {
                let changed = match unit {
                    0x08 => app_mut(hwnd).map(|a| a.backspace(false)).unwrap_or(false),
                    0x7F => app_mut(hwnd).map(|a| a.backspace(true)).unwrap_or(false),
                    u if u >= 0x20 => app_mut(hwnd).map(|a| a.insert_utf16(u)).unwrap_or(false),
                    _ => false,
                };
                if changed {
                    if let Some(app) = app_mut(hwnd) {
                        app.caret_visible = true;
                    }
                    SetTimer(Some(hwnd), CARET_TIMER_ID, CARET_BLINK_MS, None);
                    resize_to_content(hwnd);
                    invalidate(hwnd);
                }
            }
            ModeKind::TextInput => {
                if let Some(app) = app_mut(hwnd) {
                    app.text_input_char(unit);
                }
                invalidate(hwnd);
            }
            ModeKind::Form => {
                // Tab arrives as WM_CHAR 0x09 too; field movement is handled
                // in on_keydown, so swallow it here rather than inserting it.
                if unit != 0x09 {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_char(unit);
                    }
                    invalidate(hwnd);
                }
            }
            ModeKind::Actions => {}
        }
    }
}

unsafe fn toggle(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            hide(hwnd);
        } else {
            show(hwnd);
        }
    }
}

/// DPI scale factor for the monitor under the cursor, plus its work area.
unsafe fn cursor_monitor_metrics() -> (f32, RECT) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);

        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut mi);

        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        (dpi_x as f32 / 96.0, mi.rcWork)
    }
}

/// Show centered (upper third) on the monitor containing the cursor,
/// scaled to that monitor's DPI.
unsafe fn show(hwnd: HWND) {
    unsafe {
        let (scale, work) = cursor_monitor_metrics();

        // Rebuild the most-used list before measuring: the app index may have
        // finished loading, and the last launch may have reordered it.
        let content_h = match app_mut(hwnd) {
            Some(a) => {
                a.refresh_on_show();
                a.content_height()
            }
            None => render::INPUT_H,
        };
        let w = (render::WINDOW_WIDTH * scale) as i32;
        let h = (content_h * scale) as i32;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top) / 5;

        crate::dlog!("show: x={x} y={y} w={w} h={h} scale={scale}");
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        VISIBLE.store(true, Relaxed);
        force_foreground(hwnd);
        let _ = SetFocus(Some(hwnd));

        // Outside-click dismissal, active only while shown.
        if MOUSE_HOOK.load(Relaxed) == 0 {
            if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
                MOUSE_HOOK.store(h.0 as isize, Relaxed);
            }
        }

        if let Some(app) = app_mut(hwnd) {
            app.caret_visible = true;
        }
        SetTimer(Some(hwnd), CARET_TIMER_ID, CARET_BLINK_MS, None);
        invalidate(hwnd);
    }
}

/// Grow/shrink the window height to fit the current results.
unsafe fn resize_to_content(hwnd: HWND) {
    unsafe {
        let Some(app) = app_mut(hwnd) else { return };
        let content_h = app.content_height();
        let dpi = GetDpiForWindow(hwnd) as f32;
        let h = (content_h * dpi / 96.0) as i32;
        crate::dlog!("resize_to_content: dpi={dpi} content_h={content_h} h={h}");

        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        if rc.bottom - rc.top != h {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                rc.right - rc.left,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Bring the window to the foreground reliably.
///
/// A background process may not steal foreground (SetForegroundWindow is
/// blocked unless the process received the last input), and the keyboard
/// hook posts to us from outside any input grant. Ladder:
/// 1. plain SetForegroundWindow
/// 2. drop the foreground lock timeout, then AttachThreadInput to the
///    current foreground thread and retry (restoring the timeout after)
///
/// Deliberately never injects synthetic keystrokes: the classic "tap Alt to
/// earn focus" trick fires a real key event that lands in whatever window is
/// focused - with Ctrl held (as during Ctrl+Esc) it reads as AltGr and can
/// emit a stray character into the query.
unsafe fn force_foreground(hwnd: HWND) {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }

        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};

        // Remember and clear the foreground lock timeout.
        let mut prev: u32 = 0;
        let _ = SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut prev as *mut u32 as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let _ = SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(std::ptr::null_mut()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );

        let fg = GetForegroundWindow();
        let our_tid = GetCurrentThreadId();
        let fg_tid = if fg.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let attached = fg_tid != 0 && fg_tid != our_tid;
        if attached {
            let _ = AttachThreadInput(our_tid, fg_tid, true);
        }
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(our_tid, fg_tid, false);
        }

        let _ = SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(prev as usize as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        crate::dlog!(
            "force_foreground: lock_timeout={prev} attached={attached} fg_tid={fg_tid} our_tid={our_tid} won={}",
            GetForegroundWindow() == hwnd
        );
    }
}

unsafe fn hide(hwnd: HWND) {
    unsafe {
        VISIBLE.store(false, Relaxed);
        let mouse = MOUSE_HOOK.swap(0, Relaxed);
        if mouse != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(mouse as *mut core::ffi::c_void));
        }
        let _ = KillTimer(Some(hwnd), CARET_TIMER_ID);
        let _ = ShowWindow(hwnd, SW_HIDE);
        if let Some(app) = app_mut(hwnd) {
            // Fresh query next time the launcher opens.
            app.clear_query();
            // Drop the whole renderer (factories included) so the process
            // returns to baseline memory while hidden.
            app.renderer = None;
        }
    }
}
