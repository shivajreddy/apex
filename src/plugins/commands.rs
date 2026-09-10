//! Apex's own commands, as searchable rows.
//!
//! Distinct from anything the machine can do: these act on apex itself. The
//! first is "Reload Apex", which re-reads every plugin's data from disk -
//! newly installed applications, and hand-edits to `config.toml` such as new
//! quicklinks or aliases - without restarting.
//!
//! Rows here are deliberately excluded from launch history and from the
//! empty-query list: running a command is not launching something, and
//! ranking commands by use would push them in front of real results.

use std::sync::Arc;

use crate::fuzzy;
use crate::icon;
use crate::plugin::{Action, ActionResult, Icon, Plugin, ResultItem};

pub const ID: &str = "commands";

struct Command {
    id: &'static str,
    label: &'static str,
    folded: Vec<char>,
    bonus: Vec<i32>,
}

impl Command {
    fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            folded: fuzzy::fold_case(label),
            bonus: fuzzy::bonuses(label),
        }
    }
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
            commands: vec![Command::new("reload", "Reload Apex")],
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
            if let Some(score) = fuzzy::score(&query, &c.folded, &c.bonus) {
                out.push(ResultItem {
                    plugin: ID,
                    title: c.label.to_string(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_is_findable_by_either_word() {
        let mut c = Commands {
            commands: vec![Command::new("reload", "Reload Apex")],
            icon: None,
        };
        for q in ["reload", "apex", "rel", "reload apex"] {
            let mut out = Vec::new();
            c.query(q, &mut out);
            assert_eq!(out.len(), 1, "query {q:?} should find the command");
            assert_eq!(out[0].payload, "reload");
        }
    }

    #[test]
    fn unrelated_queries_match_nothing() {
        let mut c = Commands {
            commands: vec![Command::new("reload", "Reload Apex")],
            icon: None,
        };
        let mut out = Vec::new();
        c.query("spotify", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn activating_reload_asks_for_a_global_refresh() {
        let mut c = Commands {
            commands: vec![Command::new("reload", "Reload Apex")],
            icon: None,
        };
        let item = ResultItem {
            plugin: ID,
            title: "Reload Apex".into(),
            subtitle: "Command".into(),
            payload: "reload".into(),
            score: 0,
            icon: None,
        };
        assert!(matches!(c.activate(&item), ActionResult::Refresh));
    }

    #[test]
    fn commands_stay_out_of_history_and_the_default_list() {
        let mut c = Commands::new();
        // No item_for and no browse: the trait defaults keep command rows out
        // of the empty-query list even if one somehow reached frecency.
        assert!(c.item_for("reload").is_none());
        let mut out = Vec::new();
        c.browse(8, &mut out);
        assert!(out.is_empty());
    }
}
