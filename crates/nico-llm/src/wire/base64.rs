//! Base64 in both directions: the standard alphabet, padded (RFC 4648 section 4).
//!
//! An inline image travels as the payload of a `data:` URL, and the exchange needs both directions:
//! the provider adapter writes one, the client codec reads one. The codec is hand-written instead of
//! pulling in a dependency: it is three bytes into four characters, the alphabet is fixed, and a crate
//! whose only function is this table does not earn a line in the workspace's dependency graph.

/// The standard alphabet, in sextet order.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why a base64 payload could not be read.
///
/// Each variant names the rule that broke, never what the payload held: a refusal reaches a client
/// answer and a log record, and an inline payload is the caller's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Base64Error {
    /// A character outside the standard alphabet.
    #[error("the payload carries a character outside the base64 alphabet")]
    Character,
    /// The payload's length is not a multiple of four.
    #[error("the payload length is not a multiple of four")]
    Length,
    /// `=` appears anywhere but at the very end of the payload, or more than twice.
    #[error("the payload's padding is not at its end")]
    Padding,
}

/// Encodes `bytes` as standard padded base64.
pub(crate) fn encode(bytes: &[u8]) -> String {
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

/// Decodes standard padded base64.
///
/// The padding is required: `Zg` and `Zg==` are not two spellings of one payload here, because
/// [`encode`] always writes it and a lenient decoder would accept a length no encoder produces. Sextet
/// bits past the last whole byte are ignored, matching an encoder that always writes them as zero.
///
/// # Errors
/// Returns [`Base64Error::Length`] when the length is not a multiple of four,
/// [`Base64Error::Character`] for a character outside the alphabet, and [`Base64Error::Padding`] for
/// `=` in the middle of the payload or more than two of them.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(Base64Error::Length);
    }
    let groups = bytes.len() / 4;
    let mut decoded = Vec::with_capacity(groups * 3);
    for (index, group) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == groups;
        let padding = group.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return Err(Base64Error::Padding);
        }
        let mut triple = 0u32;
        for (position, byte) in group.iter().take(4 - padding).enumerate() {
            triple |= sextet(*byte)? << (18 - 6 * position);
        }
        for offset in 0..3 - padding {
            decoded.push(((triple >> (16 - 8 * offset)) & 0xff) as u8);
        }
    }
    Ok(decoded)
}

/// Returns the sextet a base64 character stands for.
fn sextet(byte: u8) -> Result<u32, Base64Error> {
    match byte {
        b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
        b'a'..=b'z' => Ok(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Ok(u32::from(byte - b'0') + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Err(Base64Error::Padding),
        _ => Err(Base64Error::Character),
    }
}

#[cfg(test)]
mod tests {
    use super::{Base64Error, decode, encode};

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

    /// Every sextet of the alphabet is reachable from arbitrary image bytes, including the two
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

    /// Decoding is the inverse of encoding, including the padding every input length produces.
    #[test]
    fn every_encoded_length_decodes_back() {
        for bytes in [
            Vec::new(),
            vec![0x00],
            b"f".to_vec(),
            vec![0xff, 0x00, 0x80],
            (0..=255u8).collect::<Vec<_>>(),
            vec![0xff; 512],
        ] {
            assert_eq!(
                decode(&encode(&bytes)),
                Ok(bytes.clone()),
                "round trip for {} bytes",
                bytes.len()
            );
        }
    }

    /// The published vectors decode, so the alphabet is read the same way it is written.
    #[test]
    fn the_published_vectors_decode() {
        for (text, expected) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(
                decode(text),
                Ok(expected.as_bytes().to_vec()),
                "decoding {text:?}"
            );
        }
    }

    #[test]
    fn a_missing_padding_is_a_length_failure() {
        for text in ["Zg", "Zm8", "Zm9vYg", "AAA"] {
            assert_eq!(decode(text), Err(Base64Error::Length), "decoding {text:?}");
        }
    }

    #[test]
    fn a_character_outside_the_alphabet_is_refused() {
        for text in ["MARKER**", "Zg-_", "Zg!=", "ZM~~"] {
            assert_eq!(
                decode(text),
                Err(Base64Error::Character),
                "decoding {text:?}"
            );
        }
    }

    /// `=` is only a suffix: in the middle or more than twice it is padding where padding cannot be.
    #[test]
    fn padding_that_is_not_a_suffix_is_refused() {
        for text in ["Zg=A", "Z===", "====", "AA=A", "Zg==A==="] {
            assert_eq!(decode(text), Err(Base64Error::Padding), "decoding {text:?}");
        }
    }

    /// A refusal quotes the rule, never the payload, because the payload is the caller's image.
    #[test]
    fn a_refusal_does_not_quote_the_payload() {
        for error in [
            decode("MARKER**").expect_err("illegal characters"),
            decode("MARKERA").expect_err("a missing pad group"),
            decode("MARKER=A").expect_err("padding in the middle"),
        ] {
            let rendered = error.to_string();
            assert!(
                !rendered.contains("MARKER"),
                "the payload leaked: {rendered}"
            );
        }
    }
}
