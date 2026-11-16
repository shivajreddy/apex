//! Scripts: every script in a set of folders, each a command of its own -
//! drop a script into one and it runs by name, the way Raycast's script
//! directories work.
//!
//! The folders are the `[scripts]` list, numbered like `[sources]`:
//!
//! ```toml
//! [scripts]
//! 1 = '~\dotfiles\windows\scripts\apex'
//! 2 = 'D:\tools\scripts'
//! ```
//!
//! With no `[scripts]` section the list is
//! [`config::DEFAULT_SCRIPT_FOLDERS`], so every machine sharing a dotfiles
//! repo finds its scripts without a line of config. A folder that does not
//! exist on this machine is skipped without complaint, so one list can serve
//! several machines. Folders are read one level deep - helpers can live in a
//! subfolder - and may start with `~` and use `%NAME%` variables.
//!
//! A script runs as one in a source folder does (see [`crate::script`]):
//! with its interpreter, from its own folder, as the signed-in user, with no
//! window. The folders are read on a background thread, at startup and on
//! reload, so one on a slow or absent drive never holds up the launcher.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use windows::Win32::UI::Shell::SIID_APPLICATION;

use crate::config;
use crate::fuzzy;
use crate::icon;
use crate::plugin::{Action, ActionResult, Aliases, Icon, Plugin, ResultItem};
use crate::script::{self, Window};

pub const ID: &str = "scripts";

/// Score boost for an exact alias hit; the figure the other plugins use.
const ALIAS_BOOST: i32 = 2000;

struct Entry {
    /// The script's full path, and the payload: launch history, aliases and
    /// hiding all key on it.
    path: String,
    /// Its file name, without the extension.
    name: String,
    name_folded: Vec<char>,
    bonus: Vec<i32>,
}

impl Entry {
    fn new(path: String, name: String) -> Self {
        Self {
            name_folded: fuzzy::fold_case(&name),
            bonus: fuzzy::bonuses(&name),
            path,
            name,
        }
    }
}

pub struct Scripts {
    entries: Vec<Entry>,
    /// A read of the folders still in progress on a background thread.
    pending: Option<Receiver<Vec<Entry>>>,
    icon: Option<Arc<Icon>>,
    /// The `[aliases]` table, shared with the other plugins. A script's
    /// alias points at its path.
    aliases: Aliases,
}

impl Scripts {
    pub fn new(folders: Vec<String>, aliases: Aliases) -> Self {
        Self {
            entries: Vec::new(),
            pending: Some(spawn_scan(folders)),
            icon: icon::stock(SIID_APPLICATION).map(Arc::new),
            aliases,
        }
    }

    /// Swap in the newest read of the folders, once it has landed.
    fn poll(&mut self) {
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

    fn find(&self, path: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case(path))
    }

    fn item(&self, e: &Entry, alias: Option<&str>, score: i32) -> ResultItem {
        ResultItem {
            plugin: ID,
            title: e.name.clone(),
            badge: alias.map(str::to_string),
            category: String::new(),
            subtitle: "Script".to_string(),
            payload: e.path.clone(),
            score,
            icon: self.icon.clone(),
        }
    }
}

