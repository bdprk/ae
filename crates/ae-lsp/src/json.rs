//! RFC 8259 JSON parser and serializer with UTF-16 surrogate pair decoding.

use std::collections::BTreeMap;
use std::fmt;

const MAX_NESTING_DEPTH: usize = 256;

/// Structured error produced when parsing invalid JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub kind: JsonErrorKind,
    /// Byte offset in the input string where the error occurred.
    pub offset: usize,
    pub message: String,
}

impl JsonError {
    fn new(kind: JsonErrorKind, offset: usize, message: impl Into<String>) -> Self {
        Self {
            kind,
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

/// Classification of [`JsonError`] failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonErrorKind {
    UnexpectedEof,
    UnexpectedToken,
    TrailingCharacters,
    InvalidNumber,
    InvalidEscape,
    LoneSurrogate,
    UnescapedControlChar,
    DepthExceeded,
}

/// Dynamically typed RFC 8259 JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    /// Parse a complete RFC 8259 JSON document.
    pub fn parse(input: &str) -> Result<Self, JsonError> {
        let mut parser = Parser::new(input);
        let value = parser.parse_value(0)?;
        parser.skip_ws();
        if !parser.is_eof() {
            return Err(JsonError::new(
                JsonErrorKind::TrailingCharacters,
                parser.pos,
                "unexpected trailing characters after JSON value",
            ));
        }
        Ok(value)
    }

    /// Serialize this value to compact RFC 8259 JSON.
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    /// Return `true` if this value is `JsonValue::Null`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Look up a field on a JSON object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// Return the boolean if this value is `JsonValue::Bool`.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Return the string slice if this value is `JsonValue::String`.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Return the number as `f64` if this value is `JsonValue::Number`.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Return the number as `i64` if this value is an exact integer in `i64` range.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n)
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= (i64::MIN as f64)
                    && *n <= (i64::MAX as f64) =>
            {
                Some(*n as i64)
            }
            _ => None,
        }
    }

    /// Return the number as `u64` if this value is a non-negative integer in `u64` range.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(n)
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= 0.0
                    && *n <= (u64::MAX as f64) =>
            {
                Some(*n as u64)
            }
            _ => None,
        }
    }

    /// Return the slice if this value is `JsonValue::Array`.
    #[must_use]
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Return the map if this value is `JsonValue::Object`.
    #[must_use]
    pub fn as_object(&self) -> Option<&BTreeMap<String, JsonValue>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(n) => {
                if !n.is_finite() {
                    out.push_str("null");
                } else if *n == 0.0 {
                    out.push('0');
                } else if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
                    out.push_str(&format!("{}", *n as i64));
                } else {
                    out.push_str(&format!("{n}"));
                }
            }
            Self::String(s) => write_escaped_string(s, out),
            Self::Array(arr) => {
                out.push('[');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped_string(k, out);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.serialize())
    }
}

