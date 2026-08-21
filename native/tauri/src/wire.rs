use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};
use tauri::ipc::Channel;

const BYTES_WIRE_TAG: &str = "$__unifiedBleBytesV2";

/// Owned IPC data with byte arrays kept distinct from ordinary JSON arrays.
#[derive(Clone, Debug, PartialEq)]
pub enum IpcValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<IpcValue>),
    Object(BTreeMap<String, IpcValue>),
}

impl IpcValue {
    /// Decodes the explicit JavaScript wire representation after Tauri has
    /// deserialized command arguments as JSON.
    pub fn from_wire(value: Value) -> Result<Self, String> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(value)),
            Value::Number(value) => Ok(Self::Number(value)),
            Value::String(value) => Ok(Self::String(value)),
            Value::Array(values) => values
                .into_iter()
                .map(Self::from_wire)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Array),
            Value::Object(mut object) if object.contains_key(BYTES_WIRE_TAG) => {
                if object.len() != 1 {
                    return Err("malformed Unified BLE Tauri byte value".to_owned());
                }
                let values = object
                    .remove(BYTES_WIRE_TAG)
                    .and_then(|value| match value {
                        Value::Array(values) => Some(values),
                        _ => None,
                    })
                    .ok_or_else(|| "malformed Unified BLE Tauri byte value".to_owned())?;
                let bytes = values
                    .into_iter()
                    .map(|value| {
                        value
                            .as_u64()
                            .filter(|value| *value <= u64::from(u8::MAX))
                            .map(|value| value as u8)
                            .ok_or_else(|| "malformed Unified BLE Tauri byte value".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::Bytes(bytes))
            }
            Value::Object(object) => object
                .into_iter()
                .map(|(key, value)| Self::from_wire(value).map(|value| (key, value)))
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Self::Object),
        }
    }

    /// Encodes typed IPC data for Tauri responses and Channel messages.
    pub fn into_wire(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Bytes(bytes) => {
                let mut object = Map::new();
                object.insert(
                    BYTES_WIRE_TAG.to_owned(),
                    Value::Array(bytes.into_iter().map(Value::from).collect()),
                );
                Value::Object(object)
            }
            Self::Array(values) => Value::Array(values.into_iter().map(Self::into_wire).collect()),
            Self::Object(object) => Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| (key, value.into_wire()))
                    .collect(),
            ),
        }
    }
}

/// The one request kind that may bind a caller's event sink.
///
/// Held here so the command layer and the dispatcher agree on it without
/// duplicating a string literal.
pub const ATTACH_REQUEST_KIND: &str = "bootstrap";

/// Encodes typed IPC events before sending them through a Tauri Channel.
#[derive(Clone)]
pub struct IpcEventSink {
    channel: Channel<Value>,
}

impl IpcEventSink {
    pub(crate) fn new(channel: Channel<Value>) -> Self {
        Self { channel }
    }

    pub fn send(&self, event: IpcValue) -> tauri::Result<()> {
        self.channel.send(event.into_wire())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{IpcValue, BYTES_WIRE_TAG};

    #[test]
    fn bytes_round_trip_through_the_explicit_wire_tag() {
        let wire = json!({ (BYTES_WIRE_TAG): [0, 127, 255] });
        let decoded = IpcValue::from_wire(wire.clone()).expect("valid bytes");

        assert_eq!(decoded, IpcValue::Bytes(vec![0, 127, 255]));
        assert_eq!(decoded.into_wire(), wire);
    }

    #[test]
    fn ordinary_numeric_arrays_remain_arrays() {
        let decoded = IpcValue::from_wire(json!([0, 127, 255])).expect("valid array");

        assert!(matches!(decoded, IpcValue::Array(_)));
    }

    #[test]
    fn malformed_byte_tags_are_rejected() {
        assert!(IpcValue::from_wire(json!({ (BYTES_WIRE_TAG): [256] })).is_err());
        assert!(IpcValue::from_wire(json!({ (BYTES_WIRE_TAG): [1], "extra": true })).is_err());
    }
}
