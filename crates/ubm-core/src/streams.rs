//! Bounded streams: item/byte/aggregate budgets plus reserved control.
//!
//! Derived from C-UBM `streams.ts` (STR-01) and the frozen numeric table in
//! `bounds.ts`. No queue may grow merely because a consumer stops reading:
//! every admission decision is explicit, counters are monotonic for a stream
//! lifetime, and drop policies keep the stream active while `error` closes
//! ingress with one terminal overflow.
//!
//! Two layers: [`apply_admission`] mirrors the C-UBM accounting function
//! verbatim (decision plus counters), while [`Stream`] adds the kernel-layer
//! occupancy model (which bytes are held) and [`StreamSet`] enforces an
//! aggregate byte budget across streams. The C-UBM accounting rule counts the
//! incoming bytes as dropped under both drop policies; occupancy additionally
//! evicts the oldest held item under `latest`/`drop-oldest` so the byte
//! budget stays truthful.

use crate::contracts::{
    BleErrorCode, BleErrorDomain, CoreError, assert_byte_capacity, assert_item_capacity,
};

/// Reserved control item slots per stream (C-UBM `RESERVED_CONTROL_CAPACITY`).
pub const RESERVED_CONTROL_CAPACITY: u64 = 1;
/// Reserved control byte budget per stream (C-UBM `RESERVED_CONTROL_BYTES`).
pub const RESERVED_CONTROL_BYTES: u64 = 64;
/// Kernel-local bound on streams per set.
pub const STREAM_SET_MAX_STREAMS: usize = 16;

/// Validated stream budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLimits {
    item_capacity: u64,
    byte_capacity: u64,
    reserved_control_capacity: u64,
    reserved_control_bytes: u64,
}

impl StreamLimits {
    /// Validate budgets. Mirrors C-UBM `validateStreamLimits` with
    /// like-with-like comparison (D3 fixed in C-UBM 0.1.1): bytes against
    /// the reserved byte budget. Data and control draw from separate item
    /// pools sharing one byte budget, so no item-against-item quota applies
    /// beyond the range checks.
    pub fn new(
        item_capacity: u64,
        byte_capacity: u64,
        reserved_control_capacity: u64,
        reserved_control_bytes: u64,
    ) -> Result<Self, CoreError> {
        let item_capacity = assert_item_capacity(item_capacity, "stream.limits.item-capacity")?;
        let byte_capacity = assert_byte_capacity(byte_capacity, "stream.limits.byte-capacity")?;
        let reserved_control_capacity = assert_item_capacity(
            reserved_control_capacity,
            "stream.limits.reserved-control-capacity",
        )?;
        let reserved_control_bytes = assert_byte_capacity(
            reserved_control_bytes,
            "stream.limits.reserved-control-bytes",
        )?;
        if byte_capacity <= reserved_control_bytes {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "stream.limits.control-quota",
            ));
        }
        Ok(Self {
            item_capacity,
            byte_capacity,
            reserved_control_capacity,
            reserved_control_bytes,
        })
    }

    /// Maximum data items held.
    #[must_use]
    pub const fn item_capacity(&self) -> u64 {
        self.item_capacity
    }

    /// Maximum bytes held (data plus control).
    #[must_use]
    pub const fn byte_capacity(&self) -> u64 {
        self.byte_capacity
    }

    /// Control-only item slots, additive to data capacity.
    #[must_use]
    pub const fn reserved_control_capacity(&self) -> u64 {
        self.reserved_control_capacity
    }

    /// Control-only byte budget within the shared byte capacity.
    #[must_use]
    pub const fn reserved_control_bytes(&self) -> u64 {
        self.reserved_control_bytes
    }
}

/// Named stream with a frozen default budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamName {
    ScanObservation,
    Notification,
    AdapterState,
    Diagnostics,
    RestorationReplay,
}

impl StreamName {
    /// Frozen wire string for this stream.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScanObservation => "scan-observation",
            Self::Notification => "notification",
            Self::AdapterState => "adapter-state",
            Self::Diagnostics => "diagnostics",
            Self::RestorationReplay => "restoration-replay",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown streams.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        // Short const matcher over the five frozen names.
        if bytes.len() == "notification".len() {
            return match_name(bytes, "notification", Self::Notification);
        }
        if bytes.len() == "diagnostics".len() {
            return match_name(bytes, "diagnostics", Self::Diagnostics);
        }
        if bytes.len() == "adapter-state".len() {
            return match_name(bytes, "adapter-state", Self::AdapterState);
        }
        if bytes.len() == "scan-observation".len() {
            return match_name(bytes, "scan-observation", Self::ScanObservation);
        }
        if bytes.len() == "restoration-replay".len() {
            return match_name(bytes, "restoration-replay", Self::RestorationReplay);
        }
        None
    }
}

const fn match_name(bytes: &[u8], text: &str, name: StreamName) -> Option<StreamName> {
    let expected = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != expected[index] {
            return None;
        }
        index += 1;
    }
    Some(name)
}

