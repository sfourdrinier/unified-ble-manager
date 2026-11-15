//! Rust half of wire revision `ubm-mobile-wire/3` (the TS half is
//! `src/backends/reactnative/rust-core-wire.ts`; golden vectors bind the
//! two, see `docs/MOBILE_RUST_WIRE.md`).
//!
//! * Args arrive as one JSON object text, bounded to
//!   [`MAX_WIRE_TEXT_BYTES`] before parsing. Key sets are exact: a missing
//!   or unknown key is `argument.invalid` before any effect.
//! * Bytes cross only as strict RFC 4648 §4 padded base64 in `…B64`
//!   fields; length is checked against [`MAX_BASE64_LENGTH`] before any
//!   decode or allocation.
//! * Integers are safe integers (≤ 2^53−1), never fractions; enums are
//!   closed sets.
//! * `invoke` answers `{"ok":true,"value":…}` or
//!   `{"ok":false,"error":{code,domain,operation,detail},"commit":…}`;
//!   `commit` is non-null exactly for the write ops.

use serde_json::{Map, Value};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, MAX_OPERATION_BYTES};
use ubm_desktop::{DesktopError, PlatformDetail, PlatformValue};

/// Wire revision this crate speaks.
pub const WIRE_REVISION: &str = "ubm-mobile-wire/3";
/// Experimental exact connection release revision, not advertised to clients.
pub const EXACT_CONNECTION_WIRE_REVISION: &str = "ubm-mobile-wire/4";
/// Bound on any JSON text crossing the native boundary, in UTF-8 bytes.
pub const MAX_WIRE_TEXT_BYTES: usize = 1 << 20;
/// Longest padded base64 text that can encode `MAX_OPERATION_BYTES`.
pub const MAX_BASE64_LENGTH: usize = 4 * (MAX_OPERATION_BYTES as usize).div_ceil(3);
/// Largest integer JavaScript represents exactly.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Union of `ubm_core::central::GATT_PROP_*` bits.
pub const GATT_PROPERTY_MASK: u8 = 0x1f;

fn wire_error(code: BleErrorCode, domain: BleErrorDomain, path: &str) -> DesktopError {
    DesktopError::new(code, domain, format!("ubm-mobile.wire.{path}"))
}

/// `argument.invalid` for one malformed argument path.
#[must_use]
pub fn invalid(path: &str) -> DesktopError {
    wire_error(BleErrorCode::ArgumentInvalid, BleErrorDomain::Core, path)
}

fn too_large(path: &str) -> DesktopError {
    wire_error(BleErrorCode::BytesTooLarge, BleErrorDomain::Core, path)
}

pub use ubm_desktop::continuation_outbox::encode_base64;

/// Strict RFC 4648 decoding is shared with the native continuation boundary.
pub fn decode_base64(text: &str, path: &str) -> Result<Vec<u8>, DesktopError> {
    decode_base64_bounded(text, path, MAX_OPERATION_BYTES as usize)
}

/// Strict RFC 4648 decoding with a caller-provided decoded-byte bound.
pub fn decode_base64_bounded(
    text: &str,
    path: &str,
    maximum_bytes: usize,
) -> Result<Vec<u8>, DesktopError> {
    ubm_desktop::continuation_outbox::decode_base64(text, maximum_bytes).map_err(
        |error| match error {
            ubm_desktop::continuation_outbox::Base64Error::Invalid => invalid(path),
            ubm_desktop::continuation_outbox::Base64Error::TooLarge => too_large(path),
        },
    )
}

/// Parsed invoke args with exact-key checking and typed readers.
pub struct Args {
    op: String,
    fields: Map<String, Value>,
}

/// Parse args text for `op`: size first, then JSON, then object shape.
pub fn parse_args(op: &str, text: &str) -> Result<Args, DesktopError> {
    if text.len() > MAX_WIRE_TEXT_BYTES {
        return Err(too_large(&format!("args.{op}")));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(fields)) => Ok(Args {
            op: op.to_owned(),
            fields,
        }),
        _ => Err(invalid(&format!("args.{op}"))),
    }
}

