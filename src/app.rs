//! Application state: query text, caret, plugins, current results, and the
//! UI mode (search / actions panel / one-line text input).

use crate::editor::{Edit, FormEntry, TextField};
use crate::frecency::{self, Frecency};
use crate::plugin::{Action, ActionResult, FormField, Plugin, ResultItem, ShellCommand};
use crate::render::Renderer;

/// Hard cap on materialised rows. Generous: the default list holds every
/// installed app, and the list scrolls. Plugins already build a `ResultItem`
/// per match before this is applied, so raising it costs nothing per
/// keystroke.
pub const MAX_RESULTS: usize = 400;

/// Most-used entries offered under "Suggestions" before the full list.
const MAX_SUGGESTIONS: usize = 5;

const SUGGESTIONS: &str = "Suggestions";
const COMMANDS: &str = "Commands";

/// Config section holding hidden `plugin/payload` keys.
const HIDDEN: &str = "hidden";

// Shell-level action ids. Double underscores keep them clear of the plain
// identifiers plugins use.
const ACTION_HIDE: &str = "__hide";
const ACTION_UNHIDE: &str = "__unhide";
const ACTION_REMOVE_SOURCE: &str = "__remove_source";

/// Owner of rows the shell builds itself, which no plugin can activate.
const SHELL: &str = "__shell";

/// Identity of a result across plugins.
///
/// `/` is a safe separator: plugin ids are bare words, and payloads are
/// Windows paths, AppUserModelIDs, quicklink slugs or command ids - none of
/// which contain a forward slash.
fn key(plugin: &str, payload: &str) -> String {
    format!("{plugin}/{payload}")
}

/// Which catalogue the result list is showing.
#[derive(PartialEq, Clone, Copy)]
pub enum Listing {
    Normal,
    /// Hidden entries only, so they can be restored.
    Hidden,
    /// Configured source folders, so they can be removed.
    Sources,
}

pub enum Mode {
    Search,
    Actions {
        actions: Vec<Action>,
        selected: usize,
    },
    TextInput {
        prompt: String,
        action_id: &'static str,
        buffer: TextField,
    },
    Form {
        title: String,
        action_id: &'static str,
        fields: Vec<FormEntry>,
        focused: usize,
    },
}

/// What the window should do after a mode-level operation.
#[derive(PartialEq)]
pub enum UiOutcome {
    Stay,
    Hide,
    /// Something only the window can do: quit, restart, toggle the tray.
    Shell(ShellCommand),
}

