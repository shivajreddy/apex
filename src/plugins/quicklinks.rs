//! Quicklinks: user-defined links, paths and commands, opened by name.
//!
//! Each one is a sub-table in the config file:
//!
//! ```toml
//! [quicklinks.github]
//! name = 'Search GitHub'
//! link = 'https://github.com/search?q={query}'
//! open_with = 'chrome'          # optional
//! ```
//!
//! A `{token}` in the link makes it take an argument: activating the
//! quicklink prompts using the token's name, and the answer is substituted
//! in (percent-encoded when the link is a URL). That single mechanism covers
//! both web search and plain bookmarks.
//!
//! Quicklinks are created and edited from inside apex, which rewrites only
//! the relevant `[quicklinks.*]` block - the rest of the file, comments
//! included, is untouched. They live in the roaming config rather than in
//! machine-local state because links and paths are portable across machines.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::UI::Shell::{
    SHSTOCKICONID, SIID_APPLICATION, SIID_FOLDER, SIID_INTERNET, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::config;
use crate::fuzzy;
use crate::icon;
use crate::plugin::{Action, ActionResult, FormField, Icon, Plugin, ResultItem};

pub const ID: &str = "quicklinks";

/// Label of the synthetic row that opens the create form. Matched fuzzily
/// like any other entry, so "create", "quicklink" and "cql" all find it.
const CREATE_LABEL: &str = "Create Quicklink";

/// Payload of that row. Slugs are sanitised to `a-z 0-9 - _ .`, so a leading
/// `!` can never collide with a real one.
const CREATE_PAYLOAD: &str = "!create";

/// Shown when a link has an empty `{}` token.
const DEFAULT_ARG: &str = "query";

struct Entry {
    /// Config section suffix, and the payload. Stable across renames so
    /// launch history survives editing the name.
    slug: String,
    name: String,
    link: String,
    open_with: String,
    name_folded: Vec<char>,
    bonus: Vec<i32>,
    icon: Option<Arc<Icon>>,
}

impl Entry {
    fn new(
        slug: String,
        name: String,
        link: String,
        open_with: String,
        icon: Option<Arc<Icon>>,
    ) -> Self {
        Self {
            name_folded: fuzzy::fold_case(&name),
            bonus: fuzzy::bonuses(&name),
            slug,
            name,
            link,
            open_with,
            icon,
        }
    }

    fn item(&self, score: i32) -> ResultItem {
        ResultItem {
            plugin: ID,
            title: self.name.clone(),
            subtitle: "Quicklink".to_string(),
            payload: self.slug.clone(),
            score,
            icon: self.icon.clone(),
        }
    }
}

/// Stock icons, extracted at most once each and shared by `Arc` across every
/// quicklink that uses them.
#[derive(Default)]
struct IconCache(HashMap<i32, Option<Arc<Icon>>>);

impl IconCache {
    fn get(&mut self, id: SHSTOCKICONID) -> Option<Arc<Icon>> {
        self.0
            .entry(id.0)
            .or_insert_with(|| icon::stock(id).map(Arc::new))
            .clone()
    }
}

/// Which stock icon suits a link. Deliberately does not extract the target's
/// real icon: that needs shell COM calls, and this runs on the UI thread
/// during startup, before there is a message pump to service them.
fn icon_id(link: &str) -> SHSTOCKICONID {
    if link.contains("://") {
        SIID_INTERNET
    } else if std::path::Path::new(link).is_dir() {
        SIID_FOLDER
    } else {
        SIID_APPLICATION
    }
}

pub struct Quicklinks {
    entries: Vec<Entry>,
    /// Precomputed match data for [`CREATE_LABEL`], so the synthetic row
    /// costs no allocation per keystroke.
    create_folded: Vec<char>,
    create_bonus: Vec<i32>,
    icons: IconCache,
}

