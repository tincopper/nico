//! A strict JSON reader for the two objects a JSON Web Token carries.
//!
//! This crate deliberately has no JSON value dependency: a rejection body is built from
//! [`Part`](nico_http::Part)s, and a token's header and claims are two small objects a fixed grammar
//! reads. Writing the reader here also makes it *strict* in the way this path needs — comments,
//! trailing commas, YAML tags and duplicate keys are all refused — instead of depending on whatever a
//! general-purpose parser decides to accept.
//!
//! The reader is bounded rather than trusting: [`MAX_DEPTH`] caps nesting so a token cannot recurse
//! the stack, and every number is converted to `f64` and rejected when it is not finite.

/// Deepest nesting this reader accepts.
///
/// A JWT header is one flat object and a claim set is a flat object with at most one array of strings
/// in it, so eight levels are already generous. The bound exists because the reader is recursive.
const MAX_DEPTH: u8 = 8;

/// A JSON value.
///
/// Only the shapes a JWT can need are represented: there is no distinction between integer and
/// fractional numbers because every number a claim uses is a `NumericDate` (RFC 7519 §2), which is
/// read as a float and compared against a float clock.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A finite number.
    Number(f64),
    /// A string, with every escape resolved.
    String(String),
    /// An array of values.
    Array(Vec<Value>),
    /// An object, in document order, with unique keys.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Returns the member `key` of an object, or `None` for another shape or a missing key.
    pub(super) fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Returns the string this value is, or `None` for another shape.
    pub(super) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the number this value is, or `None` for another shape (including a non-finite one,
    /// which cannot be read in the first place).
    pub(super) fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// Returns the items of an array, or `None` for another shape.
    pub(super) fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Why a byte string is not the JSON this reader accepts.
///
/// It carries no detail on purpose: the caller answers with one fixed sentence, and describing *where*
/// a token failed to parse would be a second channel beside the one stable error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InvalidJson;

/// Reads one JSON value that must occupy the whole input.
///
/// # Errors
/// Returns [`InvalidJson`] when `bytes` is not exactly one JSON value, when a string is not valid
/// UTF-8, when a number is not finite, when an object repeats a key, or when nesting exceeds
/// [`MAX_DEPTH`].
pub(super) fn parse(bytes: &[u8]) -> Result<Value, InvalidJson> {
    let mut reader = Reader { bytes, position: 0 };
    let value = reader.value(0)?;
    reader.skip_whitespace();
    if reader.position != bytes.len() {
        return Err(InvalidJson);
    }
    Ok(value)
}

/// A cursor over the input.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    /// Reads one value, refusing to nest deeper than [`MAX_DEPTH`].
    fn value(&mut self, depth: u8) -> Result<Value, InvalidJson> {
        if depth > MAX_DEPTH {
            return Err(InvalidJson);
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(InvalidJson),
        }
    }

    /// Reads `{ "key": value, ... }`, refusing a repeated key.
    fn object(&mut self, depth: u8) -> Result<Value, InvalidJson> {
        self.expect(b'{')?;
        let mut entries: Vec<(String, Value)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            // A repeated key has two readings, so it has none: `{"alg":"HS256","alg":"none"}` must
            // not depend on which one this reader happens to keep.
            if entries.iter().any(|(name, _)| *name == key) {
                return Err(InvalidJson);
            }
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            entries.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(Value::Object(entries));
                }
                _ => return Err(InvalidJson),
            }
        }
    }

    /// Reads `[ value, ... ]`.
    fn array(&mut self, depth: u8) -> Result<Value, InvalidJson> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(InvalidJson),
            }
        }
    }

    /// Reads a quoted string, resolving escapes and rejecting an unpaired surrogate.
    fn string(&mut self) -> Result<String, InvalidJson> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(byte) = self.next() else {
                return Err(InvalidJson);
            };
            match byte {
                b'"' => {
                    // A raw byte above `0x7f` is copied unfiltered above, so the string is only a
                    // `String` once the whole sequence is valid UTF-8.
                    return String::from_utf8(out).map_err(|_error| InvalidJson);
                }
                b'\\' => self.escape(&mut out)?,
                // RFC 8259 forbids an unescaped control character; accepting one would let a token
                // smuggle a newline into a log field it is reported in.
                byte if byte < 0x20 => return Err(InvalidJson),
                byte => out.push(byte),
            }
        }
    }

    /// Reads one escape sequence, appending its UTF-8 bytes to `out`.
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), InvalidJson> {
        let Some(byte) = self.next() else {
            return Err(InvalidJson);
        };
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let character = self.unicode_escape()?;
                let mut buffer = [0_u8; 4];
                out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
            _ => return Err(InvalidJson),
        }
        Ok(())
    }

    /// Reads the four hex digits of a `\u` escape, pairing a surrogate if one starts.
    fn unicode_escape(&mut self) -> Result<char, InvalidJson> {
        let high = self.hex_quad()?;
        // A code unit outside the BMP arrives as a surrogate pair; a lone surrogate is not a
        // character and has no UTF-8 spelling, so the escape is refused rather than replaced.
        if (0xd800..0xdc00).contains(&high) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let low = self.hex_quad()?;
            if !(0xdc00..0xe000).contains(&low) {
                return Err(InvalidJson);
            }
            let combined =
                0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
            return char::from_u32(combined).ok_or(InvalidJson);
        }
        char::from_u32(u32::from(high)).ok_or(InvalidJson)
    }

    /// Reads four hexadecimal digits.
    fn hex_quad(&mut self) -> Result<u16, InvalidJson> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let Some(byte) = self.next() else {
                return Err(InvalidJson);
            };
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(InvalidJson),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    /// Reads a number per RFC 8259 and converts it to a finite `f64`.
    fn number(&mut self) -> Result<Value, InvalidJson> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        // Integer part: `0`, or a non-zero digit followed by more digits. A leading zero would make
        // `01` a number this reader accepts and no other parser reads the same way.
        match self.next() {
            Some(b'0') => {}
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(InvalidJson),
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if self.digits() == 0 {
                return Err(InvalidJson);
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.digits() == 0 {
                return Err(InvalidJson);
            }
        }
        let text =
            std::str::from_utf8(&self.bytes[start..self.position]).map_err(|_error| InvalidJson)?;
        let number: f64 = text.parse().map_err(|_error| InvalidJson)?;
        if !number.is_finite() {
            return Err(InvalidJson);
        }
        Ok(Value::Number(number))
    }

    /// Consumes one or more digits, returning how many were consumed.
    fn digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }

    /// Reads exactly one of the JSON literals.
    fn literal(&mut self, text: &[u8], value: Value) -> Result<Value, InvalidJson> {
        let end = self.position + text.len();
        if self.bytes.get(self.position..end) != Some(text) {
            return Err(InvalidJson);
        }
        self.position = end;
        Ok(value)
    }

    /// Consumes one expected byte.
    fn expect(&mut self, expected: u8) -> Result<(), InvalidJson> {
        if self.next() == Some(expected) {
            return Ok(());
        }
        Err(InvalidJson)
    }

    /// Returns the next byte without consuming it.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    /// Consumes and returns the next byte.
    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    /// Skips JSON's four whitespace characters. Everything else, including a YAML document marker, is
    /// a syntax error rather than something to tolerate.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }
}

#[cfg(test)]
mod tests;
