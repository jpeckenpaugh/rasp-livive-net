//! A small, dependency-free JSON reader for the fixture manifest.
//!
//! `rasp batch` needs exactly one thing from JSON: the clip list in
//! `fixtures/manifest.json`. Adding `serde`/`serde_json` for that would pull a
//! proc-macro build into a crate that is adjacent to the wasm bundle, and the
//! CLI's hard rule is no new external dependencies (PLAN P-06). This module is
//! therefore a deliberately small recursive-descent parser.
//!
//! It is **not** a general-purpose JSON library: it enforces the syntax, a
//! nesting limit (so a hostile file cannot overflow the stack), and it keeps
//! object entries in file order (no hash map, so behaviour is deterministic).
//! Duplicate keys resolve to the first occurrence, which is documented and
//! irrelevant for the generated manifest.
//!
//! Numbers are `f64`; callers that need an integer (`sampleCount`) convert with
//! [`Value::as_u64`], which rejects negatives and non-integers instead of
//! silently truncating.

use std::fmt::Write as _;

/// Maximum array/object nesting. The fixture manifest needs 4; 64 is generous
/// and keeps recursion bounded far below any realistic stack limit.
const MAX_DEPTH: usize = 64;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// Any number (JSON has one number type).
    Number(f64),
    /// A string with escapes already decoded.
    String(String),
    /// An array in file order.
    Array(Vec<Value>),
    /// Object entries in file order.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The value for `key` when this is an object; `None` otherwise.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// This value as a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    /// This value as an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// This value as a non-negative integer that fits `u64`.
    ///
    /// A fractional or negative number is rejected rather than truncated: a
    /// manifest with `"sampleCount": 47528.5` is a broken manifest, not a
    /// rounding opportunity.
    pub fn as_u64(&self) -> Option<u64> {
        let value = self.as_f64()?;
        if value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
            Some(value as u64)
        } else {
            None
        }
    }

    /// This value as a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(value) if value.is_finite() => Some(*value),
            _ => None,
        }
    }
}

