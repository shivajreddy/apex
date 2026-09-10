//! Plugin architecture.
//!
//! Every apex capability ships as a [`Plugin`]. The first one is `Search`
//! (application search); more arrive over time. Disabled plugins are never
//! constructed: zero memory, zero threads, zero startup cost. The core stays
//! a thin shell around whatever plugins the user enabled.

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
}

pub trait Plugin {
    /// Stable identifier, also used in the config file.
    fn id(&self) -> &'static str;

    /// Append matches for `q` to `out`. Called on every keystroke - must be fast.
    fn query(&mut self, q: &str, out: &mut Vec<ResultItem>);

    /// Run the action for `item`. Return `true` to dismiss the window.
    fn activate(&mut self, item: &ResultItem) -> bool;
}
