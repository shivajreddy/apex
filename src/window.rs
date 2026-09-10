//! Window creation, global hotkey, and the message loop.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const HOTKEY_ID: i32 = 1;

/// Logical (96-dpi) window size. Scaled per-monitor at show time.
const WINDOW_WIDTH: i32 = 720;
const WINDOW_HEIGHT: i32 = 480;

pub fn run() -> Result<()> {
    unsafe {
        // Single instance: bail silently if apex is already running.
        CreateMutexW(None, true, w!("Local\\apex-launcher-mutex"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class_name = w!("ApexWindow");

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
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

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
                crate::dlog!("WM_HOTKEY received");
                toggle(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                on_keydown(hwnd, VIRTUAL_KEY(wparam.0 as u16));
                LRESULT(0)
            }
            // Dismiss when the window loses focus (click elsewhere), Raycast-style.
            WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                hide(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                // Placeholder dark fill until Direct2D rendering lands.
                let brush = CreateSolidBrush(COLORREF(0x001E1E1E));
                FillRect(hdc, &ps.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_DESTROY => {
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
            _ => {}
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

/// Show centered (upper third) on the monitor containing the cursor,
/// scaled to that monitor's DPI.
unsafe fn show(hwnd: HWND) {
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
        let scale = dpi_x as f32 / 96.0;

        let w = (WINDOW_WIDTH as f32 * scale) as i32;
        let h = (WINDOW_HEIGHT as f32 * scale) as i32;
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 3;

        crate::dlog!("show: x={x} y={y} w={w} h={h} scale={scale}");
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

unsafe fn hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}
