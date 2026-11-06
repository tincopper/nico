//! Base64 encoding for an inline image.
//!
//! The protocol carries an inline image as a `data:` URL, whose payload is the image bytes in the
//! standard alphabet (RFC 4648). Encoding is the only direction this adapter needs: a client codec
//! that decodes a data URL belongs to the protocol that sends one, not here.
//!
//! The encoder is hand-written instead of pulling in a dependency: it is three bytes into four
//! characters, the alphabet is fixed, and a crate whose only function is this table does not earn a
//! line in the workspace's dependency graph. The unit test below pins the published test vectors.

/// The standard alphabet, in sextet order.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` as standard padded base64.
pub(super) fn encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let first = u32::from(group[0]);
        let second = u32::from(group.get(1).copied().unwrap_or(0));
        let third = u32::from(group.get(2).copied().unwrap_or(0));
        let triple = (first << 16) | (second << 8) | third;
        for shift in [18, 12, 6, 0] {
            encoded.push(char::from(ALPHABET[((triple >> shift) & 0x3f) as usize]));
        }
        // The last group's missing bytes become padding, not zero sextets: `Zg==` is `f`, and
        // `ZgAA` would decode to three different bytes. Every character above is ASCII, so popping
        // one removes exactly one byte.
        match group.len() {
            1 => {
                // Two sextets stay, the third and fourth carry no input bit.
                encoded.pop();
                encoded.pop();
                encoded.push_str("==");
            }
            2 => {
                // Three sextets stay, the fourth carried no input bit.
                encoded.pop();
                encoded.push('=');
            }
            _ => {}
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode;

    /// The published vectors of RFC 4648 section 10, which fix the alphabet and the padding.
    #[test]
    fn the_published_vectors_encode() {
        for (bytes, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(bytes.as_bytes()), expected, "encoding {bytes:?}");
        }
    }

    /// Every sextet of the alphabet is reachable from arbitrary image bytes, including the three
    /// that are not ASCII letters: the encoder indexes with a bit mask, not with a character class.
    #[test]
    fn every_byte_value_encodes() {
        for (bytes, expected) in [
            (vec![0x00], "AA=="),
            (vec![0xff], "/w=="),
            (vec![0xff, 0xff], "//8="),
            (vec![0xff, 0xff, 0xff], "////"),
            (vec![0x00, 0x01, 0x02, 0x03], "AAECAw=="),
        ] {
            assert_eq!(encode(&bytes), expected, "encoding {bytes:?}");
        }
    }
}
