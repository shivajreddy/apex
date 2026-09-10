//! Search plugin: fuzzy application search over the shell AppsFolder.
//!
//! AppsFolder is the same list the Start menu shows: classic desktop apps
//! (Start Menu shortcuts) and packaged/UWP apps (Notepad, Settings, Store
//! apps). Indexing runs on a background thread so startup stays instant;
//! results are swapped in on the first query that finds them ready.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::System::Com::{
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, SIID_APPLICATION,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use std::collections::HashMap;

use crate::icon;
use crate::plugin::{Action, ActionResult, Icon, Plugin, ResultItem};
use crate::fuzzy;

pub const ID: &str = "search";

/// Score boost for an exact alias hit; large enough to always rank first.
const ALIAS_BOOST: i32 = 2000;

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
    /// alias (case-folded) -> app id. Loaded from `[aliases]`, updated by
    /// the Set/Remove Alias actions (which also write the config file).
    aliases: HashMap<String, String>,
}

/// Scan the AppsFolder on a background thread so startup - and reloads -
/// never block the UI.
fn spawn_index() -> Receiver<Vec<AppEntry>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let entries = index_apps();
        crate::dlog!(
            "search: indexed {} apps in {:.1?}",
            entries.len(),
            started.elapsed()
        );
        // Ignored if the receiver is gone: a second reload supersedes this one.
        let _ = tx.send(entries);
    });
    rx
}

impl Search {
    pub fn new(aliases: HashMap<String, String>) -> Self {
        Self {
            entries: Vec::new(),
            pending: Some(spawn_index()),
            aliases,
        }
    }

