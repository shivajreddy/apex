//! Plugin architecture.
//!
//! Every apex capability ships as a [`Plugin`]. The first one is `Search`
//! (application search); more arrive over time. Disabled plugins are never
//! constructed: zero memory, zero threads, zero startup cost. The core stays
//! a thin shell around whatever plugins the user enabled.

/// Decoded icon pixels: premultiplied BGRA, row-major, `width * 4` pitch.
/// Device-independent so it survives render-target recreation; shared via
/// `Arc` between the plugin's cache and result items.
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// A single search result row.
pub struct ResultItem {
    /// Id of the plugin that produced this item.
    pub plugin: &'static str,
    pub title: String,
    /// Short label drawn as a pill immediately after the title, e.g. the
    /// alias that reaches this item. Kept separate from `subtitle` so it can
    /// sit next to the name rather than in the right-hand type column.
    pub badge: Option<String>,
    /// Dimmed label drawn right after the title (before the alias pill), the
    /// group the item belongs to - e.g. "Apex" for a command. Empty when the
    /// item has no meaningful category. Distinct from `subtitle`, the
    /// right-hand type column.
    pub category: String,
    pub subtitle: String,
    /// Plugin-specific payload (e.g. path of the shortcut to launch).
    pub payload: String,
    /// Relevance; higher sorts first.
    pub score: i32,
    pub icon: Option<std::sync::Arc<Icon>>,
}

/// An entry in the actions panel (Ctrl+K) for a selected result.
pub struct Action {
    pub id: &'static str,
    pub label: String,
}

/// One editable line in a [`ActionResult::RequestForm`] form.
pub struct FormField {
    pub label: String,
    pub value: String,
}

impl FormField {
    pub fn new(label: &str, value: &str) -> Self {
        Self {
            label: label.to_string(),
            value: value.to_string(),
        }
    }
}

/// Operations a plugin can ask for but cannot carry out itself, because they
/// need the window handle or the process.
#[derive(Clone, Copy, PartialEq)]
pub enum ShellCommand {
    Quit,
    Restart,
    ToggleTray,
    ClearHistory,
    /// List hidden entries so they can be restored.
    ShowHidden,
    /// List configured source folders so they can be removed.
    ShowSources,
}

/// What the UI should do after a plugin handled an action.
pub enum ActionResult {
    /// Back to search mode; results are re-queried.
    Done,
    /// The item was launched: hide the window and record it in launch
    /// history, so it ranks higher next time.
    Close,
    /// Hide the window without recording anything. For side-actions such as
    /// revealing a file, which shouldn't inflate the item's ranking.
    Dismiss,
    /// Reload every plugin from disk and return to the default list. Not
    /// recorded: reloading is not launching anything.
    Refresh,
    /// Hand a shell-level operation up to the window. Not recorded.
    Shell(ShellCommand),
    /// Open a one-line text input (e.g. "set alias"); the entered text is
    /// delivered to [`Plugin::submit_text`] with `action_id`.
    RequestText {
        prompt: String,
        action_id: &'static str,
    },
    /// Open a multi-field form (e.g. create/edit a quicklink). The edited
    /// fields are delivered to [`Plugin::submit_form`] with `action_id`.
    RequestForm {
        title: String,
        action_id: &'static str,
        fields: Vec<FormField>,
    },
}

pub trait Plugin {
    /// Stable identifier, also used in the config file.
    fn id(&self) -> &'static str;

    /// Append matches for `q` to `out`. Called on every keystroke - must be fast.
    /// Never called with an empty query; see [`Plugin::item_for`].
    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>);

    /// Rebuild a result for a payload this plugin produced earlier.
    ///
    /// The shell keeps frecency keyed by `(plugin, payload)` and uses this to
    /// materialise the most-used entries while the query is empty, so only
    /// the handful actually shown are ever built. Return `None` if the
    /// payload no longer exists - an uninstalled app, a deleted quicklink -
    /// and the row is simply skipped. Plugins that opt out never appear on an
    /// empty query.
    fn item_for(&mut self, _payload: &str) -> Option<ResultItem> {
        None
    }

    /// Append up to `limit` of this plugin's own entries, in whatever order
    /// it considers natural.
    ///
    /// Used to pad the empty-query list once launch history runs out, so a
    /// fresh install still looks like a launcher rather than a blank box.
    /// Implementations must skip payloads already present in `out` to avoid
    /// repeating a row that history already placed above.
    fn browse(&mut self, _limit: usize, _out: &mut Vec<ResultItem>) {}

    /// Reload whatever this plugin caches from disk.
    ///
    /// Triggered by [`ActionResult::Refresh`] and applied to every plugin, so
    /// one command picks up newly installed apps *and* hand-edits to the
    /// config file. Anything slow belongs on a background thread: this runs
    /// on the UI thread.
    fn refresh(&mut self) {}

    /// Run the default action for `item` - the Enter key.
    ///
    /// Returns the same [`ActionResult`] as a panel action, so Enter can open
    /// a prompt or a form rather than only launching something.
    fn activate(&mut self, item: &ResultItem) -> ActionResult;

    /// Entries for the actions panel. Empty = no panel for this item.
    fn actions(&self, _item: &ResultItem) -> Vec<Action> {
        Vec::new()
    }

    /// Run a panel action.
    fn run_action(&mut self, _action_id: &str, _item: &ResultItem) -> ActionResult {
        ActionResult::Done
    }

    /// Commit text entered after [`ActionResult::RequestText`].
    fn submit_text(&mut self, _action_id: &str, _item: &ResultItem, _text: &str) -> ActionResult {
        ActionResult::Done
    }

    /// Commit a form filled in after [`ActionResult::RequestForm`]. Fields
    /// arrive in the order they were requested.
    fn submit_form(
        &mut self,
        _action_id: &str,
        _item: &ResultItem,
        _fields: &[FormField],
    ) -> ActionResult {
        ActionResult::Done
    }
}
