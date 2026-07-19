use std::io::{self, Write};

use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Hard ceiling for one resolved or manually entered secret.
pub const MAX_SECRET_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SecretBytesError {
    #[error("secret exceeded the {MAX_SECRET_BYTES}-byte limit")]
    TooLarge,
    #[error("secret was not valid UTF-8")]
    InvalidUtf8,
}

/// A zeroizing byte buffer with an explicit logical limit.
///
/// The backing allocation is created at the full limit, and writes are refused before they could
/// reallocate. Unlike `Vec::capacity`, `limit` is policy rather than an allocator detail.
pub(crate) struct SensitiveBuffer {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
}

impl SensitiveBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self { bytes: Zeroizing::new(Vec::with_capacity(limit)), limit }
    }

    pub(crate) fn try_extend(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let new_len = self.bytes.len().checked_add(bytes.len()).ok_or(())?;
        if new_len > self.limit {
            return Err(());
        }
        debug_assert!(new_len <= self.bytes.capacity());
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn into_secret(self) -> Result<SecretBytes, SecretBytesError> {
        SecretBytes::from_zeroizing(self.bytes)
    }

    pub(crate) fn into_bytes(self) -> Zeroizing<Vec<u8>> {
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

    pub fn from_utf8(bytes: Vec<u8>) -> Result<Self, SecretBytesError> {
        Self::from_zeroizing(Zeroizing::new(bytes))
    }

    fn from_zeroizing(bytes: Zeroizing<Vec<u8>>) -> Result<Self, SecretBytesError> {
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(SecretBytesError::TooLarge);
        }
        std::str::from_utf8(&bytes).map_err(|_| SecretBytesError::InvalidUtf8)?;
        Ok(Self { bytes })
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

    pub(crate) fn insert_char(&mut self, cursor: usize, ch: char) -> bool {
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

    pub(crate) fn remove_range(&mut self, range: std::ops::Range<usize>) {
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

    pub(crate) fn clear(&mut self) {
        self.bytes.zeroize();
    }

    #[cfg(test)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

impl Default for SecretBytes {
    fn default() -> Self {
        Self::new()
    }
}