/// Overflow policy, verbatim from C-UBM `OverflowPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverflowPolicy {
    Latest,
    DropOldest,
    DropNewest,
    Error,
}

impl OverflowPolicy {
    /// Frozen wire string for this policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::DropOldest => "drop-oldest",
            Self::DropNewest => "drop-newest",
            Self::Error => "error",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown policies.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() == "latest".len() {
            return match_policy(bytes, "latest", Self::Latest);
        }
        if bytes.len() == "error".len() {
            return match_policy(bytes, "error", Self::Error);
        }
        // `drop-oldest` and `drop-newest` share a length: try both.
        if bytes.len() == "drop-oldest".len() {
            if let Some(policy) = match_policy(bytes, "drop-oldest", Self::DropOldest) {
                return Some(policy);
            }
            return match_policy(bytes, "drop-newest", Self::DropNewest);
        }
        None
    }
}

const fn match_policy(bytes: &[u8], text: &str, policy: OverflowPolicy) -> Option<OverflowPolicy> {
    let expected = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != expected[index] {
            return None;
        }
        index += 1;
    }
    Some(policy)
}

/// Frozen default budget for one named stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDefault {
    /// Named stream.
    pub stream: StreamName,
    /// Default item capacity.
    pub item_capacity: u64,
    /// Default byte capacity.
    pub byte_capacity: u64,
    /// Default reserved control item slots.
    pub reserved_control_capacity: u64,
    /// Default reserved control byte budget.
    pub reserved_control_bytes: u64,
    /// Default overflow policy.
    pub policy: OverflowPolicy,
}

/// Frozen defaults, verbatim from C-UBM `STREAM_DEFAULTS`.
pub const STREAM_DEFAULTS: [StreamDefault; 5] = [
    StreamDefault {
        stream: StreamName::ScanObservation,
        item_capacity: 1,
        byte_capacity: 524_288,
        reserved_control_capacity: RESERVED_CONTROL_CAPACITY,
        reserved_control_bytes: RESERVED_CONTROL_BYTES,
        policy: OverflowPolicy::Latest,
    },
    StreamDefault {
        stream: StreamName::Notification,
        item_capacity: 64,
        byte_capacity: 1_048_576,
        reserved_control_capacity: RESERVED_CONTROL_CAPACITY,
        reserved_control_bytes: RESERVED_CONTROL_BYTES,
        policy: OverflowPolicy::DropOldest,
    },
    StreamDefault {
        stream: StreamName::AdapterState,
        item_capacity: 64,
        byte_capacity: 65_536,
        reserved_control_capacity: RESERVED_CONTROL_CAPACITY,
        reserved_control_bytes: RESERVED_CONTROL_BYTES,
        policy: OverflowPolicy::Latest,
    },
    StreamDefault {
        stream: StreamName::Diagnostics,
        item_capacity: 256,
        byte_capacity: 524_288,
        reserved_control_capacity: RESERVED_CONTROL_CAPACITY,
        reserved_control_bytes: RESERVED_CONTROL_BYTES,
        policy: OverflowPolicy::DropOldest,
    },
    StreamDefault {
        stream: StreamName::RestorationReplay,
        item_capacity: 64,
        byte_capacity: 262_144,
        reserved_control_capacity: RESERVED_CONTROL_CAPACITY,
        reserved_control_bytes: RESERVED_CONTROL_BYTES,
        policy: OverflowPolicy::Error,
    },
];

/// Monotonic lifetime counters for one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamAccounting {
    admitted: u64,
    dropped_oldest: u64,
    dropped_bytes: u64,
    replaced: u64,
    terminated: bool,
}

impl StreamAccounting {
    /// Items admitted (data plus control).
    #[must_use]
    pub const fn admitted(&self) -> u64 {
        self.admitted
    }

    /// Overflow events counted as drops.
    #[must_use]
    pub const fn dropped_oldest(&self) -> u64 {
        self.dropped_oldest
    }

    /// Bytes counted as dropped under the C-UBM accounting rule.
    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Items replaced under `latest`.
    #[must_use]
    pub const fn replaced(&self) -> u64 {
        self.replaced
    }

    /// Whether ingress is closed after an `error`-policy overflow.
    #[must_use]
    pub const fn terminated(&self) -> bool {
        self.terminated
    }
}

/// Admission decision, verbatim from C-UBM `AdmissionDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionDecision {
    Admit,
    Replace,
    DropOldest,
    DropNewest,
    Terminate,
}

impl AdmissionDecision {
    /// Frozen wire string for this decision.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Replace => "replace",
            Self::DropOldest => "drop-oldest",
            Self::DropNewest => "drop-newest",
            Self::Terminate => "terminate",
        }
    }
}

/// Decision plus updated accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// What the host must do.
    pub decision: AdmissionDecision,
    /// Updated monotonic counters.
    pub accounting: StreamAccounting,
}