impl Quicklinks {
    pub fn new(subtables: Vec<(String, HashMap<String, String>)>) -> Self {
        let mut icons = IconCache::default();
        let mut entries: Vec<Entry> = Vec::new();
        for (slug, kv) in subtables {
            let link = kv.get("link").map(|s| s.trim()).unwrap_or_default();
            if link.is_empty() {
                continue; // a quicklink with no target is not usable
            }
            let name = match kv.get("name").map(|s| s.trim()).filter(|s| !s.is_empty()) {
                Some(n) => n.to_string(),
                None => slug.clone(),
            };
            let open_with = kv.get("open_with").map(|s| s.trim()).unwrap_or_default();
            let art = icons.get(icon_id(link));
            entries.push(Entry::new(
                slug,
                name,
                link.to_string(),
                open_with.to_string(),
                art,
            ));
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            entries,
            create_folded: fuzzy::fold_case(CREATE_LABEL),
            create_bonus: fuzzy::bonuses(CREATE_LABEL),
            icons,
        }
    }

    fn find(&self, slug: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.slug == slug)
    }

    fn create_item(&mut self, score: i32) -> ResultItem {
        ResultItem {
            plugin: ID,
            title: CREATE_LABEL.to_string(),
            subtitle: "Quicklink".to_string(),
            payload: CREATE_PAYLOAD.to_string(),
            score,
            icon: self.icons.get(SIID_INTERNET),
        }
    }

    /// A slug that is not already taken, derived from the name.
    fn free_slug(&self, name: &str) -> String {
        let base = {
            let s = config::sanitize_key(name);
            if s.is_empty() { "link".to_string() } else { s }
        };
        if self.find(&base).is_none() {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|s| self.find(s).is_none())
            .unwrap_or(base)
    }

    fn form(title: &str, entry: Option<&Entry>, action_id: &'static str) -> ActionResult {
        let (name, link, open_with) = match entry {
            Some(e) => (e.name.as_str(), e.link.as_str(), e.open_with.as_str()),
            None => ("", "", ""),
        };
        ActionResult::RequestForm {
            title: title.to_string(),
            action_id,
            fields: vec![
                FormField::new("Name", name),
                FormField::new("Link", link),
                FormField::new("Open with (optional)", open_with),
            ],
        }
    }

    /// Open a quicklink, prompting first when its link takes an argument.
    fn run(&self, slug: &str) -> ActionResult {
        let Some(e) = self.find(slug) else {
            return ActionResult::Done;
        };
        match placeholder(&e.link) {
            Some(token) => ActionResult::RequestText {
                prompt: if token.is_empty() {
                    DEFAULT_ARG.to_string()
                } else {
                    token.to_string()
                },
                action_id: "fill",
            },
            None => {
                open(&e.link, &e.open_with);
                ActionResult::Close
            }
        }
    }

    /// Write a quicklink to memory and to the config file.
    fn save(&mut self, slug: String, name: String, link: String, open_with: String) {
        config::upsert_quicklink_file(
            &slug,
            &[
                ("name", &name),
                ("link", &link),
                ("open_with", &open_with),
            ],
        );
        let art = self.icons.get(icon_id(&link));
        let entry = Entry::new(slug, name, link, open_with, art);
        match self.entries.iter().position(|e| e.slug == entry.slug) {
            Some(i) => self.entries[i] = entry,
            None => self.entries.push(entry),
        }
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

impl Plugin for Quicklinks {
    fn id(&self) -> &'static str {
        ID
    }

    fn refresh(&mut self) {
        // Cheap and synchronous: this is one small file, unlike the app scan.
        *self = Self::new(config::Config::load().subtables(ID));
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        let query = fuzzy::fold_case(q);
        for e in &self.entries {
            if let Some(score) = fuzzy::score(&query, &e.name_folded, &e.bonus) {
                out.push(e.item(score));
            }
        }
        if let Some(score) = fuzzy::score(&query, &self.create_folded, &self.create_bonus) {
            out.push(self.create_item(score));
        }
    }

    fn item_for(&mut self, payload: &str) -> Option<ResultItem> {
        self.find(payload).map(|e| e.item(0))
    }

    fn browse(&mut self, limit: usize, out: &mut Vec<ResultItem>) {
        let mut added = 0;
        for e in &self.entries {
            if added == limit {
                break;
            }
            if out.iter().any(|i| i.plugin == ID && i.payload == e.slug) {
                continue;
            }
            out.push(e.item(0));
            added += 1;
        }
    }

    fn activate(&mut self, item: &ResultItem) -> ActionResult {
        if item.payload == CREATE_PAYLOAD {
            return Self::form("New Quicklink", None, "create");
        }
        self.run(&item.payload)
    }

    fn actions(&self, item: &ResultItem) -> Vec<Action> {
        if item.payload == CREATE_PAYLOAD {
            return vec![Action {
                id: "create",
                label: CREATE_LABEL.to_string(),
            }];
        }
        vec![
            Action {
                id: "open",
                label: "Open".to_string(),
            },
            Action {
                id: "edit",
                label: "Edit Quicklink\u{2026}".to_string(),
            },
            Action {
                id: "delete",
                label: "Delete Quicklink".to_string(),
            },
            Action {
                id: "create",
                label: "Create Quicklink\u{2026}".to_string(),
            },
        ]
    }

    fn run_action(&mut self, action_id: &str, item: &ResultItem) -> ActionResult {
        match action_id {
            "open" => self.run(&item.payload),
            "create" => Self::form("New Quicklink", None, "create"),
            "edit" => match self.find(&item.payload) {
                Some(e) => Self::form("Edit Quicklink", Some(e), "edit"),
                None => ActionResult::Done,
            },
            "delete" => {
                config::remove_quicklink_file(&item.payload);
                self.entries.retain(|e| e.slug != item.payload);
                ActionResult::Done
            }
            _ => ActionResult::Done,
        }
    }

    fn submit_text(&mut self, action_id: &str, item: &ResultItem, text: &str) -> ActionResult {
        if action_id != "fill" {
            return ActionResult::Done;
        }
        let Some(e) = self.find(&item.payload) else {
            return ActionResult::Done;
        };
        open(&fill(&e.link, text), &e.open_with);
        ActionResult::Close
    }

    fn submit_form(
        &mut self,
        action_id: &str,
        item: &ResultItem,
        fields: &[FormField],
    ) -> ActionResult {
        let value = |i: usize| {
            fields
                .get(i)
                .map(|f| config::sanitize_value(&f.value))
                .unwrap_or_default()
        };
        let (name, link, open_with) = (value(0), value(1), value(2));
        // A quicklink with no target cannot do anything. Reopen the form
        // carrying the entered values back, rather than writing a broken
        // entry or discarding what was typed.
        if link.is_empty() {
            return ActionResult::RequestForm {
                title: "Link is required".to_string(),
                action_id: if action_id == "edit" { "edit" } else { "create" },
                fields: vec![
                    FormField::new("Name", &name),
                    FormField::new("Link", &link),
                    FormField::new("Open with (optional)", &open_with),
                ],
            };
        }
        let name = if name.is_empty() { link.clone() } else { name };

        let slug = match action_id {
            // Reuse the existing slug so renaming preserves launch history.
            "edit" if self.find(&item.payload).is_some() => item.payload.clone(),
            _ => self.free_slug(&name),
        };
        self.save(slug, name, link, open_with);
        ActionResult::Done
    }
}

