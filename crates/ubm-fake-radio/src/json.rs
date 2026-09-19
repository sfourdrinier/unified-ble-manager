//! Minimal deterministic JSON value model for staged-drive step lines.
//!
//! The staged driver crosses FFI boundaries as text (one JSON object per
//! scripted synthetic-radio step in, one JSON observation object out). This
//! module owns that tiny text seam with zero dependencies, so the crate
//! stays free of Tokio, serde, and any OS service: `core`/`alloc`-level
//! `std` collections only (`String`, `Vec`), mirroring the `ubm-core`
//! portability rule. Anything the parser rejects fails closed with a
//! structured [`JsonError`]; the driver renders that as a loud staged
//! observation, never a silent skip.

/// One JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

/// Why a step line failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    message: String,
    position: usize,
}

impl JsonError {
    fn new(message: &str, position: usize) -> Self {
        Self {
            message: String::from(message),
            position,
        }
    }

    /// Human-readable reason (ASCII only; safe to embed in observations).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Byte offset where parsing stopped.
    pub const fn position(&self) -> usize {
        self.position
    }
}

impl core::fmt::Display for JsonError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.position)
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) {
        self.pos = self.pos.saturating_add(1);
    }

    fn fail<T>(&self, message: &str) -> Result<T, JsonError> {
        Err(JsonError::new(message, self.pos))
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    fn expect_byte(&mut self, want: u8, what: &str) -> Result<(), JsonError> {
        match self.peek() {
            Some(got) if got == want => {
                self.bump();
                Ok(())
            }
            _ => self.fail(what),
        }
    }

    fn parse_value(&mut self) -> Result<JsonValue, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(JsonValue::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal("false", JsonValue::Bool(false)),
            Some(b'n') => self.parse_literal("null", JsonValue::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => self.fail("expected value"),
        }
    }

    fn parse_literal(&mut self, text: &str, value: JsonValue) -> Result<JsonValue, JsonError> {
        if self.bytes.len() >= self.pos.saturating_add(text.len())
            && &self.bytes[self.pos..self.pos.saturating_add(text.len())] == text.as_bytes()
        {
            self.pos = self.pos.saturating_add(text.len());
            Ok(value)
        } else {
            self.fail("invalid literal")
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        let mut digits = 0usize;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
            digits = digits.saturating_add(1);
        }
        if digits == 0 {
            return self.fail("invalid number");
        }
        if self.peek() == Some(b'.') {
            self.bump();
            let mut frac = 0usize;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
                frac = frac.saturating_add(1);
            }
            if frac == 0 {
                return self.fail("invalid number fraction");
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            let mut exp = 0usize;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
                exp = exp.saturating_add(1);
            }
            if exp == 0 {
                return self.fail("invalid number exponent");
            }
        }
        match core::str::from_utf8(&self.bytes[start..self.pos]) {
            Ok(raw) => Ok(JsonValue::Number(String::from(raw))),
            Err(_) => self.fail("invalid number encoding"),
        }
    }

    fn hex_val(byte: u8) -> Option<u32> {
        match byte {
            b'0'..=b'9' => Some(u32::from(byte - b'0')),
            b'a'..=b'f' => Some(u32::from(byte - b'a').saturating_add(10)),
            b'A'..=b'F' => Some(u32::from(byte - b'A').saturating_add(10)),
            _ => None,
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.expect_byte(b'"', "expected string")?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.fail("unterminated string"),
                Some(b'"') => {
                    self.bump();
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{0008}'),
                        Some(b'f') => out.push('\u{000C}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            self.bump();
                            let mut unit: u32 = 0;
                            for _ in 0..4 {
                                match self.peek().and_then(Self::hex_val) {
                                    Some(v) => {
                                        unit = unit.saturating_mul(16).saturating_add(v);
                                        self.bump();
                                    }
                                    None => return self.fail("invalid unicode escape"),
                                }
                            }
                            match char::from_u32(unit) {
                                Some(c) => out.push(c),
                                None => return self.fail("invalid unicode scalar"),
                            }
                            continue;
                        }
                        _ => return self.fail("invalid escape"),
                    }
                    self.bump();
                }
                Some(_) => {
                    let rest = &self.bytes[self.pos..];
                    match core::str::from_utf8(rest) {
                        Ok(text) => match text.chars().next() {
                            Some(c) => {
                                out.push(c);
                                self.pos = self.pos.saturating_add(c.len_utf8());
                            }
                            None => return self.fail("unterminated string"),
                        },
                        Err(_) => return self.fail("invalid utf-8 in string"),
                    }
                }
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, JsonError> {
        self.expect_byte(b'[', "expected array")?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(b']') => {
                    self.bump();
                    return Ok(JsonValue::Array(items));
                }
                _ => return self.fail("expected ',' or ']'"),
            }
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, JsonError> {
        self.expect_byte(b'{', "expected object")?;
        let mut fields: Vec<(String, JsonValue)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(JsonValue::Object(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return self.fail("expected object key");
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_byte(b':', "expected ':'")?;
            let value = self.parse_value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(b'}') => {
                    self.bump();
                    return Ok(JsonValue::Object(fields));
                }
                _ => return self.fail("expected ',' or '}'"),
            }
        }
    }
}

