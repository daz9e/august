//! The input box: multi-line text, a cursor, and history (kept in `~/.august/history`).

use crate::config;
use unicode_width::UnicodeWidthChar;

const HISTORY_MAX: usize = 500;

#[derive(Default)]
pub struct Editor {
    pub text: String,
    /// Byte offset into `text`.
    pub cursor: usize,
    history: Vec<String>,
    /// Position while browsing history, and the draft it replaced.
    browsing: Option<(usize, String)>,
}

fn history_path() -> std::path::PathBuf {
    config::home().join("history")
}

impl Editor {
    pub fn new() -> Self {
        let history = std::fs::read_to_string(history_path())
            .map(|s| s.lines().map(|l| l.replace("\\n", "\n")).collect())
            .unwrap_or_default();
        Self { history, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    /// Takes the text out and remembers it.
    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            let start = self.history.len().saturating_sub(HISTORY_MAX);
            let lines: Vec<String> = self.history[start..].iter().map(|h| h.replace('\n', "\\n")).collect();
            std::fs::write(history_path(), lines.join("\n") + "\n").ok();
        }
        text
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8()).unwrap_or(self.cursor)
    }

    pub fn backspace(&mut self) {
        let p = self.prev_boundary();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    pub fn delete(&mut self) {
        let n = self.next_boundary();
        self.text.replace_range(self.cursor..n, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary();
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn word_left_pos(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        trimmed.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0)
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip = after.len() - after.trim_start().len();
        let rest = &after[skip..];
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.cursor += skip + word;
    }

    /// Ctrl+W
    pub fn delete_word(&mut self) {
        let p = self.word_left_pos();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    /// Ctrl+U: delete to the start of the line.
    pub fn delete_to_start(&mut self) {
        let p = self.line_start();
        let p = if p == self.cursor && p > 0 { p - 1 } else { p };
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    /// Ctrl+K: delete to the end of the line.
    pub fn delete_to_end(&mut self) {
        let e = self.line_end();
        self.text.replace_range(self.cursor..e, "");
    }

    /// Up: previous line, or the previous history entry from the first line.
    pub fn up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return self.history_step(-1);
        }
        let col = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.cursor = Self::offset_at(&self.text, prev_start, start - 1, col);
    }

    /// Down: next line, or the next history entry from the last line.
    pub fn down(&mut self) {
        let end = self.line_end();
        if end == self.text.len() {
            return self.history_step(1);
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..].find('\n').map(|i| next_start + i).unwrap_or(self.text.len());
        self.cursor = Self::offset_at(&self.text, next_start, next_end, col);
    }

    fn offset_at(text: &str, start: usize, end: usize, col: usize) -> usize {
        text[start..end].char_indices().nth(col).map(|(i, _)| start + i).unwrap_or(end)
    }

    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let (idx, draft) = match self.browsing.take() {
            Some(b) => b,
            None if dir < 0 => (self.history.len(), self.text.clone()),
            None => return,
        };
        let next = idx as i64 + dir as i64;
        if next < 0 {
            self.browsing = Some((idx, draft));
            return;
        }
        let next = next as usize;
        if next >= self.history.len() {
            self.set(&draft);
            return;
        }
        let entry = self.history[next].clone();
        self.set(&entry);
        self.browsing = Some((next, draft));
    }

    /// Wraps the text into rows of `width` columns; returns the rows and the cursor's
    /// (row, column).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut cursor = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor >= self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cursor = (rows.len() - 1, col);
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_and_moves() {
        let mut e = Editor::default();
        e.insert("hello world");
        e.word_left();
        e.insert("big ");
        assert_eq!(e.text, "hello big world");
        e.delete_word();
        assert_eq!(e.text, "hello world");
        e.end();
        e.insert("\nsecond");
        e.up();
        assert_eq!(&e.text[..e.cursor], "hello ");
        e.down();
        assert_eq!(e.cursor, e.text.len());
        e.backspace();
        assert!(e.text.ends_with("secon"));
        let (rows, cur) = e.layout(6);
        assert_eq!(rows, vec!["hello ", "world", "secon"]);
        assert_eq!(cur, (2, 5));
    }
}