impl Args {
    fn path(&self, key: &str) -> String {
        format!("args.{}.{key}", self.op)
    }

    /// Require the key set to be exactly `required` plus any of `optional`.
    pub fn exact(&self, required: &[&str], optional: &[&str]) -> Result<(), DesktopError> {
        for key in required {
            if !self.fields.contains_key(*key) {
                return Err(invalid(&self.path(key)));
            }
        }
        for key in self.fields.keys() {
            if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
                return Err(invalid(&self.path(key)));
            }
        }
        Ok(())
    }

    /// Remove and validate the `admission` sequence (a positive safe
    /// integer); `None` when absent.
    pub fn take_admission(&mut self) -> Result<Option<u64>, DesktopError> {
        match self.fields.remove("admission") {
            None => Ok(None),
            Some(value) => safe_integer(&value, MAX_SAFE_INTEGER)
                .filter(|admission| *admission > 0)
                .map(Some)
                .ok_or_else(|| invalid(&self.path("admission"))),
        }
    }

    /// The key's value, `null` included (presence, whatever its value).
    #[must_use]
    pub fn get_raw(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key).filter(|value| !value.is_null())
    }

    /// Non-empty string.
    pub fn string(&self, key: &str) -> Result<String, DesktopError> {
        match self.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
            _ => Err(invalid(&self.path(key))),
        }
    }

    /// Non-empty string or null/absent.
    pub fn opt_string(&self, key: &str) -> Result<Option<String>, DesktopError> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.string(key).map(Some),
        }
    }

    /// Safe non-negative integer within `max`.
    pub fn integer(&self, key: &str, max: u64) -> Result<u64, DesktopError> {
        self.get(key)
            .and_then(|value| safe_integer(value, max))
            .ok_or_else(|| invalid(&self.path(key)))
    }

    /// Canonical decimal `u64` identity, preserving values beyond JS safe integers.
    pub fn decimal_u64(&self, key: &str) -> Result<u64, DesktopError> {
        let value = self.string(key)?;
        let parsed = value.parse::<u64>().map_err(|_| invalid(&self.path(key)))?;
        if parsed.to_string() != value {
            return Err(invalid(&self.path(key)));
        }
        Ok(parsed)
    }

    /// Safe non-negative integer or null/absent.
    pub fn opt_integer(&self, key: &str, max: u64) -> Result<Option<u64>, DesktopError> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.integer(key, max).map(Some),
        }
    }

    pub fn boolean(&self, key: &str) -> Result<bool, DesktopError> {
        match self.get(key) {
            Some(Value::Bool(flag)) => Ok(*flag),
            _ => Err(invalid(&self.path(key))),
        }
    }

    pub fn opt_boolean(&self, key: &str) -> Result<Option<bool>, DesktopError> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.boolean(key).map(Some),
        }
    }

    /// Array of non-empty strings (null/absent = empty).
    pub fn strings(&self, key: &str) -> Result<Vec<String>, DesktopError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) if !text.is_empty() => Ok(text.clone()),
                    _ => Err(invalid(&self.path(key))),
                })
                .collect(),
            Some(_) => Err(invalid(&self.path(key))),
        }
    }

    /// One member of a closed set.
    pub fn one_of<'a>(&self, key: &str, members: &[&'a str]) -> Result<&'a str, DesktopError> {
        let text = self.string(key)?;
        members
            .iter()
            .copied()
            .find(|member| *member == text)
            .ok_or_else(|| invalid(&self.path(key)))
    }

    /// One member of a closed set, or null/absent.
    pub fn opt_one_of<'a>(
        &self,
        key: &str,
        members: &[&'a str],
    ) -> Result<Option<&'a str>, DesktopError> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.one_of(key, members).map(Some),
        }
    }

    /// Strict base64 field.
    pub fn bytes(&self, key: &str) -> Result<Vec<u8>, DesktopError> {
        match self.get(key) {
            Some(Value::String(text)) => decode_base64(text, &self.path(key)),
            _ => Err(invalid(&self.path(key))),
        }
    }

    /// Nested object args with the same readers.
    pub fn object(&self, key: &str) -> Result<Args, DesktopError> {
        match self.get(key) {
            Some(Value::Object(fields)) => Ok(Args {
                op: format!("{}.{key}", self.op),
                fields: fields.clone(),
            }),
            _ => Err(invalid(&self.path(key))),
        }
    }

    /// Array of nested object arguments, bounded before allocation.
    pub fn objects(&self, key: &str, max: usize) -> Result<Vec<Args>, DesktopError> {
        match self.get(key) {
            Some(Value::Array(values)) if values.len() <= max => values
                .iter()
                .enumerate()
                .map(|(index, value)| match value {
                    Value::Object(fields) => Ok(Args {
                        op: format!("{}.{key}.{index}", self.op),
                        fields: fields.clone(),
                    }),
                    _ => Err(invalid(&self.path(key))),
                })
                .collect(),
            _ => Err(invalid(&self.path(key))),
        }
    }

    /// Nested object or null/absent.
    pub fn opt_object(&self, key: &str) -> Result<Option<Args>, DesktopError> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.object(key).map(Some),
        }
    }
}