/// The first `{token}` in a link. `None` when the link takes no argument.
fn placeholder(link: &str) -> Option<&str> {
    let start = link.find('{')?;
    let end = link[start + 1..].find('}')? + start + 1;
    Some(&link[start + 1..end])
}

/// Substitute `value` for the link's placeholder token.
///
/// Percent-encodes only for URLs: a file path argument must keep its spaces
/// and backslashes intact.
fn fill(link: &str, value: &str) -> String {
    let Some(token) = placeholder(link) else {
        return link.to_string();
    };
    let encoded = if link.contains("://") {
        percent_encode(value)
    } else {
        value.to_string()
    };
    link.replace(&format!("{{{token}}}"), &encoded)
}

fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Hand the link to the shell, optionally through a specific program.
fn open(link: &str, open_with: &str) -> bool {
    let link_w = wide(link);
    let prog_w = wide(open_with);
    unsafe {
        let inst = if open_with.is_empty() {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(link_w.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        } else {
            // The chosen program becomes the target and the link its argument.
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(prog_w.as_ptr()),
                PCWSTR(link_w.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };
        let ok = inst.0 as isize > 32;
        if !ok {
            crate::dlog!("quicklinks: failed to open {link}");
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ql(pairs: &[(&str, &[(&str, &str)])]) -> Quicklinks {
        Quicklinks::new(
            pairs
                .iter()
                .map(|(slug, kv)| {
                    (
                        slug.to_string(),
                        kv.iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn placeholder_is_the_first_braced_token() {
        assert_eq!(placeholder("https://x.dev/s?q={query}"), Some("query"));
        assert_eq!(placeholder("https://x.dev/{user}/{repo}"), Some("user"));
        assert_eq!(placeholder("https://x.dev/{}"), Some(""));
        assert_eq!(placeholder("https://x.dev"), None);
        assert_eq!(placeholder("no closing {brace"), None);
    }

    #[test]
    fn fill_encodes_urls_but_not_paths() {
        assert_eq!(
            fill("https://x.dev/s?q={query}", "rust lang&c"),
            "https://x.dev/s?q=rust%20lang%26c"
        );
        assert_eq!(
            fill(r"C:\notes\{name}.md", "my file"),
            r"C:\notes\my file.md"
        );
        assert_eq!(fill("https://x.dev", "ignored"), "https://x.dev");
    }

    #[test]
    fn fill_replaces_every_occurrence_of_the_token() {
        assert_eq!(
            fill("https://x.dev/{q}?also={q}", "a b"),
            "https://x.dev/a%20b?also=a%20b"
        );
    }

    #[test]
    fn entries_without_a_link_are_dropped() {
        let q = ql(&[
            ("good", &[("name", "Good"), ("link", "https://x.dev")]),
            ("bad", &[("name", "Bad")]),
            ("blank", &[("name", "Blank"), ("link", "   ")]),
        ]);
        assert_eq!(q.entries.len(), 1);
        assert_eq!(q.entries[0].slug, "good");
    }

    #[test]
    fn name_falls_back_to_the_slug() {
        let q = ql(&[("my-link", &[("link", "https://x.dev")])]);
        assert_eq!(q.entries[0].name, "my-link");
    }

    #[test]
    fn query_matches_names_and_offers_the_create_row() {
        let mut q = ql(&[("gh", &[("name", "Search GitHub"), ("link", "https://g")])]);
        let mut out = Vec::new();
        q.query("github", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, "gh");

        out.clear();
        q.query("create", &mut out);
        assert!(out.iter().any(|i| i.payload == CREATE_PAYLOAD));

        out.clear();
        q.query("zzzz", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn free_slug_avoids_collisions() {
        let q = ql(&[
            ("my-link", &[("link", "https://a")]),
            ("my-link-2", &[("link", "https://b")]),
        ]);
        assert_eq!(q.free_slug("My Link"), "my-link-3");
        assert_eq!(q.free_slug("Fresh One"), "fresh-one");
        // Nothing survives sanitising, so fall back to a usable stem.
        assert_eq!(q.free_slug("!!!"), "link");
    }

    #[test]
    fn browse_skips_what_history_already_placed() {
        let mut q = ql(&[
            ("a", &[("name", "A"), ("link", "https://a")]),
            ("b", &[("name", "B"), ("link", "https://b")]),
        ]);
        let mut out = vec![q.item_for("a").unwrap()];
        q.browse(8, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].payload, "b");
    }

    #[test]
    fn submitting_without_a_link_reopens_the_form_with_the_input_intact() {
        let mut q = ql(&[]);
        let item = q.create_item(0);
        let fields = [
            FormField::new("Name", "Half Typed"),
            FormField::new("Link", "  "),
            FormField::new("Open with (optional)", "chrome"),
        ];
        match q.submit_form("create", &item, &fields) {
            ActionResult::RequestForm { fields, .. } => {
                assert_eq!(fields[0].value, "Half Typed");
                assert_eq!(fields[2].value, "chrome");
            }
            _ => panic!("expected the form to reopen"),
        }
        assert!(q.entries.is_empty(), "nothing should have been saved");
    }

    #[test]
    fn browse_respects_the_limit() {
        let mut q = ql(&[
            ("a", &[("link", "https://a")]),
            ("b", &[("link", "https://b")]),
            ("c", &[("link", "https://c")]),
        ]);
        let mut out = Vec::new();
        q.browse(2, &mut out);
        assert_eq!(out.len(), 2);
    }
}
