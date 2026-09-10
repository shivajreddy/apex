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

/// What the UI should do after a plugin handled an action.
pub enum ActionResult {
    /// Back to search mode; results are re-queried.
    Done,
    /// Hide the window.
    Close,
    /// Open a one-line text input (e.g. "set alias"); the entered text is
    /// delivered to [`Plugin::submit_text`] with `action_id`.
    RequestText {
        prompt: String,
        action_id: &'static str,
    },
}

pub trait Plugin {
    /// Stable identifier, also used in the config file.
    fn id(&self) -> &'static str;

    /// Append matches for `q` to `out`. Called on every keystroke - must be fast.
    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>);

    /// Run the default action for `item`. Return `true` to dismiss the window.
    fn activate(&mut self, item: &ResultItem) -> bool;

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
}
