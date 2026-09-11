//! Search plugin: fuzzy application search over the shell AppsFolder.
//!
//! AppsFolder is the same list the Start menu shows: classic desktop apps
//! (Start Menu shortcuts) and packaged/UWP apps (Notepad, Settings, Store
//! apps). The list itself comes from [`crate::appindex`]: a helper process
//! builds it and writes it to disk, and this plugin only ever reads the
//! file, so the shell's enumeration and imaging libraries never load into
//! the launcher. Loading and refreshing happen on a background thread;
//! results are swapped in on the first query that finds them ready.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use std::collections::HashMap;

use crate::appindex::{self, AppIndex};
use crate::fuzzy;
use crate::plugin::{Action, ActionResult, Icon, Plugin, ResultItem};

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

impl AppEntry {
    fn from_index(app: appindex::IndexedApp) -> Self {
        Self {
            name_folded: fuzzy::fold_case(&app.name),
            bonus: fuzzy::bonuses(&app.name),
            name: app.name,
            app_id: app.app_id,
            icon: app.icon,
        }
    }
}

pub struct Search {
    entries: Vec<AppEntry>,
    pending: Option<Receiver<Vec<AppEntry>>>,
    /// alias (case-folded) -> app id. Loaded from `[aliases]`, updated by
    /// the Set/Remove Alias actions (which also write the config file).
    aliases: HashMap<String, String>,
}

fn entries_of(index: AppIndex) -> Vec<AppEntry> {
    index.apps.into_iter().map(AppEntry::from_index).collect()
}

/// Load the index on a background thread so startup - and reloads - never
/// block the UI.
///
/// At startup the last index on disk is delivered first, so the first
/// summon already has every app, and the helper then rebuilds it to catch
/// installs and removals; that second delivery supersedes the first. A
/// reload skips the stale copy and only delivers the rebuilt one. If the
/// helper cannot run at all, the index is built in-process instead - that
/// costs the memory the helper exists to save, but apex keeps working.
fn spawn_index(use_cached: bool) -> Receiver<Vec<AppEntry>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut delivered = false;
        if use_cached && let Some(index) = appindex::load() {
            delivered = tx.send(entries_of(index)).is_ok();
        }
        let fresh = if appindex::run_helper() {
            appindex::load()
        } else {
            crate::dlog!("search: index helper failed, indexing in-process");
            let sources = crate::config::Config::load().list_values(crate::config::SOURCES);
            Some(appindex::build(&sources))
        };
        if let Some(index) = fresh {
            crate::dlog!(
                "search: indexed {} apps in {:.1?}",
                index.apps.len(),
                started.elapsed()
            );
            // Ignored if the receiver is gone: a later reload supersedes this.
            let _ = tx.send(entries_of(index));
        } else if !delivered {
            crate::dlog!("search: no index available");
        }
    });
    rx
}

impl Search {
    pub fn new(aliases: HashMap<String, String>) -> Self {
        Self {
            entries: Vec::new(),
            pending: Some(spawn_index(true)),
            aliases,
        }
    }

    fn alias_of(&self, app_id: &str) -> Option<&str> {
        self.aliases
            .iter()
            .find(|(_, id)| id.as_str() == app_id)
            .map(|(a, _)| a.as_str())
    }

    /// Swap in the newest index the background thread has delivered.
    ///
    /// Drains rather than taking one message: the startup thread delivers
    /// the cached index and then the rebuilt one, and only the last matters.
    fn poll_index(&mut self) {
        let Some(rx) = &self.pending else { return };
        loop {
            match rx.try_recv() {
                Ok(entries) => self.entries = entries,
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }
}

impl Plugin for Search {
    fn id(&self) -> &'static str {
        ID
    }

    fn refresh(&mut self) {
        // Aliases come back from disk too, so editing the config by hand and
        // reloading is enough - no restart. Source folders are read by the
        // helper itself.
        self.aliases = crate::config::Config::load().aliases_map();
        // The current index stays in place until the rebuild lands, so the
        // list never blinks empty.
        self.pending = Some(spawn_index(false));
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
        badge: alias.map(str::to_string),
        subtitle: "Application".to_string(),
        payload: e.app_id.clone(),
        score,
        icon: e.icon.clone(),
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

/// Launch an entry via the shell. Returns true on success.
///
/// Source-folder entries carry a real path and are run directly; AppsFolder
/// ids are not paths and have to go through the `shell:AppsFolder` verb.
fn launch(app_id: &str) -> bool {
    let target = if std::path::Path::new(app_id).exists() {
        app_id.to_string()
    } else {
        format!("shell:AppsFolder\\{app_id}")
    };
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
