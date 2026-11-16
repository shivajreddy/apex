//! Quicklinks: user-defined links, paths and commands, opened by name.
//!
//! Each one is a sub-table in the config file:
//!
//! ```toml
//! [quicklinks.github]
//! name = 'Search GitHub'
//! link = 'https://github.com/search?q={argument name="query"}'
//! open_with = 'chrome'          # optional
//! ```
//!
//! A placeholder in the link makes it take an argument, typed into a field
//! after the query once the quicklink is selected, the way Raycast does it.
//! Raycast's spelling works - `{argument name="query" default="rust"}`, with
//! its modifiers (`| raw`) - and so does apex's original bare `{query}`. The
//! value is substituted in, percent-encoded when the link is a URL. That
//! single mechanism covers both web search and plain bookmarks.
//!
//! A link to a script runs it, with whatever follows its path as the
//! script's arguments (see [`crate::script`]):
//!
//! ```toml
//! [quicklinks.deploy]
//! name = 'Deploy'
//! link = '~\scripts\deploy.ps1 -Env {argument name="env" default="staging"}'
//! window = 'keep'               # optional: 'hidden' (default), 'keep', 'close'
//! ```
//!
//! A path may start with `~` and use `%NAME%` environment variables, so one
//! config can serve machines whose user folders differ.
//!
//! Quicklinks are created and edited from inside apex, which rewrites only
//! the relevant `[quicklinks.*]` block - the rest of the file, comments
//! included, is untouched. They live in the roaming config rather than in
//! machine-local state because links and paths are portable across machines.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::UI::Shell::{SHSTOCKICONID, SIID_APPLICATION, SIID_FOLDER, SIID_INTERNET};

use crate::config;
use crate::fuzzy;
use crate::icon;
use crate::plugin::{Action, ActionResult, Aliases, Argument, FormField, Icon, Plugin, ResultItem};
use crate::script::{self, Window};

pub const ID: &str = "quicklinks";

/// Label of the synthetic row that opens the create form. Matched fuzzily
/// like any other entry, so "create", "quicklink" and "cql" all find it.
const CREATE_LABEL: &str = "Create Quicklink";

/// Payload of that row. Slugs are sanitised to `a-z 0-9 - _ .`, so a leading
/// `!` can never collide with a real one.
const CREATE_PAYLOAD: &str = "!create";

/// The name shown for an unnamed argument: `{argument}`, or a bare `{}`.
const DEFAULT_ARG: &str = "query";

/// Most arguments a link can take, as in Raycast: each is a field in the
/// search bar, which has room for three beside the query. A placeholder for
/// any further argument is left in the link as written.
const MAX_ARGS: usize = 3;

/// The form's link field. The label is the one place the placeholder
/// syntax is visible from inside apex.
const LINK_LABEL: &str = "Link  \u{b7}  include {argument} to take input";

/// The form's window field, which only a script link uses.
const WINDOW_LABEL: &str = "Script window (optional): hidden, keep or close";

/// Score boost for an exact alias hit; large enough to always rank first
/// (the same figure the application plugin uses).
const ALIAS_BOOST: i32 = 2000;

