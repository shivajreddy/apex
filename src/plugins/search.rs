//! Search plugin: fuzzy application search over Start Menu shortcuts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::fuzzy;
use crate::plugin::{Plugin, ResultItem};

pub const ID: &str = "search";
const MAX_SCAN_DEPTH: u32 = 4;

struct AppEntry {
    /// Display name (shortcut file stem).
    name: String,
    /// Case-folded chars aligned with `bonus`.
    name_folded: Vec<char>,
    /// Precomputed word-start bonuses.
    bonus: Vec<i32>,
    /// Full path to the .lnk file.
    path: String,
}

pub struct Search {
    entries: Vec<AppEntry>,
}

impl Search {
    pub fn new() -> Self {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();

        // User dir first so per-user shortcuts win over machine-wide ones.
        for root in start_menu_roots() {
            scan(&root, 0, &mut entries, &mut seen);
        }
        crate::dlog!("search: indexed {} applications", entries.len());
        Self { entries }
    }
}

impl Plugin for Search {
    fn id(&self) -> &'static str {
        ID
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        let query: Vec<char> = fuzzy::fold_case(q);
        for e in &self.entries {
            if let Some(score) = fuzzy::score(&query, &e.name_folded, &e.bonus) {
                out.push(ResultItem {
                    plugin: ID,
                    title: e.name.clone(),
                    subtitle: "Application".into(),
                    payload: e.path.clone(),
                    score,
                });
            }
        }
    }

    fn activate(&mut self, item: &ResultItem) -> bool {
        launch(&item.payload)
    }
}

fn start_menu_roots() -> Vec<PathBuf> {
    let suffix = r"Microsoft\Windows\Start Menu\Programs";
    let mut roots = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(Path::new(&appdata).join(suffix));
    }
    if let Ok(programdata) = std::env::var("ProgramData") {
        roots.push(Path::new(&programdata).join(suffix));
    }
    roots
}

fn scan(dir: &Path, depth: u32, entries: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, depth + 1, entries, seen);
            continue;
        }
        let is_lnk = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
        if !is_lnk {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let name = name.to_string();
        let key = name.to_lowercase();
        if !seen.insert(key) {
            continue; // duplicate shortcut name; first (user) wins
        }
        entries.push(AppEntry {
            name_folded: fuzzy::fold_case(&name),
            bonus: fuzzy::bonuses(&name),
            path: path.to_string_lossy().into_owned(),
            name,
        });
    }
}

/// Launch a shortcut via the shell. Returns true on success.
fn launch(path: &str) -> bool {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
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
            crate::dlog!("search: failed to launch {path}");
        }
        ok
    }
}
