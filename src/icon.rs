//! Turning Windows icon handles into the plain BGRA buffers the renderer
//! wants.
//!
//! Two sources, one output format: shell items (real app icons) and the
//! shell's stock icon set (generic fallbacks). Both end up as premultiplied
//! BGRA in a [`crate::plugin::Icon`], which is device-independent and so
//! survives render-target recreation.

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, ReleaseDC,
};
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SHGSI_ICON, SHGSI_LARGEICON,
    SHGetStockIconInfo, SHSTOCKICONID, SHSTOCKICONINFO, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, HICON, ICONINFO, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW,
};
use windows::core::{Interface, PCWSTR};

use crate::plugin::Icon;

/// Extraction size in physical pixels. Icons draw at 26 DIPs, so 48px stays
/// crisp up to roughly 185% scaling.
pub const ICON_PX: i32 = 48;

/// The shell's icon for a shell item. Works for desktop and packaged apps
/// alike.
pub fn from_shell_item(item: &IShellItem) -> Option<Icon> {
    unsafe {
        let factory: IShellItemImageFactory = item.cast().ok()?;
        let hbmp = factory
            .GetImage(
                SIZE {
                    cx: ICON_PX,
                    cy: ICON_PX,
                },
                SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;
        let icon = from_hbitmap(hbmp);
        let _ = DeleteObject(hbmp.into());
        icon
    }
}

/// The shell's icon for a file on disk, used by indexed source folders.
pub fn from_path(path: &str) -> Option<Icon> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        from_shell_item(&item)
    }
}

/// One of the shell's built-in icons, e.g. the generic application or globe.
pub fn stock(id: SHSTOCKICONID) -> Option<Icon> {
    unsafe {
        let mut info = SHSTOCKICONINFO {
            cbSize: size_of::<SHSTOCKICONINFO>() as u32,
            ..Default::default()
        };
        SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_LARGEICON, &mut info).ok()?;
        let icon = from_hicon(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        icon
    }
}

/// Apex's own icon, embedded as resource 1 by `build.rs`. Used to mark rows
/// that are apex commands rather than something on the system.
pub fn app() -> Option<Icon> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        // MAKEINTRESOURCE(1): the integer *is* the "pointer".
        let handle = LoadImageW(
            Some(instance.into()),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            ICON_PX,
            ICON_PX,
            LR_DEFAULTCOLOR,
        )
        .ok()?;
        let hicon = HICON(handle.0);
        let icon = from_hicon(hicon);
        let _ = DestroyIcon(hicon);
        icon
    }
}

unsafe fn from_hicon(hicon: HICON) -> Option<Icon> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;
        let icon = from_hbitmap(info.hbmColor).map(premultiply);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        icon
    }
}

unsafe fn from_hbitmap(hbmp: HBITMAP) -> Option<Icon> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            hbmp.into(),
            size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 {
            return None;
        }

        let mut info = BITMAPINFO {
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
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            hbmp,
            0,
            h as u32,
            Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        if lines == 0 {
            return None;
        }
        Some(Icon {
            width: w as u32,
            height: h as u32,
            bgra,
        })
    }
}

/// Icon colour bitmaps carry *straight* alpha, but Direct2D - and the rest of
/// this pipeline - expects it premultiplied.
///
/// Only needed for `HICON` sources; `IShellItemImageFactory` already returns
/// premultiplied pixels.
fn premultiply(mut icon: Icon) -> Icon {
    let (pixels, _) = icon.bgra.as_chunks_mut::<4>();
    // Legacy icons have no alpha channel at all, leaving it uniformly zero.
    // Taking that literally would render nothing, so read it as opaque.
    if pixels.iter().all(|p| p[3] == 0) {
        for p in pixels.iter_mut() {
            p[3] = 255;
        }
        return icon;
    }
    for p in pixels.iter_mut() {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a) / 255) as u8;
        }
    }
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon_of(bgra: Vec<u8>) -> Icon {
        Icon {
            width: (bgra.len() / 4) as u32,
            height: 1,
            bgra,
        }
    }

    #[test]
    fn premultiply_scales_colour_by_alpha() {
        // Half-transparent white -> half-intensity white.
        let out = premultiply(icon_of(vec![255, 255, 255, 128]));
        assert_eq!(out.bgra, vec![128, 128, 128, 128]);
    }

    #[test]
    fn fully_transparent_alpha_is_read_as_opaque() {
        // A legacy icon with no alpha channel must not vanish.
        let out = premultiply(icon_of(vec![10, 20, 30, 0, 40, 50, 60, 0]));
        assert_eq!(out.bgra, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn transparent_pixels_survive_alongside_opaque_ones() {
        let out = premultiply(icon_of(vec![255, 255, 255, 0, 255, 255, 255, 255]));
        assert_eq!(out.bgra, vec![0, 0, 0, 0, 255, 255, 255, 255]);
    }
}
