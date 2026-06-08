//! Copy to the system clipboard with OSC 52 — the terminal-native escape, so it works locally and
//! over SSH with no platform clipboard dependency. The terminal must allow clipboard writes (most
//! modern ones do; a few need it enabled).

use std::io::{self, Write};

/// Write `text` to the clipboard via `ESC ] 52 ; c ; <base64> BEL`.
pub fn copy(text: &str) -> io::Result<()> {
    let sequence = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let triple = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for (shift, present) in [(18, true), (12, true), (6, chunk.len() > 1), (0, chunk.len() > 2)] {
            out.push(if present { ALPHABET[(triple >> shift & 0x3f) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