pub struct App {
    pub mode: Mode,
    /// Present only while the window is visible; dropped on hide so the
    /// process returns to baseline memory. Recreated lazily (~1ms) on show.
    pub renderer: Option<Renderer>,
    /// Whether the desktop compositor is blurring what is behind the
    /// window, in which case the background is painted as a tint rather
    /// than solid.
    pub translucent: bool,
    /// Play the short scale-and-fade when summoned.
    pub animate: bool,
    /// `[appearance] theme` pinned to dark (`Some(true)`) or light; `None`
    /// follows Windows' setting for apps, re-read on every show.
    pub forced_dark: Option<bool>,
    /// Palette in use for the current show.
    pub dark: bool,
    /// `[appearance] opacity` override for the tint strength over the blur,
    /// if the user set one; otherwise the palette's own value is used.
    pub tint: Option<f32>,
    /// The frosted background for the current summon: a blurred snapshot of
    /// what was behind the window, captured by the shell just before showing.
    /// Rebuilt every summon; `None` paints solid.
    pub backdrop: Option<crate::render::Backdrop>,
    /// When the current summon animation started, until it has finished.
    pub summon: Option<std::time::Instant>,
    /// DPI of the monitor the window was summoned on. Everything is laid
    /// out in DIPs and converted with this at present time.
    pub dpi: f32,
    pub query: TextField,
    pub caret_visible: bool,
    pub plugins: Vec<Box<dyn Plugin>>,
    pub results: Vec<ResultItem>,
    pub selected: usize,
    /// `(first row index, heading)`. Only the default list is sectioned; a
    /// typed query is one ranked list, so headings would be arbitrary.
    pub sections: Vec<(usize, &'static str)>,
    /// List scroll offset in DIPs.
    pub scroll: f32,
    /// `plugin/payload` keys the user has hidden. Filtered out of every
    /// listing, here rather than per-plugin so one action covers them all.
    hidden: std::collections::HashSet<String>,
    listing: Listing,
    folder_icon: Option<std::sync::Arc<crate::plugin::Icon>>,
    /// Launch history, applied across all plugins. Lives here rather than in
    /// any one plugin: ranking by past use is a property of the shell, so
    /// every plugin gets it without reimplementing it.
    frecency: Frecency,
}

impl App {
    pub fn new(plugins: Vec<Box<dyn Plugin>>, config: &crate::config::Config) -> Self {
        Self {
            mode: Mode::Search,
            renderer: None,
            translucent: false,
            // Off by default: an instant, snappy show reads as faster than
            // any open animation, however short. Opt in with the config key.
            animate: config.flag("appearance", "animation", false),
            forced_dark: match config.string("appearance", "theme") {
                Some(t) if t.eq_ignore_ascii_case("dark") => Some(true),
                Some(t) if t.eq_ignore_ascii_case("light") => Some(false),
                _ => None,
            },
            dark: true,
            tint: config.float("appearance", "opacity"),
            backdrop: None,
            summon: None,
            dpi: 96.0,
            query: TextField::default(),
            caret_visible: true,
            plugins,
            results: Vec::new(),
            selected: 0,
            sections: Vec::new(),
            scroll: 0.0,
            hidden: config.list_values(HIDDEN).into_iter().collect(),
            listing: Listing::Normal,
            folder_icon: None,
            frecency: Frecency::load(),
        }
    }

    /// Rebuild the most-used list just before the window appears.
    ///
    /// Not done in [`App::new`]: at startup the app index is still loading on
    /// its background thread, so a list built then would be empty and would
    /// stay empty until the first hide.
    pub fn refresh_on_show(&mut self) {
        if self.query.is_empty() {
            self.refresh_results();
        }
    }

    /// Get the renderer, creating it if needed (first show or after hide).
    pub fn ensure_renderer(&mut self) -> Option<&mut Renderer> {
        if self.renderer.is_none() {
            match Renderer::new(self.translucent, self.dark, self.tint) {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    crate::dlog!("renderer creation failed: {e}");
                    return None;
                }
            }
        }
        self.renderer.as_mut()
    }

    // ---- text editing --------------------------------------------------

    /// The field keyboard input currently goes to: the query in search mode,
    /// the prompt's buffer, or the focused form field. None while the
    /// actions panel is up.
    fn focused_field(&mut self) -> Option<&mut TextField> {
        match &mut self.mode {
            Mode::Search => Some(&mut self.query),
            Mode::TextInput { buffer, .. } => Some(buffer),
            Mode::Form {
                fields, focused, ..
            } => fields.get_mut(*focused).map(|f| &mut f.value),
            Mode::Actions { .. } => None,
        }
    }

    /// Apply an edit to the focused field. Returns whether its text changed;
    /// a changed query is re-run against the plugins here, so callers only
    /// need to repaint (and refit the window).
    pub fn edit(&mut self, edit: Edit) -> bool {
        let Some(field) = self.focused_field() else {
            return false;
        };
        let changed = field.apply(edit);
        if changed && matches!(self.mode, Mode::Search) {
            self.refresh_results();
        }
        changed
    }

    /// Selected text of the focused field, for Ctrl+C.
    pub fn selected_text(&mut self) -> Option<String> {
        self.focused_field()?.selected_text().map(str::to_string)
    }