struct Entry {
    /// Config section suffix, and the payload. Stable across renames so
    /// launch history survives editing the name.
    slug: String,
    name: String,
    link: String,
    open_with: String,
    /// The `window` setting as written, for a script link. Kept verbatim so
    /// the edit form shows what the file says, even if it is not valid.
    window: String,
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
        window: String,
        icon: Option<Arc<Icon>>,
    ) -> Self {
        Self {
            name_folded: fuzzy::fold_case(&name),
            bonus: fuzzy::bonuses(&name),
            slug,
            name,
            link,
            open_with,
            window,
            icon,
        }
    }

    /// Where the script is, when the link runs one. With `open_with` set the
    /// script is opened by that program instead - in an editor, say.
    fn script(&self, template: &Template) -> Option<ScriptLink> {
        if self.open_with.is_empty() {
            template.script()
        } else {
            None
        }
    }

    fn runs_script(&self) -> bool {
        self.script(&Template::parse(&self.link)).is_some()
    }

    /// The window a script runs with. A setting apex cannot read means the
    /// default, not a refusal to run.
    fn window(&self) -> Window {
        Window::parse(&self.window).unwrap_or_else(|| {
            crate::dlog!(
                "quicklinks: {}: unknown window {:?}",
                self.slug,
                self.window
            );
            Window::default()
        })
    }

    fn item(&self, alias: Option<&str>, score: i32) -> ResultItem {
        ResultItem {
            plugin: ID,
            title: self.name.clone(),
            badge: alias.map(str::to_string),
            category: String::new(),
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
    } else if std::path::Path::new(&config::expand_path(link)).is_dir() {
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
    /// The `[aliases]` table, one instance shared with the application
    /// plugin. An entry is ours when its target is one of our slugs (a slug
    /// never looks like an app id); the table keeps one alias per target.
    aliases: Aliases,
}

impl Quicklinks {
    pub fn new(subtables: Vec<(String, HashMap<String, String>)>, aliases: Aliases) -> Self {
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
            let window = kv.get("window").map(|s| s.trim()).unwrap_or_default();
            let art = icons.get(icon_id(link));
            entries.push(Entry::new(
                slug,
                name,
                link.to_string(),
                open_with.to_string(),
                window.to_string(),
                art,
            ));
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            entries,
            create_folded: fuzzy::fold_case(CREATE_LABEL),
            create_bonus: fuzzy::bonuses(CREATE_LABEL),
            icons,
            aliases,
        }
    }

    fn find(&self, slug: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.slug == slug)
    }

    /// The alias pointing at `slug`, if any.
    fn alias_of(&self, slug: &str) -> Option<String> {
        self.aliases.of(slug)
    }

    /// The row for an entry, alias pill included.
    fn make_item(&self, e: &Entry, score: i32) -> ResultItem {
        e.item(self.alias_of(&e.slug).as_deref(), score)
    }

    fn create_item(&mut self, score: i32) -> ResultItem {
        ResultItem {
            plugin: ID,
            title: CREATE_LABEL.to_string(),
            badge: None,
            category: String::new(),
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
        let (name, link, open_with, window) = match entry {
            Some(e) => (
                e.name.as_str(),
                e.link.as_str(),
                e.open_with.as_str(),
                e.window.as_str(),
            ),
            None => ("", "", "", ""),
        };
        ActionResult::RequestForm {
            title: title.to_string(),
            action_id,
            fields: Self::fields(name, link, open_with, window),
        }
    }

    fn fields(name: &str, link: &str, open_with: &str, window: &str) -> Vec<FormField> {
        vec![
            FormField::new("Name", name),
            FormField::new(LINK_LABEL, link),
            FormField::new("Open with (optional)", open_with),
            FormField::new(WINDOW_LABEL, window),
        ]
    }

    /// Open a quicklink with what was typed for its arguments - or, while a
    /// required one is empty, ask for it instead. A link to a script runs
    /// it: in the quicklink's own window unless `window` overrides that, and
    /// as administrator with `admin`.
    fn run(
        &self,
        slug: &str,
        args: &[String],
        window: Option<Window>,
        admin: bool,
    ) -> ActionResult {
        let Some(e) = self.find(slug) else {
            return ActionResult::Done;
        };
        let template = Template::parse(&e.link);
        if template.missing(args) {
            return ActionResult::RequestArguments;
        }
        match e.script(&template) {
            Some(link) => {
                let path = template.render(link.path.clone(), args, false, true);
                let argv = template.argv(link.args, args);
                script::run(&path, &argv, window.unwrap_or_else(|| e.window()), admin);
            }
            None => {
                open(&template.fill(args), &e.open_with);
            }
        }
        ActionResult::Close
    }

    /// Write a quicklink to memory and to the config file.
    fn save(
        &mut self,
        slug: String,
        name: String,
        link: String,
        open_with: String,
        window: String,
    ) {
        config::upsert_quicklink_file(
            &slug,
            &[
                ("name", &name),
                ("link", &link),
                ("open_with", &open_with),
                ("window", &window),
            ],
        );
        let art = self.icons.get(icon_id(&link));
        let entry = Entry::new(slug, name, link, open_with, window, art);
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
        let cfg = config::Config::load();
        // The table is shared: reload it in place so the other plugin sees
        // the re-read too, then rebuild the entries around the same handle.
        self.aliases.reload(cfg.aliases_map());
        *self = Self::new(cfg.subtables(ID), self.aliases.clone());
    }

    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>) {
        let query = fuzzy::fold_case(q);
        let query_str: String = query.iter().collect();
        for e in &self.entries {
            let alias = self.alias_of(&e.slug);
            let alias_hit = alias.as_deref() == Some(query_str.as_str());
            let fuzzy_score = fuzzy::score(&query, &e.name_folded, &e.bonus);
            if fuzzy_score.is_none() && !alias_hit {
                continue;
            }
            let score = fuzzy_score.unwrap_or(0) + if alias_hit { ALIAS_BOOST } else { 0 };
            out.push(e.item(alias.as_deref(), score));
        }
        if let Some(score) = fuzzy::score(&query, &self.create_folded, &self.create_bonus) {
            out.push(self.create_item(score));
        }
    }

    fn item_for(&mut self, payload: &str) -> Option<ResultItem> {
        self.find(payload).map(|e| self.make_item(e, 0))
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
            out.push(self.make_item(e, 0));
            added += 1;
        }
    }

    fn arguments(&self, item: &ResultItem) -> Vec<Argument> {
        self.find(&item.payload)
            .map(|e| Template::parse(&e.link).arguments())
            .unwrap_or_default()
    }

    fn activate(&mut self, item: &ResultItem, args: &[String]) -> ActionResult {
        if item.payload == CREATE_PAYLOAD {
            return Self::form("New Quicklink", None, "create");
        }
        self.run(&item.payload, args, None, false)
    }

    fn actions(&self, item: &ResultItem) -> Vec<Action> {
        if item.payload == CREATE_PAYLOAD {
            return vec![Action {
                id: "create",
                label: CREATE_LABEL.to_string(),
            }];
        }
        // No "Create" here: that belongs to the create row, not to the
        // actions of an existing quicklink.
        let entry = self.find(&item.payload);
        let script = entry.is_some_and(Entry::runs_script);
        let mut actions = vec![Action {
            id: "open",
            label: if script { "Run" } else { "Open" }.to_string(),
        }];
        if script {
            // A script that runs unseen can still be watched, once.
            if entry.is_some_and(|e| e.window() != Window::Keep) {
                actions.push(Action {
                    id: "run_terminal",
                    label: "Run in Terminal".to_string(),
                });
            }
            actions.push(Action {
                id: "run_as_admin",
                label: "Run as Administrator".to_string(),
            });
        }
        actions.push(Action {
            id: "edit",
            label: "Edit Quicklink\u{2026}".to_string(),
        });
        actions.push(Action {
            id: "delete",
            label: "Delete Quicklink".to_string(),
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

    fn run_action(&mut self, action_id: &str, item: &ResultItem, args: &[String]) -> ActionResult {
        match action_id {
            "open" => self.run(&item.payload, args, None, false),
            "run_terminal" => self.run(&item.payload, args, Some(Window::Keep), false),
            "run_as_admin" => self.run(&item.payload, args, None, true),
            "create" => Self::form("New Quicklink", None, "create"),
            "edit" => match self.find(&item.payload) {
                Some(e) => Self::form("Edit Quicklink", Some(e), "edit"),
                None => ActionResult::Done,
            },
            "delete" => {
                config::remove_quicklink_file(&item.payload);
                self.entries.retain(|e| e.slug != item.payload);
                // Its alias would otherwise linger in the config, pointing
                // at nothing.
                if self.alias_of(&item.payload).is_some() {
                    self.aliases.remove(&item.payload);
                    config::remove_alias_file(&item.payload);
                }
                ActionResult::Done
            }
            "set_alias" => ActionResult::RequestText {
                prompt: format!("Alias for {}", item.title),
                action_id: "set_alias",
            },
            "remove_alias" => {
                self.aliases.remove(&item.payload);
                config::remove_alias_file(&item.payload);
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
        let (name, link, open_with, window) = (value(0), value(1), value(2), value(3));
        // A quicklink with no target cannot do anything, and a window
        // setting apex cannot read would be quietly ignored. Reopen the form
        // carrying the entered values back, rather than writing a broken
        // entry or discarding what was typed.
        let problem = if link.is_empty() {
            Some("Link is required")
        } else if Window::parse(&window).is_none() {
            Some("Script window must be hidden, keep or close")
        } else {
            None
        };
        if let Some(title) = problem {
            return ActionResult::RequestForm {
                title: title.to_string(),
                action_id: if action_id == "edit" { "edit" } else { "create" },
                fields: Self::fields(&name, &link, &open_with, &window),
            };
        }
        let name = if name.is_empty() { link.clone() } else { name };
        // Written as apex spells it; the default is not written at all.
        let window = Window::parse(&window)
            .unwrap_or_default()
            .setting()
            .to_string();

        let slug = match action_id {
            // Reuse the existing slug so renaming preserves launch history.
            "edit" if self.find(&item.payload).is_some() => item.payload.clone(),
            _ => self.free_slug(&name),
        };
        self.save(slug, name, link, open_with, window);
        ActionResult::Done
    }
}

/// What a link asks for, and where each answer goes.
struct Template<'a> {
    link: &'a str,
    holes: Vec<Placeholder>,
    /// Per hole, the argument it reads; `None` past [`MAX_ARGS`], where the
    /// placeholder is left as written.
    slots: Vec<Option<usize>>,
    /// Per argument, in order of first appearance: the name it goes by
    /// (`None` for an unnamed `{argument}`) and its default value.
    args: Vec<(Option<String>, Option<String>)>,
}

impl<'a> Template<'a> {
    fn parse(link: &'a str) -> Self {
        let holes = placeholders(link);
        let mut args: Vec<(Option<String>, Option<String>)> = Vec::new();
        let mut slots = Vec::with_capacity(holes.len());
        for hole in &holes {
            // A name seen before is the same argument; an unnamed one is
            // always a new one.
            let known = hole
                .name
                .as_ref()
                .and_then(|n| args.iter().position(|(a, _)| a.as_ref() == Some(n)));
            let slot = match known {
                Some(i) => Some(i),
                None if args.len() < MAX_ARGS => {
                    args.push((hole.name.clone(), None));
                    Some(args.len() - 1)
                }
                None => None,
            };
            // An argument keeps the first default any of its placeholders
            // gives.
            if let Some(i) = slot
                && args[i].1.is_none()
            {
                args[i].1 = hole.default.clone();
            }
            slots.push(slot);
        }
        Self {
            link,
            holes,
            slots,
            args,
        }
    }

    /// The fields to show for this link, in order.
    fn arguments(&self) -> Vec<Argument> {
        self.args
            .iter()
            .map(|(name, default)| Argument {
                name: match name {
                    Some(n) if !n.is_empty() => n.clone(),
                    _ => DEFAULT_ARG.to_string(),
                },
                required: default.is_none(),
            })
            .collect()
    }

    /// Whether `values` leaves a required argument empty.
    fn missing(&self, values: &[String]) -> bool {
        self.args.iter().enumerate().any(|(i, (_, default))| {
            default.is_none() && values.get(i).is_none_or(|v| v.is_empty())
        })
    }

    /// What argument `i` comes to: what was typed, else its default.
    fn value<'v>(&'v self, values: &'v [String], i: usize) -> &'v str {
        match values.get(i) {
            Some(v) if !v.is_empty() => v,
            _ => self.args[i].1.as_deref().unwrap_or(""),
        }
    }

    /// The link with each placeholder replaced by its argument's value, or
    /// by the default when that was left empty.
    ///
    /// Percent-encodes only for URLs, unless a modifier says otherwise: a
    /// file path argument must keep its spaces and backslashes intact. A
    /// link that is a path rather than a URI has `~` and `%NAME%` expanded.
    fn fill(&self, values: &[String]) -> String {
        let all = 0..self.link.len();
        self.render(all, values, self.link.contains("://"), !is_uri(self.link))
    }

    /// `range` of the link with the placeholders in it filled in. `encode`
    /// percent-encodes the values; `expand` expands what the link itself
    /// spells out - a `~` the range starts with, and `%NAME%` variables -
    /// but never a value that was typed.
    fn render(
        &self,
        range: std::ops::Range<usize>,
        values: &[String],
        encode: bool,
        expand: bool,
    ) -> String {
        let mut out = String::with_capacity(range.len());
        let mut copied = range.start;
        for (hole, slot) in self.holes.iter().zip(&self.slots) {
            let Some(i) = *slot else { continue };
            if hole.span.start < range.start || hole.span.end > range.end {
                continue;
            }
            let first = copied == range.start;
            out.push_str(&self.literal(copied..hole.span.start, first, expand));
            out.push_str(&hole.render(self.value(values, i), encode));
            copied = hole.span.end;
        }
        let first = copied == range.start;
        out.push_str(&self.literal(copied..range.end, first, expand));
        out
    }

    /// Text the link spells out itself, expanded when `expand`: `%NAME%`
    /// variables, and a leading `~` when it is the `first` text of its part.
    fn literal(&self, span: std::ops::Range<usize>, first: bool, expand: bool) -> String {
        let text = &self.link[span];
        match (expand, first) {
            (false, _) => text.to_string(),
            (true, true) => config::expand_env(&config::expand_home(text)),
            (true, false) => config::expand_env(text),
        }
    }

    /// Where the script is, if the link runs one: a path ending in a
    /// script's extension (see [`crate::script`]), then its arguments. The
    /// path runs to the first word that ends in one, so it needs no quotes
    /// even with spaces in it; quotes around it work too.
    fn script(&self) -> Option<ScriptLink> {
        let link = self.link;
        if is_uri(link) {
            return None;
        }
        if let Some(rest) = link.strip_prefix('"') {
            let end = 1 + rest.find('"')?;
            return script::is_script(&link[1..end]).then_some(ScriptLink {
                path: 1..end,
                args: end + 1,
            });
        }
        link.char_indices()
            .filter(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .chain([link.len()])
            .find(|&end| script::is_script(&link[..end]))
            .map(|end| ScriptLink {
                path: 0..end,
                args: end,
            })
    }

    /// The link from `from` on, as a script's arguments: split at spaces
    /// the way a command line is, except inside double quotes, which come
    /// off. A placeholder never splits, and neither does the value typed for
    /// it: a value with spaces in it stays one argument. `%NAME%` variables
    /// the link spells out are expanded; typed values never are.
    fn argv(&self, from: usize, values: &[String]) -> Vec<String> {
        let mut args = Vec::new();
        // The argument being read, once one has begun, and link text of it
        // still to be expanded into it.
        let mut arg: Option<String> = None;
        let mut text = String::new();
        let mut quoted = false;
        let mut holes = self
            .holes
            .iter()
            .zip(&self.slots)
            .skip_while(|(hole, _)| hole.span.start < from)
            .peekable();
        let mut i = from;
        while let Some(c) = self.link[i..].chars().next() {
            if let Some((hole, slot)) = holes.next_if(|(hole, _)| hole.span.start == i) {
                let a = arg.get_or_insert_with(String::new);
                a.push_str(&config::expand_env(&std::mem::take(&mut text)));
                match *slot {
                    Some(n) => a.push_str(&hole.render(self.value(values, n), false)),
                    // Past MAX_ARGS: as written.
                    None => a.push_str(&self.link[hole.span.clone()]),
                }
                i = hole.span.end;
                continue;
            }
            i += c.len_utf8();
            if c.is_whitespace() && !quoted {
                if let Some(mut a) = arg.take() {
                    a.push_str(&config::expand_env(&std::mem::take(&mut text)));
                    args.push(a);
                }
                continue;
            }
            // A quote begins an argument too, so `""` passes an empty one.
            if arg.is_none() {
                arg = Some(String::new());
            }
            if c == '"' {
                quoted = !quoted;
            } else {
                text.push(c);
            }
        }
        if let Some(mut a) = arg {
            a.push_str(&config::expand_env(&text));
            args.push(a);
        }
        args
    }
}

/// Where the parts of a link that runs a script are.
struct ScriptLink {
    /// The script's path, without any quotes around it.
    path: std::ops::Range<usize>,
    /// Where its arguments begin.
    args: usize,
}

/// A `{...}` in a link that reads an argument.
#[derive(Debug, PartialEq)]
struct Placeholder {
    /// Where it sits in the link, braces included.
    span: std::ops::Range<usize>,
    /// The argument it reads: placeholders sharing a name share a value. An
    /// unnamed `{argument}` (`None`) is an argument of its own.
    name: Option<String>,
    /// Substituted when the argument is left empty, which makes it optional.
    default: Option<String>,
    modifiers: Vec<Modifier>,
}

impl Placeholder {
    /// Read the text between a pair of braces. Raycast's spelling is the
    /// word `argument` and then `key="value"` attributes; anything else is
    /// apex's original bare form, where the token itself is the name -
    /// except a GUID, which is a shell folder (`shell:::{...}`), not a
    /// question.
    fn parse(inner: &str, span: std::ops::Range<usize>) -> Option<Self> {
        let (head, modifiers) = split_modifiers(inner);
        let head = head.trim();
        let modifiers = modifiers.split('|').filter_map(Modifier::parse).collect();
        let (keyword, attrs) = head.split_once(char::is_whitespace).unwrap_or((head, ""));
        if keyword.eq_ignore_ascii_case("argument") {
            let attrs = attributes(attrs);
            let get = |key: &str| {
                attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v.to_string())
            };
            Some(Self {
                span,
                name: get("name"),
                default: get("default"),
                modifiers,
            })
        } else if is_guid(head) {
            None
        } else {
            Some(Self {
                span,
                name: Some(head.to_string()),
                default: None,
                modifiers,
            })
        }
    }

    /// What goes into the link in place of this placeholder.
    fn render(&self, value: &str, url: bool) -> String {
        let mut out = value.to_string();
        // A URL percent-encodes by default; `raw` opts out, and an explicit
        // `percent-encode` has already done it.
        let mut formatted = false;
        for m in &self.modifiers {
            match m {
                Modifier::Raw => formatted = true,
                Modifier::Encode => {
                    out = percent_encode(&out);
                    formatted = true;
                }
                Modifier::Trim => out = out.trim().to_string(),
                Modifier::Upper => out = out.to_uppercase(),
                Modifier::Lower => out = out.to_lowercase(),
            }
        }
        if url && !formatted {
            percent_encode(&out)
        } else {
            out
        }
    }
}

/// A Raycast modifier, written after a `|` in a placeholder
/// (`{argument | raw}`): a transform applied to the value before it goes
/// into the link.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Modifier {
    /// Substitute the value as typed, even into a URL.
    Raw,
    /// Percent-encode the value, even into a path.
    Encode,
    Trim,
    Upper,
    Lower,
}

impl Modifier {
    /// `None` for a modifier apex does not know, which is then ignored: a
    /// link written for Raycast still opens.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Some(Self::Raw),
            "percent-encode" => Some(Self::Encode),
            "trim" => Some(Self::Trim),
            "uppercase" => Some(Self::Upper),
            "lowercase" => Some(Self::Lower),
            _ => None,
        }
    }
}