    fn alias_of(&self, app_id: &str) -> Option<&str> {
        self.aliases
            .iter()
            .find(|(_, id)| id.as_str() == app_id)
            .map(|(a, _)| a.as_str())
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

    fn refresh(&mut self) {
        // Aliases come back from disk too, so editing the config by hand and
        // reloading is enough - no restart.
        self.aliases = crate::config::Config::load().aliases_map();
        // The current index stays in place until the rescan lands, so the
        // list never blinks empty.
        self.pending = Some(spawn_index());
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        self.poll_index();
        let query: Vec<char> = fuzzy::fold_case(q);
        let query_str: String = query.iter().collect();
        for e in &self.entries {
            let alias = self.alias_of(&e.app_id);
            let alias_hit = alias == Some(query_str.as_str());
            let fuzzy_score = fuzzy::score(&query, &e.name_folded, &e.bonus);
            if fuzzy_score.is_none() && !alias_hit {
                continue;
            }
            let score = fuzzy_score.unwrap_or(0) + if alias_hit { ALIAS_BOOST } else { 0 };
            out.push(make_item(e, alias, score));
        }
    }

    fn item_for(&mut self, payload: &str) -> Option<ResultItem> {
        self.poll_index();
        let e = self.entries.iter().find(|e| e.app_id == payload)?;
        // Score is irrelevant here: the shell orders these by frecency.
        Some(make_item(e, self.alias_of(&e.app_id), 0))
    }

    fn browse(&mut self, limit: usize, out: &mut Vec<ResultItem>) {
        self.poll_index();
        // `entries` is sorted by name at index time, so this pads the list
        // alphabetically - stable and predictable, and it disappears as
        // launch history fills the rows above it.
        let mut added = 0;
        for e in &self.entries {
            if added == limit {
                break;
            }
            if out
                .iter()
                .any(|i| i.plugin == ID && i.payload == e.app_id)
            {
                continue;
            }
            out.push(make_item(e, self.alias_of(&e.app_id), 0));
            added += 1;
        }
    }

    fn activate(&mut self, item: &ResultItem) -> ActionResult {
        if launch(&item.payload) {
            ActionResult::Close
        } else {
            // Leave the window up so the failure is visible rather than
            // looking like a successful launch.
            ActionResult::Done
        }
    }

    fn actions(&self, item: &ResultItem) -> Vec<Action> {
        let mut actions = vec![Action {
            id: "open",
            label: "Open".to_string(),
        }];
        actions.push(Action {
            id: "reveal",
            label: "Open in Explorer".to_string(),
        });
        if let Some(alias) = self.alias_of(&item.payload) {
            actions.push(Action {
                id: "remove_alias",
                label: format!("Remove Alias \u{201c}{alias}\u{201d}"),
            });
        }
        actions.push(Action {
            id: "set_alias",
            label: "Set Alias\u{2026}".to_string(),
        });
        actions
    }

    fn run_action(&mut self, action_id: &str, item: &ResultItem) -> ActionResult {
        match action_id {
            "open" => {
                launch(&item.payload);
                ActionResult::Close
            }
            "reveal" => {
                reveal(&item.payload);
                // Dismiss, not Close: looking at where an app lives is not
                // launching it, and shouldn't inflate its ranking.
                ActionResult::Dismiss
            }
            "set_alias" => ActionResult::RequestText {
                prompt: format!("Alias for {}", item.title),
                action_id: "set_alias",
            },
            "remove_alias" => {
                self.aliases.retain(|_, id| id != &item.payload);
                crate::config::remove_alias_file(&item.payload);
                ActionResult::Done
            }
            _ => ActionResult::Done,
        }
    }

    fn submit_text(&mut self, action_id: &str, item: &ResultItem, text: &str) -> ActionResult {
        if action_id == "set_alias" {
            let alias = crate::config::sanitize_key(text);
            if !alias.is_empty() {
                self.aliases.retain(|_, id| id != &item.payload);
                self.aliases.insert(alias.clone(), item.payload.clone());
                crate::config::upsert_alias_file(&alias, &item.payload);
            }
        }
        ActionResult::Done
    }
}

/// Build a result row for an indexed app. Takes the alias rather than
/// looking it up so callers that already resolved it don't pay twice.
fn make_item(e: &AppEntry, alias: Option<&str>, score: i32) -> ResultItem {
    ResultItem {
        plugin: ID,
        title: e.name.clone(),
        subtitle: match alias {
            Some(a) => format!("Application \u{b7} {a}"),
            None => "Application".to_string(),
        },
        payload: e.app_id.clone(),
        score,
        icon: e.icon.clone(),
    }
}

fn index_apps() -> Vec<AppEntry> {
    unsafe {
        // MTA: this worker never pumps messages, and STA COM without a pump
        // can deadlock inside shell calls (icon extraction did exactly that).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
        // Extracted once and shared by every entry that has no icon of its
        // own, so the fallback costs one bitmap rather than one per app.
        let fallback = icon::stock(SIID_APPLICATION).map(Arc::new);
        let mut out = Vec::new();
        if let Err(e) = enum_apps_folder(&mut out, fallback.as_ref()) {
            crate::dlog!("search: AppsFolder enumeration failed: {e}");
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name && a.app_id == b.app_id);
        out
    }
}

unsafe fn enum_apps_folder(
    out: &mut Vec<AppEntry>,
    fallback: Option<&Arc<Icon>>,
) -> windows::core::Result<()> {
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
                    icon: icon::from_shell_item(item)
                        .map(Arc::new)
                        .or_else(|| fallback.cloned()),
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

/// Show in Explorer where an entry came from.
///
/// Desktop apps carry a real filesystem path as their AppsFolder parsing
/// name, so the file gets selected in its folder. Packaged apps only have an
/// AppUserModelID and no file to point at, so fall back to the shell's
/// Applications folder - the very list apex indexes.
fn reveal(app_id: &str) -> bool {
    let is_path = std::path::Path::new(app_id).exists();
    let param: Vec<u16> = if is_path {
        format!("/select,\"{app_id}\"")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    } else {
        Vec::new()
    };
    unsafe {
        let inst = if is_path {
            ShellExecuteW(
                None,
                w!("open"),
                w!("explorer.exe"),
                PCWSTR(param.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        } else {
            ShellExecuteW(
                None,
                w!("open"),
                w!("shell:AppsFolder"),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        let ok = inst.0 as isize > 32;
        if !ok {
            crate::dlog!("search: failed to reveal {app_id}");
        }
        ok
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
