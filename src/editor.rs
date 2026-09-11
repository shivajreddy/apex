//! A one-line text field with a caret and a selection.
//!
//! Shared by the query box, the text prompt and every form field, so caret
//! movement, word jumps, selection and clipboard editing behave identically
//! everywhere. Positions are byte offsets into the UTF-8 text, always on a
//! char boundary; the renderer turns them into x coordinates by measuring the
//! prefix, so nothing here knows about pixels.

/// Where a caret movement goes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Motion {
    Left,
    Right,
    Home,
    End,
}

/// One editing operation, as decoded from a key or clipboard event.
#[derive(Debug)]
pub enum Edit<'a> {
    /// A UTF-16 unit from `WM_CHAR`. Surrogate pairs are reassembled.
    Char(u16),
    /// Typed or pasted text, inserted at the caret (replacing a selection).
    Text(&'a str),
    /// Delete backwards: the selection, else one char, else (`word`) to the
    /// start of the previous word.
    Backspace { word: bool },
    /// Delete forwards: the selection, else one char, else (`word`) to the
    /// start of the next word.
    Delete { word: bool },
    /// Move the caret; `select` extends the selection instead of collapsing
    /// it, `word` jumps by words for `Left`/`Right`.
    Move {
        motion: Motion,
        word: bool,
        select: bool,
    },
    SelectAll,
}

/// A label paired with an editable value: one row of a form.
pub struct FormEntry {
    pub label: String,
    pub value: TextField,
}

#[derive(Default, Clone, Debug)]
pub struct TextField {
    text: String,
    /// Byte offset of the caret.
    caret: usize,
    /// The other end of the selection; equal to `caret` when nothing is
    /// selected. Kept separate from the caret so Shift+arrows can grow and
    /// shrink the selection from either side.
    anchor: usize,
    /// High surrogate waiting for its pair (WM_CHAR delivers UTF-16 units).
    pending_surrogate: Option<u16>,
}

