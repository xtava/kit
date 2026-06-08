//! System-clipboard copy via OSC 52 — the terminal-native escape, so it works locally and over SSH
//! with no platform clipboard dependency. The terminal must allow clipboard writes (most modern ones
//! do; a few need it enabled).
//!
//! This module only *builds* the escape sequence. Writing it is [`crate::tui::Session::copy`]'s job,
//! so the bytes go through the same terminal handle ratatui draws on and never race a redraw.

/// The OSC 52 sequence that asks the terminal to set the clipboard to `text`:
/// `ESC ] 52 ; c ; <base64> BEL`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let triple = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for (shift, present) in [(18, true), (12, true), (6, chunk.len() > 1), (0, chunk.len() > 2)]
        {
            out.push(if present {
                ALPHABET[(triple >> shift & 0x3f) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, osc52};

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_base64_in_the_escape() {
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
    }
}