fn write_escaped_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek_byte() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(JsonError::new(
                JsonErrorKind::DepthExceeded,
                self.pos,
                "JSON nesting depth exceeded maximum",
            ));
        }

        self.skip_ws();
        let Some(b) = self.peek_byte() else {
            return Err(JsonError::new(
                JsonErrorKind::UnexpectedEof,
                self.pos,
                "unexpected end of JSON input",
            ));
        };

        match b {
            b'n' => self.parse_literal(b"null", JsonValue::Null),
            b't' => self.parse_literal(b"true", JsonValue::Bool(true)),
            b'f' => self.parse_literal(b"false", JsonValue::Bool(false)),
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(JsonError::new(
                JsonErrorKind::UnexpectedToken,
                self.pos,
                format!("unexpected byte 0x{b:02x}"),
            )),
        }
    }

    fn parse_literal(&mut self, expected: &[u8], value: JsonValue) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.bytes[start..].starts_with(expected) {
            self.pos += expected.len();
            Ok(value)
        } else {
            Err(JsonError::new(
                JsonErrorKind::UnexpectedToken,
                start,
                "invalid literal token",
            ))
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        // Skip opening quote.
        let Some(b'"') = self.peek_byte() else {
            return Err(JsonError::new(
                JsonErrorKind::UnexpectedToken,
                self.pos,
                "expected '\"'",
            ));
        };
        self.pos += 1;

        let mut out = String::new();
        loop {
            let Some(b) = self.peek_byte() else {
                return Err(JsonError::new(
                    JsonErrorKind::UnexpectedEof,
                    self.pos,
                    "unterminated JSON string",
                ));
            };

            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }

            if b == b'\\' {
                let esc_pos = self.pos;
                self.pos += 1;
                let Some(esc) = self.peek_byte() else {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedEof,
                        self.pos,
                        "unterminated escape sequence",
                    ));
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0c'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4(esc_pos)?;
                        let ch = self.decode_utf16_unit(code, esc_pos)?;
                        out.push(ch);
                    }
                    _ => {
                        return Err(JsonError::new(
                            JsonErrorKind::InvalidEscape,
                            esc_pos,
                            format!("invalid escape '\\{}'", esc as char),
                        ));
                    }
                }
                continue;
            }

            if b < 0x20 {
                return Err(JsonError::new(
                    JsonErrorKind::UnescapedControlChar,
                    self.pos,
                    format!("unescaped control character 0x{b:02x} in string"),
                ));
            }

            if b < 0x80 {
                out.push(b as char);
                self.pos += 1;
            } else {
                let rest = &self.input[self.pos..];
                let Some(ch) = rest.chars().next() else {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedEof,
                        self.pos,
                        "unexpected end of UTF-8 sequence",
                    ));
                };
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
    }

    fn parse_hex4(&mut self, err_pos: usize) -> Result<u16, JsonError> {
        let Some(slice) = self.bytes.get(self.pos..self.pos + 4) else {
            return Err(JsonError::new(
                JsonErrorKind::InvalidEscape,
                err_pos,
                "incomplete \\uXXXX escape",
            ));
        };
        let mut val: u16 = 0;
        for &b in slice {
            let digit = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a' + 10),
                b'A'..=b'F' => u16::from(b - b'A' + 10),
                _ => {
                    return Err(JsonError::new(
                        JsonErrorKind::InvalidEscape,
                        err_pos,
                        "non-hex digit in \\uXXXX escape",
                    ));
                }
            };
            val = (val << 4) | digit;
        }
        self.pos += 4;
        Ok(val)
    }

    fn decode_utf16_unit(&mut self, code: u16, esc_pos: usize) -> Result<char, JsonError> {
        if (0xD800..=0xDBFF).contains(&code) {
            // High surrogate: must be immediately followed by \uXXXX low surrogate.
            let pair_pos = self.pos;
            if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                return Err(JsonError::new(
                    JsonErrorKind::LoneSurrogate,
                    esc_pos,
                    format!("lone high surrogate \\u{code:04X}"),
                ));
            }
            self.pos += 2;
            let low = self.parse_hex4(pair_pos)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(JsonError::new(
                    JsonErrorKind::LoneSurrogate,
                    esc_pos,
                    format!("high surrogate \\u{code:04X} not followed by low surrogate"),
                ));
            }
            let combined =
                0x10000 + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
            char::from_u32(combined).ok_or_else(|| {
                JsonError::new(
                    JsonErrorKind::LoneSurrogate,
                    esc_pos,
                    "invalid surrogate pair code point",
                )
            })
        } else if (0xDC00..=0xDFFF).contains(&code) {
            Err(JsonError::new(
                JsonErrorKind::LoneSurrogate,
                esc_pos,
                format!("lone low surrogate \\u{code:04X}"),
            ))
        } else {
            char::from_u32(u32::from(code)).ok_or_else(|| {
                JsonError::new(
                    JsonErrorKind::InvalidEscape,
                    esc_pos,
                    "invalid unicode scalar value",
                )
            })
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;

        if self.peek_byte() == Some(b'-') {
            self.pos += 1;
        }

        match self.peek_byte() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                    return Err(JsonError::new(
                        JsonErrorKind::InvalidNumber,
                        start,
                        "leading zero in JSON number",
                    ));
                }
            }
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => {
                return Err(JsonError::new(
                    JsonErrorKind::InvalidNumber,
                    start,
                    "expected digit in JSON number",
                ));
            }
        }

        if self.peek_byte() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                return Err(JsonError::new(
                    JsonErrorKind::InvalidNumber,
                    start,
                    "expected digit after decimal point",
                ));
            }
            while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                return Err(JsonError::new(
                    JsonErrorKind::InvalidNumber,
                    start,
                    "expected digit in exponent",
                ));
            }
            while matches!(self.peek_byte(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let num = self.input[start..self.pos].parse::<f64>().map_err(|_| {
            JsonError::new(JsonErrorKind::InvalidNumber, start, "invalid JSON number")
        })?;
        if !num.is_finite() {
            return Err(JsonError::new(
                JsonErrorKind::InvalidNumber,
                start,
                "JSON number out of f64 range",
            ));
        }
        Ok(JsonValue::Number(num))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1; // '['
        let mut elements = Vec::new();
        self.skip_ws();
        if self.peek_byte() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(elements));
        }

        loop {
            let val = self.parse_value(depth)?;
            elements.push(val);
            self.skip_ws();
            match self.peek_byte() {
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(elements));
                }
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b) => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedToken,
                        self.pos,
                        format!("expected ',' or ']', found '{}'", b as char),
                    ));
                }
                None => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedEof,
                        self.pos,
                        "unterminated JSON array",
                    ));
                }
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1; // '{'
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek_byte() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }

        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            match self.peek_byte() {
                Some(b':') => self.pos += 1,
                Some(b) => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedToken,
                        self.pos,
                        format!("expected ':', found '{}'", b as char),
                    ));
                }
                None => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedEof,
                        self.pos,
                        "unterminated JSON object",
                    ));
                }
            }
            let val = self.parse_value(depth)?;
            map.insert(key, val);
            self.skip_ws();
            match self.peek_byte() {
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(map));
                }
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b) => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedToken,
                        self.pos,
                        format!("expected ',' or '}}', found '{}'", b as char),
                    ));
                }
                None => {
                    return Err(JsonError::new(
                        JsonErrorKind::UnexpectedEof,
                        self.pos,
                        "unterminated JSON object",
                    ));
                }
            }
        }
    }
}
