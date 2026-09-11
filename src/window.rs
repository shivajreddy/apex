//! Window creation, global hotkey, input routing, and the message loop.
//!
//! The global hotkey uses a low-level keyboard hook (WH_KEYBOARD_LL) rather
//! than RegisterHotKey: the hook sees the combo before the shell does, which
//! lets Apex claim system-reserved combos like Ctrl+Esc (Start menu) exactly
//! the way Raycast does - and it can never fail with "hotkey already
//! registered".
//!
//! The hook runs on its own thread whose only job is to host it. A
//! `WH_KEYBOARD_LL` proc must return within `LowLevelHooksTimeout` (capped
//! at 1s), or Windows silently drops that event and passes the chord
//! through to the shell - so a hook sharing the UI thread misses summons
//! whenever the UI thread is mid-render, mid-launch, or broadcasting a
//! setting change. A dedicated thread never stalls, so the chord is never
//! missed. It only posts to the window; all the work stays on the UI thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering::Relaxed};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::app::{App, Mode, UiOutcome};
use crate::editor::{Edit, Motion};
use crate::plugin::ShellCommand;
use crate::render;

const CARET_TIMER_ID: usize = 1;
const CARET_BLINK_MS: u32 = 530;
/// Drives the summon animation; fires as fast as USER timers go (~10ms).
const ANIM_TIMER_ID: usize = 2;
/// Length of the summon animation. Short enough that typing straight
/// after the hotkey never feels held up - input is live from frame one.
const SUMMON_MS: u128 = 110;
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
/// Window is currently shown (intent, set the instant `show` begins its
/// work - not a promise that focus has landed yet).
static VISIBLE: AtomicBool = AtomicBool::new(false);
/// Held for the duration of `show`/`hide` so a `WM_ACTIVATE` delivered
/// synchronously from inside a foreground call can't re-enter and hide the
/// window mid-show (or re-hide mid-hide).
static BUSY: AtomicBool = AtomicBool::new(false);
/// `GetTickCount64` value before which automatic dismissals are ignored.
/// Set just after a summon: focus is grabbed a few ms into `show`, and until
/// it lands a keystroke or activation bounce would otherwise dismiss the
/// window the user just opened (and wipe what they typed). The explicit
/// toggle chord is exempt - closing on purpose should be instant.
static GUARD_UNTIL: AtomicU64 = AtomicU64::new(0);
/// How long that guard lasts. Comfortably longer than the ~50ms it takes to
/// show and win foreground, short enough to never swallow a real dismissal.
const GUARD_MS: u64 = 250;
/// Mouse hook handle; installed only while the window is visible.
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
/// Single-instance mutex, kept so Restart can release it before relaunching.
static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);
/// Thread id of the keyboard-hook thread, so shutdown can post it WM_QUIT.
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// Embedded app icon, reused for the tray.
static APP_ICON: AtomicIsize = AtomicIsize::new(0);
/// Whether the tray icon is currently registered.
static TRAY_SHOWN: AtomicBool = AtomicBool::new(false);
/// Whether the acrylic backdrop is in use, so `show` can re-assert it: the
/// DWM system backdrop is not reliably kept across a hide/re-show and has to
/// be re-applied each time the window comes back.
static ACRYLIC: AtomicBool = AtomicBool::new(false);
/// Last WM_MOUSEMOVE lparam, so a stationary pointer is not treated as hover.
static LAST_MOUSE: AtomicIsize = AtomicIsize::new(-1);

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
        // WS_EX_LAYERED: the window is presented as a per-pixel-alpha bitmap
        // (see `render`), which is what lets the compositor blur behind it.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
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
        // Acrylic is rendered by apex itself now (a blurred snapshot baked
        // into the window bitmap on each summon - see `capture_behind` and
        // `render`), not by the compositor, so there is no DWM backdrop to
        // set here. This flag just says whether to capture that snapshot.
        let translucent = config.acrylic();
        ACRYLIC.store(translucent, Relaxed);

        // Attach application state to the window.
        let mut app = Box::new(App::new(crate::plugins(config), config));
        app.translucent = translucent;
        app.dark = app.forced_dark.unwrap_or_else(|| !system_light_theme());
        let dark = BOOL(app.dark as i32);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const core::ffi::c_void,
            size_of_val(&dark) as u32,
        );
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        if config.general_flag("tray_icon", true) {
            add_tray(hwnd);
        }

        // Global hotkey via low-level keyboard hook, on its own thread so it
        // can never miss the chord (see module docs).
        HOOK_HWND.store(hwnd.0 as isize, Relaxed);
        HOOK_MODS.store(config.hotkey_mods, Relaxed);
        HOOK_VK.store(config.hotkey_vk, Relaxed);
        let hook_thread = spawn_hook_thread();

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
        // Wake the hook thread out of GetMessage so it unhooks and exits.
        let hook_tid = HOOK_THREAD.swap(0, Relaxed);
        if hook_tid != 0 {
            let _ = PostThreadMessageW(hook_tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        let _ = hook_thread.join();
        let mouse = MOUSE_HOOK.swap(0, Relaxed);
        if mouse != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(mouse as *mut core::ffi::c_void));
        }
        Ok(())
    }
}