impl TextField {
    /// A field holding `text`, caret at the end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            caret: text.len(),
            anchor: text.len(),
            pending_surrogate: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected byte range, ordered, if anything is selected.
    pub fn selection(&self) -> Option<(usize, usize)> {
        if self.caret == self.anchor {
            None
        } else {
            Some((self.caret.min(self.anchor), self.caret.max(self.anchor)))
        }
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(a, b)| &self.text[a..b])
    }

    /// Replace the whole text, caret at the end, nothing selected.
    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.pending_surrogate = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Apply an edit. Returns whether the *text* changed - callers re-run
    /// the search on that, and merely repaint otherwise.
    pub fn apply(&mut self, edit: Edit) -> bool {
        match edit {
            Edit::Char(unit) => self.insert_utf16(unit),
            Edit::Text(s) => self.insert_str(s),
            Edit::Backspace { word } => self.backspace(word),
            Edit::Delete { word } => self.delete(word),
            Edit::Move {
                motion,
                word,
                select,
            } => {
                self.move_caret(motion, word, select);
                false
            }
            Edit::SelectAll => {
                self.select_all();
                false
            }
        }
    }

    /// Remove and return the selection, for Ctrl+X.
    pub fn cut(&mut self) -> Option<String> {
        let text = self.selected_text()?.to_string();
        self.delete_selection();
        Some(text)
    }

    /// Handle a UTF-16 unit from WM_CHAR. Returns true if the text changed.
    fn insert_utf16(&mut self, unit: u16) -> bool {
        match unit {
            0xD800..=0xDBFF => {
                self.pending_surrogate = Some(unit);
                false
            }
            0xDC00..=0xDFFF => {
                let Some(high) = self.pending_surrogate.take() else {
                    return false;
                };
                let cp = 0x10000 + (((high as u32) - 0xD800) << 10) + ((unit as u32) - 0xDC00);
                match char::from_u32(cp) {
                    Some(c) => self.insert_char(c),
                    None => false,
                }
            }
            _ => {
                self.pending_surrogate = None;
                match char::from_u32(unit as u32) {
                    Some(c) => self.insert_char(c),
                    None => false,
                }
            }
        }
    }

    fn insert_char(&mut self, c: char) -> bool {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf))
    }

    fn insert_str(&mut self, s: &str) -> bool {
        if s.is_empty() && self.selection().is_none() {
            return false;
        }
        self.delete_selection();
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = self.caret;
        true
    }

    /// Remove the selection, leaving the caret where it started. Returns
    /// whether there was one.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            return false;
        };
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
        true
    }

    fn backspace(&mut self, word: bool) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret == 0 {
            return false;
        }
        let from = if word {
            self.prev_word(self.caret)
        } else {
            self.prev_char(self.caret)
        };
        self.text.replace_range(from..self.caret, "");
        self.caret = from;
        self.anchor = from;
        true
    }

    fn delete(&mut self, word: bool) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.caret == self.text.len() {
            return false;
        }
        let to = if word {
            self.next_word(self.caret)
        } else {
            self.next_char(self.caret)
        };
        self.text.replace_range(self.caret..to, "");
        self.anchor = self.caret;
        true
    }

    fn move_caret(&mut self, motion: Motion, word: bool, select: bool) {
        // Without Shift, an arrow collapses the selection to its near end
        // rather than moving from the caret - what every Windows edit
        // control does, and what makes "select a word, press Left" land at
        // the start of the word.
        if !select
            && let Some((a, b)) = self.selection()
            && matches!(motion, Motion::Left | Motion::Right)
            && !word
        {
            self.caret = if motion == Motion::Left { a } else { b };
            self.anchor = self.caret;
            return;
        }
        self.caret = match motion {
            Motion::Left if word => self.prev_word(self.caret),
            Motion::Left => self.prev_char(self.caret),
            Motion::Right if word => self.next_word(self.caret),
            Motion::Right => self.next_char(self.caret),
            Motion::Home => 0,
            Motion::End => self.text.len(),
        };
        if !select {
            self.anchor = self.caret;
        }
    }

    fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Place the caret at `pos` (a byte offset on a char boundary), e.g. from
    /// a mouse click. `select` extends the selection from the anchor.
    pub fn set_caret(&mut self, pos: usize, select: bool) {
        let pos = pos.min(self.text.len());
        if !self.text.is_char_boundary(pos) {
            return;
        }
        self.caret = pos;
        if !select {
            self.anchor = pos;
        }
    }

    fn prev_char(&self, from: usize) -> usize {
        self.text[..from]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_char(&self, from: usize) -> usize {
        self.text[from..]
            .chars()
            .next()
            .map_or(from, |c| from + c.len_utf8())
    }

    /// Start of the word before `from`: skip whitespace backwards, then the
    /// word itself. Matches what Ctrl+Backspace has always deleted.
    fn prev_word(&self, from: usize) -> usize {
        let mut pos = from;
        let mut it = self.text[..from].char_indices().rev().peekable();
        while let Some((i, c)) = it.peek().copied()
            && c.is_whitespace()
        {
            pos = i;
            it.next();
        }
        while let Some((i, c)) = it.peek().copied()
            && !c.is_whitespace()
        {
            pos = i;
            it.next();
        }
        pos
    }

    /// Start of the word after `from`: skip the current word, then the
    /// whitespace following it - the Windows convention for Ctrl+Right, so
    /// repeated presses land on word starts rather than word ends.
    fn next_word(&self, from: usize) -> usize {
        let mut pos = from;
        let mut it = self.text[from..].char_indices().peekable();
        while let Some((i, c)) = it.peek().copied()
            && !c.is_whitespace()
        {
            pos = from + i + c.len_utf8();
            it.next();
        }
        while let Some((i, c)) = it.peek().copied()
            && c.is_whitespace()
        {
            pos = from + i + c.len_utf8();
            it.next();
        }
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str) -> TextField {
        TextField::new(text)
    }

    fn mv(motion: Motion, word: bool, select: bool) -> Edit<'static> {
        Edit::Move {
            motion,
            word,
            select,
        }
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut f = field("ac");
        f.apply(mv(Motion::Left, false, false));
        assert!(f.apply(Edit::Char(b'b' as u16)));
        assert_eq!(f.text(), "abc");
        assert_eq!(f.caret(), 2);
        assert!(f.selection().is_none());
    }

    #[test]
    fn surrogate_pairs_are_reassembled() {
        let mut f = field("");
        // U+1F600 as UTF-16: D83D DE00.
        assert!(!f.apply(Edit::Char(0xD83D)));
        assert!(f.apply(Edit::Char(0xDE00)));
        assert_eq!(f.text(), "\u{1F600}");
        assert_eq!(f.caret(), 4);
    }

    #[test]
    fn a_stray_low_surrogate_is_ignored() {
        let mut f = field("x");
        assert!(!f.apply(Edit::Char(0xDE00)));
        assert_eq!(f.text(), "x");
    }

    #[test]
    fn backspace_and_delete_take_one_char() {
        let mut f = field("héllo");
        f.apply(mv(Motion::Left, false, false));
        f.apply(mv(Motion::Left, false, false));
        f.apply(mv(Motion::Left, false, false));
        assert_eq!(f.caret(), 3); // after "hé"
        assert!(f.apply(Edit::Backspace { word: false }));
        assert_eq!(f.text(), "hllo");
        assert_eq!(f.caret(), 1);
        assert!(f.apply(Edit::Delete { word: false }));
        assert_eq!(f.text(), "hlo");
        assert_eq!(f.caret(), 1);
    }

    #[test]
    fn backspace_at_start_and_delete_at_end_do_nothing() {
        let mut f = field("ab");
        assert!(!f.apply(Edit::Delete { word: false }));
        f.apply(mv(Motion::Home, false, false));
        assert!(!f.apply(Edit::Backspace { word: false }));
        assert_eq!(f.text(), "ab");
    }

    #[test]
    fn word_backspace_matches_the_old_behaviour() {
        // Trailing spaces go first, then the word - exactly what the query
        // box did before the editor existed.
        let mut f = field("visual studio  ");
        assert!(f.apply(Edit::Backspace { word: true }));
        assert_eq!(f.text(), "visual ");
        assert!(f.apply(Edit::Backspace { word: true }));
        assert_eq!(f.text(), "");
    }

    #[test]
    fn word_delete_eats_the_word_and_its_trailing_space() {
        let mut f = field("one two three");
        f.apply(mv(Motion::Home, false, false));
        assert!(f.apply(Edit::Delete { word: true }));
        assert_eq!(f.text(), "two three");
        assert_eq!(f.caret(), 0);
    }

    #[test]
    fn word_jumps_land_on_word_starts() {
        let mut f = field("one  two three");
        f.apply(mv(Motion::Home, false, false));
        f.apply(mv(Motion::Right, true, false));
        assert_eq!(f.caret(), 5); // start of "two"
        f.apply(mv(Motion::Right, true, false));
        assert_eq!(f.caret(), 9); // start of "three"
        f.apply(mv(Motion::Right, true, false));
        assert_eq!(f.caret(), 14); // end
        f.apply(mv(Motion::Left, true, false));
        assert_eq!(f.caret(), 9);
        f.apply(mv(Motion::Left, true, false));
        assert_eq!(f.caret(), 5);
        f.apply(mv(Motion::Left, true, false));
        assert_eq!(f.caret(), 0);
    }

    #[test]
    fn caret_stays_on_char_boundaries() {
        let mut f = field("aé😀b");
        f.apply(mv(Motion::Home, false, false));
        let mut seen = vec![f.caret()];
        while f.caret() < f.text().len() {
            f.apply(mv(Motion::Right, false, false));
            seen.push(f.caret());
        }
        assert_eq!(seen, vec![0, 1, 3, 7, 8]);
        for pos in seen {
            assert!(f.text().is_char_boundary(pos));
        }
    }

    #[test]
    fn shift_extends_and_plain_arrows_collapse_to_the_near_end() {
        let mut f = field("hello world");
        f.apply(mv(Motion::Left, true, true)); // select "world"
        assert_eq!(f.selected_text(), Some("world"));
        assert_eq!(f.caret(), 6);
        f.apply(mv(Motion::Right, false, false)); // collapse to the end
        assert!(f.selection().is_none());
        assert_eq!(f.caret(), 11);
        f.apply(mv(Motion::Left, true, true));
        f.apply(mv(Motion::Left, false, false)); // collapse to the start
        assert_eq!(f.caret(), 6);
    }

    #[test]
    fn selection_can_grow_from_either_side() {
        let mut f = field("abcdef");
        f.apply(mv(Motion::Left, false, true));
        f.apply(mv(Motion::Left, false, true));
        assert_eq!(f.selected_text(), Some("ef"));
        f.apply(mv(Motion::Right, false, true));
        assert_eq!(f.selected_text(), Some("f"));
        f.apply(mv(Motion::Right, false, true));
        assert!(f.selection().is_none());
    }

    #[test]
    fn home_and_end_with_shift_select_to_the_edges() {
        let mut f = field("abc");
        f.apply(mv(Motion::Home, false, true));
        assert_eq!(f.selected_text(), Some("abc"));
        f.apply(mv(Motion::End, false, false));
        assert!(f.selection().is_none());
        assert_eq!(f.caret(), 3);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut f = field("hello world");
        f.apply(mv(Motion::Left, true, true));
        assert!(f.apply(Edit::Text("there")));
        assert_eq!(f.text(), "hello there");
        assert_eq!(f.caret(), 11);
        f.apply(Edit::SelectAll);
        assert!(f.apply(Edit::Char(b'x' as u16)));
        assert_eq!(f.text(), "x");
    }

    #[test]
    fn backspace_and_delete_remove_only_the_selection() {
        let mut f = field("abcdef");
        f.apply(mv(Motion::Home, false, false));
        f.apply(mv(Motion::Right, false, true));
        f.apply(mv(Motion::Right, false, true));
        assert!(f.apply(Edit::Delete { word: true }));
        assert_eq!(f.text(), "cdef");
        f.apply(mv(Motion::End, false, false));
        f.apply(mv(Motion::Left, false, true));
        assert!(f.apply(Edit::Backspace { word: true }));
        assert_eq!(f.text(), "cde");
    }

    #[test]
    fn select_all_then_cut() {
        let mut f = field("hello");
        f.apply(Edit::SelectAll);
        assert_eq!(f.cut(), Some("hello".to_string()));
        assert_eq!(f.text(), "");
        assert_eq!(f.cut(), None);
    }

    #[test]
    fn empty_paste_without_selection_changes_nothing() {
        let mut f = field("abc");
        assert!(!f.apply(Edit::Text("")));
        f.apply(Edit::SelectAll);
        // An empty paste over a selection still deletes it.
        assert!(f.apply(Edit::Text("")));
        assert_eq!(f.text(), "");
    }

    #[test]
    fn set_caret_rejects_positions_inside_a_char() {
        let mut f = field("é");
        f.set_caret(1, false);
        assert_eq!(f.caret(), 2);
        f.set_caret(0, true);
        assert_eq!(f.selected_text(), Some("é"));
        f.set_caret(99, false);
        assert_eq!(f.caret(), 2);
    }

    #[test]
    fn set_text_resets_caret_and_selection() {
        let mut f = field("abc");
        f.apply(Edit::SelectAll);
        f.set_text("xy");
        assert_eq!(f.caret(), 2);
        assert!(f.selection().is_none());
        f.clear();
        assert!(f.is_empty());
        assert_eq!(f.caret(), 0);
    }
}
