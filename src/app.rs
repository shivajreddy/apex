//! Application state: query text, caret, plugins, current results, and the
//! UI mode (search / actions panel / one-line text input).

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
        buffer: String,
    },
    Form {
        title: String,
        action_id: &'static str,
        fields: Vec<FormField>,
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
    pub query: String,
    /// High surrogate waiting for its pair (WM_CHAR delivers UTF-16 units).
    pending_surrogate: Option<u16>,
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
            query: String::new(),
            pending_surrogate: None,
            caret_visible: true,
            plugins,
            results: Vec::new(),
            selected: 0,
            sections: Vec::new(),
            scroll: 0.0,
            hidden: config.list_values(HIDDEN).into_iter().collect(),
            listing: Listing::Normal,
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
            match Renderer::new() {
                Ok(r) => self.renderer = Some(r),
                Err(e) => {
                    crate::dlog!("renderer creation failed: {e}");
                    return None;
                }
            }
        }
        self.renderer.as_mut()
    }

    /// Handle a UTF-16 unit from WM_CHAR. Returns true if the query changed.
    pub fn insert_utf16(&mut self, unit: u16) -> bool {
        match unit {
            0xD800..=0xDBFF => {
                self.pending_surrogate = Some(unit);
                false
            }
            0xDC00..=0xDFFF => {
                if let Some(high) = self.pending_surrogate.take() {
                    let cp = 0x10000 + (((high as u32) - 0xD800) << 10) + ((unit as u32) - 0xDC00);
                    if let Some(c) = char::from_u32(cp) {
                        self.query.push(c);
                        self.refresh_results();
                        return true;
                    }
                }
                false
            }
            _ => {
                self.pending_surrogate = None;
                match char::from_u32(unit as u32) {
                    Some(c) => {
                        self.query.push(c);
                        self.refresh_results();
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// Delete one char, or the trailing word when `word` is set (Ctrl+Backspace).
    pub fn backspace(&mut self, word: bool) -> bool {
        if self.query.is_empty() {
            return false;
        }
        if word {
            while self.query.ends_with(' ') {
                self.query.pop();
            }
            while self.query.chars().next_back().is_some_and(|c| c != ' ') {
                self.query.pop();
            }
        } else {
            self.query.pop();
        }
        self.refresh_results();
        true
    }

    pub fn clear_query(&mut self) {
        self.mode = Mode::Search;
        self.query.clear();
        self.pending_surrogate = None;
        self.refresh_results();
    }

    // ---- actions panel -------------------------------------------------

    /// Open the actions panel for the selected result.
    pub fn open_actions(&mut self) -> bool {
        let Some(item) = self.results.get(self.selected) else {
            return false;
        };
        let Some(plugin) = self.plugins.iter().find(|p| p.id() == item.plugin) else {
            return false;
        };
        let mut actions = plugin.actions(item);
        // Shell-level, so it is offered on every row whatever produced it,
        // rather than each plugin reimplementing the same entry.
        actions.push(if self.listing == Listing::Hidden {
            Action {
                id: ACTION_UNHIDE,
                label: "Unhide".to_string(),
            }
        } else {
            Action {
                id: ACTION_HIDE,
                label: "Hide from Apex".to_string(),
            }
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
            } => (*action_id, buffer.clone()),
            _ => return UiOutcome::Stay,
        };
        let result = self.dispatch(|p, item| p.submit_text(action_id, item, text.trim()));
        self.apply(result)
    }

    /// Append clipboard text to whichever field currently has focus.
    /// Returns true if anything changed.
    pub fn paste(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        // Checked first so the mutable borrow of `mode` below doesn't
        // collide with touching `query` and re-running the search.
        if matches!(self.mode, Mode::Search) {
            self.query.push_str(text);
            self.refresh_results();
            return true;
        }
        match &mut self.mode {
            Mode::TextInput { buffer, .. } => {
                buffer.push_str(text);
                true
            }
            Mode::Form {
                fields, focused, ..
            } => match fields.get_mut(*focused) {
                Some(field) => {
                    field.value.push_str(text);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// Feed a WM_CHAR unit to the text-input buffer.
    pub fn text_input_char(&mut self, unit: u16) {
        if let Mode::TextInput { buffer, .. } = &mut self.mode {
            match unit {
                0x08 => {
                    buffer.pop();
                }
                u if u >= 0x20 => {
                    if let Some(c) = char::from_u32(u as u32) {
                        buffer.push(c);
                    }
                }
                _ => {}
            }
        }
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
                self.pending_surrogate = None;
                self.refresh_results();
                UiOutcome::Stay
            }
            ActionResult::RequestText { prompt, action_id } => {
                self.mode = Mode::TextInput {
                    prompt,
                    action_id,
                    buffer: String::new(),
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
                    fields,
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

    /// Feed a WM_CHAR unit to the focused form field.
    pub fn form_char(&mut self, unit: u16) {
        let Mode::Form {
            fields, focused, ..
        } = &mut self.mode
        else {
            return;
        };
        let Some(field) = fields.get_mut(*focused) else {
            return;
        };
        match unit {
            0x08 => {
                field.value.pop();
            }
            // Ctrl+Backspace: drop the trailing word, as in the main query.
            0x7F => {
                while field.value.ends_with(' ') {
                    field.value.pop();
                }
                while field.value.chars().next_back().is_some_and(|c| c != ' ') {
                    field.value.pop();
                }
            }
            u if u >= 0x20 => {
                if let Some(c) = char::from_u32(u as u32) {
                    field.value.push(c);
                }
            }
            _ => {}
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
                    .map(|f| FormField::new(&f.label, f.value.trim()))
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
        if self.listing == Listing::Hidden {
            self.fill_hidden();
            return;
        }

        // Empty query: show the most-used entries instead of nothing. These
        // arrive already ranked, so they bypass the scoring sort below.
        if self.query.is_empty() {
            self.fill_default(now);
            return;
        }

        for p in &mut self.plugins {
            p.query(&self.query, &mut self.results);
        }
        self.drop_hidden();
        for item in &mut self.results {
            item.score += self.frecency.bonus(item.plugin, &item.payload, now);
        }
        crate::dlog!(
            "refresh: query='{}' -> {} results",
            self.query,
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

    /// Switch to the hidden-entry listing.
    pub fn show_hidden(&mut self) {
        self.listing = Listing::Hidden;
        self.query.clear();
        self.pending_surrogate = None;
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
        self.pending_surrogate = None;
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
