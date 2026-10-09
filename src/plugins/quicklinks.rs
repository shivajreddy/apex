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
        let (name, link, open_with) = match entry {
            Some(e) => (e.name.as_str(), e.link.as_str(), e.open_with.as_str()),
            None => ("", "", ""),
        };
        ActionResult::RequestForm {
            title: title.to_string(),
            action_id,
            fields: vec![
                FormField::new("Name", name),
                FormField::new(LINK_LABEL, link),
                FormField::new("Open with (optional)", open_with),
            ],
        }
    }

    /// Open a quicklink with what was typed for its arguments - or, while a
    /// required one is empty, ask for it instead.
    fn run(&self, slug: &str, args: &[String]) -> ActionResult {
        let Some(e) = self.find(slug) else {
            return ActionResult::Done;
        };
        let template = Template::parse(&e.link);
        if template.missing(args) {
            return ActionResult::RequestArguments;
        }
        open(&template.fill(args), &e.open_with);
        ActionResult::Close
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
        self.run(&item.payload, args)
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
        let mut actions = vec![
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
        ];
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
            "open" => self.run(&item.payload, args),
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
                    FormField::new(LINK_LABEL, &link),
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

    /// The link with each placeholder replaced by its argument's value, or
    /// by the default when that was left empty.
    ///
    /// Percent-encodes only for URLs, unless a modifier says otherwise: a
    /// file path argument must keep its spaces and backslashes intact.
    fn fill(&self, values: &[String]) -> String {
        let url = self.link.contains("://");
        let mut out = String::with_capacity(self.link.len());
        let mut copied = 0;
        for (hole, slot) in self.holes.iter().zip(&self.slots) {
            let Some(i) = *slot else { continue };
            let value = match values.get(i) {
                Some(v) if !v.is_empty() => v.as_str(),
                _ => self.args[i].1.as_deref().unwrap_or(""),
            };
            out.push_str(&self.link[copied..hole.span.start]);
            out.push_str(&hole.render(value, url));
            copied = hole.span.end;
        }
        out.push_str(&self.link[copied..]);
        out
    }
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
/// Routes through [`crate::launch::open`] so a plain link opens unelevated
/// (via Explorer) even though apex is elevated. A link with `open_with` names
/// a program, which Explorer cannot forward, so that case launches directly
/// and inherits elevation - the one documented gap.
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
}