/// Parse one JSON document.
///
/// Returns a one-line error with the byte offset on malformed input; never
/// panics, including on invalid UTF-8 (input is already a `&str`).
pub fn parse(text: &str) -> Result<Value, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("trailing data after the document"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> String {
        format!("JSON at byte {}: {message}", self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        match self.bump() {
            Some(found) if found == byte => Ok(()),
            Some(found) => Err(self.error(&format!(
                "expected '{}', found '{}'",
                char::from(byte),
                char::from(found)
            ))),
            None => Err(self.error(&format!(
                "expected '{}', found end of input",
                char::from(byte)
            ))),
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting is too deep"));
        }
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(Value::String),
            Some(b't') => self.parse_literal("true", Value::Bool(true)),
            Some(b'f') => self.parse_literal("false", Value::Bool(false)),
            Some(b'n') => self.parse_literal("null", Value::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(byte) => Err(self.error(&format!("unexpected byte '{}'", char::from(byte)))),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn parse_literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        let end = self.pos + word.len();
        if self.bytes.get(self.pos..end) == Some(word.as_bytes()) {
            self.pos = end;
            Ok(value)
        } else {
            Err(self.error(&format!("expected '{word}'")))
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            entries.push((key, value));
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                Some(byte) => {
                    return Err(self.error(&format!(
                        "expected ',' or '}}', found '{}'",
                        char::from(byte)
                    )))
                }
                None => return Err(self.error("unterminated object")),
            }
        }
        Ok(Value::Object(entries))
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                Some(byte) => {
                    return Err(self.error(&format!(
                        "expected ',' or ']', found '{}'",
                        char::from(byte)
                    )))
                }
                None => return Err(self.error("unterminated array")),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self
                .bump()
                .ok_or_else(|| self.error("unterminated string"))?;
            match byte {
                b'"' => return Ok(out),
                b'\\' => self.parse_escape(&mut out)?,
                0x00..=0x1f => {
                    return Err(self.error("raw control character in a string"));
                }
                _ => {
                    // Re-decode one UTF-8 scalar from the input instead of
                    // pushing raw bytes: the source is valid UTF-8, so this is
                    // safe and keeps the output a valid `String`.
                    let start = self.pos - 1;
                    let width = utf8_width(byte);
                    let end = start + width;
                    let chunk = self
                        .bytes
                        .get(start..end)
                        .ok_or_else(|| self.error("truncated UTF-8 sequence"))?;
                    let text = std::str::from_utf8(chunk)
                        .map_err(|_| self.error("invalid UTF-8 sequence"))?;
                    out.push_str(text);
                    self.pos = end;
                }
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        let escape = self
            .bump()
            .ok_or_else(|| self.error("unterminated escape"))?;
        match escape {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.parse_hex4()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    // High surrogate: must be followed by `\uDC00..=DFFF`.
                    if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                        return Err(self.error("lone high surrogate in a \\u escape"));
                    }
                    let second = self.parse_hex4()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(self.error("invalid low surrogate in a \\u escape"));
                    }
                    0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(self.error("lone low surrogate in a \\u escape"));
                } else {
                    first
                };
                let character = char::from_u32(scalar)
                    .ok_or_else(|| self.error("\\u escape is not a Unicode scalar value"))?;
                out.push(character);
            }
            other => return Err(self.error(&format!("unknown escape '\\{}'", char::from(other)))),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let digit = self
                .bump()
                .ok_or_else(|| self.error("truncated \\u escape"))?;
            let nibble = match digit {
                b'0'..=b'9' => u32::from(digit - b'0'),
                b'a'..=b'f' => u32::from(digit - b'a') + 10,
                b'A'..=b'F' => u32::from(digit - b'A') + 10,
                _ => return Err(self.error("invalid hex digit in a \\u escape")),
            };
            value = value * 16 + nibble;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("leading zero in a number"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit in the exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid number"))?;
        let value: f64 = text.parse().map_err(|_| self.error("invalid number"))?;
        if !value.is_finite() {
            return Err(self.error("number is not finite"));
        }
        Ok(Value::Number(value))
    }
}

/// UTF-8 sequence length from its first byte. Continuation bytes are only
/// reached after their lead byte, since the source is a valid `&str`.
fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Escape a string for JSON output (quotes and control characters).
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                // `write!` to a String cannot fail; the result is discarded.
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_manifest_shape_and_escapes() {
        let text = r#"{"clips":[{"name":"a__b","sampleCount":12,"ok":true,
            "note":"quote \" slash \\ tab \t unicode \u00e9"}],"n":-1.5e2}"#;
        let value = parse(text).expect("valid JSON");
        let clips = value.get("clips").and_then(Value::as_array).expect("array");
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].get("name").and_then(Value::as_str), Some("a__b"));
        assert_eq!(
            clips[0].get("sampleCount").and_then(Value::as_u64),
            Some(12)
        );
        assert_eq!(clips[0].get("ok"), Some(&Value::Bool(true)));
        assert_eq!(
            clips[0].get("note").and_then(Value::as_str),
            Some("quote \" slash \\ tab \t unicode é")
        );
        assert_eq!(value.get("n").and_then(Value::as_f64), Some(-150.0));
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for text in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "\"unterminated",
            "{\"a\":1} trailing",
            "\"\\uD800\"",
            "1e999",
            "{\"a\":01}",
        ] {
            assert!(parse(text).is_err(), "expected an error for {text:?}");
        }
        // Deep nesting is reported rather than recursing without bound.
        let deep = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
        assert!(parse(&deep).is_err());
        // A fractional count is not silently truncated.
        let value = parse(r#"{"n":1.5}"#).expect("valid JSON");
        assert_eq!(value.get("n").and_then(Value::as_u64), None);
    }

    #[test]
    fn escaping_round_trips() {
        let raw = "a\"b\\c\nd\te\u{1}é";
        let encoded = escape(raw);
        let value = parse(&encoded).expect("escaped text parses");
        assert_eq!(value.as_str(), Some(raw));
    }
}
