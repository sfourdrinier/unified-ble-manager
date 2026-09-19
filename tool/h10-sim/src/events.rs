//! JSON-lines event log: every GATT event with a timestamp and sequence number.
//!
//! Format per line: `{"seq":N,"ts":"<RFC3339 UTC>","kind":"...","...":...}`.
//! Nothing is ever dropped silently: malformed input and failed operations are
//! logged as events with an `error` field.

use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

/// Monotonic sequence guard for the stdout event log.
#[derive(Debug, Default)]
pub struct EventLog {
    next_seq: u64,
    listeners: Vec<mpsc::Sender<Value>>,
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            listeners: Vec::new(),
        }
    }

    /// Subscribes a driver client (or test) to every event. Closed receivers
    /// are dropped on the next emission; a full queue drops that one event
    /// with a loud stderr note — never silently.
    pub fn add_listener(&mut self, sender: mpsc::Sender<Value>) {
        self.listeners.push(sender);
    }

    /// Emits one event line to stdout.
    pub fn log(&mut self, kind: &str, detail: Value) {
        let mut object = Map::with_capacity(4);
        object.insert("seq".to_string(), Value::from(self.next_seq));
        self.next_seq += 1;
        object.insert(
            "ts".to_string(),
            Value::from(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
        object.insert("kind".to_string(), Value::from(kind));
        if let Value::Object(fields) = detail {
            for (key, value) in fields {
                object.insert(key, value);
            }
        } else {
            object.insert("detail".to_string(), detail);
        }
        let event = Value::Object(object);
        println!("{event}");
        let mut index = 0;
        while index < self.listeners.len() {
            match self.listeners[index].try_send(event.clone()) {
                Ok(()) => index += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.listeners.remove(index);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    eprintln!("h10-sim: driver event queue full, dropping one event");
                    index += 1;
                }
            }
        }
    }

    /// Convenience for events carrying no extra fields.
    pub fn log_simple(&mut self, kind: &str) {
        self.log(kind, Value::Object(Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_increase_monotonically() {
        let mut log = EventLog::new();
        assert_eq!(log.next_seq, 1);
        log.log_simple("a");
        log.log_simple("b");
        assert_eq!(log.next_seq, 3);
    }
}