fn safe_integer(value: &Value, max: u64) -> Option<u64> {
    let number = value.as_u64()?;
    // `as_u64` refuses negatives and fractions; a float that happens to be
    // integral (`1.0`) is refused too: one spelling per fact.
    if value.is_f64() || number > max.min(MAX_SAFE_INTEGER) {
        return None;
    }
    Some(number)
}

/// Success envelope text.
#[must_use]
pub fn ok_envelope(value: Value) -> String {
    let mut map = Map::new();
    map.insert("ok".to_owned(), Value::Bool(true));
    map.insert("value".to_owned(), value);
    Value::Object(map).to_string()
}

/// The `error` object of a failure envelope or cleanup failure.
#[must_use]
pub fn error_object(error: &DesktopError) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("code".to_owned(), Value::from(error.code_str()));
    map.insert("domain".to_owned(), Value::from(error.domain().as_str()));
    map.insert("operation".to_owned(), Value::from(error.operation()));
    map.insert(
        "detail".to_owned(),
        error.detail().map_or(Value::Null, Value::from),
    );
    map.insert(
        "platform".to_owned(),
        error.platform().map_or(Value::Null, platform_value),
    );
    map
}

/// The platform's own error identity (finding 113):
/// `{domain,code,message,metadata}` with integer, text or boolean metadata.
fn platform_value(detail: &PlatformDetail) -> Value {
    let metadata: Map<String, Value> = detail
        .metadata
        .iter()
        .map(|(key, value)| {
            let value = match value {
                PlatformValue::Int(number) => Value::from(*number),
                PlatformValue::Text(text) => Value::from(text.as_str()),
                PlatformValue::Bool(flag) => Value::Bool(*flag),
            };
            (key.clone(), value)
        })
        .collect();
    object(vec![
        ("domain", Value::from(detail.domain.as_str())),
        ("code", Value::from(detail.code.as_str())),
        ("message", opt_text(detail.message.as_deref())),
        ("metadata", Value::Object(metadata)),
    ])
}

/// Failure envelope text. `commit` is present for writes and effectful link controls.
/// `retryability` is the owner's own answer (`never` / `caller-decides`),
/// never re-derived from the code by the caller; any request whose commit is
/// `uncertain` is always `never`.
#[must_use]
pub fn error_envelope(error: &DesktopError, commit: Option<&str>) -> String {
    let retryability = if commit == Some("uncertain") {
        ubm_desktop::Retryability::Never
    } else {
        error.retryability()
    };
    let mut map = Map::new();
    map.insert("ok".to_owned(), Value::Bool(false));
    map.insert("error".to_owned(), Value::Object(error_object(error)));
    map.insert("commit".to_owned(), commit.map_or(Value::Null, Value::from));
    map.insert(
        "retryability".to_owned(),
        Value::from(retryability.as_str()),
    );
    Value::Object(map).to_string()
}