/// Mirror of C-UBM `applyStreamAdmission` (STR-01): drop policies keep the
/// stream active with cumulative counters; `error` closes ingress with one
/// terminal overflow. Infallible by construction: policies are typed and
/// counters are `u64`.
#[must_use]
pub fn apply_admission(
    accounting: &StreamAccounting,
    policy: OverflowPolicy,
    at_capacity: bool,
    incoming_bytes: u64,
) -> Admission {
    if !at_capacity {
        return Admission {
            decision: AdmissionDecision::Admit,
            accounting: StreamAccounting {
                admitted: accounting.admitted.saturating_add(1),
                ..*accounting
            },
        };
    }
    match policy {
        OverflowPolicy::Latest => Admission {
            decision: AdmissionDecision::Replace,
            accounting: StreamAccounting {
                replaced: accounting.replaced.saturating_add(1),
                ..*accounting
            },
        },
        OverflowPolicy::DropOldest => Admission {
            decision: AdmissionDecision::DropOldest,
            accounting: StreamAccounting {
                dropped_oldest: accounting.dropped_oldest.saturating_add(1),
                dropped_bytes: accounting.dropped_bytes.saturating_add(incoming_bytes),
                ..*accounting
            },
        },
        OverflowPolicy::DropNewest => Admission {
            decision: AdmissionDecision::DropNewest,
            accounting: StreamAccounting {
                dropped_oldest: accounting.dropped_oldest.saturating_add(1),
                dropped_bytes: accounting.dropped_bytes.saturating_add(incoming_bytes),
                ..*accounting
            },
        },
        OverflowPolicy::Error => Admission {
            decision: AdmissionDecision::Terminate,
            accounting: StreamAccounting {
                terminated: true,
                ..*accounting
            },
        },
    }
}

/// Queue effect of one push: what entered and what left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushEffect {
    /// Host-visible decision.
    pub decision: AdmissionDecision,
    /// Bytes accepted into the queue.
    pub admitted_bytes: u64,
    /// Bytes evicted from the queue.
    pub evicted_bytes: u64,
}

/// One bounded stream: data slots, additive control-only slots, and a shared
/// byte budget. Item sizes are tracked FIFO so eviction keeps the byte total
/// truthful; the queue holds at most `item_capacity` sizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    limits: StreamLimits,
    policy: OverflowPolicy,
    data_sizes: Vec<u64>,
    control_items: u64,
    bytes: u64,
    accounting: StreamAccounting,
}

impl Stream {
    /// Build a stream from validated budgets.
    pub fn new(limits: StreamLimits, policy: OverflowPolicy) -> Self {
        Self {
            limits,
            policy,
            data_sizes: Vec::new(),
            control_items: 0,
            bytes: 0,
            accounting: StreamAccounting::default(),
        }
    }

