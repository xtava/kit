use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::onepassword::SecretBytes;

/// A deliberately non-`Clone`, non-`Debug` line editor for secret input.
#[derive(Default)]
pub struct SensitiveInput {
    value: SecretBytes,
    cursor: usize,
}

impl SensitiveInput {
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn concealed(&self) -> String {
        "•".repeat(self.value.as_str().chars().count())
    }

    pub fn take(&mut self) -> SecretBytes {
        self.cursor = 0;
        std::mem::take(&mut self.value)
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    pub fn apply_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left => self.cursor = previous_boundary(self.value.as_str(), self.cursor),
            KeyCode::Right => self.cursor = next_boundary(self.value.as_str(), self.cursor),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = previous_boundary(self.value.as_str(), self.cursor);
                self.value.remove_range(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < self.value.len() => {
                let next = next_boundary(self.value.as_str(), self.cursor);
                self.value.remove_range(self.cursor..next);
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.value.insert_char(self.cursor, ch) =>
            {
                self.cursor += ch.len_utf8();
            }
            _ => {}
        }
    }
}

impl Drop for SensitiveInput {
    fn drop(&mut self) {
        self.clear();
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
    use crate::onepassword::MAX_SECRET_BYTES;

    #[test]
    fn take_moves_the_secret_and_replaces_it_with_a_preallocated_buffer() {
        let mut input = SensitiveInput::default();
        input.apply_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(input.concealed(), "••");

        let value = input.take();
        assert_eq!(value.as_str(), "sé");
        assert!(input.is_empty());
        assert_eq!(input.value.capacity(), MAX_SECRET_BYTES);
    }

    #[test]
    fn edits_do_not_reallocate_the_secret_buffer() {
        let mut input = SensitiveInput::default();
        let initial_pointer = input.value.as_ptr();

        for _ in 0..MAX_SECRET_BYTES {
            input.apply_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        }
        input.apply_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

        assert_eq!(input.value.as_ptr(), initial_pointer);
        assert_eq!(input.value.capacity(), MAX_SECRET_BYTES);
        assert_eq!(input.value.len(), MAX_SECRET_BYTES - 1);
    }

    #[test]
    fn deletion_preserves_utf8_boundaries() {
        let mut input = SensitiveInput::default();
        for ch in ['a', 'é', 'z'] {
            input.apply_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        input.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.value.as_str(), "az");
    }
}
