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

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Soft-wraps the prompt to `width` columns, with the cursor's cell in the
    /// wrapped layout.
    ///
    /// The prompt is edited in logical lines but shown in display rows, so
    /// wrapping and cursor placement have to agree: both come from this one
    /// pass over the buffer.
    pub fn wrapped(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let ranges = self.rows(width);
        let index = self.cursor_row(&ranges);
        let (start, _) = ranges[index];
        let mut rows: Vec<String> = ranges
            .iter()
            .map(|(start, end)| self.text[*start..*end].to_string())
            .collect();
        let mut cursor = (index, self.text[start..self.cursor].width());
        if cursor.1 >= width.max(1) {
            rows.push(String::new());
            cursor = (rows.len() - 1, 0);
        }
        (rows, cursor)
    }

    /// Display rows the prompt needs at `width` columns.
    pub fn display_rows(&self, width: usize) -> usize {
        self.wrapped(width).0.len()
    }

    /// Moves to the display row above, keeping the column. Returns false on the
    /// first row so the caller can fall back to history browsing.
    pub fn move_row_up(&mut self, width: usize) -> bool {
        let ranges = self.rows(width);
        let index = self.cursor_row(&ranges);
        if index == 0 {
            return false;
        }
        self.move_between_rows(ranges[index], ranges[index - 1]);
        true
    }

    /// Moves to the display row below, keeping the column.
    pub fn move_row_down(&mut self, width: usize) -> bool {
        let ranges = self.rows(width);
        let index = self.cursor_row(&ranges);
        if index + 1 >= ranges.len() {
            return false;
        }
        self.move_between_rows(ranges[index], ranges[index + 1]);
        true
    }

    fn move_between_rows(&mut self, from: (usize, usize), to: (usize, usize)) {
        let column = self.text[from.0..self.cursor].chars().count();
        self.cursor = to.0 + char_offset(&self.text[to.0..to.1], column);
    }

    /// Byte ranges of the display rows, one per wrapped or logical line.
    fn rows(&self, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let mut ranges = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            ranges.extend(
                segments(line, width)
                    .into_iter()
                    .map(|(start, end)| (line_start + start, line_start + end)),
            );
            line_start += line.len() + 1;
        }
        ranges
    }

    /// The display row holding the cursor. A cursor resting exactly on a soft
    /// break belongs to the row that follows it.
    fn cursor_row(&self, ranges: &[(usize, usize)]) -> usize {
        ranges
            .iter()
            .position(|(_, end)| self.cursor < *end || (self.cursor == *end && self.ends_row(*end)))
            .unwrap_or_else(|| ranges.len() - 1)
    }

    fn ends_row(&self, at: usize) -> bool {
        at == self.text.len() || self.text[at..].starts_with('\n')
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

    /// Inserts text at the cursor, made safe to edit and to draw.
    ///
    /// Pasted text arrives here whole, so this is the one place foreign content
    /// is normalised: every line ending becomes `\n`, tabs become spaces so
    /// wrapping and cursor columns agree with what is on screen, and any other
    /// control character is dropped rather than shown as a hole in the prompt.
    pub fn insert_str(&mut self, text: &str) {
        let mut cleaned = String::with_capacity(text.len());
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '\r' => {
                    characters.next_if_eq(&'\n');
                    cleaned.push('\n');
                }
                '\n' => cleaned.push('\n'),
                '\t' => cleaned.push_str("    "),
                other if other.is_control() => {}
                other => cleaned.push(other),
            }
        }
        self.text.insert_str(self.cursor, &cleaned);
        self.cursor += cleaned.len();
    }

    pub fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) -> bool {
        if range.start > range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return false;
        }
        self.text.replace_range(range.clone(), replacement);
        self.cursor = range.start + replacement.len();
        true
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

/// Byte ranges of one logical line's display rows, breaking after spaces when
/// possible and mid-word only when a word cannot fit on a row of its own.
fn segments(line: &str, width: usize) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut used = 0;
    let mut last_break = None;
    for (index, character) in line.char_indices() {
        let advance = character.to_string().width();
        if used + advance > width {
            let cut = last_break.filter(|cut| *cut > start).unwrap_or(index);
            if cut > start {
                segments.push((start, cut));
                start = cut;
                used = line[start..index].width();
            } else {
                // One character wider than the whole row: let it stand alone.
                used = 0;
            }
            last_break = None;
        }
        used += advance;
        if character == ' ' {
            last_break = Some(index + character.len_utf8());
        }
    }
    segments.push((start, line.len()));
    segments
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
        assert!(editor.move_row_up(40));
        assert!(editor.move_row_up(40));
        assert_eq!(editor.wrapped(40).1, (0, 2));
        assert!(editor.move_row_down(40));
        assert_eq!(editor.wrapped(40).1, (1, 2));
    }

    #[test]
    fn moves_between_wrapped_rows_before_reaching_history() {
        let mut editor = editor("one two three four");
        assert!(editor.move_row_up(8));
        assert_eq!(editor.wrapped(8).1, (1, 4));
        assert!(editor.move_row_up(8));
        assert!(!editor.move_row_up(8));
    }

    #[test]
    fn normalises_pasted_text() {
        let editor = editor("one\r\ntwo\rthree\tfour\u{7}");
        assert_eq!(editor.text(), "one\ntwo\nthree    four");
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
    fn soft_wraps_the_prompt_and_follows_the_cursor() {
        let mut editor = editor("hello world again");
        let (rows, cursor) = editor.wrapped(8);
        assert_eq!(rows, ["hello ", "world ", "again"]);
        assert_eq!(cursor, (2, 5));
        editor.move_line_start();
        assert_eq!(editor.wrapped(8).1, (0, 0));
    }

    #[test]
    fn puts_the_cursor_on_the_next_row_at_a_soft_break() {
        let mut editor = editor("hello world");
        for _ in 0..5 {
            editor.move_left();
        }
        assert_eq!(editor.wrapped(8).1, (1, 0));
    }

    #[test]
    fn wraps_words_wider_than_the_row() {
        let editor = editor("supercalifragilistic");
        assert_eq!(editor.wrapped(6).0, ["superc", "alifra", "gilist", "ic"]);
    }

    #[test]
    fn counts_wrapped_rows_for_the_prompt_box() {
        let editor = editor("one two three four five");
        assert_eq!(editor.display_rows(10), 3);
        assert_eq!(editor.display_rows(80), 1);
    }

    #[test]
    fn opens_a_fresh_row_when_the_cursor_fills_the_last_one() {
        let editor = editor("abcd");
        assert_eq!(
            editor.wrapped(4),
            (vec!["abcd".into(), String::new()], (1, 0))
        );
    }

    #[test]
    fn moves_across_multibyte_characters() {
        let mut editor = editor("héllo");
        editor.move_line_start();
        editor.move_right();
        editor.delete_forward();
        assert_eq!(editor.text(), "hllo");
    }

    #[test]
    fn replaces_a_multibyte_range_and_moves_the_cursor() {
        let mut editor = editor("pick @caf\u{e9} now");
        assert!(editor.replace_range(5..11, "@src/caf\u{e9}.rs"));
        assert_eq!(editor.text(), "pick @src/caf\u{e9}.rs now");
        assert_eq!(editor.cursor(), 18);
    }

    #[test]
    fn rejects_invalid_replacement_boundaries() {
        let mut editor = editor("caf\u{e9}");
        assert!(!editor.replace_range(0..4, "x"));
        assert_eq!(editor.text(), "caf\u{e9}");
        assert_eq!(editor.cursor(), 5);
    }
}