impl Plugin for Scripts {
    fn id(&self) -> &'static str {
        ID
    }

    fn refresh(&mut self) {
        let cfg = config::Config::load();
        // The alias table is shared, but reloading it here as well keeps it
        // current when this is the only plugin with aliases.
        self.aliases.reload(cfg.aliases_map());
        // The current list stays until the new read lands, so it never
        // blinks empty.
        self.pending = Some(spawn_scan(cfg.script_folders()));
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        self.poll();
        let query = fuzzy::fold_case(q);
        let query_str: String = query.iter().collect();
        for e in &self.entries {
            let alias = self.aliases.of(&e.path);
            let alias_hit = alias.as_deref() == Some(query_str.as_str());
            let fuzzy_score = fuzzy::score(&query, &e.name_folded, &e.bonus);
            if fuzzy_score.is_none() && !alias_hit {
                continue;
            }
            let score = fuzzy_score.unwrap_or(0) + if alias_hit { ALIAS_BOOST } else { 0 };
            out.push(self.item(e, alias.as_deref(), score));
        }
    }

    fn item_for(&mut self, payload: &str) -> Option<ResultItem> {
        self.poll();
        let e = self.find(payload)?;
        Some(self.item(e, self.aliases.of(&e.path).as_deref(), 0))
    }

    fn browse(&mut self, limit: usize, out: &mut Vec<ResultItem>) {
        self.poll();
        let mut added = 0;
        for e in &self.entries {
            if added == limit {
                break;
            }
            if out.iter().any(|i| i.plugin == ID && i.payload == e.path) {
                continue;
            }
            out.push(self.item(e, self.aliases.of(&e.path).as_deref(), 0));
            added += 1;
        }
    }

    fn activate(&mut self, item: &ResultItem, _args: &[String]) -> ActionResult {
        if script::run(&item.payload, &[], Window::default(), false) {
            ActionResult::Close
        } else {
            // Leave the window up: nothing ran.
            ActionResult::Done
        }
    }

    fn actions(&self, item: &ResultItem) -> Vec<Action> {
        let action = |id, label: &str| Action {
            id,
            label: label.to_string(),
        };
        let mut actions = vec![
            action("run", "Run"),
            // A script runs unseen; this runs it once where it can be watched.
            action("run_terminal", "Run in Terminal"),
            action("run_as_admin", "Run as Administrator"),
            action("reveal", "Open in Explorer"),
        ];
        if let Some(alias) = self.aliases.of(&item.payload) {
            actions.push(Action {
                id: "remove_alias",
                label: format!("Remove Alias \u{201c}{alias}\u{201d}"),
            });
        }
        actions.push(action("set_alias", "Set Alias\u{2026}"));
        actions
    }

    fn run_action(&mut self, action_id: &str, item: &ResultItem, args: &[String]) -> ActionResult {
        let path = item.payload.as_str();
        let ran = |started: bool| {
            if started {
                ActionResult::Close
            } else {
                ActionResult::Done
            }
        };
        match action_id {
            "run" => self.activate(item, args),
            "run_terminal" => ran(script::run(path, &[], Window::Keep, false)),
            "run_as_admin" => ran(script::run(path, &[], Window::default(), true)),
            "reveal" => {
                crate::launch::reveal(path);
                // Dismiss, not Close: looking at a script is not running it,
                // and shouldn't inflate its ranking.
                ActionResult::Dismiss
            }
            "set_alias" => ActionResult::RequestText {
                prompt: format!("Alias for {}", item.title),
                action_id: "set_alias",
            },
            "remove_alias" => {
                self.aliases.remove(path);
                config::remove_alias_file(path);
                ActionResult::Done
            }
            _ => ActionResult::Done,
        }
    }

    fn submit_text(&mut self, action_id: &str, item: &ResultItem, text: &str) -> ActionResult {
        if action_id == "set_alias" {
            let alias = config::sanitize_key(text);
            if !alias.is_empty() {
                self.aliases.set(&alias, &item.payload);
                config::upsert_alias_file(&alias, &item.payload);
            }
        }
        ActionResult::Done
    }
}

/// Read the folders on a background thread, so one on a slow or absent
/// drive never holds up the launcher.
fn spawn_scan(folders: Vec<String>) -> Receiver<Vec<Entry>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Ignored if the receiver is gone: a later reload supersedes this.
        let _ = tx.send(scan(&folders));
    });
    rx
}