/// Build a JSON object from `(key, value)` pairs.
#[must_use]
pub fn object(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// `Some(text)` → string, `None` → null.
#[must_use]
pub fn opt_text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_edge_bytes() {
        for bytes in [
            vec![],
            vec![0x00],
            vec![0x80],
            vec![0xff],
            vec![0x00, 0x80, 0xff],
            vec![0x10, 0x55, 0x20, 0x03],
        ] {
            let text = encode_base64(&bytes);
            assert_eq!(decode_base64(&text, "t").ok(), Some(bytes));
        }
        assert_eq!(encode_base64(&[0x00, 0x55]), "AFU=");
    }

    #[test]
    fn base64_is_strict() {
        for bad in [
            "A", "AAA", "A===", "AF=U", "AFV=", "AB==", "A B=", "AF-_", "====", "AA=A",
        ] {
            assert!(decode_base64(bad, "t").is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn base64_rejects_oversize_before_decoding() {
        let text = "A".repeat(MAX_BASE64_LENGTH + 4);
        let error = decode_base64(&text, "t").err();
        assert_eq!(error.map(|e| e.code_str()), Some("bytes.too-large"));
        let max = encode_base64(&vec![0xa5; MAX_OPERATION_BYTES as usize]);
        assert_eq!(max.len(), MAX_BASE64_LENGTH);
        assert_eq!(
            decode_base64(&max, "t").map(|bytes| bytes.len()).ok(),
            Some(MAX_OPERATION_BYTES as usize)
        );
    }

    #[test]
    fn bounded_base64_decode_refuses_bytes_above_the_reservation() {
        let text = encode_base64(&[0xa5, 0x5a]);
        assert_eq!(
            decode_base64_bounded(&text, "t", 1)
                .err()
                .map(|error| error.code_str()),
            Some("bytes.too-large")
        );
        assert_eq!(
            decode_base64_bounded(&text, "t", 2).unwrap(),
            vec![0xa5, 0x5a]
        );
    }

    #[test]
    fn args_are_exact_and_integers_safe() {
        let args = parse_args("x", r#"{"a":1,"b":1.5,"c":9007199254740992,"d":-1}"#)
            .ok()
            .unwrap_or_else(|| unreachable!());
        assert!(args.exact(&["a"], &[]).is_err());
        assert_eq!(args.integer("a", 10).ok(), Some(1));
        assert!(args.integer("b", u64::MAX).is_err());
        assert!(args.integer("c", u64::MAX).is_err());
        assert!(args.integer("d", u64::MAX).is_err());
        assert!(parse_args("x", "[]").is_err());
        let oversize = format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_WIRE_TEXT_BYTES));
        assert_eq!(
            parse_args("x", &oversize).err().map(|e| e.code_str()),
            Some("bytes.too-large")
        );
    }

    #[test]
    fn decimal_identity_is_canonical_and_preserves_unsigned_range() {
        for (input, expected) in [
            ("0", 0),
            ("9007199254740993", 9_007_199_254_740_993),
            ("18446744073709551615", u64::MAX),
        ] {
            let args = parse_args("ordered", &format!("{{\"id\":\"{input}\"}}")).unwrap();
            assert_eq!(args.decimal_u64("id").unwrap(), expected);
        }
        for input in ["", "01", "+1", "-1", "18446744073709551616"] {
            let args = parse_args("ordered", &format!("{{\"id\":\"{input}\"}}")).unwrap();
            assert!(args.decimal_u64("id").is_err());
        }
    }
}
