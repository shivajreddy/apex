//! Search plugin: fuzzy application search over the shell AppsFolder.
//!
//! AppsFolder is the same list the Start menu shows: classic desktop apps
//! (Start Menu shortcuts) and packaged/UWP apps (Notepad, Settings, Store
//! apps). Indexing runs on a background thread so startup stays instant;
//! results are swapped in on the first query that finds them ready.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW,
    HBITMAP, ReleaseDC,
};
use windows::Win32::System::Com::{
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellItemImageFactory,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY,
    SIGDN_PARENTRELATIVEPARSING, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{Interface, PCWSTR, w};

use crate::fuzzy;
use crate::plugin::{Icon, Plugin, ResultItem};

pub const ID: &str = "search";

/// Icon extraction size in physical pixels (drawn at 26 DIPs, so 48px stays
/// crisp up to ~185% scaling).
const ICON_PX: i32 = 48;

struct AppEntry {
    /// Display name as shown in the Start menu.
    name: String,
    /// Case-folded chars aligned with `bonus`.
    name_folded: Vec<char>,
    /// Precomputed word-start bonuses.
    bonus: Vec<i32>,
    /// AppsFolder parsing name; launched as `shell:AppsFolder\<id>`.
    app_id: String,
    icon: Option<Arc<Icon>>,
}

pub struct Search {
    entries: Vec<AppEntry>,
    pending: Option<Receiver<Vec<AppEntry>>>,
}

impl Search {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let entries = index_apps();
            crate::dlog!(
                "search: indexed {} apps in {:.1?}",
                entries.len(),
                started.elapsed()
            );
            let _ = tx.send(entries);
        });
        Self {
            entries: Vec::new(),
            pending: Some(rx),
        }
    }

    /// Swap in the index once the background scan finishes.
    fn poll_index(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(entries) => {
                self.entries = entries;
                self.pending = None;
            }
            Err(TryRecvError::Disconnected) => self.pending = None,
            Err(TryRecvError::Empty) => {}
        }
    }
}

impl Plugin for Search {
    fn id(&self) -> &'static str {
        ID
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        self.poll_index();
        let query: Vec<char> = fuzzy::fold_case(q);
        for e in &self.entries {
            if let Some(score) = fuzzy::score(&query, &e.name_folded, &e.bonus) {
                out.push(ResultItem {
                    plugin: ID,
                    title: e.name.clone(),
                    subtitle: "Application".into(),
                    payload: e.app_id.clone(),
                    score,
                    icon: e.icon.clone(),
                });
            }
        }
    }

    fn activate(&mut self, item: &ResultItem) -> bool {
        launch(&item.payload)
    }
}

fn index_apps() -> Vec<AppEntry> {
    unsafe {
        // MTA: this worker never pumps messages, and STA COM without a pump
        // can deadlock inside shell calls (icon extraction did exactly that).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
        let mut out = Vec::new();
        if let Err(e) = enum_apps_folder(&mut out) {
            crate::dlog!("search: AppsFolder enumeration failed: {e}");
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name && a.app_id == b.app_id);
        out
    }
}

unsafe fn enum_apps_folder(out: &mut Vec<AppEntry>) -> windows::core::Result<()> {
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        loop {
            let mut batch: [Option<IShellItem>; 16] = Default::default();
            let mut fetched = 0u32;
            let _ = items.Next(&mut batch, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            for item in batch.iter().take(fetched as usize).flatten() {
                let Ok(name) = display_name(item, SIGDN_NORMALDISPLAY) else {
                    continue;
                };
                let Ok(app_id) = display_name(item, SIGDN_PARENTRELATIVEPARSING) else {
                    continue;
                };
                if name.is_empty() || app_id.is_empty() {
                    continue;
                }
                out.push(AppEntry {
                    name_folded: fuzzy::fold_case(&name),
                    bonus: fuzzy::bonuses(&name),
                    app_id,
                    icon: extract_icon(item).map(Arc::new),
                    name,
                });
            }
        }
        Ok(())
    }
}

/// Shell-provided icon for an AppsFolder item (works for both desktop and
/// packaged apps). Returns premultiplied BGRA pixels.
fn extract_icon(item: &IShellItem) -> Option<Icon> {
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
        let icon = hbitmap_to_icon(hbmp);
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(hbmp.into());
        icon
    }
}

unsafe fn hbitmap_to_icon(hbmp: HBITMAP) -> Option<Icon> {
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

unsafe fn display_name(item: &IShellItem, kind: SIGDN) -> windows::core::Result<String> {
    unsafe {
        let pw = item.GetDisplayName(kind)?;
        let s = pw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pw.0 as *const _));
        Ok(s)
    }
}

/// Launch an AppsFolder entry via the shell. Returns true on success.
fn launch(app_id: &str) -> bool {
    let target = format!("shell:AppsFolder\\{app_id}");
    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let inst = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        let ok = inst.0 as isize > 32;
        if !ok {
            crate::dlog!("search: failed to launch {target}");
        }
        ok
    }
}