/// Every script directly inside `folders`, sorted by name. A folder that
/// does not exist on this machine, or cannot be read, is skipped without
/// complaint.
fn scan(folders: &[String]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for folder in folders {
        let dir = config::expand_path(folder);
        let Ok(listing) = std::fs::read_dir(&dir) else {
            crate::dlog!("scripts: no folder {dir:?} here, skipping it");
            continue;
        };
        for item in listing.flatten() {
            let path = item.path();
            let (Some(full), Some(name)) =
                (path.to_str(), path.file_stem().and_then(|s| s.to_str()))
            else {
                continue;
            };
            // A folder named like a script is not one.
            if !script::is_script(full) || !path.is_file() {
                continue;
            }
            // The same folder listed twice, written two ways.
            if entries.iter().any(|e| e.path.eq_ignore_ascii_case(full)) {
                continue;
            }
            entries.push(Entry::new(full.to_string(), name.to_string()));
        }
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of files under `target`, removed again afterwards. A name
    /// ending in `\` is made a folder.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(name: &str, files: &[&str]) -> Self {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!("scripts-test-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for file in files {
                let path = dir.join(file);
                if file.ends_with('\\') {
                    std::fs::create_dir_all(&path).unwrap();
                } else {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, "").unwrap();
                }
            }
            Self(dir)
        }

        fn dir(&self) -> String {
            self.0.to_str().unwrap().to_string()
        }

        fn file(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn plugin(entries: Vec<Entry>) -> Scripts {
        Scripts {
            entries,
            pending: None,
            icon: None,
            aliases: Aliases::default(),
        }
    }

    #[test]
    fn every_script_directly_in_a_folder_is_found() {
        let f = Fixture::new(
            "found",
            &[
                "flush-dns.ps1",
                "Backup.sh",
                "build.cmd",
                "tool.py",
                "notes.txt",
                "app.exe",
                r"helpers\lib.ps1",
                r"not-a-script.ps1\",
            ],
        );
        let entries = scan(&[f.dir()]);
        // Sorted by name whatever its case; one level deep; files only.
        assert_eq!(names(&entries), ["Backup", "build", "flush-dns", "tool"]);
        assert_eq!(entries[2].path, f.file("flush-dns.ps1"));
    }

    #[test]
    fn a_folder_that_is_not_here_is_skipped_and_one_listed_twice_counts_once() {
        let f = Fixture::new("skipped", &["a.ps1", "b.bat"]);
        let entries = scan(&[
            r"Q:\no\such\folder".to_string(),
            r"~\apex-surely-no-such-folder".to_string(),
            String::new(),
            f.dir(),
            // The same folder, written with forward slashes and a trailing
            // one.
            format!("{}/", f.dir().replace('\\', "/")),
        ]);
        assert_eq!(names(&entries), ["a", "b"]);
        assert_eq!(entries[0].path, f.file("a.ps1"));
        // Nothing at all to read is no error either.
        assert!(scan(&[]).is_empty());
    }

    #[test]
    fn a_new_plugin_reads_its_folders_in_the_background() {
        let f = Fixture::new("background", &["flush-dns.ps1"]);
        let mut p = Scripts::new(vec![f.dir()], Aliases::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let item = loop {
            if let Some(item) = p.item_for(&f.file("flush-dns.ps1")) {
                break item;
            }
            assert!(std::time::Instant::now() < deadline, "never read");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(item.title, "flush-dns");
        assert_eq!(item.subtitle, "Script");
        assert_eq!(item.plugin, ID);
    }

    #[test]
    fn scripts_are_found_by_name_and_alias() {
        let mut p = plugin(vec![
            Entry::new(r"C:\s\flush-dns.ps1".into(), "flush-dns".into()),
            Entry::new(r"C:\s\deploy.sh".into(), "deploy".into()),
        ]);
        let mut out = Vec::new();
        p.query("dns", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, r"C:\s\flush-dns.ps1");

        // An exact alias hit ranks first, and shows as the badge.
        p.aliases.set("fd", r"C:\s\flush-dns.ps1");
        out.clear();
        p.query("fd", &mut out);
        assert_eq!(out[0].payload, r"C:\s\flush-dns.ps1");
        assert!(out[0].score >= ALIAS_BOOST);
        assert_eq!(out[0].badge.as_deref(), Some("fd"));

        // Paths compare as Windows compares them.
        assert!(p.item_for(r"c:\S\FLUSH-DNS.PS1").is_some());
        assert!(p.item_for(r"C:\s\gone.ps1").is_none());
    }

    #[test]
    fn browse_skips_what_history_already_placed() {
        let mut p = plugin(vec![
            Entry::new(r"C:\s\a.ps1".into(), "a".into()),
            Entry::new(r"C:\s\b.ps1".into(), "b".into()),
        ]);
        let mut out = vec![p.item_for(r"C:\s\a.ps1").unwrap()];
        p.browse(8, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].payload, r"C:\s\b.ps1");
    }

    #[test]
    fn a_script_offers_running_it_watching_it_and_finding_it() {
        let p = plugin(vec![Entry::new(r"C:\s\a.ps1".into(), "a".into())]);
        let item = p.item(&p.entries[0], None, 0);
        let ids: Vec<&str> = p.actions(&item).iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            ["run", "run_terminal", "run_as_admin", "reveal", "set_alias"]
        );
        p.aliases.set("x", r"C:\s\a.ps1");
        let ids: Vec<&str> = p.actions(&item).iter().map(|a| a.id).collect();
        assert!(ids.contains(&"remove_alias"));
    }
}