    /// Remove and return the focused field's selection, for Ctrl+X.
    pub fn cut(&mut self) -> Option<String> {
        let text = self.focused_field()?.cut()?;
        if matches!(self.mode, Mode::Search) {
            self.refresh_results();
        }
        Some(text)
    }

    /// Put the query caret at a byte offset, e.g. under a mouse click.
    pub fn place_caret(&mut self, pos: usize, select: bool) {
        self.query.set_caret(pos, select);
    }

    pub fn clear_query(&mut self) {
        self.mode = Mode::Search;
        self.query.clear();
        self.refresh_results();
    }

    // ---- actions panel -------------------------------------------------

    /// Open the actions panel for the selected result.
    pub fn open_actions(&mut self) -> bool {
        let Some(item) = self.results.get(self.selected) else {
            return false;
        };
        // Rows the shell builds itself - source folders - have no owning
        // plugin. They still get the shell-level action appended below, so a
        // missing plugin means "no plugin actions", not "no panel".
        let mut actions = self
            .plugins
            .iter()
            .find(|p| p.id() == item.plugin)
            .map(|p| p.actions(item))
            .unwrap_or_default();
        // Shell-level, so it is offered on every row whatever produced it,
        // rather than each plugin reimplementing the same entry.
        actions.push(match self.listing {
            Listing::Hidden => Action {
                id: ACTION_UNHIDE,
                label: "Unhide".to_string(),
            },
            Listing::Sources => Action {
                id: ACTION_REMOVE_SOURCE,
                label: "Remove Source Folder".to_string(),
            },
            Listing::Normal => Action {
                id: ACTION_HIDE,
                label: "Hide from Apex".to_string(),
            },
        });
        self.mode = Mode::Actions {
            actions,
            selected: 0,
        };
        true
    }

    pub fn close_panel(&mut self) {
        self.mode = Mode::Search;
    }

    pub fn panel_move(&mut self, delta: i32) {
        if let Mode::Actions { actions, selected } = &mut self.mode {
            let len = actions.len() as i32;
            *selected = ((*selected as i32 + delta).rem_euclid(len)) as usize;
        }
    }

    /// Run the highlighted panel action.
    pub fn run_panel_action(&mut self) -> UiOutcome {
        let action_id = match &self.mode {
            Mode::Actions { actions, selected } => actions[*selected].id,
            _ => return UiOutcome::Stay,
        };
        // Shell-level actions never reach a plugin.
        match action_id {
            ACTION_HIDE => {
                self.set_hidden(true);
                return UiOutcome::Stay;
            }
            ACTION_UNHIDE => {
                self.set_hidden(false);
                return UiOutcome::Stay;
            }
            ACTION_REMOVE_SOURCE => {
                self.remove_source();
                return UiOutcome::Stay;
            }
            _ => {}
        }
        let result = self.dispatch(|p, item| p.run_action(action_id, item));
        self.apply(result)
    }

    /// Commit the text-input buffer to its plugin action.
    pub fn submit_text_input(&mut self) -> UiOutcome {
        let (action_id, text) = match &self.mode {
            Mode::TextInput {
                action_id, buffer, ..
            } => (*action_id, buffer.text().trim().to_string()),
            _ => return UiOutcome::Stay,
        };
        let result = self.dispatch(|p, item| p.submit_text(action_id, item, &text));
        self.apply(result)
    }

    fn dispatch(
        &mut self,
        f: impl FnOnce(&mut Box<dyn Plugin>, &ResultItem) -> ActionResult,
    ) -> ActionResult {
        let Some(item) = self.results.get(self.selected) else {
            return ActionResult::Done;
        };
        for p in &mut self.plugins {
            if p.id() == item.plugin {
                return f(p, item);
            }
        }
        ActionResult::Done
    }

