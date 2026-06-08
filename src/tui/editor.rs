use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single-line text input with a UTF-8-aware cursor. The shared editing core every TUI input uses.
#[derive(Clone, Debug, Default)]
pub struct LineEditor {
    value: String,
    cursor: usize,
}

impl LineEditor {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, value: String) {
        self.value = value;
        self.cursor = self.value.len();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    pub fn apply_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert(ch);
            }
            _ => {}
        }
    }

    pub fn insert(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = previous_boundary(&self.value, self.cursor);
        self.value.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = next_boundary(&self.value, self.cursor);
        self.value.replace_range(self.cursor..next, "");
    }

    fn move_left(&mut self) {
        self.cursor = previous_boundary(&self.value, self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = next_boundary(&self.value, self.cursor);
    }
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor].char_indices().next_back().map(|(index, _)| index).unwrap_or(0)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..].chars().next().map(|ch| cursor + ch.len_utf8()).unwrap_or(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_deletes_mid_line() {
        let mut input = LineEditor::default();
        input.insert('a');
        input.insert('c');
        input.move_left();
        input.insert('b');

        assert_eq!(input.value(), "abc");
        assert_eq!(input.cursor(), 2);

        input.delete();
        assert_eq!(input.value(), "ab");

        input.backspace();
        assert_eq!(input.value(), "a");
    }

    #[test]
    fn moves_by_utf8_boundaries() {
        let mut input = LineEditor::default();
        input.insert('é');
        input.insert('x');
        input.move_left();

        assert_eq!(input.cursor(), "é".len());
        input.backspace();
        assert_eq!(input.value(), "x");
    }
}
