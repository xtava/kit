//! Minimal base64 — the engine decodes protocol payloads (screenshots, inline source maps) and
//! encodes evidence bodies. One implementation for the whole crate; ~40 lines beats a dependency.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn value(ch: char) -> Option<u8> {
    match ch {
        'A'..='Z' => Some(ch as u8 - b'A'),
        'a'..='z' => Some(ch as u8 - b'a' + 26),
        '0'..='9' => Some(ch as u8 - b'0' + 52),
        '+' => Some(62),
        '/' => Some(63),
        _ => None,
    }
}

pub(crate) fn encode(bytes: &[u8]) -> String {
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

/// Decode, tolerating missing padding and embedded whitespace (inline source maps wrap lines).
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in text.chars().filter(|ch| *ch != '=' && !ch.is_whitespace()) {
        let value = value(ch).ok_or_else(|| format!("bad base64 char '{ch}'"))?;
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_decode_round_trip_every_padding_length() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\x89PNG\r\n\x1a\n\x00\xff"] {
            let encoded = encode(bytes);
            assert_eq!(decode(&encoded).unwrap(), bytes, "round trip of {bytes:?} via {encoded}");
        }
    }

    #[test]
    fn decode_tolerates_unpadded_and_wrapped_input() {
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        assert_eq!(decode("Zm8").unwrap(), b"fo");
        assert_eq!(decode("Zm\n9v").unwrap(), b"foo");
        assert!(decode("Zm9v!").is_err());
    }
}
