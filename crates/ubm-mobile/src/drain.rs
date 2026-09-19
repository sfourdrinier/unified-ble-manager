//! Per-session drain queues and wake arming.
//!
//! Two bounded queues share one session-monotonic ordinal: data (`adv`,
//! `value`) bounded by items and bytes, control (everything else) bounded
//! by items. Drain merges them by ordinal, so causality holds (a value
//! that arrived before a link loss drains before it), and a full data
//! queue never pushes out a control record.
//!
//! Wake arming (no lost wakeups): `armed` means JavaScript drained to
//! empty and waits. A producer that makes the session non-empty while
//! armed disarms and wakes exactly once. A drain that empties the queues
//! arms, then re-checks: a record that raced in after the take disarms
//! again and the drain answers `more: true`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::foreign::lock;
use crate::radio::{IngressClass, WakeSink};

/// Data records (`adv`, `value`) queued per session.
pub const DATA_RECORD_CAP: usize = 2048;
/// Encoded bytes of queued data records per session.
pub const DATA_RECORD_BYTES: usize = 4 << 20;
/// Control records queued per session. Past this, control records are
/// counted and reported as one `ingress-drop{class:"control"}`.
pub const CONTROL_RECORD_CAP: usize = 1024;

struct Entry {
    ordinal: u64,
    record: Value,
    bytes: usize,
}

#[derive(Default)]
struct Queues {
    data: VecDeque<Entry>,
    data_bytes: usize,
    control: VecDeque<Entry>,
    ordinal: u64,
    /// Control records refused past the cap, reported at the next drain.
    control_lost: u64,
}

/// A data record the session could not queue: the caller turns it into a
/// terminal (`stream-end overflow`) or an `ingress-drop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataOverflow {
    pub bytes: usize,
}

/// One session's outgoing records plus its wake state.
pub struct Outbox {
    session_id: u64,
    queues: Mutex<Queues>,
    armed: AtomicBool,
    wake: Arc<dyn WakeSink>,
}

fn with_ordinal(mut record: Value, ordinal: u64) -> Value {
    if let Value::Object(map) = &mut record {
        map.insert("ordinal".to_owned(), Value::from(ordinal));
    }
    record
}

impl Outbox {
    #[must_use]
    pub fn new(session_id: u64, wake: Arc<dyn WakeSink>) -> Self {
        Self {
            session_id,
            queues: Mutex::new(Queues::default()),
            armed: AtomicBool::new(true),
            wake,
        }
    }

    fn signal(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.wake.wake(self.session_id);
        }
    }

    /// Queue one data record (`adv` / `value`).
    pub fn push_data(&self, record: Value) -> Result<(), DataOverflow> {
        let bytes = record.to_string().len();
        {
            let mut queues = lock(&self.queues);
            if queues.data.len() >= DATA_RECORD_CAP || queues.data_bytes + bytes > DATA_RECORD_BYTES
            {
                return Err(DataOverflow { bytes });
            }
            queues.ordinal += 1;
            let ordinal = queues.ordinal;
            queues.data_bytes += bytes;
            queues.data.push_back(Entry {
                ordinal,
                record,
                bytes,
            });
        }
        self.signal();
        Ok(())
    }

    /// Queue one control record.
    pub fn push_control(&self, record: Value) {
        {
            let mut queues = lock(&self.queues);
            if queues.control.len() >= CONTROL_RECORD_CAP {
                queues.control_lost += 1;
            } else {
                queues.ordinal += 1;
                let ordinal = queues.ordinal;
                queues.control.push_back(Entry {
                    ordinal,
                    record,
                    bytes: 0,
                });
            }
        }
        self.signal();
    }

    /// Report one ingress drop, coalescing into an undrained
    /// `ingress-drop` record of the same class at the control tail.
    pub fn push_ingress_drop(&self, class: IngressClass) {
        {
            let mut queues = lock(&self.queues);
            if let Some(tail) = queues.control.back_mut()
                && tail.record.get("t").and_then(Value::as_str) == Some("ingress-drop")
                && tail.record.get("class").and_then(Value::as_str) == Some(class.as_str())
                && let Some(count) = tail.record.get("count").and_then(Value::as_u64)
                && let Value::Object(map) = &mut tail.record
            {
                map.insert("count".to_owned(), Value::from(count + 1));
                drop(queues);
                self.signal();
                return;
            }
        }
        self.push_control(crate::wire::object(vec![
            ("t", Value::from("ingress-drop")),
            ("class", Value::from(class.as_str())),
            ("count", Value::from(1u64)),
        ]));
    }

    /// Queued data records (retained byte buffers).
    #[must_use]
    pub fn queued_data(&self) -> usize {
        lock(&self.queues).data.len()
    }

    /// Take up to `max_items` records (at least one when any is waiting)
    /// within `max_bytes` of encoded data, in ordinal order.
    #[must_use]
    pub fn drain(&self, max_items: usize, max_bytes: usize) -> Value {
        let mut records = Vec::new();
        let mut taken_bytes = 0usize;
        let more = {
            let mut queues = lock(&self.queues);
            while records.len() < max_items.max(1) {
                let data_head = queues.data.front().map(|entry| entry.ordinal);
                let control_head = queues.control.front().map(|entry| entry.ordinal);
                let take_data = match (data_head, control_head) {
                    (Some(d), Some(c)) => d < c,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                let next_bytes = if take_data {
                    queues.data.front().map_or(0, |entry| entry.bytes)
                } else {
                    0
                };
                if !records.is_empty() && taken_bytes + next_bytes > max_bytes {
                    break;
                }
                let entry = if take_data {
                    let entry = queues.data.pop_front();
                    if let Some(entry) = &entry {
                        queues.data_bytes = queues.data_bytes.saturating_sub(entry.bytes);
                    }
                    entry
                } else {
                    queues.control.pop_front()
                };
                if let Some(entry) = entry {
                    taken_bytes += entry.bytes;
                    records.push(with_ordinal(entry.record, entry.ordinal));
                }
            }
            // The lost-control report takes the next ordinal, so it only goes
            // out once every earlier record has drained (ordinals increase).
            if queues.control_lost > 0
                && queues.data.is_empty()
                && queues.control.is_empty()
                && records.len() < max_items.max(1)
            {
                queues.ordinal += 1;
                let ordinal = queues.ordinal;
                let lost = std::mem::take(&mut queues.control_lost);
                records.push(with_ordinal(
                    crate::wire::object(vec![
                        ("t", Value::from("ingress-drop")),
                        ("class", Value::from(IngressClass::Control.as_str())),
                        ("count", Value::from(lost)),
                    ]),
                    ordinal,
                ));
            }
            !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0)
        };
        let more = if more {
            true
        } else {
            // Arm, then re-check: a producer that pushed between the take
            // above and this store saw `armed == false` and did not wake.
            self.armed.store(true, Ordering::SeqCst);
            let refilled = {
                let queues = lock(&self.queues);
                !(queues.data.is_empty() && queues.control.is_empty() && queues.control_lost == 0)
            };
            refilled && self.armed.swap(false, Ordering::SeqCst)
        };
        crate::wire::object(vec![
            ("more", Value::Bool(more)),
            ("records", Value::Array(records)),
        ])
    }
}