/// Every placeholder in `link`, in order. A `{` inside a pair restarts it,
/// so `{{query}}` reads `query` and leaves the outer braces as they are.
fn placeholders(link: &str) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let mut open = None;
    for (i, c) in link.char_indices() {
        match c {
            '{' => open = Some(i),
            '}' => {
                if let Some(start) = open.take()
                    && let Some(p) = Placeholder::parse(&link[start + 1..i], start..i + 1)
                {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
    out
}

/// Split a placeholder at its first `|`: what it reads, and the modifiers.
/// A `|` inside a quoted attribute value does not count; a quote counts as
/// one only straight after `=`, so an apostrophe in a bare name is just a
/// letter.
fn split_modifiers(inner: &str) -> (&str, &str) {
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in inner.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '"' | '\'') && prev == '=' => quote = Some(c),
            None if c == '|' => return (&inner[..i], &inner[i + 1..]),
            None => {}
        }
        if !c.is_whitespace() {
            prev = c;
        }
    }
    (inner, "")
}

/// The `key=value` pairs of a Raycast placeholder, each value in either
/// quote style or bare.
fn attributes(mut s: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    while let Some(eq) = s.find('=') {
        // The key is the word right before the `=`, so stray text ahead of
        // it costs only itself.
        let key = s[..eq].split_whitespace().next_back().unwrap_or("");
        let rest = s[eq + 1..].trim_start();
        let (value, after) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => match rest[1..].find(q) {
                Some(end) => (&rest[1..1 + end], &rest[2 + end..]),
                None => (&rest[1..], ""),
            },
            _ => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        out.push((key, value));
        s = after;
    }
    out
}

