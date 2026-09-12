//! Apex's own commands, as searchable rows.
//!
//! Distinct from anything the machine can do: these act on apex itself. They
//! share an `Apex: ` prefix so the whole set is one keystroke away - typing
//! "apex" lists them - while each remains reachable by its own word, since
//! fuzzy matching only needs a subsequence.
//!
//! Rows here are deliberately excluded from launch history and from the
//! empty-query list: running a command is not launching something, and
//! ranking commands by use would push them in front of real results.

use std::sync::Arc;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::config;
use crate::fuzzy;
use crate::icon;
use crate::plugin::{Action, ActionResult, FormField, Icon, Plugin, ResultItem, ShellCommand};

pub const ID: &str = "commands";

/// How much a keyword-only hit is docked, so a command whose *label* matches
/// always outranks one that only matched a hidden synonym.
const KEYWORD_PENALTY: i32 = 12;

struct Command {
    id: &'static str,
    label: &'static str,
    folded: Vec<char>,
    bonus: Vec<i32>,
    /// Label plus hidden synonyms, matched separately so that scoring the
    /// label is unaffected by however many keywords a command carries.
    alt_folded: Vec<char>,
    alt_bonus: Vec<i32>,
}

impl Command {
    fn new(id: &'static str, label: &'static str, keywords: &str) -> Self {
        let alt = format!("{label} {keywords}");
        Self {
            id,
            label,
            folded: fuzzy::fold_case(label),
            bonus: fuzzy::bonuses(label),
            alt_folded: fuzzy::fold_case(&alt),
            alt_bonus: fuzzy::bonuses(&alt),
        }
    }

