//! Window creation, global hotkey, input routing, and the message loop.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::app::App;
use crate::render;

const HOTKEY_ID: i32 = 1;
const CARET_TIMER_ID: usize = 1;
const CARET_BLINK_MS: u32 = 530;

pub fn run() -> Result<()> {
    unsafe {
        // Single instance: bail silently if apex is already running.
        CreateMutexW(None, true, w!("Local\\apex-launcher-mutex"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        // Shell launches (shell:AppsFolder) want COM on the calling thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class_name = w!("ApexWindow");

        // Embedded app icon (id 1, from build.rs / assets/apex.ico).
        let icon = LoadImageW(
            Some(instance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();

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
            w!("apex"),
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
        let app = Box::new(App::new(crate::plugins()));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        RegisterHotKey(
            Some(hwnd),
            HOTKEY_ID,
            MOD_ALT | MOD_NOREPEAT,
            VK_SPACE.0 as u32,
        )?;
        crate::dlog!("hotkey registered, entering message loop");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
        Ok(())
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
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
                toggle(hwnd);
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
                        let (query, caret, selected) =
                            (&app.query, app.caret_visible, app.selected);
                        if let Some(r) = app.renderer.as_mut() {
                            r.draw(hwnd, query, caret, &app.results, selected);
                        }
                    }
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Dismiss when the window loses focus (click elsewhere), Raycast-style.
            WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                hide(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
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

unsafe fn on_keydown(hwnd: HWND, key: VIRTUAL_KEY) {
    unsafe {
        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        match key {
            VK_ESCAPE => hide(hwnd),
            // Ctrl+Q quits entirely (dev convenience until a tray icon exists).
            VK_Q if ctrl => {
                let _ = DestroyWindow(hwnd);
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
                let close = app_mut(hwnd).map(|a| a.activate_selected()).unwrap_or(false);
                if close {
                    hide(hwnd);
                }
            }
            _ => {}
        }
    }
}

unsafe fn on_char(hwnd: HWND, unit: u16) {
    unsafe {
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

        let content_h = app_mut(hwnd)
            .map(|a| a.content_height())
            .unwrap_or(render::INPUT_H);
        let w = (render::WINDOW_WIDTH * scale) as i32;
        let h = (content_h * scale) as i32;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top) / 5;

        crate::dlog!("show: x={x} y={y} w={w} h={h} scale={scale}");
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

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

unsafe fn hide(hwnd: HWND) {
    unsafe {
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