/// Host the low-level keyboard hook on a thread that does nothing else, so
/// its proc always returns well within `LowLevelHooksTimeout` and Windows
/// never drops the chord. The proc only reads the `HOOK_*` statics and posts
/// to the window, so no state has to cross the thread boundary here.
fn spawn_hook_thread() -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| unsafe {
        HOOK_THREAD.store(GetCurrentThreadId(), Relaxed);
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
            Ok(h) => h,
            Err(e) => {
                crate::dlog!("keyboard hook install failed: {e}");
                return;
            }
        };
        crate::dlog!("keyboard hook installed on its own thread");
        // No windows live on this thread, so nothing needs dispatching; the
        // loop exists only to keep the queue pumped and to catch the WM_QUIT
        // posted at shutdown.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    })
}

/// Grab a blurred snapshot of the desktop behind where the window is about
/// to appear, to bake in as the acrylic background. Called while the window
/// is positioned but still hidden, so the screen there shows what is behind
/// it. `x, y, w, h` are physical pixels.
///
/// This is the whole point of the rewrite: rather than asking DWM for a live
/// backdrop (which lagged, dropped to solid on re-show, and sometimes never
/// appeared), apex renders the frost itself from a captured bitmap. The
/// capture is downscaled hard and box-blurred; the renderer stretches it
/// back up with bilinear filtering, which finishes the blur.
unsafe fn capture_behind(x: i32, y: i32, w: i32, h: i32) -> Option<render::Backdrop> {
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        let backdrop = (|| {
            let dib = dib.ok()?;
            let prev = SelectObject(mem, dib.into());
            let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).is_ok();
            let _ = GdiFlush();
            let result = if ok && !bits.is_null() {
                let raw = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                Some(downscale_blur(raw, w as u32, h as u32))
            } else {
                None
            };
            SelectObject(mem, prev);
            let _ = DeleteObject(dib.into());
            result
        })();
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        backdrop
    }
}

/// Longest side of the downscaled snapshot. Small on purpose: heavy
/// downscaling is most of the blur, and the renderer's bilinear upscale
/// smooths what is left. ~84px erases even sharp edges into a soft frost at
/// any window size while staying a sub-millisecond amount of pixels to blur.
const BLUR_SIZE: u32 = 84;