/// Whether `s` is a GUID, such as the `20D04FE0-3AEA-1069-A2D8-08002B30309D`
/// in `shell:::{...}` (This PC).
fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Whether a link is a URI - `https://...`, `mailto:...`, `shell:...` -
/// rather than a path, which apex expands. A drive letter is no scheme.
fn is_uri(link: &str) -> bool {
    link.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
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

/// Hand the link to the shell, optionally through a specific program.
///
/// Routes through [`crate::launch::open`] so the link opens unelevated even
/// though apex is elevated - with `open_with` too, which goes by way of the
/// desktop's Explorer.
fn open(link: &str, open_with: &str) -> bool {
    let program = (!open_with.is_empty()).then_some(open_with);
    crate::launch::open(link, program)
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
            Aliases::default(),
        )
    }

    #[test]
    fn an_alias_pins_a_quicklink_first_and_shows_as_its_badge() {
        let mut q = ql(&[
            ("gh", &[("name", "Search GitHub"), ("link", "https://github.com/search?q={query}")]),
            ("docs", &[("name", "Docs"), ("link", "https://docs.rs")]),
        ]);
        q.aliases.set("d", "docs");
        // The alias alone finds it, ranked above anything fuzzy.
        let mut out = Vec::new();
        q.query("d", &mut out);
        assert_eq!(out.first().map(|i| i.payload.as_str()), Some("docs"));
        assert!(out[0].score >= ALIAS_BOOST);
        assert_eq!(out[0].badge.as_deref(), Some("d"));
        // Actions: no "Create" on an existing quicklink; alias actions present.
        let item = q.item_for("docs").unwrap();
        let ids: Vec<&str> = q.actions(&item).iter().map(|a| a.id).collect();
        assert!(!ids.contains(&"create"));
        assert!(ids.contains(&"set_alias"));
        assert!(ids.contains(&"remove_alias"));
        let other = q.item_for("gh").unwrap();
        let ids: Vec<&str> = q.actions(&other).iter().map(|a| a.id).collect();
        assert!(!ids.contains(&"remove_alias"), "no alias, nothing to remove");
    }

    #[test]
    fn the_alias_table_is_one_shared_source_of_truth() {
        // Both plugins get the same handle; a change through one is seen by
        // the other at once, and moving an alias takes it off its old target.
        let shared = Aliases::new(
            [("x".to_string(), "Some.App".to_string())]
                .into_iter()
                .collect(),
        );
        let other_view = shared.clone();
        shared.set("x", "docs");
        assert_eq!(other_view.of("docs").as_deref(), Some("x"));
        assert_eq!(other_view.of("Some.App"), None, "the app lost the alias");
        // A hand-written value in the section header's original casing still
        // resolves: targets compare case-insensitively.
        shared.set("gh", "GitHub");
        assert_eq!(shared.of("github").as_deref(), Some("gh"));
        shared.remove("GITHUB");
        assert_eq!(shared.of("github"), None);
    }

    fn arg(name: &str, required: bool) -> Argument {
        Argument {
            name: name.to_string(),
            required,
        }
    }

    fn args_of(link: &str) -> Vec<Argument> {
        Template::parse(link).arguments()
    }

    fn fill(link: &str, values: &[&str]) -> String {
        let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        Template::parse(link).fill(&values)
    }

    #[test]
    fn both_spellings_of_a_placeholder_take_an_argument() {
        // apex's original bare token, named by its text.
        assert_eq!(
            args_of("https://x.dev/s?q={query}"),
            vec![arg("query", true)]
        );
        // Raycast's, named by an attribute in either quote style, or bare.
        assert_eq!(
            args_of(r#"https://giphy.com/search/{argument name="search"}"#),
            vec![arg("search", true)]
        );
        assert_eq!(
            args_of("https://x.dev/{argument name='a b'}"),
            vec![arg("a b", true)]
        );
        assert_eq!(
            args_of("https://x.dev/{argument name=q}"),
            vec![arg("q", true)]
        );
        // Unnamed: Raycast's `{argument}`, or apex's empty `{}`.
        assert_eq!(
            args_of("https://x.dev/{argument}"),
            vec![arg(DEFAULT_ARG, true)]
        );
        assert_eq!(args_of("https://x.dev/{}"), vec![arg(DEFAULT_ARG, true)]);
        // No placeholder, no argument; an unclosed brace is just a brace.
        assert!(args_of("https://x.dev").is_empty());
        assert!(args_of("no closing {brace").is_empty());
    }

    #[test]
    fn a_shared_name_is_one_argument_and_three_is_the_most() {
        // One name, one field, however often it appears and in either form.
        assert_eq!(
            args_of(r#"https://x.dev/{q}?also={argument name="q"}"#),
            vec![arg("q", true)]
        );
        // Distinct names in order of first appearance.
        assert_eq!(
            args_of("https://github.com/{user}/{repo}"),
            vec![arg("user", true), arg("repo", true)]
        );
        // Each unnamed `{argument}` is a field of its own.
        assert_eq!(args_of("https://x.dev/{argument}/{argument}").len(), 2);
        // Past three, a placeholder is left as written.
        let link = "https://x.dev/{a}/{b}/{c}/{d}";
        assert_eq!(args_of(link).len(), MAX_ARGS);
        assert_eq!(fill(link, &["1", "2", "3"]), "https://x.dev/1/2/3/{d}");
    }

    #[test]
    fn a_default_makes_an_argument_optional() {
        let link = r#"https://translate.google.com/?sl={argument name="from" default="auto"}&tl={argument name="to"}"#;
        assert_eq!(args_of(link), vec![arg("from", false), arg("to", true)]);
        // Left empty, it fills in; typed, the typed value wins.
        assert_eq!(
            fill(link, &["", "de"]),
            "https://translate.google.com/?sl=auto&tl=de"
        );
        assert_eq!(
            fill(link, &["fr", "de"]),
            "https://translate.google.com/?sl=fr&tl=de"
        );
        // Only an argument with no default stops the link opening.
        let t = Template::parse(link);
        assert!(t.missing(&["fr".into(), String::new()]));
        assert!(!t.missing(&[String::new(), "de".into()]));
        assert_eq!(
            args_of(r#"https://x.dev/{argument default="x"}"#),
            vec![arg(DEFAULT_ARG, false)]
        );
    }

    #[test]
    fn fill_encodes_urls_but_not_paths() {
        assert_eq!(
            fill("https://x.dev/s?q={query}", &["rust lang&c"]),
            "https://x.dev/s?q=rust%20lang%26c"
        );
        assert_eq!(
            fill(r"C:\notes\{name}.md", &["my file"]),
            r"C:\notes\my file.md"
        );
        assert_eq!(fill("https://x.dev", &["ignored"]), "https://x.dev");
    }

    #[test]
    fn fill_replaces_every_occurrence_of_the_token() {
        assert_eq!(
            fill("https://x.dev/{q}?also={q}", &["a b"]),
            "https://x.dev/a%20b?also=a%20b"
        );
    }

    #[test]
    fn modifiers_transform_the_value_before_it_goes_in() {
        // The default encodes a slash; `raw` keeps it, for a path in a URL.
        assert_eq!(
            fill("https://github.com/{repo}", &["rust-lang/rust"]),
            "https://github.com/rust-lang%2Frust"
        );
        assert_eq!(
            fill(
                r#"https://github.com/{argument name="repo" | raw}"#,
                &["rust-lang/rust"]
            ),
            "https://github.com/rust-lang/rust"
        );
        // `percent-encode` encodes even where the link is not a URL.
        assert_eq!(
            fill(
                "mailto:me@x.dev?subject={s | percent-encode}",
                &["hi there"]
            ),
            "mailto:me@x.dev?subject=hi%20there"
        );
        // They chain in order, and the URL's own encoding comes last.
        assert_eq!(
            fill("https://x.dev/{q | trim | uppercase}", &["  ab c "]),
            "https://x.dev/AB%20C"
        );
        assert_eq!(
            fill("https://x.dev/{q|lowercase}", &["ABC"]),
            "https://x.dev/abc"
        );
        // An unknown modifier is ignored, and does not muddle the name.
        assert_eq!(
            args_of(r#"https://x.dev/{argument name="q" | json-stringify}"#),
            vec![arg("q", true)]
        );
        // A `|` inside a quoted value is part of the value.
        assert_eq!(
            args_of(r#"https://x.dev/{argument name="a|b"}"#),
            vec![arg("a|b", true)]
        );
    }

    #[test]
    fn a_guid_is_a_shell_folder_not_an_argument() {
        let this_pc = "shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
        assert!(args_of(this_pc).is_empty());
        assert_eq!(fill(this_pc, &[]), this_pc);
        // In doubled braces, the inner pair is the placeholder.
        assert_eq!(fill("https://x.dev/{{q}}", &["v"]), "https://x.dev/{v}");
    }

    #[test]
    fn opening_with_a_required_argument_empty_asks_for_it_instead() {
        let mut q = ql(&[(
            "gify",
            &[
                ("name", "Gify"),
                (
                    "link",
                    r#"https://giphy.com/search/{argument name="search"}"#,
                ),
            ],
        )]);
        let item = q.item_for("gify").unwrap();
        assert_eq!(q.arguments(&item), vec![arg("search", true)]);
        // Nothing typed: not opened, from Enter and the actions panel alike.
        assert!(matches!(
            q.activate(&item, &[String::new()]),
            ActionResult::RequestArguments
        ));
        assert!(matches!(
            q.run_action("open", &item, &[]),
            ActionResult::RequestArguments
        ));
        // The create row takes none.
        let create = q.create_item(0);
        assert!(q.arguments(&create).is_empty());
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

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn home() -> String {
        std::env::var("USERPROFILE").unwrap()
    }

    /// A script link's path and arguments, as running it would pass them -
    /// without running it.
    fn script_of(link: &str, values: &[&str]) -> Option<(String, Vec<String>)> {
        let values = strings(values);
        let t = Template::parse(link);
        let s = t.script()?;
        Some((
            t.render(s.path.clone(), &values, false, true),
            t.argv(s.args, &values),
        ))
    }

    #[test]
    fn a_link_to_a_script_splits_into_its_path_and_arguments() {
        // The path runs to its extension, spaces and all.
        assert_eq!(
            script_of(r"C:\My Scripts\deploy.ps1 -Env prod", &[]),
            Some((
                r"C:\My Scripts\deploy.ps1".to_string(),
                strings(&["-Env", "prod"])
            ))
        );
        // Quotes around it work too.
        assert_eq!(
            script_of(r#""C:\My Scripts\x.bat" one"#, &[]),
            Some((r"C:\My Scripts\x.bat".to_string(), strings(&["one"])))
        );
        assert_eq!(
            script_of(r"D:\sync.sh", &[]),
            Some((r"D:\sync.sh".to_string(), Vec::new()))
        );
        // A URL to a script is a download, and a program is no script.
        assert_eq!(script_of("https://x.dev/install.sh", &[]), None);
        assert_eq!(script_of(r"C:\tools\app.exe --flag", &[]), None);
        assert_eq!(script_of(r"C:\notes", &[]), None);
    }

    #[test]
    fn script_arguments_split_like_a_command_line_but_values_never_do() {
        let link = r#"C:\s\x.ps1 -Msg {argument name="msg"} --tag "two words" "" -Who "Hi {argument name="who"}!""#;
        let (_, args) = script_of(link, &["hello world", "Bo"]).unwrap();
        assert_eq!(
            args,
            strings(&[
                "-Msg",
                "hello world",
                "--tag",
                "two words",
                "",
                "-Who",
                "Hi Bo!"
            ])
        );
        // Modifiers apply; a script argument is never percent-encoded
        // unless asked.
        let (_, args) =
            script_of("C:\\s\\x.sh {q | uppercase} {q | percent-encode}", &["a b"]).unwrap();
        assert_eq!(args, strings(&["A B", "a%20b"]));
    }

    #[test]
    fn a_path_may_start_with_home_and_name_environment_variables() {
        let root = std::env::var("SystemRoot").unwrap();
        assert_eq!(
            script_of(r"~\dotfiles\scripts\flush-dns.ps1", &[])
                .unwrap()
                .0,
            format!(r"{}\dotfiles\scripts\flush-dns.ps1", home())
        );
        // In the link's own text, the path and the arguments alike; never
        // in a typed value.
        assert_eq!(
            script_of(
                r#"%SystemRoot%\x.cmd %SystemRoot%\log {argument name="dir"}"#,
                &["%SystemRoot%"]
            ),
            Some((
                format!(r"{root}\x.cmd"),
                vec![format!(r"{root}\log"), "%SystemRoot%".to_string()]
            ))
        );
        // Plain path links expand too; URIs never do.
        assert_eq!(fill(r"~\Downloads", &[]), format!(r"{}\Downloads", home()));
        assert_eq!(fill("~", &[]), home());
        assert_eq!(fill(r"~user\x", &[]), r"~user\x");
        assert_eq!(
            fill("https://x.dev/~me/%SystemRoot%", &[]),
            "https://x.dev/~me/%SystemRoot%"
        );
        assert_eq!(
            fill("mailto:a@b.c?subject=%SystemRoot%", &[]),
            "mailto:a@b.c?subject=%SystemRoot%"
        );
        assert_eq!(
            fill(r"C:\notes\{name}.md", &["%SystemRoot%"]),
            r"C:\notes\%SystemRoot%.md"
        );
    }

    #[test]
    fn uris_are_told_from_paths() {
        for uri in [
            "https://x.dev",
            "mailto:a@b.c",
            "shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            "ms-settings:display",
        ] {
            assert!(is_uri(uri), "{uri}");
        }
        for path in [
            r"C:\tools\x.ps1",
            r"~\x.sh",
            r"%USERPROFILE%\x",
            r"\\server\share\x.bat",
            "notes",
        ] {
            assert!(!is_uri(path), "{path}");
        }
    }

    #[test]
    fn a_script_quicklink_runs_and_offers_a_terminal_and_admin() {
        let mut q = ql(&[
            (
                "dns",
                &[("name", "Flush DNS"), ("link", r"~\s\flush-dns.ps1")],
            ),
            (
                "status",
                &[
                    ("name", "Status"),
                    ("link", r"C:\s\status.ps1"),
                    ("window", "keep"),
                ],
            ),
            (
                "editor",
                &[
                    ("name", "Edit"),
                    ("link", r"C:\s\x.ps1"),
                    ("open_with", "notepad"),
                ],
            ),
            (
                "web",
                &[("name", "Web"), ("link", "https://x.dev/install.sh")],
            ),
            (
                "odd",
                &[
                    ("name", "Odd"),
                    ("link", r"C:\s\odd.sh"),
                    ("window", "sometimes"),
                ],
            ),
        ]);
        let mut actions = |slug: &str| -> Vec<(&'static str, String)> {
            let item = q.item_for(slug).unwrap();
            q.actions(&item)
                .into_iter()
                .map(|a| (a.id, a.label))
                .collect()
        };
        let has = |list: &[(&str, String)], id: &str| list.iter().any(|(i, _)| *i == id);

        let dns = actions("dns");
        assert_eq!(dns[0], ("open", "Run".to_string()));
        assert!(has(&dns, "run_terminal") && has(&dns, "run_as_admin"));
        // Already in a terminal: no point offering one.
        let status = actions("status");
        assert!(!has(&status, "run_terminal") && has(&status, "run_as_admin"));
        // With open_with the script is opened, not run; a URL is a URL.
        for slug in ["editor", "web"] {
            let list = actions(slug);
            assert_eq!(list[0], ("open", "Open".to_string()), "{slug}");
            assert!(
                !has(&list, "run_terminal") && !has(&list, "run_as_admin"),
                "{slug}"
            );
        }

        assert_eq!(q.find("status").unwrap().window(), Window::Keep);
        assert_eq!(q.find("dns").unwrap().window(), Window::Hidden);
        // A setting apex cannot read runs with the default, not at all.
        assert_eq!(q.find("odd").unwrap().window(), Window::Hidden);
    }

    #[test]
    fn a_script_waits_for_its_required_arguments_from_every_action() {
        let mut q = ql(&[(
            "deploy",
            &[("link", r#"C:\s\deploy.ps1 -Env {argument name="env"}"#)],
        )]);
        let item = q.item_for("deploy").unwrap();
        assert_eq!(q.arguments(&item), vec![arg("env", true)]);
        assert!(matches!(
            q.activate(&item, &[String::new()]),
            ActionResult::RequestArguments
        ));
        for action in ["run_terminal", "run_as_admin"] {
            assert!(matches!(
                q.run_action(action, &item, &[]),
                ActionResult::RequestArguments
            ));
        }
    }

    #[test]
    fn the_form_carries_the_window_and_refuses_one_it_cannot_read() {
        let mut q = ql(&[(
            "s",
            &[("name", "S"), ("link", r"C:\s\x.ps1"), ("window", "close")],
        )]);
        let item = q.item_for("s").unwrap();
        match q.run_action("edit", &item, &[]) {
            ActionResult::RequestForm { fields, .. } => {
                assert_eq!(fields.len(), 4);
                assert_eq!(fields[3].value, "close");
            }
            _ => panic!("expected the edit form"),
        }
        // Reopens with what was typed, and saves nothing.
        let typed = Quicklinks::fields("S", r"C:\s\x.ps1", "", "sometimes");
        match q.submit_form("edit", &item, &typed) {
            ActionResult::RequestForm { title, fields, .. } => {
                assert!(title.contains("hidden, keep or close"), "{title}");
                assert_eq!(fields[1].value, r"C:\s\x.ps1");
                assert_eq!(fields[3].value, "sometimes");
            }
            _ => panic!("expected the form to reopen"),
        }
        assert_eq!(q.find("s").unwrap().window, "close");
    }
}