    /// Build a stream from a frozen default including reserved control budgets.
    pub fn from_default(name: StreamName) -> Result<Self, CoreError> {
        for default in &STREAM_DEFAULTS {
            if default.stream == name {
                let limits = StreamLimits::new(
                    default.item_capacity,
                    default.byte_capacity,
                    default.reserved_control_capacity,
                    default.reserved_control_bytes,
                )?;
                return Ok(Self::new(limits, default.policy));
            }
        }
        Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Stream,
            "stream.default",
        ))
    }

    /// Borrow the budgets.
    #[must_use]
    pub const fn limits(&self) -> &StreamLimits {
        &self.limits
    }

    /// Overflow policy.
    #[must_use]
    pub const fn policy(&self) -> OverflowPolicy {
        self.policy
    }

    /// Data items currently held.
    #[must_use]
    pub fn data_items(&self) -> u64 {
        self.data_sizes.len() as u64
    }

    /// Control items currently held.
    #[must_use]
    pub const fn control_items(&self) -> u64 {
        self.control_items
    }

    /// Bytes currently held (data plus control).
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Lifetime counters.
    #[must_use]
    pub const fn accounting(&self) -> &StreamAccounting {
        &self.accounting
    }

    /// Whether ingress is closed.
    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        self.accounting.terminated
    }

    /// Oldest held data item size, or zero when empty.
    #[must_use]
    pub fn oldest_data_bytes(&self) -> u64 {
        match self.data_sizes.first() {
            Some(size) => *size,
            None => 0,
        }
    }

    /// Whether a data push of `incoming` bytes would contend.
    #[must_use]
    pub fn data_at_capacity(&self, incoming: u64) -> bool {
        self.data_items() >= self.limits.item_capacity
            || self.bytes.saturating_add(incoming) > self.limits.byte_capacity
    }

    /// Push one data item. Control slots are never consumed by data: data
    /// contends only against the data item budget and the shared byte budget.
    pub fn push_data(&mut self, incoming: u64) -> Result<PushEffect, CoreError> {
        if self.is_terminated() {
            return Err(CoreError::new(
                BleErrorCode::StreamClosed,
                BleErrorDomain::Stream,
                "stream.push",
            ));
        }
        // Empty-queue eviction admits nothing: with no held data item to
        // displace, `latest`/`drop-oldest` degrade to `drop-newest` before
        // accounting runs, so the byte total stays truthful when control
        // alone exhausts the shared budget.
        let policy = match self.policy {
            OverflowPolicy::Latest | OverflowPolicy::DropOldest
                if self.data_at_capacity(incoming) && self.data_sizes.is_empty() =>
            {
                OverflowPolicy::DropNewest
            }
            policy => policy,
        };
        let admission = apply_admission(
            &self.accounting,
            policy,
            self.data_at_capacity(incoming),
            incoming,
        );
        self.accounting = admission.accounting;
        match admission.decision {
            AdmissionDecision::Admit => {
                self.data_sizes.push(incoming);
                self.bytes = self.bytes.saturating_add(incoming);
                Ok(PushEffect {
                    decision: admission.decision,
                    admitted_bytes: incoming,
                    evicted_bytes: 0,
                })
            }
            AdmissionDecision::Replace | AdmissionDecision::DropOldest => {
                // Evict the oldest held item to keep the newest: occupancy is
                // unchanged, the byte total stays truthful, and the decision
                // label plus counters tell the host which policy fired.
                let evicted = if self.data_sizes.is_empty() {
                    0
                } else {
                    self.data_sizes.remove(0)
                };
                self.data_sizes.push(incoming);
                self.bytes = self.bytes.saturating_sub(evicted).saturating_add(incoming);
                Ok(PushEffect {
                    decision: admission.decision,
                    admitted_bytes: incoming,
                    evicted_bytes: evicted,
                })
            }
            AdmissionDecision::DropNewest | AdmissionDecision::Terminate => Ok(PushEffect {
                decision: admission.decision,
                admitted_bytes: 0,
                evicted_bytes: 0,
            }),
        }
    }

    /// Push one control item into the reserved slots. Control bypasses a full
    /// data queue but shares the byte budget and never displaces data: when
    /// the reserved slots or the byte budget are exhausted, control terminates
    /// ingress like the `error` policy.
    pub fn push_control(&mut self, incoming: u64) -> Result<PushEffect, CoreError> {
        if self.is_terminated() {
            return Err(CoreError::new(
                BleErrorCode::StreamClosed,
                BleErrorDomain::Stream,
                "stream.push-control",
            ));
        }
        let control_full = self.control_items >= self.limits.reserved_control_capacity;
        let bytes_full = self.bytes.saturating_add(incoming) > self.limits.byte_capacity;
        if control_full || bytes_full {
            self.accounting.terminated = true;
            return Ok(PushEffect {
                decision: AdmissionDecision::Terminate,
                admitted_bytes: 0,
                evicted_bytes: 0,
            });
        }
        self.control_items = self.control_items.saturating_add(1);
        self.bytes = self.bytes.saturating_add(incoming);
        self.accounting.admitted = self.accounting.admitted.saturating_add(1);
        Ok(PushEffect {
            decision: AdmissionDecision::Admit,
            admitted_bytes: incoming,
            evicted_bytes: 0,
        })
    }

    /// Release up to `count` oldest data items. Returns bytes freed.
    /// Infallible: releasing more than held frees what exists.
    pub fn release_oldest_data(&mut self, count: usize) -> u64 {
        let mut freed = 0u64;
        let mut remaining = count;
        while remaining > 0 && !self.data_sizes.is_empty() {
            freed = freed.saturating_add(self.data_sizes.remove(0));
            remaining -= 1;
        }
        self.bytes = self.bytes.saturating_sub(freed);
        freed
    }
}

/// A set of streams under one aggregate byte budget (client, backend-ingress,
/// or adapter-owner scale from the frozen table). Aggregate admission is
/// checked before the stream mutates, so a quota rejection changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSet {
    streams: Vec<Stream>,
    aggregate_bytes: u64,
    aggregate_cap: u64,
}