    fn score(&self, query: &[char]) -> Option<i32> {
        let direct = fuzzy::score(query, &self.folded, &self.bonus);
        let alt = fuzzy::score(query, &self.alt_folded, &self.alt_bonus)
            .map(|s| s - KEYWORD_PENALTY);
        match (direct, alt) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Hidden keywords exist because the obvious search term is often not in the
/// label: "startup" appears nowhere in "Toggle Start at Login".
fn all() -> Vec<Command> {
    vec![
        Command::new("reload", "Apex: Reload", "refresh rescan reindex"),
        Command::new("restart", "Apex: Restart", "relaunch"),
        Command::new("quit", "Apex: Quit", "exit close kill"),
        Command::new("config", "Apex: Open Config", "settings toml preferences edit"),
        Command::new("config_folder", "Apex: Open Config Folder", "directory appdata"),
        Command::new("tray", "Apex: Toggle Tray Icon", "systray notification area"),
        Command::new(
            "startup",
            "Apex: Toggle Start at Login",
            "startup autostart boot signin",
        ),
        Command::new(
            "add_source",
            "Apex: Add Source Folder",
            "index scan portable tools directory",
        ),
        Command::new(
            "sources",
            "Apex: Manage Source Folders",
            "remove delete indexed directory folders",
        ),
        Command::new(
            "hidden",
            "Apex: Manage Hidden Entries",
            "unhide restore show excluded",
        ),
        Command::new(
            "clear_history",
            "Apex: Clear Launch History",
            "frecency reset forget ranking",
        ),
    ]
}

pub struct Commands {
    commands: Vec<Command>,
    icon: Option<Arc<Icon>>,
}

impl Default for Commands {
    fn default() -> Self {
        Self::new()
    }
}

impl Commands {
    pub fn new() -> Self {
        Self {
            commands: all(),
            icon: icon::app().map(Arc::new),
        }
    }
}

impl Plugin for Commands {
    fn id(&self) -> &'static str {
        ID
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        let query = fuzzy::fold_case(q);
        for c in &self.commands {
            if let Some(score) = c.score(&query) {
                out.push(ResultItem {
                    plugin: ID,
                    // Titles read "Apex: Reload"; show "Reload" with "Apex" as
                    // the category, the way Raycast lists an extension's
                    // commands. Search still matches the full label.
                    title: c.label.strip_prefix("Apex: ").unwrap_or(c.label).to_string(),
                    badge: None,
                    category: "Apex".to_string(),
                    subtitle: "Command".to_string(),
                    payload: c.id.to_string(),
                    score,
                    icon: self.icon.clone(),
                });
            }
        }
    }

    fn activate(&mut self, item: &ResultItem) -> ActionResult {
        match item.payload.as_str() {
            "reload" => ActionResult::Refresh,
            "restart" => ActionResult::Shell(ShellCommand::Restart),
            "quit" => ActionResult::Shell(ShellCommand::Quit),
            "tray" => ActionResult::Shell(ShellCommand::ToggleTray),
            "clear_history" => ActionResult::Shell(ShellCommand::ClearHistory),
            "hidden" => ActionResult::Shell(ShellCommand::ShowHidden),
            "sources" => ActionResult::Shell(ShellCommand::ShowSources),
            "add_source" => source_form("Add Source Folder", ""),
            "config" => {
                open_config(false);
                ActionResult::Dismiss
            }
            "config_folder" => {
                open_config(true);
                ActionResult::Dismiss
            }
            "startup" => {
                toggle_startup();
                // Stay open: the change is silent, and Done re-queries so the
                // row is still there to toggle back.
                ActionResult::Done
            }
            _ => ActionResult::Done,
        }
    }

    fn actions(&self, _item: &ResultItem) -> Vec<Action> {
        vec![Action {
            id: "run",
            label: "Run".to_string(),
        }]
    }

    fn run_action(&mut self, _action_id: &str, item: &ResultItem) -> ActionResult {
        self.activate(item)
    }

    fn submit_form(
        &mut self,
        action_id: &str,
        _item: &ResultItem,
        fields: &[FormField],
    ) -> ActionResult {
        if action_id != "add_source" {
            return ActionResult::Done;
        }
        let path = fields
            .first()
            .map(|f| config::sanitize_value(&f.value))
            .unwrap_or_default();
        // Reopen the form rather than writing a source that indexes nothing.
        if path.is_empty() || !std::path::Path::new(&path).is_dir() {
            return source_form("Folder not found", &path);
        }
        let mut sources = config::Config::load().list_values(config::SOURCES);
        if !sources.iter().any(|s| s.eq_ignore_ascii_case(&path)) {
            sources.push(path);
            config::set_list_file(config::SOURCES, &sources);
        }
        // Reload so the new folder is indexed immediately.
        ActionResult::Refresh
    }
}

fn source_form(title: &str, value: &str) -> ActionResult {
    ActionResult::RequestForm {
        title: title.to_string(),
        action_id: "add_source",
        fields: vec![FormField::new("Folder (paste with Ctrl+V)", value)],
    }
}

/// Open `config.toml` in the default editor. Also used by the tray menu.
pub fn open_config_file() {
    open_config(false);
}

/// Open `config.toml`, or the folder holding it.
fn open_config(folder: bool) {
    let Some(path) = config::config_path() else {
        return;
    };
    let target = if folder {
        path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
    } else {
        path
    };
    let wide: Vec<u16> = target
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let inst = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if inst.0 as isize <= 32 {
            crate::dlog!("commands: failed to open {}", target.display());
        }
    }
}