    fn apply(&mut self, result: ActionResult) -> UiOutcome {
        match result {
            ActionResult::Done => {
                self.mode = Mode::Search;
                // Re-query so alias badges/ordering reflect the change.
                self.requery();
                UiOutcome::Stay
            }
            ActionResult::Close => {
                // The one place launches are recorded, so Enter and every
                // Ctrl+K action that opens something stay consistent.
                self.mode = Mode::Search;
                self.record_launch();
                UiOutcome::Hide
            }
            ActionResult::Dismiss => {
                self.mode = Mode::Search;
                UiOutcome::Hide
            }
            ActionResult::Shell(cmd) => {
                self.mode = Mode::Search;
                // Launch history lives here, so clearing it never needs to
                // reach the window.
                // Both are answered here: App owns the history and the
                // hidden set, so neither needs the window.
                if cmd == ShellCommand::ClearHistory {
                    self.clear_history();
                    return UiOutcome::Stay;
                }
                if cmd == ShellCommand::ShowHidden {
                    self.show_hidden();
                    return UiOutcome::Stay;
                }
                if cmd == ShellCommand::ShowSources {
                    self.show_sources();
                    return UiOutcome::Stay;
                }
                UiOutcome::Shell(cmd)
            }
            ActionResult::Refresh => {
                self.mode = Mode::Search;
                for p in &mut self.plugins {
                    p.refresh();
                }
                // Hand-edits to [hidden] count as config too.
                self.hidden = crate::config::Config::load()
                    .list_values(HIDDEN)
                    .into_iter()
                    .collect();
                self.listing = Listing::Normal;
                // Clear back to the default list: the reload is otherwise
                // invisible, since the query still matches the command row.
                self.query.clear();
                self.refresh_results();
                UiOutcome::Stay
            }
            ActionResult::RequestText { prompt, action_id } => {
                self.mode = Mode::TextInput {
                    prompt,
                    action_id,
                    buffer: TextField::default(),
                };
                UiOutcome::Stay
            }
            ActionResult::RequestForm {
                title,
                action_id,
                fields,
            } => {
                self.mode = Mode::Form {
                    title,
                    action_id,
                    fields: fields
                        .into_iter()
                        .map(|f| FormEntry {
                            value: TextField::new(&f.value),
                            label: f.label,
                        })
                        .collect(),
                    focused: 0,
                };
                UiOutcome::Stay
            }
        }
    }

    // ---- form mode -----------------------------------------------------

    /// Move focus between form fields, wrapping around.
    pub fn form_move(&mut self, delta: i32) {
        if let Mode::Form {
            fields, focused, ..
        } = &mut self.mode
        {
            if fields.is_empty() {
                return;
            }
            let len = fields.len() as i32;
            *focused = ((*focused as i32 + delta).rem_euclid(len)) as usize;
        }
    }

    /// Commit the form to its plugin action.
    pub fn submit_form(&mut self) -> UiOutcome {
        let (action_id, fields) = match &self.mode {
            Mode::Form {
                action_id, fields, ..
            } => (
                *action_id,
                fields
                    .iter()
                    .map(|f| FormField::new(&f.label, f.value.text().trim()))
                    .collect::<Vec<_>>(),
            ),
            _ => return UiOutcome::Stay,
        };
        let result = self.dispatch(|p, item| p.submit_form(action_id, item, &fields));
        self.apply(result)
    }

    /// Re-run the current query, keeping the selection where possible.
    fn requery(&mut self) {
        let keep = self.selected;
        self.refresh_results();
        self.selected = keep.min(self.results.len().saturating_sub(1));
    }

