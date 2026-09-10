//! Search plugin: fuzzy application search over the shell AppsFolder.
//!
//! AppsFolder is the same list the Start menu shows: classic desktop apps
//! (Start Menu shortcuts) and packaged/UWP apps (Notepad, Settings, Store
//! apps). Indexing runs on a background thread so startup stays instant;
//! results are swapped in on the first query that finds them ready.

use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::fuzzy;
use crate::plugin::{Plugin, ResultItem};

pub const ID: &str = "search";

struct AppEntry {
    /// Display name as shown in the Start menu.
    name: String,
    /// Case-folded chars aligned with `bonus`.
    name_folded: Vec<char>,
    /// Precomputed word-start bonuses.
    bonus: Vec<i32>,
    /// AppsFolder parsing name; launched as `shell:AppsFolder\<id>`.
    app_id: String,
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
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
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
                    name,
                });
            }
        }
        Ok(())
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