/// Flip `[general] start_on_startup` and apply it straight away, so the Run
/// key matches the file without waiting for a restart.
fn toggle_startup() {
    let enabled = config::Config::load().general_flag("start_on_startup", true);
    config::set_general_flag_file("start_on_startup", !enabled);
    crate::setup::ensure(&config::Config::load());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Commands {
        Commands {
            commands: all(),
            icon: None,
        }
    }

    /// Payloads best-first. Mirrors what `App::refresh_results` does with a
    /// plugin's output - without the sort, this would assert on the order
    /// commands happen to be declared in rather than on ranking.
    fn find(q: &str) -> Vec<String> {
        let mut c = commands();
        let mut out = Vec::new();
        c.query(q, &mut out);
        out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        out.into_iter().map(|i| i.payload).collect()
    }

    #[test]
    fn the_prefix_lists_every_command() {
        assert_eq!(find("apex").len(), all().len());
    }

    #[test]
    fn hidden_keywords_find_commands_their_label_does_not_contain() {
        // "startup" shares no subsequence with "Toggle Start at Login".
        assert_eq!(find("startup").first().map(String::as_str), Some("startup"));
        assert_eq!(find("exit").first().map(String::as_str), Some("quit"));
        assert_eq!(
            find("settings").first().map(String::as_str),
            Some("config")
        );
        assert_eq!(
            find("frecency").first().map(String::as_str),
            Some("clear_history")
        );
    }

    #[test]
    fn a_label_match_outranks_a_keyword_match() {
        // "reload" is Reload's label and also a keyword on nothing else, but
        // "refresh" is only a keyword - both must still land on reload.
        assert_eq!(find("reload").first().map(String::as_str), Some("reload"));
        assert_eq!(find("refresh").first().map(String::as_str), Some("reload"));
    }

    #[test]
    fn each_command_is_reachable_by_its_own_word() {
        for (q, id) in [
            ("reload", "reload"),
            ("restart", "restart"),
            ("quit", "quit"),
            ("tray", "tray"),
            ("startup", "startup"),
            ("clear history", "clear_history"),
        ] {
            let hits = find(q);
            assert!(
                hits.first().map(String::as_str) == Some(id),
                "query {q:?} ranked {hits:?}, wanted {id} first"
            );
        }
    }

    #[test]
    fn open_config_outranks_open_config_folder_for_its_own_name() {
        let hits = find("open config");
        assert_eq!(hits.first().map(String::as_str), Some("config"));
    }

    #[test]
    fn unrelated_queries_match_nothing() {
        assert!(find("spotify").is_empty());
    }

    #[test]
    fn commands_map_to_the_right_outcomes() {
        let mut c = commands();
        let item = |id: &str| ResultItem {
            plugin: ID,
            title: String::new(),
            badge: None,
            category: String::new(),
            subtitle: String::new(),
            payload: id.to_string(),
            score: 0,
            icon: None,
        };
        assert!(matches!(c.activate(&item("reload")), ActionResult::Refresh));
        assert!(matches!(
            c.activate(&item("quit")),
            ActionResult::Shell(ShellCommand::Quit)
        ));
        assert!(matches!(
            c.activate(&item("restart")),
            ActionResult::Shell(ShellCommand::Restart)
        ));
        assert!(matches!(
            c.activate(&item("tray")),
            ActionResult::Shell(ShellCommand::ToggleTray)
        ));
        assert!(matches!(
            c.activate(&item("clear_history")),
            ActionResult::Shell(ShellCommand::ClearHistory)
        ));
        assert!(matches!(
            c.activate(&item("hidden")),
            ActionResult::Shell(ShellCommand::ShowHidden)
        ));
        assert!(matches!(
            c.activate(&item("sources")),
            ActionResult::Shell(ShellCommand::ShowSources)
        ));
    }

    #[test]
    fn adding_and_managing_sources_are_told_apart() {
        // Two commands about the same noun, so the distinguishing verb has
        // to win rather than the shared word.
        assert_eq!(
            find("add source").first().map(String::as_str),
            Some("add_source")
        );
        for q in ["manage sources", "source folders", "remove", "delete"] {
            assert_eq!(
                find(q).first().map(String::as_str),
                Some("sources"),
                "query {q:?} should find Manage Source Folders"
            );
        }
    }

    #[test]
    fn keywords_are_synonyms_not_phrase_material() {
        // Keywords sit after the label in the haystack, so a query pairing a
        // keyword with a label word in the other order cannot match as a
        // subsequence. Documented here so the limit is deliberate rather
        // than discovered.
        assert!(find("remove source").is_empty());
        assert_eq!(find("remove").first().map(String::as_str), Some("sources"));
    }

    #[test]
    fn manage_hidden_is_findable_by_the_words_people_reach_for() {
        for q in ["hidden", "unhide", "restore"] {
            assert_eq!(
                find(q).first().map(String::as_str),
                Some("hidden"),
                "query {q:?} should find Manage Hidden Entries"
            );
        }
    }

    #[test]
    fn commands_stay_out_of_history_and_the_default_list() {
        let mut c = commands();
        assert!(c.item_for("reload").is_none());
        let mut out = Vec::new();
        c.browse(8, &mut out);
        assert!(out.is_empty());
    }
}