    /// Move selection by `delta`, wrapping around, scrolling to follow.
    pub fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.results.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
        self.ensure_visible();
    }

    fn refresh_results(&mut self) {
        self.results.clear();
        self.sections.clear();
        self.selected = 0;
        self.scroll = 0.0;
        let now = frecency::now();

        // Typing leaves the hidden-entry view: it is a list to review, not
        // something to search within.
        if !self.query.is_empty() {
            self.listing = Listing::Normal;
        }
        match self.listing {
            Listing::Hidden => {
                self.fill_hidden();
                return;
            }
            Listing::Sources => {
                self.fill_sources();
                return;
            }
            Listing::Normal => {}
        }

        // Empty query: show the most-used entries instead of nothing. These
        // arrive already ranked, so they bypass the scoring sort below.
        if self.query.is_empty() {
            self.fill_default(now);
            return;
        }

        for p in &mut self.plugins {
            p.query(self.query.text(), &mut self.results);
        }
        self.drop_hidden();
        for item in &mut self.results {
            item.score += self.frecency.bonus(item.plugin, &item.payload, now);
        }
        crate::dlog!(
            "refresh: query='{}' -> {} results",
            self.query.text(),
            self.results.len()
        );
        self.results
            .sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        self.results.truncate(MAX_RESULTS);
    }

    /// Build the default list for an empty query: "Suggestions" from launch
    /// history, then "Commands" holding everything else.
    ///
    /// Suggestions is omitted entirely on a fresh install, or after the
    /// history is cleared - an empty heading is worse than no heading.
    /// Payloads whose plugin is gone, or whose target no longer exists, are
    /// skipped.
    fn fill_default(&mut self, now: u64) {
        // Take more than needed and stop at the cap: hidden entries are
        // skipped, and they should not eat suggestion slots.
        for (plugin_id, payload) in self.frecency.top(MAX_RESULTS, now) {
            if self.results.len() == MAX_SUGGESTIONS {
                break;
            }
            if self.hidden.contains(&key(plugin_id, payload)) {
                continue;
            }
            let Some(p) = self.plugins.iter_mut().find(|p| p.id() == plugin_id) else {
                continue;
            };
            if let Some(item) = p.item_for(payload) {
                self.results.push(item);
            }
        }
        if !self.results.is_empty() {
            self.sections.push((0, SUGGESTIONS));
        }

        // Everything else, in each plugin's own order. `browse` skips
        // payloads already placed above, so nothing appears twice.
        let commands_start = self.results.len();
        for p in &mut self.plugins {
            let free = MAX_RESULTS.saturating_sub(self.results.len());
            if free == 0 {
                break;
            }
            p.browse(free, &mut self.results);
        }
        // Safe to filter after recording the start: suggestions were already
        // filtered, so nothing before `commands_start` can be removed.
        self.drop_hidden();
        if self.results.len() > commands_start {
            self.sections.push((commands_start, COMMANDS));
        }
        self.results.truncate(MAX_RESULTS);
    }

    fn drop_hidden(&mut self) {
        if self.hidden.is_empty() {
            return;
        }
        let hidden = &self.hidden;
        self.results
            .retain(|i| !hidden.contains(&key(i.plugin, &i.payload)));
    }

    /// List the hidden entries so they can be restored.
    fn fill_hidden(&mut self) {
        let mut keys: Vec<String> = self.hidden.iter().cloned().collect();
        keys.sort();
        for k in keys {
            let Some((plugin_id, payload)) = k.split_once('/') else {
                continue;
            };
            let Some(p) = self.plugins.iter_mut().find(|p| p.id() == plugin_id) else {
                continue;
            };
            if let Some(item) = p.item_for(payload) {
                self.results.push(item);
            }
        }
        if !self.results.is_empty() {
            self.sections.push((0, "Hidden"));
        }
    }

    /// List the configured source folders so they can be removed.
    ///
    /// Built here rather than by a plugin: these rows describe apex's own
    /// configuration, and nothing owns them. They are not launchable, so
    /// Enter does nothing and removal lives behind Ctrl+K - deleting a
    /// folder of entries is not something to trigger by pressing Return.
    fn fill_sources(&mut self) {
        let sources = crate::config::Config::load().list_values(crate::config::SOURCES);
        let art = self.folder_icon();
        for path in sources {
            let name = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path.as_str())
                .to_string();
            self.results.push(ResultItem {
                plugin: SHELL,
                title: name,
                badge: None,
                subtitle: "Source folder".to_string(),
                payload: path,
                score: 0,
                icon: art.clone(),
            });
        }
        if !self.results.is_empty() {
            self.sections.push((0, "Source folders"));
        }
    }

    /// Folder icon for the sources listing, fetched on first use so it costs
    /// nothing for anyone who never opens that view.
    ///
    /// Read from the index file, which the helper stocks with it, rather
    /// than extracted here: extraction loads the imaging DLLs for good.
    /// Falls back to extracting only when there is no index yet.
    fn folder_icon(&mut self) -> Option<std::sync::Arc<crate::plugin::Icon>> {
        if self.folder_icon.is_none() {
            self.folder_icon = crate::appindex::load()
                .and_then(|index| index.folder)
                .or_else(|| {
                    crate::icon::stock(windows::Win32::UI::Shell::SIID_FOLDER)
                        .map(std::sync::Arc::new)
                });
        }
        self.folder_icon.clone()
    }

    /// Stop indexing a source folder, and drop the entries it contributed.
    fn remove_source(&mut self) {
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        let target = item.payload.clone();
        let mut sources = crate::config::Config::load().list_values(crate::config::SOURCES);
        sources.retain(|s| !s.eq_ignore_ascii_case(&target));
        crate::config::set_list_file(crate::config::SOURCES, &sources);

        self.mode = Mode::Search;
        // Reindex, or the folder's entries linger until the next reload.
        for p in &mut self.plugins {
            p.refresh();
        }
        let keep = self.selected;
        self.refresh_results();
        self.selected = keep.min(self.results.len().saturating_sub(1));
    }

    /// Switch to the hidden-entry listing.
    pub fn show_hidden(&mut self) {
        self.listing = Listing::Hidden;
        self.query.clear();
        self.refresh_results();
    }

    /// Switch to the source-folder listing.
    pub fn show_sources(&mut self) {
        self.listing = Listing::Sources;
        self.query.clear();
        self.refresh_results();
    }

    /// Hide or restore the selected entry, and persist the change.
    fn set_hidden(&mut self, hide: bool) {
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        let k = key(item.plugin, &item.payload);
        if hide {
            self.hidden.insert(k);
        } else {
            self.hidden.remove(&k);
        }
        let mut values: Vec<String> = self.hidden.iter().cloned().collect();
        values.sort();
        crate::config::set_list_file(HIDDEN, &values);

        self.mode = Mode::Search;
        let keep = self.selected;
        self.refresh_results();
        self.selected = keep.min(self.results.len().saturating_sub(1));
    }

    // ---- scrolling and hit-testing --------------------------------------

    fn list_metrics(&self) -> (f32, f32) {
        let (rows, headers) = (self.results.len(), self.sections.len());
        (
            crate::render::list_viewport_height(rows, headers),
            crate::render::list_content_height(rows, headers),
        )
    }

    fn max_scroll(&self) -> f32 {
        let (view, content) = self.list_metrics();
        (content - view).max(0.0)
    }

    /// Scroll so the selected row sits fully inside the viewport.
    fn ensure_visible(&mut self) {
        let (view, _) = self.list_metrics();
        let row_top = crate::render::row_offset(&self.sections, self.selected);
        let bottom = row_top + crate::render::ROW_H;
        // A row that begins a section should bring its heading along.
        let top = if self.sections.iter().any(|(s, _)| *s == self.selected) {
            row_top - crate::render::HEADER_H
        } else {
            row_top
        };
        if top < self.scroll {
            self.scroll = top;
        } else if bottom > self.scroll + view {
            self.scroll = bottom - view;
        }
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    /// Scroll by a wheel delta in DIPs. True if anything moved.
    pub fn scroll_by(&mut self, delta: f32) -> bool {
        let before = self.scroll;
        self.scroll = (self.scroll - delta).clamp(0.0, self.max_scroll());
        self.scroll != before
    }

    /// Row under a client-space y coordinate, in DIPs.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        let list_top = crate::render::INPUT_H + 1.0;
        let (view, _) = self.list_metrics();
        if y < list_top || y > list_top + view {
            return None;
        }
        let target = y - list_top + self.scroll;
        (0..self.results.len()).find(|&i| {
            let top = crate::render::row_offset(&self.sections, i);
            target >= top && target < top + crate::render::ROW_H
        })
    }

    /// Point the selection at a row, e.g. from a hover. True if it moved.
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.results.len() || index == self.selected {
            return false;
        }
        self.selected = index;
        true
    }

    /// Forget all launch history, in memory and on disk, and show the result.
    fn clear_history(&mut self) {
        self.frecency = Frecency::default();
        self.frecency.save(frecency::now());
        self.query.clear();
        self.refresh_results();
    }

    /// Record the selected result as launched and persist immediately - the
    /// window is about to disappear, and a lost update would be invisible.
    fn record_launch(&mut self) {
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        let now = frecency::now();
        self.frecency.record(item.plugin, &item.payload, now);
        self.frecency.save(now);
    }

    /// Activate the selected result (Enter). Now that `activate` returns an
    /// [`ActionResult`], this is exactly the panel-action path: dispatch to
    /// the owning plugin, then apply the outcome - which is also what records
    /// the launch, so Enter and Ctrl+K > Open stay consistent by construction.
    pub fn activate_selected(&mut self) -> UiOutcome {
        let result = self.dispatch(|p, item| p.activate(item));
        self.apply(result)
    }

    /// Window content height in logical DIPs for the current results.
    ///
    /// A form panel is taller than the actions panel and can exceed the
    /// result list, so the window grows to fit it rather than clipping.
    pub fn content_height(&self) -> f32 {
        let base = crate::render::content_height(self.results.len(), self.sections.len());
        match &self.mode {
            Mode::Form { fields, .. } => base.max(crate::render::form_window_height(fields.len())),
            _ => base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Vec::new(), &crate::config::Config::from_text(""))
    }

    fn row(plugin: &'static str) -> ResultItem {
        ResultItem {
            plugin,
            title: "Tools".into(),
            badge: None,
            subtitle: "Source folder".into(),
            payload: r"D:\Tools".into(),
            score: 0,
            icon: None,
        }
    }

    /// Regression: source rows carry no owning plugin, and open_actions used
    /// to bail out when it could not find one - so Ctrl+K did nothing at all.
    #[test]
    fn actions_open_for_rows_that_no_plugin_owns() {
        let mut a = app();
        a.listing = Listing::Sources;
        a.results.push(row(SHELL));
        assert!(a.open_actions(), "panel should open for a shell-built row");
        match &a.mode {
            Mode::Actions { actions, .. } => {
                assert_eq!(actions.len(), 1);
                assert_eq!(actions[0].id, ACTION_REMOVE_SOURCE);
            }
            _ => panic!("expected the actions panel"),
        }
    }

    #[test]
    fn the_offered_action_follows_the_listing() {
        for (listing, expected) in [
            (Listing::Normal, ACTION_HIDE),
            (Listing::Hidden, ACTION_UNHIDE),
            (Listing::Sources, ACTION_REMOVE_SOURCE),
        ] {
            let mut a = app();
            a.listing = listing;
            a.results.push(row(SHELL));
            assert!(a.open_actions());
            match &a.mode {
                Mode::Actions { actions, .. } => assert_eq!(actions[0].id, expected),
                _ => panic!("expected the actions panel"),
            }
        }
    }

    #[test]
    fn no_results_means_no_panel() {
        let mut a = app();
        assert!(!a.open_actions());
    }
}