/// Parse one complete JSON document. Trailing garbage fails closed.
pub fn parse(text: &str) -> Result<JsonValue, JsonError> {
    let mut parser = Parser::new(text);
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.peek().is_some() {
        return parser.fail("trailing characters");
    }
    Ok(value)
}

impl JsonValue {
    /// Borrow one object field by name.
    pub fn field(&self, name: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Borrow as a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// Borrow as an array slice.
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Require a string field; missing/wrong-typed is a staged error.
    pub fn require_str<'b>(&'b self, name: &str) -> Result<&'b str, StepFieldError> {
        match self.field(name) {
            Some(Self::Str(text)) => Ok(text),
            Some(_) => Err(StepFieldError::wrong_type(name, "string")),
            None => Err(StepFieldError::missing(name)),
        }
    }

    /// Optional string field; present-but-wrong-typed is a staged error.
    pub fn optional_str<'b>(&'b self, name: &str) -> Result<Option<&'b str>, StepFieldError> {
        match self.field(name) {
            None | Some(Self::Null) => Ok(None),
            Some(Self::Str(text)) => Ok(Some(text)),
            Some(_) => Err(StepFieldError::wrong_type(name, "string")),
        }
    }
}

/// Why one step object field was unusable (surfaced as a staged observation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFieldError {
    detail: String,
}

impl StepFieldError {
    fn missing(name: &str) -> Self {
        let mut detail = String::from("missing-field:");
        detail.push_str(name);
        Self { detail }
    }

    fn wrong_type(name: &str, want: &str) -> Self {
        let mut detail = String::from("field-not-");
        detail.push_str(want);
        detail.push(':');
        detail.push_str(name);
        Self { detail }
    }

    /// Machine-readable detail for the staged error wire.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl core::fmt::Display for StepFieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Append a JSON-escaped string (quotes included) to `out`.
pub fn push_quoted(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                let v = c as u32;
                out.push(char::from_digit(v >> 4, 16).unwrap_or('0'));
                out.push(char::from_digit(v & 0xF, 16).unwrap_or('0'));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Render raw bytes as lowercase hex (the parity observation encoding for
/// synthetic payload bytes; never truncated, never redacted: fixtures only).
pub fn hex_of(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// Parse even-length lowercase/uppercase hex into bytes. Odd length,
/// non-hex digits, or emptiness rules follow the caller: empty encodes zero
/// bytes (a valid empty payload); malformed hex is a staged error.
pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut index = 0usize;
    while index < bytes.len() {
        let hi = Parser::hex_val(bytes[index])?;
        let lo = Parser::hex_val(bytes[index.saturating_add(1)])?;
        out.push((hi.saturating_mul(16).saturating_add(lo)) as u8);
        index = index.saturating_add(2);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_nested_documents() {
        let doc =
            "{\"step\":\"gatt.read\",\"path\":1,\"tags\":[\"a\",\"b\"],\"ok\":true,\"n\":null}";
        let parsed = parse(doc).expect("fixture must parse");
        assert_eq!(parsed.require_str("step"), Ok("gatt.read"));
        assert!(parsed.field("ok") == Some(&JsonValue::Bool(true)));
        assert!(parsed.field("n") == Some(&JsonValue::Null));
        let tags = parsed
            .field("tags")
            .and_then(JsonValue::as_array)
            .expect("array");
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn trailing_garbage_fails_closed() {
        assert!(parse("{\"a\":1} }").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex_of(&[0x00, 0xAB, 0xFF]), "00abff");
        assert_eq!(parse_hex("00abff"), Some(vec![0x00, 0xAB, 0xFF]));
        assert_eq!(parse_hex(""), Some(Vec::new()));
        assert_eq!(parse_hex("0"), None);
        assert_eq!(parse_hex("zz"), None);
    }
}