impl StreamSet {
    /// Build an empty set under `aggregate_cap` bytes. The cap lives on the
    /// frozen aggregate scale (`1..=64MiB`).
    pub fn new(aggregate_cap: u64) -> Result<Self, CoreError> {
        if aggregate_cap == 0 || aggregate_cap > crate::contracts::ADAPTER_OWNER_AGGREGATE_BYTES {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.aggregate-cap",
            ));
        }
        Ok(Self {
            streams: Vec::new(),
            aggregate_bytes: 0,
            aggregate_cap,
        })
    }

    /// Add a stream. The set holds at most [`STREAM_SET_MAX_STREAMS`].
    pub fn add_stream(&mut self, stream: Stream) -> Result<usize, CoreError> {
        if self.streams.len() >= STREAM_SET_MAX_STREAMS {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "stream-set.bound",
            ));
        }
        self.streams.push(stream);
        Ok(self.streams.len() - 1)
    }

    /// Number of member streams.
    #[must_use]
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Whether the set holds no streams.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Aggregate bytes currently held.
    #[must_use]
    pub const fn aggregate_bytes(&self) -> u64 {
        self.aggregate_bytes
    }

    /// Aggregate byte budget.
    #[must_use]
    pub const fn aggregate_cap(&self) -> u64 {
        self.aggregate_cap
    }

    /// Borrow one member stream.
    #[must_use]
    pub fn stream(&self, index: usize) -> Option<&Stream> {
        self.streams.get(index)
    }

    /// Push data into member `index`. The aggregate check runs first: a quota
    /// rejection leaves the stream untouched.
    pub fn push_data(&mut self, index: usize, incoming: u64) -> Result<PushEffect, CoreError> {
        let Some(stream) = self.streams.get(index) else {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.index",
            ));
        };
        if stream.is_terminated() {
            return Err(CoreError::new(
                BleErrorCode::StreamClosed,
                BleErrorDomain::Stream,
                "stream.push",
            ));
        }
        // Net aggregate delta accounts for the eviction that `latest` and
        // `drop-oldest` perform on a full queue.
        let evicted = match stream.policy {
            OverflowPolicy::Latest | OverflowPolicy::DropOldest
                if stream.data_at_capacity(incoming) && !stream.data_sizes.is_empty() =>
            {
                stream.oldest_data_bytes()
            }
            _ => 0,
        };
        let net = incoming.saturating_sub(evicted);
        // `drop-newest` and `error` admit nothing on a full queue.
        let admits = match stream.policy {
            OverflowPolicy::DropNewest | OverflowPolicy::Error
                if stream.data_at_capacity(incoming) =>
            {
                0
            }
            _ => net,
        };
        if self.aggregate_bytes.saturating_add(admits) > self.aggregate_cap {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "stream-set.aggregate",
            ));
        }
        let Some(stream) = self.streams.get_mut(index) else {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.index",
            ));
        };
        let effect = stream.push_data(incoming)?;
        self.aggregate_bytes = self
            .aggregate_bytes
            .saturating_add(effect.admitted_bytes)
            .saturating_sub(effect.evicted_bytes);
        Ok(effect)
    }

    /// Push control into member `index` under the same aggregate budget.
    pub fn push_control(&mut self, index: usize, incoming: u64) -> Result<PushEffect, CoreError> {
        let Some(stream) = self.streams.get(index) else {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.index",
            ));
        };
        if stream.is_terminated() {
            return Err(CoreError::new(
                BleErrorCode::StreamClosed,
                BleErrorDomain::Stream,
                "stream.push-control",
            ));
        }
        // Control that would terminate admits nothing; only admitted control
        // consumes aggregate budget.
        let control_full = stream.control_items() >= stream.limits().reserved_control_capacity;
        let bytes_full = stream.bytes().saturating_add(incoming) > stream.limits().byte_capacity;
        let admits = if control_full || bytes_full {
            0
        } else {
            incoming
        };
        if self.aggregate_bytes.saturating_add(admits) > self.aggregate_cap {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "stream-set.aggregate",
            ));
        }
        let Some(stream) = self.streams.get_mut(index) else {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.index",
            ));
        };
        let effect = stream.push_control(incoming)?;
        self.aggregate_bytes = self.aggregate_bytes.saturating_add(effect.admitted_bytes);
        Ok(effect)
    }

    /// Release up to `count` oldest data items from member `index`. Returns
    /// bytes freed.
    pub fn release_oldest_data(&mut self, index: usize, count: usize) -> Result<u64, CoreError> {
        let Some(stream) = self.streams.get_mut(index) else {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Stream,
                "stream-set.index",
            ));
        };
        let freed = stream.release_oldest_data(count);
        self.aggregate_bytes = self.aggregate_bytes.saturating_sub(freed);
        Ok(freed)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionDecision, OverflowPolicy, RESERVED_CONTROL_BYTES, RESERVED_CONTROL_CAPACITY,
        STREAM_DEFAULTS, STREAM_SET_MAX_STREAMS, Stream, StreamAccounting, StreamLimits,
        StreamName, StreamSet, apply_admission,
    };
    use crate::check;
    use crate::contracts::{BleErrorCode, CLIENT_AGGREGATE_BYTES};

    fn limits(item: u64, bytes: u64, reserved: u64) -> Option<StreamLimits> {
        StreamLimits::new(item, bytes, reserved, RESERVED_CONTROL_BYTES).ok()
    }

    #[test]
    fn frozen_stream_defaults() {
        assert_eq!(STREAM_DEFAULTS.len(), 5);
        assert_eq!(RESERVED_CONTROL_CAPACITY, 1);
        assert_eq!(RESERVED_CONTROL_BYTES, 64);
        let table: &[(StreamName, u64, u64, OverflowPolicy)] = &[
            (
                StreamName::ScanObservation,
                1,
                524_288,
                OverflowPolicy::Latest,
            ),
            (
                StreamName::Notification,
                64,
                1_048_576,
                OverflowPolicy::DropOldest,
            ),
            (StreamName::AdapterState, 64, 65_536, OverflowPolicy::Latest),
            (
                StreamName::Diagnostics,
                256,
                524_288,
                OverflowPolicy::DropOldest,
            ),
            (
                StreamName::RestorationReplay,
                64,
                262_144,
                OverflowPolicy::Error,
            ),
        ];
        for (index, (stream, items, bytes, policy)) in table.iter().enumerate() {
            assert_eq!(STREAM_DEFAULTS[index].stream, *stream);
            assert_eq!(STREAM_DEFAULTS[index].item_capacity, *items);
            assert_eq!(STREAM_DEFAULTS[index].byte_capacity, *bytes);
            assert_eq!(STREAM_DEFAULTS[index].policy, *policy);
            assert_eq!(
                STREAM_DEFAULTS[index].reserved_control_capacity,
                RESERVED_CONTROL_CAPACITY
            );
            assert_eq!(
                STREAM_DEFAULTS[index].reserved_control_bytes,
                RESERVED_CONTROL_BYTES
            );
        }
        for default in &STREAM_DEFAULTS {
            match Stream::from_default(default.stream) {
                Ok(stream) => {
                    assert_eq!(stream.limits().item_capacity(), default.item_capacity);
                    assert_eq!(stream.limits().byte_capacity(), default.byte_capacity);
                    assert_eq!(
                        stream.limits().reserved_control_capacity(),
                        RESERVED_CONTROL_CAPACITY
                    );
                    assert_eq!(
                        stream.limits().reserved_control_bytes(),
                        RESERVED_CONTROL_BYTES
                    );
                    assert_eq!(stream.policy(), default.policy);
                }
                Err(_) => check(false, "frozen defaults must validate"),
            }
        }
    }

    #[test]
    fn stream_names_and_policies_round_trip() {
        assert_eq!(
            StreamName::from_str("scan-observation"),
            Some(StreamName::ScanObservation)
        );
        assert_eq!(
            StreamName::from_str("notification"),
            Some(StreamName::Notification)
        );
        assert_eq!(
            StreamName::from_str("adapter-state"),
            Some(StreamName::AdapterState)
        );
        assert_eq!(
            StreamName::from_str("diagnostics"),
            Some(StreamName::Diagnostics)
        );
        assert_eq!(
            StreamName::from_str("restoration-replay"),
            Some(StreamName::RestorationReplay)
        );
        assert_eq!(StreamName::from_str("telemetry"), None);
        assert_eq!(
            OverflowPolicy::from_str("latest"),
            Some(OverflowPolicy::Latest)
        );
        assert_eq!(
            OverflowPolicy::from_str("drop-oldest"),
            Some(OverflowPolicy::DropOldest)
        );
        assert_eq!(
            OverflowPolicy::from_str("drop-newest"),
            Some(OverflowPolicy::DropNewest)
        );
        assert_eq!(
            OverflowPolicy::from_str("error"),
            Some(OverflowPolicy::Error)
        );
        assert_eq!(OverflowPolicy::from_str("coalesce"), None);
        assert_eq!(StreamName::ScanObservation.as_str(), "scan-observation");
        assert_eq!(AdmissionDecision::DropNewest.as_str(), "drop-newest");
        assert_eq!(AdmissionDecision::Terminate.as_str(), "terminate");
    }

    #[test]
    fn limit_validation_boundaries() {
        // Mirrors the stream-limits fixtures: (64, 1MiB, 1, 64) and (1, 2, 1, 1).
        assert!(limits(64, 1_048_576, 1).is_some());
        assert!(limits(1, 2, 1).is_none());
        assert!(limits(0, 1_048_576, 1).is_none());
        assert!(limits(65_537, 1_048_576, 1).is_none());
        assert!(limits(64, 0, 1).is_none());
        assert!(limits(64, 4_194_305, 1).is_none());
        assert!(limits(64, 1_048_576, 0).is_none());
        // Like-with-like quota: bytes at/below the reserved byte budget.
        assert!(limits(64, 64, 1).is_none());
        assert!(StreamLimits::new(1, 2, 1, 1).is_ok());
        match StreamLimits::new(64, 4_194_305, 1, RESERVED_CONTROL_BYTES) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "byte ceiling breach is quota"),
        }
        match StreamLimits::new(64, 64, 1, 64) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "reserved byte budget breach is quota"),
        }
    }

    #[test]
    fn admission_matrix_per_policy() {
        let fresh = StreamAccounting::default();
        // Room: every policy admits with a monotonic admitted counter.
        for policy in [
            OverflowPolicy::Latest,
            OverflowPolicy::DropOldest,
            OverflowPolicy::DropNewest,
            OverflowPolicy::Error,
        ] {
            let admission = apply_admission(&fresh, policy, false, 128);
            assert_eq!(admission.decision, AdmissionDecision::Admit);
            assert_eq!(admission.accounting.admitted(), 1);
            assert!(!admission.accounting.terminated());
        }
        // Full: latest replaces, drops count the incoming bytes, error ends.
        let admission = apply_admission(&fresh, OverflowPolicy::Latest, true, 128);
        assert_eq!(admission.decision, AdmissionDecision::Replace);
        assert_eq!(admission.accounting.replaced(), 1);
        assert_eq!(admission.accounting.admitted(), 0);
        let admission = apply_admission(&fresh, OverflowPolicy::DropOldest, true, 128);
        assert_eq!(admission.decision, AdmissionDecision::DropOldest);
        assert_eq!(admission.accounting.dropped_oldest(), 1);
        assert_eq!(admission.accounting.dropped_bytes(), 128);
        let admission = apply_admission(&fresh, OverflowPolicy::DropNewest, true, 200);
        assert_eq!(admission.decision, AdmissionDecision::DropNewest);
        assert_eq!(admission.accounting.dropped_oldest(), 1);
        assert_eq!(admission.accounting.dropped_bytes(), 200);
        let admission = apply_admission(&fresh, OverflowPolicy::Error, true, 128);
        assert_eq!(admission.decision, AdmissionDecision::Terminate);
        assert!(admission.accounting.terminated());
        // Counters accumulate across admissions.
        let second = apply_admission(&admission.accounting, OverflowPolicy::Error, false, 1);
        assert!(second.accounting.terminated());
        assert_eq!(second.accounting.admitted(), 1);
    }

    #[test]
    fn item_budget_boundary() {
        let Some(limits) = limits(1, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::DropOldest);
        match stream.push_data(100) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::Admit);
                assert_eq!(effect.admitted_bytes, 100);
            }
            Err(_) => check(false, "first item must admit"),
        }
        assert_eq!(stream.data_items(), 1);
        match stream.push_data(50) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::DropOldest);
                assert_eq!(effect.admitted_bytes, 50);
                assert_eq!(effect.evicted_bytes, 100);
            }
            Err(_) => check(false, "overflow must decide, not fail"),
        }
        assert_eq!(stream.data_items(), 1);
        assert_eq!(stream.bytes(), 50);
        assert_eq!(stream.accounting().dropped_oldest(), 1);
        assert_eq!(stream.accounting().dropped_bytes(), 50);
    }

    #[test]
    fn byte_budget_boundary() {
        let Some(limits) = limits(64, 256, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::Latest);
        assert!(stream.push_data(256).is_ok());
        assert!(stream.data_at_capacity(1));
        match stream.push_data(1) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::Replace);
                assert_eq!(effect.evicted_bytes, 256);
            }
            Err(_) => check(false, "byte overflow must replace"),
        }
        assert_eq!(stream.bytes(), 1);
    }

    #[test]
    fn empty_queue_eviction_admits_nothing() {
        // M2 closure (review probe): `AdapterState` default,
        // `push_control(65536)`, `push_data(1)` must not admit over the
        // 65536-byte cap when no data item is evictable. With nothing to
        // displace, `latest` degrades to `drop-newest`: admit nothing so the
        // byte total stays truthful.
        let Ok(mut stream) = Stream::from_default(StreamName::AdapterState) else {
            check(false, "adapter-state default must validate");
            return;
        };
        match stream.push_control(65_536) {
            Ok(_) => {}
            Err(_) => {
                check(false, "control must fit the reserved slot");
                return;
            }
        }
        assert_eq!(stream.bytes(), 65_536);
        match stream.push_data(1) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::DropNewest);
                assert_eq!(effect.admitted_bytes, 0);
                assert_eq!(effect.evicted_bytes, 0);
            }
            Err(_) => check(false, "empty-queue eviction must decide, not fail"),
        }
        assert!(stream.bytes() <= stream.limits().byte_capacity());
        assert_eq!(stream.data_items(), 0);
    }

    #[test]
    fn error_policy_closes_ingress_once() {
        let Some(limits) = limits(1, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::Error);
        assert!(stream.push_data(10).is_ok());
        match stream.push_data(10) {
            Ok(effect) => assert_eq!(effect.decision, AdmissionDecision::Terminate),
            Err(_) => check(false, "error policy terminates, not errors"),
        }
        assert!(stream.is_terminated());
        match stream.push_data(10) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamClosed),
            Ok(_) => check(false, "terminated ingress must reject"),
        }
        match stream.push_control(1) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamClosed),
            Ok(_) => check(false, "terminated control must reject"),
        }
    }

    #[test]
    fn reserved_control_bypasses_full_data() {
        let Some(limits) = limits(1, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::Latest);
        assert!(stream.push_data(100).is_ok());
        assert!(stream.data_at_capacity(1));
        // One reserved control slot survives a full data queue.
        match stream.push_control(8) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::Admit);
                assert_eq!(stream.control_items(), 1);
            }
            Err(_) => check(false, "reserved control must bypass"),
        }
        assert_eq!(stream.data_items(), 1);
        assert_eq!(stream.bytes(), 108);
        // A second control item exhausts the reservation and terminates.
        match stream.push_control(8) {
            Ok(effect) => assert_eq!(effect.decision, AdmissionDecision::Terminate),
            Err(_) => check(false, "control overflow terminates"),
        }
        assert!(stream.is_terminated());
    }

    #[test]
    fn control_shares_the_byte_budget() {
        let Some(limits) = limits(64, 100, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::Latest);
        assert!(stream.push_data(100).is_ok());
        match stream.push_control(1) {
            Ok(effect) => assert_eq!(effect.decision, AdmissionDecision::Terminate),
            Err(_) => check(false, "control over byte cap terminates"),
        }
    }

    #[test]
    fn release_frees_oldest_first() {
        let Some(limits) = limits(64, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let mut stream = Stream::new(limits, OverflowPolicy::DropNewest);
        assert!(stream.push_data(10).is_ok());
        assert!(stream.push_data(20).is_ok());
        assert!(stream.push_data(30).is_ok());
        assert_eq!(stream.release_oldest_data(2), 30);
        assert_eq!(stream.data_items(), 1);
        assert_eq!(stream.bytes(), 30);
        // Releasing more than held frees what exists.
        assert_eq!(stream.release_oldest_data(99), 30);
        assert_eq!(stream.data_items(), 0);
        assert_eq!(stream.bytes(), 0);
    }

    #[test]
    fn aggregate_budget_boundary() {
        // The client aggregate scale is accepted as a set budget.
        assert!(StreamSet::new(CLIENT_AGGREGATE_BYTES).is_ok());
        let mut set = match StreamSet::new(150) {
            Ok(set) => set,
            Err(_) => {
                check(false, "aggregate cap must validate");
                return;
            }
        };
        assert!(set.is_empty());
        let Some(limits) = limits(64, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let index = match set.add_stream(Stream::new(limits, OverflowPolicy::DropNewest)) {
            Ok(index) => index,
            Err(_) => {
                check(false, "stream must fit the set");
                return;
            }
        };
        assert_eq!(set.len(), 1);
        assert!(set.push_data(index, 100).is_ok());
        assert_eq!(set.aggregate_bytes(), 100);
        // Net +100 over the 150 cap fails closed and changes nothing.
        match set.push_data(index, 100) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "aggregate breach is quota"),
        }
        match set.stream(index) {
            Some(stream) => assert_eq!(stream.data_items(), 1),
            None => check(false, "member must exist"),
        }
        assert_eq!(set.aggregate_bytes(), 100);
        // Release reopens budget.
        match set.release_oldest_data(index, 1) {
            Ok(freed) => assert_eq!(freed, 100),
            Err(_) => check(false, "release must succeed"),
        }
        assert_eq!(set.aggregate_bytes(), 0);
        assert!(set.push_data(index, 1).is_ok());
    }

    #[test]
    fn aggregate_accounts_eviction_net_delta() {
        let mut set = match StreamSet::new(150) {
            Ok(set) => set,
            Err(_) => {
                check(false, "aggregate cap must validate");
                return;
            }
        };
        let Some(limits) = limits(1, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        let index = match set.add_stream(Stream::new(limits, OverflowPolicy::Latest)) {
            Ok(index) => index,
            Err(_) => {
                check(false, "stream must fit the set");
                return;
            }
        };
        assert!(set.push_data(index, 100).is_ok());
        // Replace evicts 100 and admits 120: net +20 fits the 150 cap.
        match set.push_data(index, 120) {
            Ok(effect) => {
                assert_eq!(effect.decision, AdmissionDecision::Replace);
                assert_eq!(effect.evicted_bytes, 100);
            }
            Err(_) => check(false, "net delta must fit"),
        }
        assert_eq!(set.aggregate_bytes(), 120);
    }

    #[test]
    fn set_rejects_bad_indices_caps_and_counts() {
        assert!(StreamSet::new(0).is_err());
        assert!(StreamSet::new(67_108_865).is_err());
        let mut set = match StreamSet::new(1_048_576) {
            Ok(set) => set,
            Err(_) => {
                check(false, "aggregate cap must validate");
                return;
            }
        };
        assert!(set.push_data(0, 1).is_err());
        assert!(set.push_control(0, 1).is_err());
        assert!(set.release_oldest_data(0, 1).is_err());
        let Some(limits) = limits(64, 1_048_576, 1) else {
            check(false, "limits must validate");
            return;
        };
        for _ in 0..STREAM_SET_MAX_STREAMS {
            assert!(
                set.add_stream(Stream::new(limits, OverflowPolicy::DropNewest))
                    .is_ok()
            );
        }
        match set.add_stream(Stream::new(limits, OverflowPolicy::DropNewest)) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "set bound must hold"),
        }
    }
}
