use std::io::{self, Write};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::{Zeroize, Zeroizing};

/// Hard ceiling for a single field read or manually entered password.
///
/// Preallocating the complete buffer prevents allocator growth from leaving superseded plaintext
/// allocations behind. Four KiB is intentionally far above a practical login-password length.
pub const MAX_SECRET_BYTES: usize = 4 * 1024;

/// A zeroizing byte buffer with an explicit logical limit.
///
/// The backing allocation is created at the full limit, and writes are refused before they could
/// reallocate. Unlike `Vec::capacity`, `limit` is policy rather than an allocator detail.
pub(super) struct SensitiveBuffer {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
}

impl SensitiveBuffer {
    pub fn new(limit: usize) -> Self {
        Self { bytes: Zeroizing::new(Vec::with_capacity(limit)), limit }
    }

    pub fn try_extend(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let new_len = self.bytes.len().checked_add(bytes.len()).ok_or(())?;
        if new_len > self.limit {
            return Err(());
        }
        debug_assert!(new_len <= self.bytes.capacity());
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn into_secret(self) -> Result<SecretBytes, ()> {
        if self.bytes.len() > MAX_SECRET_BYTES || std::str::from_utf8(&self.bytes).is_err() {
            return Err(());
        }
        Ok(SecretBytes { bytes: self.bytes })
    }

    pub fn into_bytes(self) -> Zeroizing<Vec<u8>> {
        self.bytes
    }
}

impl Write for SensitiveBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.try_extend(bytes)
            .map_err(|()| io::Error::other("sensitive buffer exceeded its fixed limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A bounded, zeroizing UTF-8 buffer for one secret value.
///
/// Deliberately does not implement `Clone`, `Debug`, `Display`, `Serialize`, or `Deserialize`.
pub struct SecretBytes {
    bytes: Zeroizing<Vec<u8>>,
}

impl SecretBytes {
    pub fn new() -> Self {
        Self { bytes: Zeroizing::new(Vec::with_capacity(MAX_SECRET_BYTES)) }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_str(&self) -> &str {
        // Construction and mutation preserve UTF-8; raw subprocess bytes are validated before
        // `SecretBytes` is constructed.
        std::str::from_utf8(&self.bytes).expect("SecretBytes UTF-8 invariant")
    }

    fn insert_char(&mut self, cursor: usize, ch: char) -> bool {
        let mut encoded = Zeroizing::new([0_u8; 4]);
        let encoded_len = ch.encode_utf8(&mut encoded[..]).len();
        let old_len = self.bytes.len();
        let Some(new_len) = old_len.checked_add(encoded_len) else { return false };
        if new_len > MAX_SECRET_BYTES {
            return false;
        }

        self.bytes.resize(new_len, 0);
        self.bytes.copy_within(cursor..old_len, cursor + encoded_len);
        self.bytes[cursor..cursor + encoded_len].copy_from_slice(&encoded[..encoded_len]);
        true
    }

    fn remove_range(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        let old_len = self.bytes.len();
        let removed = range.end - range.start;
        self.bytes.copy_within(range.end..old_len, range.start);
        let new_len = old_len - removed;
        self.bytes[new_len..old_len].zeroize();
        self.bytes.truncate(new_len);
    }

    fn clear(&mut self) {
        self.bytes.zeroize();
    }
}

impl Default for SecretBytes {
    fn default() -> Self {
        Self::new()
    }
}

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
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if self.value.insert_char(self.cursor, ch) {
                    self.cursor += ch.len_utf8();
                }
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

    #[test]
    fn take_moves_the_secret_and_replaces_it_with_a_preallocated_buffer() {
        let mut input = SensitiveInput::default();
        input.apply_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(input.concealed(), "••");

        let value = input.take();
        assert_eq!(value.as_str(), "sé");
        assert!(input.is_empty());
        assert_eq!(input.value.bytes.capacity(), MAX_SECRET_BYTES);
    }

    #[test]
    fn edits_do_not_reallocate_the_secret_buffer() {
        let mut input = SensitiveInput::default();
        let initial_pointer = input.value.bytes.as_ptr();

        for _ in 0..MAX_SECRET_BYTES {
            input.apply_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        }
        input.apply_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

        assert_eq!(input.value.bytes.as_ptr(), initial_pointer);
        assert_eq!(input.value.bytes.capacity(), MAX_SECRET_BYTES);
        assert_eq!(input.value.len(), MAX_SECRET_BYTES - 1);
    }

    #[test]
    fn sensitive_buffer_enforces_its_logical_limit_without_reallocation() {
        let mut buffer = SensitiveBuffer::new(MAX_SECRET_BYTES);
        let initial_pointer = buffer.bytes.as_ptr();

        buffer.try_extend(&vec![b'x'; MAX_SECRET_BYTES]).unwrap();

        assert_eq!(buffer.bytes.as_ptr(), initial_pointer);
        assert!(buffer.try_extend(b"x").is_err());
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
