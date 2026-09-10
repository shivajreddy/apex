//! Application state: query text, caret, plugins, and current results.

use crate::plugin::{Plugin, ResultItem};
use crate::render::Renderer;

pub const MAX_RESULTS: usize = 8;

pub struct App {
    pub renderer: Renderer,
    pub query: String,
    /// High surrogate waiting for its pair (WM_CHAR delivers UTF-16 units).
    pending_surrogate: Option<u16>,
    pub caret_visible: bool,
    pub plugins: Vec<Box<dyn Plugin>>,
    pub results: Vec<ResultItem>,
    pub selected: usize,
}

impl App {
    pub fn new(plugins: Vec<Box<dyn Plugin>>) -> windows::core::Result<Self> {
        Ok(Self {
            renderer: Renderer::new()?,
            query: String::new(),
            pending_surrogate: None,
            caret_visible: true,
            plugins,
            results: Vec::new(),
            selected: 0,
        })
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
        self.query.clear();
        self.pending_surrogate = None;
        self.refresh_results();
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
        self.results.sort_by(|a, b| b.score.cmp(&a.score));
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
