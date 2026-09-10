//! Application state: query text, caret, plugins, current results, and the
//! UI mode (search / actions panel / one-line text input).

use crate::plugin::{Action, ActionResult, Plugin, ResultItem};
use crate::render::Renderer;

pub const MAX_RESULTS: usize = 8;

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
}

/// What the window should do after a mode-level operation.
#[derive(PartialEq)]
pub enum UiOutcome {
    Stay,
    Hide,
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
}

impl App {
    pub fn new(plugins: Vec<Box<dyn Plugin>>) -> Self {
        Self {
            mode: Mode::Search,
            renderer: None,
            query: String::new(),
            pending_surrogate: None,
            caret_visible: true,
            plugins,
            results: Vec::new(),
            selected: 0,
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
        let actions = plugin.actions(item);
        if actions.is_empty() {
            return false;
        }
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
                self.mode = Mode::Search;
                UiOutcome::Hide
            }
            ActionResult::RequestText { prompt, action_id } => {
                self.mode = Mode::TextInput {
                    prompt,
                    action_id,
                    buffer: String::new(),
                };
                UiOutcome::Stay
            }
        }
    }

    /// Re-run the current query, keeping the selection where possible.
    fn requery(&mut self) {
        let keep = self.selected;
        self.refresh_results();
        self.selected = keep.min(self.results.len().saturating_sub(1));
    }

    /// Move selection by `delta`, wrapping around.
    pub fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.results.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }

    fn refresh_results(&mut self) {
        self.results.clear();
        self.selected = 0;
        if self.query.is_empty() {
            return;
        }
        for p in &mut self.plugins {
            p.query(&self.query, &mut self.results);
        }
        crate::dlog!("refresh: query='{}' -> {} results", self.query, self.results.len());
        self.results
            .sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        self.results.truncate(MAX_RESULTS);
    }

    /// Activate the selected result. Returns true if the window should close.
    pub fn activate_selected(&mut self) -> bool {
        let Some(item) = self.results.get(self.selected) else {
            return false;
        };
        for p in &mut self.plugins {
            if p.id() == item.plugin {
                return p.activate(item);
            }
        }
        false
    }

    /// Window content height in logical DIPs for the current results.
    pub fn content_height(&self) -> f32 {
        crate::render::content_height(self.results.len())
    }
}