/// Box-average `raw` (top-down BGRA, `w`x`h`) down to at most [`BLUR_SIZE`] on
/// its longest side, force it opaque, then box-blur it a couple of times.
fn downscale_blur(raw: &[u8], w: u32, h: u32) -> render::Backdrop {
    let scale = (w.max(h) as f32 / BLUR_SIZE as f32).max(1.0);
    let sw = ((w as f32 / scale) as u32).max(1);
    let sh = ((h as f32 / scale) as u32).max(1);
    let mut small = vec![0u8; (sw * sh * 4) as usize];
    for ty in 0..sh {
        // Source block for this row.
        let y0 = (ty * h / sh) as usize;
        let y1 = (((ty + 1) * h / sh).max(y0 as u32 + 1)).min(h) as usize;
        for tx in 0..sw {
            let x0 = (tx * w / sw) as usize;
            let x1 = (((tx + 1) * w / sw).max(x0 as u32 + 1)).min(w) as usize;
            let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                let row = sy * w as usize * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    b += raw[i] as u32;
                    g += raw[i + 1] as u32;
                    r += raw[i + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = ((ty * sw + tx) * 4) as usize;
            small[o] = (b / n) as u8;
            small[o + 1] = (g / n) as u8;
            small[o + 2] = (r / n) as u8;
            small[o + 3] = 255;
        }
    }
    for _ in 0..3 {
        box_blur(&mut small, sw, sh, 2);
    }
    render::Backdrop {
        width: sw,
        height: sh,
        bgra: small,
    }
}

/// One separable box-blur pass over BGRA pixels, radius `r`, alpha left as-is.
fn box_blur(px: &mut [u8], w: u32, h: u32, r: i32) {
    let (w, h) = (w as i32, h as i32);
    let mut tmp = px.to_vec();
    // Horizontal.
    for y in 0..h {
        for x in 0..w {
            let (mut b, mut g, mut rr, mut n) = (0u32, 0u32, 0u32, 0u32);
            for dx in -r..=r {
                let xx = x + dx;
                if xx >= 0 && xx < w {
                    let i = ((y * w + xx) * 4) as usize;
                    b += px[i] as u32;
                    g += px[i + 1] as u32;
                    rr += px[i + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = ((y * w + x) * 4) as usize;
            tmp[o] = (b / n) as u8;
            tmp[o + 1] = (g / n) as u8;
            tmp[o + 2] = (rr / n) as u8;
        }
    }
    // Vertical.
    for y in 0..h {
        for x in 0..w {
            let (mut b, mut g, mut rr, mut n) = (0u32, 0u32, 0u32, 0u32);
            for dy in -r..=r {
                let yy = y + dy;
                if yy >= 0 && yy < h {
                    let i = ((yy * w + x) * 4) as usize;
                    b += tmp[i] as u32;
                    g += tmp[i + 1] as u32;
                    rr += tmp[i + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = ((y * w + x) * 4) as usize;
            px[o] = (b / n) as u8;
            px[o + 1] = (g / n) as u8;
            px[o + 2] = (rr / n) as u8;
        }
    }
}

/// Whether Windows is set to light mode for apps (the Colors page under
/// Settings, Personalization). Read from the registry on every show - a few
/// microseconds - rather than cached, so flipping the setting takes effect
/// at the next summon without a restart. Missing value (older systems)
/// means dark.
fn system_light_theme() -> bool {
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value != 0
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
                    // Hotkey key pressed without its modifiers: not the chord,
                    // and a good moment to clear the held latch in case the
                    // matching key-up was ever missed (which would otherwise
                    // wedge the next real chord into a no-op).
                    HOOK_HELD.store(false, Relaxed);
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

/// Draw the current state and put it on screen.
///
/// Layered windows are not painted through `WM_PAINT`: the whole bitmap is
/// handed over in one call, which also positions and sizes the window, so
/// this is where the height follows the content and where the summon
/// animation's scale and opacity are applied. Called directly wherever the
/// old design invalidated; each call is well under a millisecond.
unsafe fn repaint(hwnd: HWND) {
    unsafe {
        if !VISIBLE.load(Relaxed) {
            return;
        }
        let Some(app) = app_mut(hwnd) else { return };
        if app.ensure_renderer().is_none() {
            return;
        }

        // Animation progress, eased so the motion settles rather than stops.
        let (opacity, scale) = match app.summon {
            Some(started) if app.animate => {
                let t = (started.elapsed().as_millis() as f32 / SUMMON_MS as f32).min(1.0);
                if t >= 1.0 {
                    app.summon = None;
                    let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
                }
                let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                (0.6 + 0.4 * eased, 0.965 + 0.035 * eased)
            }
            _ => (1.0, 1.0),
        };

        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let scale_px = app.dpi / 96.0;
        let place = render::Placement {
            x: rc.left,
            y: rc.top,
            width: (render::WINDOW_WIDTH * scale_px) as u32,
            height: (app.content_height() * scale_px) as u32,
            dpi: app.dpi,
            opacity,
            scale,
        };
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
            sections: &app.sections,
            scroll: app.scroll,
            panel,
            backdrop: app.backdrop.as_ref(),
        };
        if let Some(r) = app.renderer.as_mut() {
            r.present(hwnd, &frame, &place);
        }
    }
}

/// Restart the caret blink with the caret showing, so it never blinks off
/// in the middle of typing.
unsafe fn nudge_caret(hwnd: HWND) {
    unsafe {
        if let Some(app) = app_mut(hwnd) {
            app.caret_visible = true;
        }
        SetTimer(Some(hwnd), CARET_TIMER_ID, CARET_BLINK_MS, None);
    }
}

/// Apply a text edit to the focused field and show the result.
unsafe fn apply_edit(hwnd: HWND, edit: Edit) {
    unsafe {
        if let Some(app) = app_mut(hwnd) {
            app.edit(edit);
        }
        nudge_caret(hwnd);
        repaint(hwnd);
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
                match wparam.0 & 0xFFFF {
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
                dismiss(hwnd);
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
                repaint(hwnd);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == ANIM_TIMER_ID => {
                repaint(hwnd);
                LRESULT(0)
            }
            // The bitmap is pushed with UpdateLayeredWindow, so there is
            // nothing to draw here; validating keeps WM_PAINT from repeating.
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            // Hover to highlight. Guarded on the cursor actually having
            // moved: showing the window under a stationary pointer emits
            // WM_MOUSEMOVE, which would otherwise yank the selection away
            // from the top row before the user has touched anything.
            WM_MOUSEMOVE => {
                let pos = lparam.0 as u32;
                if LAST_MOUSE.swap(pos as isize, Relaxed) != pos as isize
                    && mode_kind(hwnd) == ModeKind::Search
                {
                    let (_, y) = client_dips(hwnd, lparam);
                    if let Some(app) = app_mut(hwnd)
                        && let Some(row) = app.row_at(y)
                        && app.select(row)
                    {
                        repaint(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if mode_kind(hwnd) == ModeKind::Search {
                    let (x, y) = client_dips(hwnd, lparam);
                    if y < render::INPUT_H {
                        // A click in the input places the caret; with Shift
                        // it extends the selection, as in any edit control.
                        let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                        if let Some(app) = app_mut(hwnd)
                            && let Some(pos) = app
                                .renderer
                                .as_ref()
                                .and_then(|r| r.hit_test_input(&app.query, x))
                        {
                            app.place_caret(pos, shift);
                            nudge_caret(hwnd);
                            repaint(hwnd);
                        }
                        return LRESULT(0);
                    }
                    let hit = app_mut(hwnd).and_then(|a| {
                        let row = a.row_at(y)?;
                        a.select(row);
                        Some(a.activate_selected())
                    });
                    if let Some(outcome) = hit {
                        settle(hwnd, outcome);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
                // Three rows per notch, the usual Windows convention.
                let dips = delta / WHEEL_DELTA as f32 * render::ROW_H * 3.0;
                if let Some(app) = app_mut(hwnd)
                    && app.scroll_by(dips)
                {
                    repaint(hwnd);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Dismiss when the window loses activation (click elsewhere,
            // Alt+Tab), Raycast-style - through the same guarded funnel as
            // the hook-driven dismissals, so an activation bounce during the
            // summon itself doesn't hide the window before it settles.
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    dismiss(hwnd);
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

/// Client-space mouse position from a message's lparam, in logical DIPs -
/// what all the layout maths uses.
unsafe fn client_dips(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    unsafe {
        let x = (lparam.0 & 0xFFFF) as i16 as f32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
        let scale = 96.0 / GetDpiForWindow(hwnd) as f32;
        (x * scale, y * scale)
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
            UiOutcome::Stay => repaint(hwnd),
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
            // Answered by App, which owns both the history and the
            // hidden set.
            ShellCommand::ClearHistory
            | ShellCommand::ShowHidden
            | ShellCommand::ShowSources => repaint(hwnd),
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

/// Put `text` on the clipboard, for Ctrl+C / Ctrl+X.
unsafe fn set_clipboard_text(hwnd: HWND, text: &str) {
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        if EmptyClipboard().is_ok()
            && let Ok(hglobal) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2)
        {
            let ptr = GlobalLock(hglobal) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
                let _ = GlobalUnlock(hglobal);
                // On success the system owns the memory; on failure it is
                // still ours to free.
                if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))).is_err() {
                    let _ = GlobalFree(Some(hglobal));
                }
            } else {
                let _ = GlobalFree(Some(hglobal));
            }
        }
        let _ = CloseClipboard();
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

        // Clipboard and caret keys, in whichever text field has focus.
        // Handled here rather than in on_char because the Ctrl chords arrive
        // as WM_CHAR control codes, which the char handler correctly ignores.
        if (key == VK_V && ctrl) || (key == VK_INSERT && shift) {
            if let Some(text) = clipboard_text(hwnd) {
                apply_edit(hwnd, Edit::Text(&text));
            }
            return;
        }
        if (key == VK_C && ctrl) || (key == VK_INSERT && ctrl) {
            if let Some(text) = app_mut(hwnd).and_then(|a| a.selected_text()) {
                set_clipboard_text(hwnd, &text);
            }
            return;
        }
        if (key == VK_X && ctrl) || (key == VK_DELETE && shift) {
            if let Some(text) = app_mut(hwnd).and_then(|a| a.cut()) {
                set_clipboard_text(hwnd, &text);
                nudge_caret(hwnd);
                repaint(hwnd);
            }
            return;
        }
        let motion = match key {
            VK_LEFT => Some(Motion::Left),
            VK_RIGHT => Some(Motion::Right),
            VK_HOME => Some(Motion::Home),
            VK_END => Some(Motion::End),
            _ => None,
        };
        if let Some(motion) = motion {
            apply_edit(
                hwnd,
                Edit::Move {
                    motion,
                    word: ctrl,
                    select: shift,
                },
            );
            return;
        }
        if key == VK_DELETE {
            apply_edit(hwnd, Edit::Delete { word: ctrl });
            return;
        }
        if key == VK_A && ctrl {
            apply_edit(hwnd, Edit::SelectAll);
            return;
        }

        match mode_kind(hwnd) {
            ModeKind::Search => match key {
                VK_ESCAPE => hide(hwnd),
                VK_K if ctrl => {
                    if app_mut(hwnd).map(|a| a.open_actions()).unwrap_or(false) {
                        repaint(hwnd);
                    }
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.move_selection(1);
                    }
                    repaint(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.move_selection(-1);
                    }
                    repaint(hwnd);
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
                VK_ESCAPE | VK_K if key == VK_ESCAPE || ctrl => {
                    if let Some(app) = app_mut(hwnd) {
                        app.close_panel();
                    }
                    repaint(hwnd);
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.panel_move(1);
                    }
                    repaint(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.panel_move(-1);
                    }
                    repaint(hwnd);
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
                    repaint(hwnd);
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
                    repaint(hwnd);
                }
                // Tab and the arrows both move between fields; Shift+Tab and
                // Up go back.
                VK_TAB => {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(if shift { -1 } else { 1 });
                    }
                    nudge_caret(hwnd);
                    repaint(hwnd);
                }
                VK_DOWN => {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(1);
                    }
                    nudge_caret(hwnd);
                    repaint(hwnd);
                }
                VK_UP => {
                    if let Some(app) = app_mut(hwnd) {
                        app.form_move(-1);
                    }
                    nudge_caret(hwnd);
                    repaint(hwnd);
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

/// Typed text goes to whichever field has focus; the actions panel has none,
/// so there it is dropped.
///
/// Only Backspace (0x08), Ctrl+Backspace (0x7F) and printable units are
/// edits. Every other control code - Tab, Enter, Escape, the Ctrl chords -
/// is a command, already handled in `on_keydown`.
unsafe fn on_char(hwnd: HWND, unit: u16) {
    let edit = match unit {
        0x08 => Edit::Backspace { word: false },
        0x7F => Edit::Backspace { word: true },
        u if u >= 0x20 => Edit::Char(u),
        _ => return,
    };
    unsafe { apply_edit(hwnd, edit) }
}

unsafe fn toggle(hwnd: HWND) {
    unsafe {
        if !VISIBLE.load(Relaxed) {
            show(hwnd);
        } else if GetForegroundWindow() == hwnd {
            // Up and focused: the chord dismisses it.
            hide(hwnd);
        } else {
            // Up but not frontmost - a previous summon lost the race for
            // foreground, or an elevated window held it. Bring it forward
            // and focus it rather than toggling off, so the chord always
            // leaves apex ready to type into instead of needing a re-press.
            // Same interlock as show(): arm the guard before grabbing
            // foreground and hold BUSY across it, so a keystroke right after
            // re-focus (or a synchronous activation bounce) can't dismiss.
            BUSY.store(true, Relaxed);
            GUARD_UNTIL.store(GetTickCount64() + GUARD_MS, Relaxed);
            force_foreground(hwnd);
            let _ = SetFocus(Some(hwnd));
            BUSY.store(false, Relaxed);
            repaint(hwnd);
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
        // Held across the whole summon so a synchronous WM_ACTIVATE from
        // inside force_foreground cannot re-enter and hide the window.
        BUSY.store(true, Relaxed);
        let (scale, work) = cursor_monitor_metrics();

        // Rebuild the most-used list before measuring: the app index may have
        // finished loading, and the last launch may have reordered it.
        let content_h = match app_mut(hwnd) {
            Some(a) => {
                a.dpi = 96.0 * scale;
                a.caret_visible = true;
                // The renderer was dropped on hide, so a theme change is
                // picked up simply by choosing the palette again here.
                a.dark = a.forced_dark.unwrap_or_else(|| !system_light_theme());
                a.summon = a.animate.then(std::time::Instant::now);
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
        // Place while still hidden, paint the first frame into the layered
        // surface, and only then show: the window never appears blank.
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        // With the window positioned but not yet visible, the screen where it
        // will land still shows what's behind it: capture and blur that now,
        // so the very first painted frame already carries the frost.
        if ACRYLIC.load(Relaxed) {
            let bd = capture_behind(x, y, w, h);
            if let Some(app) = app_mut(hwnd) {
                app.backdrop = bd;
            }
        }
        VISIBLE.store(true, Relaxed);
        repaint(hwnd);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        force_foreground(hwnd);
        let _ = SetFocus(Some(hwnd));

        // Outside-click dismissal, active only while shown.
        if MOUSE_HOOK.load(Relaxed) == 0 {
            if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
                MOUSE_HOOK.store(h.0 as isize, Relaxed);
            }
        }

        SetTimer(Some(hwnd), CARET_TIMER_ID, CARET_BLINK_MS, None);
        if app_mut(hwnd).is_some_and(|a| a.summon.is_some()) {
            SetTimer(Some(hwnd), ANIM_TIMER_ID, USER_TIMER_MINIMUM, None);
        }

        // Open the guard window and release the interlock only now that the
        // window is up, focused, and painted: any activation bounce or early
        // keystroke from the last few ms is ignored rather than dismissing.
        GUARD_UNTIL.store(GetTickCount64() + GUARD_MS, Relaxed);
        BUSY.store(false, Relaxed);
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
unsafe fn force_foreground(hwnd: HWND) -> bool {
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            let _ = SetFocus(Some(hwnd));
            return true;
        }

        // Remember and clear the foreground lock timeout - but only if we can
        // actually read it back, so a failed read never leaves us "restoring"
        // it to 0 and pinning the lock open system-wide.
        let mut prev: u32 = 0;
        let has_prev = SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut prev as *mut u32 as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        if has_prev {
            let _ = SystemParametersInfoW(
                SPI_SETFOREGROUNDLOCKTIMEOUT,
                0,
                Some(std::ptr::null_mut()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }

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
        // With the input queues attached, all four take effect against the
        // shared foreground state, so keyboard focus lands on us in one go.
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        if attached {
            let _ = AttachThreadInput(our_tid, fg_tid, false);
        }

        if has_prev {
            let _ = SystemParametersInfoW(
                SPI_SETFOREGROUNDLOCKTIMEOUT,
                0,
                Some(prev as usize as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        let won = GetForegroundWindow() == hwnd;
        crate::dlog!(
            "force_foreground: has_prev={has_prev} prev={prev} attached={attached} fg_tid={fg_tid} our_tid={our_tid} won={won}"
        );
        won
    }
}

/// The single gate every *automatic* dismissal passes through - focus loss,
/// a click outside, a keystroke that landed elsewhere. Refused while hidden,
/// while a show/hide is mid-flight (re-entrancy), or inside the post-summon
/// guard window. The explicit toggle chord does not come here: closing on
/// purpose is never guarded.
unsafe fn dismiss(hwnd: HWND) {
    unsafe {
        if !VISIBLE.load(Relaxed) || BUSY.load(Relaxed) {
            return;
        }
        if GetTickCount64() < GUARD_UNTIL.load(Relaxed) {
            crate::dlog!("dismiss ignored: within post-summon guard window");
            return;
        }
        hide(hwnd);
    }
}

unsafe fn hide(hwnd: HWND) {
    unsafe {
        BUSY.store(true, Relaxed);
        VISIBLE.store(false, Relaxed);
        let mouse = MOUSE_HOOK.swap(0, Relaxed);
        if mouse != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(mouse as *mut core::ffi::c_void));
        }
        let _ = KillTimer(Some(hwnd), CARET_TIMER_ID);
        let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
        let _ = ShowWindow(hwnd, SW_HIDE);
        if let Some(app) = app_mut(hwnd) {
            app.summon = None;
            // Fresh query next time the launcher opens.
            app.clear_query();
            // Drop the whole renderer (factories included) so the process
            // returns to baseline memory while hidden.
            app.renderer = None;
            // Discard the captured backdrop; the next summon grabs a fresh one.
            app.backdrop = None;
        }
        BUSY.store(false, Relaxed);
    }
}
