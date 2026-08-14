//! A multi-line prompt editor with the line-editing keys people already have
//! in their fingers.
//!
//! The buffer is a plain `String` with a byte cursor; newlines are ordinary
//! characters, so a prompt can span lines without a separate line model. Every
//! mutation keeps the cursor on a character boundary.

use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    history: Vec<String>,
    /// Index into `history` while browsing it, newest last.
    browsing: Option<usize>,
    /// The in-progress prompt parked while browsing history.
    stashed: String,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    pub fn line_count(&self) -> usize {
        self.text.bytes().filter(|byte| *byte == b'\n').count() + 1
    }

    /// Cursor as a (row, display column) pair for terminal placement.
    pub fn cursor_cell(&self) -> (usize, usize) {
        let row = self.text[..self.cursor]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let (start, _) = self.line_bounds(self.cursor);
        (row, self.text[start..self.cursor].width())
    }

    /// Takes the prompt, records it in the history, and clears the buffer.
    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        self.stashed.clear();
        if self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
        }
        text
    }

    /// Drops the prompt without recording it in the history.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    pub fn insert_char(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn insert_str(&mut self, text: &str) {
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &cleaned);
        self.cursor += cleaned.len();
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.prev_boundary(self.cursor) {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
    }

    pub fn delete_forward(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn delete_word_back(&mut self) {
        let target = self.word_start();
        self.text.replace_range(target..self.cursor, "");
        self.cursor = target;
    }

    pub fn delete_word_forward(&mut self) {
        let target = self.word_end();
        self.text.replace_range(self.cursor..target, "");
    }

    /// `ctrl+u` / `cmd+backspace`: erase from the cursor to the line start.
    pub fn delete_to_line_start(&mut self) {
        let (start, _) = self.line_bounds(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// `ctrl+k`: erase from the cursor to the line end.
    pub fn delete_to_line_end(&mut self) {
        let (_, end) = self.line_bounds(self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn move_left(&mut self) {
        if let Some(previous) = self.prev_boundary(self.cursor) {
            self.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.cursor = next;
        }
    }

    pub fn move_word_left(&mut self) {
        self.cursor = self.word_start();
    }

    pub fn move_word_right(&mut self) {
        self.cursor = self.word_end();
    }

    pub fn move_line_start(&mut self) {
        self.cursor = self.line_bounds(self.cursor).0;
    }

    pub fn move_line_end(&mut self) {
        self.cursor = self.line_bounds(self.cursor).1;
    }

    /// Moves up a line, keeping the column. Returns false at the first line so
    /// the caller can fall back to history browsing.
    pub fn move_up(&mut self) -> bool {
        let (start, _) = self.line_bounds(self.cursor);
        if start == 0 {
            return false;
        }
        let column = self.text[start..self.cursor].chars().count();
        let (previous_start, previous_end) = self.line_bounds(start - 1);
        self.cursor =
            char_offset(&self.text[previous_start..previous_end], column) + previous_start;
        true
    }

    /// Moves down a line, keeping the column. Returns false at the last line.
    pub fn move_down(&mut self) -> bool {
        let (start, end) = self.line_bounds(self.cursor);
        if end == self.text.len() {
            return false;
        }
        let column = self.text[start..self.cursor].chars().count();
        let (next_start, next_end) = self.line_bounds(end + 1);
        self.cursor = char_offset(&self.text[next_start..next_end], column) + next_start;
        true
    }

    /// Recalls the previous prompt, parking any unsent draft.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.browsing {
            None => {
                self.stashed = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.browsing = Some(index);
        self.set_text(self.history[index].clone());
    }

    /// Walks back towards the parked draft.
    pub fn history_next(&mut self) {
        let Some(index) = self.browsing else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.browsing = None;
            let stashed = std::mem::take(&mut self.stashed);
            self.set_text(stashed);
            return;
        }
        self.browsing = Some(index + 1);
        self.set_text(self.history[index + 1].clone());
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
    }

    /// Byte range of the line containing `at`, excluding the newline.
    fn line_bounds(&self, at: usize) -> (usize, usize) {
        let start = self.text[..at].rfind('\n').map_or(0, |newline| newline + 1);
        let end = self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |newline| at + newline);
        (start, end)
    }

    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.text[..at]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.text[at..]
            .chars()
            .next()
            .map(|character| at + character.len_utf8())
    }

    /// Start of the word before the cursor: skip separators, then word bytes.
    fn word_start(&self) -> usize {
        let mut index = self.cursor;
        while let Some(previous) = self.prev_boundary(index) {
            if is_word(self.text[previous..].chars().next().unwrap_or(' ')) {
                break;
            }
            index = previous;
        }
        while let Some(previous) = self.prev_boundary(index) {
            if !is_word(self.text[previous..].chars().next().unwrap_or(' ')) {
                break;
            }
            index = previous;
        }
        index
    }

    /// End of the word after the cursor.
    fn word_end(&self) -> usize {
        let mut index = self.cursor;
        while let Some(character) = self.text[index..].chars().next() {
            if is_word(character) {
                break;
            }
            index += character.len_utf8();
        }
        while let Some(character) = self.text[index..].chars().next() {
            if !is_word(character) {
                break;
            }
            index += character.len_utf8();
        }
        index
    }
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Byte offset of the `column`-th character, clamped to the end of `line`.
fn char_offset(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::Editor;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::default();
        editor.insert_str(text);
        editor
    }

    #[test]
    fn deletes_the_word_before_the_cursor() {
        let mut editor = editor("cargo test --all");
        editor.delete_word_back();
        assert_eq!(editor.text(), "cargo test --");
    }

    #[test]
    fn deletes_to_the_start_of_the_current_line_only() {
        let mut editor = editor("first\nsecond");
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "first\n");
    }

    #[test]
    fn keeps_the_column_when_moving_between_lines() {
        let mut editor = editor("abcdef\nxy\nlonger");
        editor.move_up();
        editor.move_up();
        assert_eq!(editor.cursor_cell(), (0, 2));
        editor.move_down();
        assert_eq!(editor.cursor_cell(), (1, 2));
    }

    #[test]
    fn browses_history_and_restores_the_parked_draft() {
        let mut editor = editor("build it");
        editor.submit();
        editor.insert_str("draft");
        editor.history_prev();
        assert_eq!(editor.text(), "build it");
        editor.history_next();
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn moves_across_multibyte_characters() {
        let mut editor = editor("héllo");
        editor.move_line_start();
        editor.move_right();
        editor.delete_forward();
        assert_eq!(editor.text(), "hllo");
    }
}
