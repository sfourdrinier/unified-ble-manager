//! Simulator state: configuration, fault injection and PMD command handling.
//!
//! All GATT behaviour is decided here against pure data; `radio.rs` only
//! transports bytes. That keeps every behaviour below unit-testable without
//! Bluetooth hardware.

use std::time::Instant;

use chrono::{SecondsFormat, Utc};
use serde::Deserialize;

use crate::control::RunMode;
use crate::gatt_spec;

/// Pairing/bonding policy of the simulated strap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairPolicy {
    /// Bonding works when the central requests it; nothing is gated on a
    /// bond — this matches the H10, whose PMD streams without pairing.
    JustWorks,
    /// Pairing is refused at the policy level (recorded in state; BlueZ-side
    /// enforcement is documented in the README).
    Disabled,
}

impl PairPolicy {
    /// Parses a CLI value case-insensitively; anything else is a loud error.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_ascii_lowercase().as_str() {
            "just-works" => Ok(Self::JustWorks),
            "disabled" => Ok(Self::Disabled),
            _ => Err(format!(
                "pair policy must be just-works or disabled, got {text:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::JustWorks => "just-works",
            Self::Disabled => "disabled",
        }
    }
}

/// Device clock for ECG frame timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClock {
    /// Polar epoch: nanoseconds since 2000-01-01T00:00:00Z, like the strap.
    PolarEpoch,
    /// Explicitly unsynchronised: nanoseconds since simulator boot, with no
    /// wall-clock anchor.
    Unsynchronized,
}

impl DeviceClock {
    /// Parses a CLI value; anything else is a loud error.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_ascii_lowercase().as_str() {
            "polar-epoch" => Ok(Self::PolarEpoch),
            "unsynchronized" => Ok(Self::Unsynchronized),
            _ => Err(format!(
                "clock must be polar-epoch or unsynchronized, got {text:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PolarEpoch => "polar-epoch",
            Self::Unsynchronized => "unsynchronized",
        }
    }
}

/// Nanoseconds between the Unix epoch (1970-01-01) and the Polar epoch
/// (2000-01-01T00:00:00Z): 946684800 seconds.
pub const POLAR_EPOCH_OFFSET_NS: u64 = 946_684_800_000_000_000;

/// Convert a rate-independent sensor elapsed time to the selected device clock.
/// Refuses invalid epoch anchors and overflow instead of clipping a timestamp.
pub fn device_timestamp_from_elapsed_ns(
    clock: DeviceClock,
    boot_unix_ns: u64,
    elapsed_ns: u64,
) -> Result<u64, String> {
    match clock {
        DeviceClock::Unsynchronized => Ok(elapsed_ns),
        DeviceClock::PolarEpoch => boot_unix_ns
            .checked_sub(POLAR_EPOCH_OFFSET_NS)
            .ok_or_else(|| "sensor clock anchor precedes the Polar epoch".to_owned())?
            .checked_add(elapsed_ns)
            .ok_or_else(|| "sensor timestamp exceeds u64 nanoseconds".to_owned()),
    }
}

/// ECG waveform source: `"synthetic"`, `"recorded"` or `{"file": "path"}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum EcgSource {
    /// Deterministic synthetic PQRST waveform.
    #[serde(rename = "synthetic")]
    Synthetic,
    /// The real strap recording compiled in from
    /// `fixtures/h10-raw/ecg-E9B93D29-2026-09-19-130hz.txt`, cycling forever.
    #[serde(rename = "recorded")]
    Recorded,
    /// Replay a text file (one integer µV per line @130 Hz), cycling forever.
    File { file: String },
}

/// Heart-rate source: `"synthetic"` or `{"file": "path"}` in profiles.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub enum HrSource {
    /// Synthetic beat from the configured bpm (one RR interval, Task 6 jitter).
    #[default]
    #[serde(rename = "synthetic")]
    Synthetic,
    /// Replay the `raw.hrMeasurements` packets of a committed raw capture
    /// (`fixtures/h10-raw/*.json`: hex packets with timings), cycling forever.
    File { file: String },
}

/// Recorded Heart Rate Measurement packets, replayed verbatim in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HrReplay {
    pub packets: Vec<Vec<u8>>,
}

/// Loads recorded HR packets from a raw capture file: `{raw:
/// {hrMeasurements: [{atMs, hex}]}}`. Every packet is decoded and shape
/// checked (flags format bit, RR tail alignment); any problem names the
/// packet index — never a silent skip.
pub fn load_hr_replay(path: &str) -> Result<HrReplay, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read HR replay file {path}: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("HR replay file {path} is not valid JSON: {error}"))?;
    let measurements = value
        .get("raw")
        .and_then(|raw| raw.get("hrMeasurements"))
        .and_then(|list| list.as_array())
        .ok_or_else(|| format!("HR replay file {path} is missing array \"raw.hrMeasurements\""))?;
    if measurements.is_empty() {
        return Err(format!("HR replay file {path} has no hrMeasurements"));
    }
    let mut packets = Vec::with_capacity(measurements.len());
    for (index, measurement) in measurements.iter().enumerate() {
        let hex = measurement
            .get("hex")
            .and_then(|hex| hex.as_str())
            .ok_or_else(|| {
                format!("HR replay file {path} packet {index}: missing string \"hex\"")
            })?;
        let bytes = crate::profile::decode_hex(hex)
            .map_err(|error| format!("HR replay file {path} packet {index}: {error}"))?;
        let tail = match bytes.first() {
            Some(flags) if flags & 0x01 == 0 => bytes.get(2..),
            Some(_) => bytes.get(3..),
            None => None,
        };
        match tail {
            Some(tail) if tail.len().is_multiple_of(2) => packets.push(bytes),
            _ => {
                return Err(format!(
                    "HR replay file {path} packet {index}: {hex:?} is not a Heart Rate Measurement"
                ));
            }
        }
    }
    Ok(HrReplay { packets })
}

/// One step of a scripted heart-rate curve: hold `bpm` from `at_s` on.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct BpmStep {
    pub at_s: f64,
    pub bpm: u8,
}

/// Runtime configuration of the simulated strap.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub name: String,
    pub bpm: u8,
    pub battery_percent: u8,
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub hardware: String,
    pub software: String,
    /// 40-bit manufacturer identifier of the System ID (0x2A23).
    pub system_id_manufacturer: u64,
    /// 24-bit OUI of the System ID (0x2A23).
    pub system_id_oui: [u8; 3],
    /// Whether the HR flags carry sensor-contact bits at all. The strap
    /// reports contact not supported (`0x10`), so this defaults to false and
    /// `contact_detected` only takes effect when it is true.
    pub contact_supported: bool,
    /// Sensor-contact state reported in the HR flags when supported.
    pub contact_detected: bool,
    /// Battery drain in percent per minute (0 = fixed level).
    pub drain_per_min: f64,
    /// RR-interval jitter in milliseconds (deterministic sine, Task 6).
    pub rr_jitter_ms: f64,
    /// Manufacturer data company id for the advertisement.
    pub mfr_company: u16,
    /// Manufacturer data payload bytes for the advertisement.
    pub mfr_payload: Vec<u8>,
    /// Pairing/bonding policy.
    pub pair_policy: PairPolicy,
    /// ECG waveform source.
    pub ecg_source: EcgSource,
    /// Heart-rate source (synthetic beat or recorded replay).
    pub hr_source: HrSource,
    /// Scripted heart-rate curve (empty = fixed bpm).
    pub bpm_curve: Vec<BpmStep>,
    /// Profile file this config was loaded from, if any.
    pub profile_path: Option<String>,
    /// Heart-rate notification rate in Hz.
    pub hr_hz: f64,
    /// ECG samples per data frame.
    pub ecg_frame_samples: usize,
    /// ECG dispatch opportunities per second, not sensor sample rate (fixed 130 Hz).
    /// Each opportunity sends zero or bounded multiple acquired frames.
    pub ecg_frames_per_sec: f64,
    /// Device clock for ECG timestamps (default: the strap's Polar epoch).
    pub clock: DeviceClock,
    /// Extra Bluetooth addresses `drop-link` disconnects on top of the
    /// tracked GATT clients (centrals whose addresses touched this
    /// peripheral's GATT application). Only listed addresses and tracked
    /// clients are ever touched — the adapter's other devices never are.
    /// The other isolation option is a dedicated adapter; see the README.
    pub drop_link_allowlist: Vec<String>,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            name: gatt_spec::DEFAULT_ADV_NAME.to_string(),
            bpm: 72,
            battery_percent: 90,
            manufacturer: "Polar Electro Oy".to_string(),
            model: "H10".to_string(),
            serial: "SIM000001".to_string(),
            firmware: "3.2.1".to_string(),
            hardware: "9".to_string(),
            software: "3.2.1".to_string(),
            system_id_manufacturer: 1,
            system_id_oui: [0x6B, 0x00, 0x00],
            contact_supported: false,
            contact_detected: false,
            drain_per_min: 0.0,
            rr_jitter_ms: 0.0,
            mfr_company: gatt_spec::POLAR_COMPANY_ID,
            mfr_payload: Vec::new(),
            pair_policy: PairPolicy::JustWorks,
            ecg_source: EcgSource::Recorded,
            hr_source: HrSource::Synthetic,
            bpm_curve: Vec::new(),
            profile_path: None,
            hr_hz: 1.0,
            ecg_frame_samples: gatt_spec::H10_ECG_SAMPLES_PER_FRAME,
            ecg_frames_per_sec: gatt_spec::H10_ECG_FRAMES_PER_SEC,
            clock: DeviceClock::PolarEpoch,
            drop_link_allowlist: Vec::new(),
        }
    }
}

/// What handling a PMD write asks the transport to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmdAction {
    None,
    StartEcg,
    StopEcg,
    StartAcc(crate::acc::Settings),
    StopAcc,
}

/// The answer to a PMD control-point write: bytes to indicate back, plus the
/// streaming action. `indicate: None` means the write was malformed and must
/// be logged without answering (never silently dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmdWriteOutcome {
    pub indicate: Option<Vec<u8>>,
    pub action: PmdAction,
}

/// A PMD indication waiting out its measured response latency: the ATT
/// write was already answered, and the tick loop sends this when due — so a
/// measured latency never blocks the event loop.
#[derive(Debug, Clone)]
pub struct PendingIndication {
    pub due: Instant,
    pub response: Vec<u8>,
    /// Streaming action committed when the indication actually goes out.
    pub action: PmdAction,
}

/// Mutable simulator state: config plus faults.
#[derive(Debug)]
pub struct SimState {
    pub config: SimConfig,
    /// When set, notifications stop but the link stays up.
    pub silent: bool,
    /// Whether the ECG stream is currently running.
    pub ecg_streaming: bool,
    /// Selected ACC mode; absent until its successful START indication.
    pub acc_settings: Option<crate::acc::Settings>,
    /// Sensor scheduler origin, committed with ACC settings, never at write time.
    pub acc_started_at: Option<Instant>,
    pub ecg_started_at: Option<Instant>,
    /// Status code forced onto the next PMD command response, then cleared.
    pub reject_next_status: Option<u8>,
    /// Running ECG sample index (130 Hz clock).
    pub ecg_sample_index: u64,
    /// Running heart-rate beat index (drives the deterministic RR jitter).
    pub hr_beat_index: u64,
    /// Recorded HR packets (None = synthetic beat).
    pub hr_replay: Option<HrReplay>,
    /// Next recorded HR packet to serve.
    pub hr_replay_index: u64,
    /// Fractional battery drain accumulator (percent, Task 5).
    pub battery_carry: f64,
    /// Recorded ECG replay samples (None = synthetic waveform).
    pub ecg_replay: Option<Vec<i32>>,
    /// PMD indications whose measured latency has not expired yet.
    pub pending_indications: Vec<PendingIndication>,
    /// Transport-queued responses whose OS acceptance is still unresolved.
    /// IDs are process-monotonic and never reused by the transport.
    pending_pmd_actions: Vec<(u64, PmdAction)>,
    /// Run posture (`--mode`; profiles cannot change it).
    pub run_mode: RunMode,
    /// Timing seed for this run (`--timing-seed`; same seed replays a run).
    pub run_seed: u64,
    /// When the run started (RFC3339 UTC), for the run record.
    pub run_started_at: String,
    /// Injected fault sequence with timestamps; `run-record` reports it.
    pub faults: Vec<FaultEntry>,
    /// Extra PMD response latency in ms (adversarial `delay-responses`;
    /// 0 = off, on top of any measured latency).
    pub response_delay_ms: u64,
    /// Armed by adversarial `interrupt-next-subscribe`: the next
    /// subscription setup is torn down as soon as it completes.
    pub interrupt_next_subscribe: bool,
    /// Deliver every n-th ECG frame only (adversarial
    /// `constrain-delivery`; 1 = every frame).
    pub delivery_keep_every: u64,
    /// Sequence number of the next deliverable ECG frame (shed accounting).
    pub delivery_seq: u64,
    /// Last PMD response bytes sent (adversarial `stale-callback` replays
    /// them out of sequence).
    pub last_pmd_response: Option<Vec<u8>>,
    /// Run-wide observed HR activity. Never inferred from a link-drop request.
    hr_recovery: HrRecovery,
}

/// Cumulative evidence, not a delivery guarantee: peripheral APIs do not
/// expose a portable central identity or confirmation of a notification read.
#[derive(Debug, Default)]
struct HrRecovery {
    subscription_enable_events: u64,
    subscription_disable_events: u64,
    notification_attempts: u64,
    notifications_queued: u64,
    notifications_os_accepted: u64,
    notifications_not_subscribed: u64,
    notifications_failed: u64,
    counters_saturated: bool,
}

impl HrRecovery {
    fn increment(counter: &mut u64, saturated: &mut bool) {
        match counter.checked_add(1) {
            Some(next) => *counter = next,
            None => *saturated = true,
        }
    }

    fn observe_outcome(&mut self, outcome: &crate::radio::SendOutcome) {
        use crate::radio::SendOutcome;
        let counter = match outcome {
            SendOutcome::Queued { .. } => &mut self.notifications_queued,
            SendOutcome::OsAccepted => &mut self.notifications_os_accepted,
            SendOutcome::NotSubscribed => &mut self.notifications_not_subscribed,
            SendOutcome::Failed(_) => &mut self.notifications_failed,
        };
        Self::increment(counter, &mut self.counters_saturated);
    }

    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "scope":"characteristic",
            "clientAttribution":"unavailable",
            "subscriptionEnableEvents":self.subscription_enable_events,
            "subscriptionDisableEvents":self.subscription_disable_events,
            "notificationAttempts":self.notification_attempts,
            "notificationsQueued":self.notifications_queued,
            "notificationsOsAccepted":self.notifications_os_accepted,
            "notificationsNotSubscribed":self.notifications_not_subscribed,
            "notificationsFailed":self.notifications_failed,
            "countersSaturated":self.counters_saturated,
        })
    }
}

fn is_hr_characteristic(characteristic: &str) -> bool {
    uuid::Uuid::parse_str(characteristic).is_ok_and(|uuid| {
        uuid == crate::advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_MEASUREMENT)
    })
}

/// One injected fault with its timestamp: the labelled sequence
/// `run-record` reports. Never silent — every adversarial command that
/// fires appends exactly one entry.
#[derive(Debug, Clone)]
pub struct FaultEntry {
    pub ts: String,
    pub fault: String,
    pub detail: serde_json::Value,
}

/// Whether ECG frame `seq` goes out under `keep_every` shedding: the first
/// frame of each group is kept, the rest are shed with a loud log line.
/// `keep_every <= 1` disables shedding. Pure so tests pin it exactly.
pub fn should_deliver(seq: u64, keep_every: u64) -> bool {
    keep_every <= 1 || seq.is_multiple_of(keep_every)
}

impl SimState {
    pub fn new(config: SimConfig) -> Self {
        Self {
            config,
            silent: false,
            ecg_streaming: false,
            acc_settings: None,
            acc_started_at: None,
            ecg_started_at: None,
            reject_next_status: None,
            ecg_sample_index: 0,
            hr_beat_index: 0,
            hr_replay: None,
            hr_replay_index: 0,
            battery_carry: 0.0,
            ecg_replay: None,
            pending_indications: Vec::new(),
            pending_pmd_actions: Vec::new(),
            run_mode: RunMode::Faithful,
            run_seed: 0,
            run_started_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            faults: Vec::new(),
            response_delay_ms: 0,
            interrupt_next_subscribe: false,
            delivery_keep_every: 1,
            delivery_seq: 0,
            last_pmd_response: None,
            hr_recovery: HrRecovery::default(),
        }
    }

    /// Records one injected fault with its timestamp. Every adversarial
    /// command that fires calls this exactly once — the labelled sequence
    /// [`Self::run_record`] reports.
    pub fn record_fault(&mut self, fault: &str, detail: serde_json::Value) {
        self.faults.push(FaultEntry {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            fault: fault.to_string(),
            detail,
        });
    }

    pub fn observe_subscription(&mut self, characteristic: &str, enabled: bool) {
        if !enabled && characteristic.eq_ignore_ascii_case(gatt_spec::pmd::DATA) {
            self.reset_pmd_session();
        } else if !enabled && characteristic.eq_ignore_ascii_case(gatt_spec::pmd::CONTROL_POINT) {
            // Releasing command acknowledgements does not release the data lane.
            self.pending_indications.clear();
            self.pending_pmd_actions.clear();
            self.last_pmd_response = None;
        }
        if !is_hr_characteristic(characteristic) {
            return;
        }
        let counter = if enabled {
            &mut self.hr_recovery.subscription_enable_events
        } else {
            &mut self.hr_recovery.subscription_disable_events
        };
        HrRecovery::increment(counter, &mut self.hr_recovery.counters_saturated);
    }

    pub fn observe_hr_notify(
        &mut self,
        outcome: &Result<crate::radio::SendOutcome, crate::radio::RadioError>,
    ) {
        HrRecovery::increment(
            &mut self.hr_recovery.notification_attempts,
            &mut self.hr_recovery.counters_saturated,
        );
        match outcome {
            Ok(outcome) => self.hr_recovery.observe_outcome(outcome),
            Err(_) => HrRecovery::increment(
                &mut self.hr_recovery.notifications_failed,
                &mut self.hr_recovery.counters_saturated,
            ),
        }
    }

    pub fn observe_notify_settled(
        &mut self,
        characteristic: &str,
        outcome: &crate::radio::SendOutcome,
    ) {
        if is_hr_characteristic(characteristic) {
            self.hr_recovery.observe_outcome(outcome);
        }
    }

    /// This run's seed/profile, mode and injected fault sequence with
    /// timestamps — the `run-record` answer.
    pub fn run_record(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.run_mode.as_str(),
            "seed": self.run_seed,
            "profile": self.config.profile_path.clone().unwrap_or_else(|| "<builtin stock-h10>".to_string()),
            "name": self.config.name,
            "startedAt": self.run_started_at,
            "hrRecovery": self.hr_recovery.snapshot(),
            "faults": self.faults.iter().map(|entry| {
                serde_json::json!({"ts": entry.ts, "fault": entry.fault, "detail": entry.detail})
            }).collect::<Vec<_>>(),
        })
    }

    /// RR interval for one beat: the base `60/bpm` plus the configured
    /// jitter as a deterministic sine of the beat index (period 10 beats).
    /// Pure and allocation-free, so tests pin exact bytes without a radio.
    pub fn rr_interval_s(bpm: u8, jitter_ms: f64, beat: u64) -> f64 {
        let base = if bpm == 0 { 0.0 } else { 60.0 / f64::from(bpm) };
        if jitter_ms <= 0.0 || base <= 0.0 {
            return base;
        }
        base + (jitter_ms / 1000.0) * (2.0 * std::f64::consts::PI * beat as f64 / 10.0).sin()
    }

    /// BPM selected by the scripted curve at `elapsed_s` (last step at or
    /// before now wins; order-independent). Empty curve keeps the fixed bpm.
    pub fn curve_bpm(&self, elapsed_s: f64) -> u8 {
        let mut bpm = self.config.bpm;
        let mut best_at = f64::NEG_INFINITY;
        for step in &self.config.bpm_curve {
            if elapsed_s >= step.at_s && step.at_s >= best_at {
                bpm = step.bpm;
                best_at = step.at_s;
            }
        }
        bpm
    }

    /// Current Heart Rate Measurement payload. A loaded HR replay serves the
    /// recorded packets verbatim, cycling forever; otherwise one RR interval
    /// is synthesized from the bpm. Like the strap, contact bits are absent
    /// (`0x10`) unless the profile declares contact supported. Advances the
    /// beat and replay indexes, so each call is the next beat.
    pub fn hr_payload(&mut self) -> Vec<u8> {
        if let Some(replay) = &self.hr_replay {
            if !replay.packets.is_empty() {
                let at = (self.hr_replay_index as usize) % replay.packets.len();
                self.hr_replay_index = self.hr_replay_index.saturating_add(1);
                self.hr_beat_index = self.hr_beat_index.saturating_add(1);
                return replay.packets[at].clone();
            }
        }
        let beat = self.hr_beat_index;
        self.hr_beat_index = beat.saturating_add(1);
        let rr_s = Self::rr_interval_s(self.config.bpm, self.config.rr_jitter_ms, beat);
        if self.config.contact_supported {
            gatt_spec::encode_hr_measurement_with_contact(
                self.config.bpm,
                &[rr_s],
                self.config.contact_detected,
            )
        } else {
            gatt_spec::encode_hr_measurement_no_contact(self.config.bpm, &[rr_s])
        }
    }

    /// Bytes returned by a PMD control-point read: the feature set.
    pub fn pmd_features(&self) -> Vec<u8> {
        gatt_spec::encode_pmd_features()
    }

    /// Reads a 128-bit vendor characteristic by full UUID. The canonical
    /// `6217ff4c` UUID is retained for fingerprints, but no vendor value is
    /// modeled or served, so pure reads return `None` rather than an empty
    /// placeholder.
    pub fn vendor_read(&self, _uuid: &uuid::Uuid) -> Option<Vec<u8>> {
        None
    }

    /// Whether a full-UUID characteristic is writable but has no behaviour
    /// model (`6217ff4d`, FEEE `0x53`): writes there are refused
    /// loudly as `unmodeled-vendor-write`, never absorbed.
    pub fn vendor_writable(&self, uuid: &uuid::Uuid) -> bool {
        let text = uuid.to_string();
        [gatt_spec::vendor::WRITE_INDICATE, gatt_spec::feee::CHAR_53]
            .iter()
            .any(|known| text.eq_ignore_ascii_case(known))
    }

    /// Device Information / Battery read handler. DIS strings carry the
    /// strap's trailing NUL. `None` means the characteristic does not exist
    /// on an H10 (notably PnP ID 0x2A50).
    pub fn static_read(&self, char_uuid16: u16) -> Option<Vec<u8>> {
        match char_uuid16 {
            x if x == gatt_spec::uuid16::MANUFACTURER_NAME => {
                Some(gatt_spec::encode_dis_string(&self.config.manufacturer))
            }
            x if x == gatt_spec::uuid16::MODEL_NUMBER => {
                Some(gatt_spec::encode_dis_string(&self.config.model))
            }
            x if x == gatt_spec::uuid16::SERIAL_NUMBER => {
                Some(gatt_spec::encode_dis_string(&self.config.serial))
            }
            x if x == gatt_spec::uuid16::FIRMWARE_REVISION => {
                Some(gatt_spec::encode_dis_string(&self.config.firmware))
            }
            x if x == gatt_spec::uuid16::HARDWARE_REVISION => {
                Some(gatt_spec::encode_dis_string(&self.config.hardware))
            }
            x if x == gatt_spec::uuid16::SOFTWARE_REVISION => {
                Some(gatt_spec::encode_dis_string(&self.config.software))
            }
            x if x == gatt_spec::uuid16::SYSTEM_ID => Some(gatt_spec::encode_system_id(
                self.config.system_id_manufacturer,
                self.config.system_id_oui,
            )),
            x if x == gatt_spec::uuid16::BODY_SENSOR_LOCATION => {
                Some(gatt_spec::encode_body_sensor_location())
            }
            x if x == gatt_spec::uuid16::BATTERY_LEVEL => {
                Some(gatt_spec::encode_battery_level(self.config.battery_percent))
            }
            _ => None,
        }
    }

    /// Applies battery drain for `elapsed_s` seconds. Returns true when the
    /// reported level changed (fractional drain accumulates in
    /// `battery_carry`); the level saturates at zero.
    pub fn tick_battery(&mut self, elapsed_s: f64) -> bool {
        if self.config.drain_per_min <= 0.0 || elapsed_s <= 0.0 {
            return false;
        }
        if self.config.battery_percent == 0 {
            return false;
        }
        self.battery_carry += self.config.drain_per_min * elapsed_s / 60.0;
        let whole = self.battery_carry.floor() as u8;
        if whole == 0 {
            return false;
        }
        self.battery_carry -= f64::from(whole);
        let next = self.config.battery_percent.saturating_sub(whole);
        let changed = next != self.config.battery_percent;
        self.config.battery_percent = next;
        if next == 0 {
            self.battery_carry = 0.0;
        }
        changed
    }

    /// JSON snapshot of the live state for `get-state`.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.config.name,
            "bpm": self.config.bpm,
            "batteryPercent": self.config.battery_percent,
            "contactDetected": self.config.contact_detected,
            "pairPolicy": self.config.pair_policy.as_str(),
            "profile": self.config.profile_path,
            "silent": self.silent,
            "ecgStreaming": self.ecg_streaming,
            "accStreaming": self.acc_settings.is_some(),
            "accSettings": self.acc_settings.map(|settings| serde_json::json!({
                "sampleRateHz": settings.sample_rate_hz,
                "resolutionBits": crate::acc::RESOLUTION_BITS,
                "rangeG": settings.range_g,
                "channels": crate::acc::CHANNELS,
            })),
            "rejectNextPmd": self.reject_next_status,
            "hrHz": self.config.hr_hz,
            "ecgFramesPerSec": self.config.ecg_frames_per_sec,
            "ecgFrameSamples": self.config.ecg_frame_samples,
            "ecgSampleIndex": self.ecg_sample_index,
            "clock": self.config.clock.as_str(),
            "dropLinkAllowlist": self.config.drop_link_allowlist,
            "mode": self.run_mode.as_str(),
            "responseDelayMs": self.response_delay_ms,
            "deliveryKeepEvery": self.delivery_keep_every,
            "faults": self.faults.len(),
            "hrRecovery": self.hr_recovery.snapshot(),
        })
    }

    /// Bound delayed responses and queued streaming actions together. The
    /// transport separately bounds queued responses that have no state action.
    pub fn can_admit_pmd_command(&self) -> bool {
        self.pending_indications
            .len()
            .saturating_add(self.pending_pmd_actions.len())
            < crate::radio::SEND_QUEUE_CAPACITY
    }

    /// Handles a PMD control-point write, returning the indicate payload.
    /// Decides a PMD control-point write: response bytes plus the streaming
    /// action. Decide-only for streaming state — the action commits via
    /// [`Self::apply_pmd_action`] when the indication actually goes out, so
    /// frames never precede the START/STOP response the central waits for,
    /// and a central that vanishes mid-latency leaves no stuck stream
    /// behind. (Takes `&mut` only to consume a pending injected fault.)
    pub fn handle_pmd_write(&mut self, bytes: &[u8]) -> PmdWriteOutcome {
        if !self.can_admit_pmd_command() {
            return PmdWriteOutcome {
                indicate: None,
                action: PmdAction::None,
            };
        }
        let [op, measurement_type, parameters @ ..] = bytes else {
            return PmdWriteOutcome {
                indicate: None,
                action: PmdAction::None,
            };
        };
        let (op, measurement_type) = (*op, *measurement_type);
        let answer = |status: u8, params: &[u8], action: PmdAction| PmdWriteOutcome {
            indicate: Some(gatt_spec::encode_pmd_response(
                op,
                measurement_type,
                status,
                false,
                params,
            )),
            action,
        };
        if let Some(status) = self.reject_next_status.take() {
            return answer(status, &[], PmdAction::None);
        }
        if !matches!(
            op,
            gatt_spec::PMD_OP_GET_SETTINGS | gatt_spec::PMD_OP_START | gatt_spec::PMD_OP_STOP
        ) {
            return answer(gatt_spec::PMD_STATUS_INVALID_OP, &[], PmdAction::None);
        }
        // Polar SDK PmdRecordingType.asBitField uses bit 7 for offline mode.
        // This simulator implements online ECG/ACC only. Do not mask mode
        // bits and silently start a different mechanism. Bit 6 is likewise
        // unimplemented; exact real-firmware refusal precedence is unmeasured.
        if measurement_type & 0xc0 != 0 {
            return answer(gatt_spec::PMD_STATUS_NOT_SUPPORTED, &[], PmdAction::None);
        }
        match measurement_type {
            gatt_spec::PMD_MEASUREMENT_ECG | gatt_spec::PMD_MEASUREMENT_ACC => {}
            0x01 | 0x03 => {
                return answer(gatt_spec::PMD_STATUS_NOT_SUPPORTED, &[], PmdAction::None);
            }
            _ => {
                return answer(
                    gatt_spec::PMD_STATUS_INVALID_MEASUREMENT_TYPE,
                    &[],
                    PmdAction::None,
                );
            }
        }
        if op != gatt_spec::PMD_OP_START && !parameters.is_empty() {
            return answer(gatt_spec::PMD_STATUS_INVALID_LENGTH, &[], PmdAction::None);
        }
        let pending_actions = self
            .pending_pmd_actions
            .iter()
            .map(|(_, action)| *action)
            .chain(
                self.pending_indications
                    .iter()
                    .map(|pending| pending.action),
            );
        let (ecg_pending, acc_pending) = pending_actions.fold(
            (self.ecg_streaming, self.acc_settings.is_some()),
            |(ecg, acc), action| match action {
                PmdAction::StartEcg => (true, acc),
                PmdAction::StopEcg => (false, acc),
                PmdAction::StartAcc(_) => (ecg, true),
                PmdAction::StopAcc => (ecg, false),
                PmdAction::None => (ecg, acc),
            },
        );
        let is_acc = measurement_type == gatt_spec::PMD_MEASUREMENT_ACC;
        let streaming = if is_acc { acc_pending } else { ecg_pending };
        match op {
            gatt_spec::PMD_OP_GET_SETTINGS => answer(
                gatt_spec::PMD_STATUS_SUCCESS,
                &if is_acc {
                    crate::acc::settings_payload()
                } else {
                    gatt_spec::encode_ecg_settings()
                },
                PmdAction::None,
            ),
            gatt_spec::PMD_OP_START => {
                // Like the strap, starting twice is ALREADY_IN_STATE: the
                // stream keeps running, no second start is emitted.
                if streaming {
                    return answer(gatt_spec::PMD_STATUS_ALREADY_IN_STATE, &[], PmdAction::None);
                }
                let action = if is_acc {
                    crate::acc::validate_start(parameters).map(PmdAction::StartAcc)
                } else {
                    validate_start_settings(parameters).map(|()| PmdAction::StartEcg)
                };
                match action {
                    Ok(action) => answer(gatt_spec::PMD_STATUS_SUCCESS, &[], action),
                    Err(status) => answer(status, &[], PmdAction::None),
                }
            }
            gatt_spec::PMD_OP_STOP => {
                // Like the strap, stopping while idle is ALREADY_IN_STATE.
                if !streaming {
                    return answer(gatt_spec::PMD_STATUS_ALREADY_IN_STATE, &[], PmdAction::None);
                }
                answer(
                    gatt_spec::PMD_STATUS_SUCCESS,
                    &[],
                    if is_acc {
                        PmdAction::StopAcc
                    } else {
                        PmdAction::StopEcg
                    },
                )
            }
            _ => unreachable!("op validity is checked above"),
        }
    }

    /// Commits a decided PMD action: call when the indication actually goes
    /// out (inline answer or deferred drain), never at write time.
    pub fn apply_pmd_action(&mut self, action: PmdAction) {
        match action {
            PmdAction::StartEcg => {
                self.ecg_streaming = true;
                self.ecg_started_at = Some(Instant::now());
            }
            PmdAction::StopEcg => {
                self.ecg_streaming = false;
                self.ecg_started_at = None;
            }
            PmdAction::StartAcc(settings) => {
                self.acc_settings = Some(settings);
                self.acc_started_at = Some(Instant::now());
            }
            PmdAction::StopAcc => {
                self.acc_settings = None;
                self.acc_started_at = None;
            }
            PmdAction::None => {}
        }
    }

    /// End the scoped PMD session without leaking old actions into a new one.
    /// Run-wide configuration, injected faults and evidence counters survive.
    pub fn reset_pmd_session(&mut self) {
        self.ecg_streaming = false;
        self.ecg_started_at = None;
        self.acc_settings = None;
        self.acc_started_at = None;
        self.pending_indications.clear();
        self.pending_pmd_actions.clear();
        self.last_pmd_response = None;
    }

    /// Remember transport admission without claiming OS acceptance.
    pub fn queue_pmd_action(&mut self, id: u64, action: PmdAction) {
        if action != PmdAction::None {
            self.pending_pmd_actions.push((id, action));
        }
    }

    /// Return an exactly matched accepted action for the caller to commit/log.
    /// A failed, repeated or prior-session completion must not start a stream.
    pub fn settle_pmd_action(&mut self, id: u64, accepted: bool) -> Option<PmdAction> {
        let index = self
            .pending_pmd_actions
            .iter()
            .position(|(pending, _)| *pending == id)?;
        let (_, action) = self.pending_pmd_actions.remove(index);
        accepted.then_some(action)
    }

    /// A later response cannot overtake an earlier delayed START/STOP.
    /// Dequeue is not commit: transport applies the action only after the
    /// indication is accepted, and otherwise reports the failed indication.
    pub fn take_due_indication(&mut self, now: Instant) -> Option<PendingIndication> {
        self.pending_indications
            .first()
            .filter(|pending| pending.due <= now)?;
        Some(self.pending_indications.remove(0))
    }
}

/// Validates the settings TLV of a PMD start command against the H10's fixed
/// ECG configuration (130 Hz / 14 bit). Returns the status code to report.
fn validate_start_settings(tlv: &[u8]) -> Result<(), u8> {
    let mut offset = 0;
    let mut selected = [false; 2];
    while offset < tlv.len() {
        let setting = tlv[offset];
        let Some(&count) = tlv.get(offset + 1) else {
            return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
        };
        let Some(seen) = selected.get_mut(usize::from(setting)) else {
            return Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER);
        };
        if count != 1 || *seen {
            return Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER);
        }
        *seen = true;
        let value_at = |index: usize| -> Option<u16> {
            let base = offset + 2 + index * 2;
            Some(u16::from_le_bytes([*tlv.get(base)?, *tlv.get(base + 1)?]))
        };
        match setting {
            0x00 => {
                for index in 0..usize::from(count) {
                    let Some(rate) = value_at(index) else {
                        return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
                    };
                    if rate != gatt_spec::H10_ECG_SAMPLE_RATE_HZ {
                        return Err(gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE);
                    }
                }
                offset += 2 + usize::from(count) * 2;
            }
            0x01 => {
                for index in 0..usize::from(count) {
                    let Some(resolution) = value_at(index) else {
                        return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
                    };
                    if resolution != gatt_spec::H10_ECG_RESOLUTION_BITS {
                        return Err(gatt_spec::PMD_STATUS_INVALID_RESOLUTION);
                    }
                }
                offset += 2 + usize::from(count) * 2;
            }
            _ => {
                // Other setting kinds are length-prefixed but opaque here: skip
                // only what the count byte promises for known 2-byte fields is
                // unsafe, so an unknown setting ends validation as malformed.
                return Err(gatt_spec::PMD_STATUS_INVALID_LENGTH);
            }
        }
    }
    if selected.into_iter().all(|present| present) {
        Ok(())
    } else {
        Err(gatt_spec::PMD_STATUS_INVALID_PARAMETER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SimState {
        SimState::new(SimConfig::default())
    }

    #[test]
    fn arbitrary_rate_sensor_clock_checks_epoch_and_elapsed_overflow() {
        assert_eq!(
            device_timestamp_from_elapsed_ns(DeviceClock::Unsynchronized, 0, 123),
            Ok(123)
        );
        assert_eq!(
            device_timestamp_from_elapsed_ns(
                DeviceClock::PolarEpoch,
                POLAR_EPOCH_OFFSET_NS + 10,
                20
            ),
            Ok(30)
        );
        assert!(device_timestamp_from_elapsed_ns(
            DeviceClock::PolarEpoch,
            POLAR_EPOCH_OFFSET_NS - 1,
            0
        )
        .is_err());
        assert!(device_timestamp_from_elapsed_ns(
            DeviceClock::PolarEpoch,
            u64::MAX,
            POLAR_EPOCH_OFFSET_NS + 1
        )
        .is_err());
    }

    #[test]
    fn hr_recovery_records_actual_acceptance_without_counting_queued_as_delivered() {
        use crate::radio::{RadioError, SendOutcome};
        let mut sim = state();
        let hr = "00002a37-0000-1000-8000-00805f9b34fb";
        sim.observe_subscription(hr, true);
        sim.observe_hr_notify(&Ok(SendOutcome::Queued { id: 1 }));
        assert_eq!(sim.snapshot()["hrRecovery"]["notificationsOsAccepted"], 0);
        sim.observe_notify_settled(hr, &SendOutcome::OsAccepted);
        sim.observe_subscription(hr, false);
        sim.record_fault("drop-link", serde_json::json!({"dropped":["peer"]}));
        sim.observe_hr_notify(&Ok(SendOutcome::NotSubscribed));
        sim.observe_subscription(hr, true);
        sim.observe_hr_notify(&Ok(SendOutcome::OsAccepted));
        sim.observe_hr_notify(&Ok(SendOutcome::Failed("queue-full".to_owned())));
        sim.observe_hr_notify(&Err(RadioError("native refusal".to_owned())));
        sim.observe_hr_notify(&Ok(SendOutcome::Queued { id: 2 }));
        sim.observe_notify_settled(hr, &SendOutcome::Failed("old generation".to_owned()));
        let telemetry = sim.snapshot()["hrRecovery"].clone();
        assert_eq!(telemetry["subscriptionEnableEvents"], 2);
        assert_eq!(telemetry["subscriptionDisableEvents"], 1);
        assert_eq!(telemetry["notificationAttempts"], 6);
        assert_eq!(telemetry["notificationsQueued"], 2);
        assert_eq!(telemetry["notificationsOsAccepted"], 2);
        assert_eq!(telemetry["notificationsNotSubscribed"], 1);
        assert_eq!(telemetry["notificationsFailed"], 3);
        assert_eq!(telemetry["clientAttribution"], "unavailable");
        assert_eq!(telemetry, sim.run_record()["hrRecovery"]);
        sim.config.bpm = 90;
        assert_eq!(
            telemetry,
            sim.snapshot()["hrRecovery"],
            "configuration changes cannot erase run evidence"
        );
    }

    #[test]
    fn other_characteristics_do_not_pollute_hr_recovery_evidence() {
        let mut sim = state();
        let before = sim.snapshot()["hrRecovery"].clone();
        sim.observe_subscription(gatt_spec::pmd::DATA, true);
        sim.observe_notify_settled(gatt_spec::pmd::DATA, &crate::radio::SendOutcome::OsAccepted);
        assert_eq!(before, sim.snapshot()["hrRecovery"]);
        sim.record_fault("drop-link", serde_json::json!({"dropped":["peer"]}));
        assert_eq!(
            before,
            sim.snapshot()["hrRecovery"],
            "a requested link drop cannot manufacture unsubscribe or delivery evidence"
        );
    }

    #[test]
    fn hr_recovery_counter_saturation_is_explicit() {
        let mut sim = state();
        sim.hr_recovery.notification_attempts = u64::MAX;
        sim.observe_hr_notify(&Ok(crate::radio::SendOutcome::NotSubscribed));
        let telemetry = sim.snapshot()["hrRecovery"].clone();
        assert_eq!(telemetry["notificationAttempts"], u64::MAX);
        assert_eq!(telemetry["notificationsNotSubscribed"], 1);
        assert_eq!(telemetry["countersSaturated"], true);
    }

    #[test]
    fn run_record_reports_mode_seed_profile_and_faults() {
        let mut sim = state();
        assert_eq!(sim.run_mode, crate::control::RunMode::Faithful);
        sim.run_mode = crate::control::RunMode::Adversarial;
        sim.run_seed = 7;
        sim.record_fault(
            "drop-link",
            serde_json::json!({"dropped": ["AA:AA:AA:AA:AA:AA"]}),
        );
        let record = sim.run_record();
        assert_eq!(record["mode"], serde_json::json!("adversarial"));
        assert_eq!(record["seed"], serde_json::json!(7));
        assert!(record["profile"].is_string(), "profile must be named");
        assert!(record["startedAt"].is_string(), "start must be timestamped");
        let faults = record["faults"].as_array().expect("faults must be a list");
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0]["fault"], serde_json::json!("drop-link"));
        assert!(faults[0]["ts"].is_string(), "faults must be timestamped");
    }

    #[test]
    fn delivery_shed_keeps_every_nth_frame() {
        assert!(super::should_deliver(0, 1));
        assert!(super::should_deliver(3, 1));
        assert!(super::should_deliver(0, 4));
        assert!(!super::should_deliver(1, 4));
        assert!(!super::should_deliver(7, 4));
        assert!(super::should_deliver(8, 4));
    }

    #[test]
    fn ecg_timestamps_use_the_polar_epoch() {
        // Booted half a second after 2000-01-01: the frame ending at sample
        // 130 (one second of 130 Hz ECG) stamps 1.5 s in Polar-epoch
        // nanoseconds — never a Unix-epoch value like the old wall clock.
        let boot_unix_ns = super::POLAR_EPOCH_OFFSET_NS + 500_000_000;
        assert_eq!(
            super::device_timestamp_from_elapsed_ns(
                super::DeviceClock::PolarEpoch,
                boot_unix_ns,
                1_000_000_000
            )
            .unwrap(),
            1_500_000_000
        );
    }

    #[test]
    fn unsynchronised_clock_counts_from_boot() {
        // Explicitly unsynchronised: the wall clock never enters the stamp.
        assert_eq!(
            super::device_timestamp_from_elapsed_ns(
                super::DeviceClock::Unsynchronized,
                9_999_999_999,
                1_000_000_000
            )
            .unwrap(),
            1_000_000_000
        );
    }

    #[test]
    fn polar_epoch_frame_encodes_exact_bytes() {
        // One second after the Polar epoch, one zero sample: the full frame
        // is pinned byte for byte.
        let timestamp_ns = super::device_timestamp_from_elapsed_ns(
            super::DeviceClock::PolarEpoch,
            super::POLAR_EPOCH_OFFSET_NS,
            1_000_000_000,
        )
        .unwrap();
        assert_eq!(timestamp_ns, 1_000_000_000);
        let frame = gatt_spec::encode_ecg_frame(timestamp_ns, &[0]);
        assert_eq!(
            frame,
            vec![0x00, 0x00, 0xCA, 0x9A, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn device_clock_parses_explicitly() {
        assert_eq!(
            super::DeviceClock::parse("polar-epoch"),
            Ok(super::DeviceClock::PolarEpoch)
        );
        assert_eq!(
            super::DeviceClock::parse("unsynchronized"),
            Ok(super::DeviceClock::Unsynchronized)
        );
        assert!(super::DeviceClock::parse("unix").is_err());
    }

    #[test]
    fn hr_payload_carries_configured_bpm() {
        let mut sim = state();
        sim.config.bpm = 96;
        let payload = sim.hr_payload();
        assert_eq!(payload[1], 96);
        assert_eq!(
            payload[0] & gatt_spec::HR_FLAG_RR_PRESENT,
            gatt_spec::HR_FLAG_RR_PRESENT
        );
    }

    #[test]
    fn drain_applies_per_minute_and_floors_at_zero() {
        let mut sim = state();
        sim.config.battery_percent = 10;
        sim.config.drain_per_min = 6.0;
        assert!(sim.tick_battery(30.0), "3 points drained: changed");
        assert_eq!(sim.config.battery_percent, 7);
        assert!(!sim.tick_battery(0.0), "no time passes: unchanged");
        assert!(sim.tick_battery(600.0), "drains to zero: changed");
        assert_eq!(sim.config.battery_percent, 0);
        assert!(!sim.tick_battery(60.0), "already zero: unchanged");
    }

    #[test]
    fn rr_jitter_varies_intervals_deterministically() {
        let with_jitter = |beat: u64| SimState::rr_interval_s(60, 50.0, beat);
        assert_eq!(with_jitter(0), with_jitter(10), "sine period is 10 beats");
        assert_ne!(with_jitter(0), with_jitter(3), "adjacent beats differ");
        assert_eq!(
            SimState::rr_interval_s(60, 0.0, 0),
            1.0,
            "no jitter: exact base interval"
        );
        let mut a = state();
        a.config.bpm = 60;
        a.config.rr_jitter_ms = 50.0;
        let first = a.hr_payload();
        let mut b = state();
        b.config.bpm = 60;
        b.config.rr_jitter_ms = 50.0;
        assert_eq!(first, b.hr_payload(), "same beat index, same bytes");
        assert_ne!(first, a.hr_payload(), "next beat differs under jitter");
    }

    #[test]
    fn bpm_curve_advances_with_elapsed_time() {
        let mut sim = state();
        sim.config.bpm = 72;
        sim.config.bpm_curve = vec![
            BpmStep { at_s: 0.0, bpm: 60 },
            BpmStep {
                at_s: 30.0,
                bpm: 120,
            },
        ];
        assert_eq!(sim.curve_bpm(0.0), 60);
        assert_eq!(sim.curve_bpm(29.9), 60);
        assert_eq!(sim.curve_bpm(30.0), 120);
        assert_eq!(sim.curve_bpm(3600.0), 120);
        assert_eq!(state().curve_bpm(999.0), 72, "empty curve: fixed bpm");
    }

    #[test]
    fn hr_payload_reports_no_contact_like_the_strap_by_default() {
        // All 120 raw HR packets start with 0x10 (contact not supported).
        let mut sim = state();
        assert!(!sim.config.contact_supported);
        let payload = sim.hr_payload();
        assert_eq!(payload[0], 0x10);
        assert_eq!(payload[1], 72);
    }

    #[test]
    fn contact_simulation_needs_a_supported_profile() {
        let mut sim = state();
        sim.config.contact_supported = true;
        sim.config.contact_detected = true;
        assert_eq!(sim.hr_payload()[0] & 0x06, 0x06);
        sim.config.contact_detected = false;
        assert_eq!(sim.hr_payload()[0] & 0x06, 0x04);
        sim.config.contact_supported = false;
        sim.config.contact_detected = true;
        assert_eq!(
            sim.hr_payload()[0] & 0x06,
            0x00,
            "unsupported contact reports no contact bits even when detected"
        );
    }

    #[test]
    fn hr_replay_cycles_recorded_packets_verbatim() {
        let mut sim = state();
        sim.hr_replay = Some(HrReplay {
            packets: vec![vec![0x10, 88, 0xBD, 0x02], vec![0x10, 87, 0xDD, 0x02]],
        });
        assert_eq!(sim.hr_payload(), vec![0x10, 88, 0xBD, 0x02]);
        assert_eq!(sim.hr_payload(), vec![0x10, 87, 0xDD, 0x02]);
        assert_eq!(
            sim.hr_payload(),
            vec![0x10, 88, 0xBD, 0x02],
            "replay cycles forever"
        );
    }

    #[test]
    fn hr_replay_loader_decodes_the_committed_raw_capture() {
        let replay = load_hr_replay("fixtures/h10-raw/tauri-E9B93D29-2026-09-19-raw.json")
            .expect("committed raw capture must load");
        assert_eq!(replay.packets.len(), 120);
        assert_eq!(replay.packets[0], vec![0x10, 0x58, 0xBD, 0x02]);
        assert!(
            replay.packets.iter().all(|packet| packet[0] == 0x10),
            "every recorded packet reports contact-not-supported with RR"
        );
    }

    #[test]
    fn hr_replay_loader_fails_loudly() {
        assert!(load_hr_replay("fixtures/does-not-exist.json").is_err());
        assert!(load_hr_replay("profiles/stock-h10.json")
            .unwrap_err()
            .contains("hrMeasurements"));
    }

    #[test]
    fn pmd_read_reports_features() {
        assert_eq!(state().pmd_features(), gatt_spec::encode_pmd_features());
    }

    #[test]
    fn dis_reads_match_h10_strings_and_omit_pnp_id() {
        let sim = state();
        let text = |uuid: u16| String::from_utf8(sim.static_read(uuid).unwrap()).unwrap();
        // Trailing NULs are what the strap sends (see the .raw fields in
        // fixtures/h10-fingerprints).
        assert_eq!(
            text(gatt_spec::uuid16::MANUFACTURER_NAME),
            "Polar Electro Oy\0"
        );
        assert_eq!(text(gatt_spec::uuid16::MODEL_NUMBER), "H10\0");
        assert!(sim.static_read(gatt_spec::uuid16::SERIAL_NUMBER).is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::FIRMWARE_REVISION)
            .is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::HARDWARE_REVISION)
            .is_some());
        assert!(sim
            .static_read(gatt_spec::uuid16::SOFTWARE_REVISION)
            .is_some());
        assert_eq!(
            sim.static_read(gatt_spec::uuid16::SYSTEM_ID).unwrap().len(),
            8
        );
        assert_eq!(
            sim.static_read(gatt_spec::uuid16::BATTERY_LEVEL).unwrap(),
            vec![90],
            "stock charge state matches the captures (0x5a on all three hosts)"
        );
        assert_eq!(sim.static_read(0x2A50), None, "H10 omits PnP ID");
    }

    #[test]
    fn get_settings_returns_ecg_settings() {
        let mut sim = state();
        let outcome = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(outcome.action, PmdAction::None);
        let expected = gatt_spec::encode_pmd_response(
            0x01,
            0x00,
            gatt_spec::PMD_STATUS_SUCCESS,
            false,
            &gatt_spec::encode_ecg_settings(),
        );
        assert_eq!(outcome.indicate, Some(expected));
    }

    fn acc_start(rate: u16, range: u16) -> Vec<u8> {
        let mut bytes = vec![gatt_spec::PMD_OP_START, gatt_spec::PMD_MEASUREMENT_ACC];
        for (kind, value) in [(0, rate), (1, crate::acc::RESOLUTION_BITS), (2, range)] {
            bytes.extend_from_slice(&[kind, 1]);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn ecg_acquisition_origin_begins_at_start_commit_and_restarts_after_stop() {
        let mut sim = state();
        assert!(sim.ecg_started_at.is_none());
        let before = Instant::now();
        sim.apply_pmd_action(PmdAction::StartEcg);
        let first = sim.ecg_started_at.unwrap();
        assert!(first >= before);
        sim.apply_pmd_action(PmdAction::StopEcg);
        assert!(sim.ecg_started_at.is_none());
        sim.apply_pmd_action(PmdAction::StartEcg);
        assert!(sim.ecg_started_at.unwrap() >= first);
        sim.reset_pmd_session();
        assert!(sim.ecg_started_at.is_none());
    }

    #[test]
    fn get_acc_settings_uses_the_shared_h10_catalog() {
        let mut sim = state();
        let outcome = sim.handle_pmd_write(&[
            gatt_spec::PMD_OP_GET_SETTINGS,
            gatt_spec::PMD_MEASUREMENT_ACC,
        ]);
        assert_eq!(outcome.action, PmdAction::None);
        assert_eq!(
            outcome.indicate,
            Some(gatt_spec::encode_pmd_response(
                gatt_spec::PMD_OP_GET_SETTINGS,
                gatt_spec::PMD_MEASUREMENT_ACC,
                gatt_spec::PMD_STATUS_SUCCESS,
                false,
                &crate::acc::settings_payload(),
            ))
        );
    }

    #[test]
    fn all_twelve_acc_modes_start_and_stop_independently_of_ecg_at_indication() {
        for rate in crate::acc::SAMPLE_RATES_HZ {
            for range in crate::acc::RANGES_G {
                let mut sim = state();
                sim.apply_pmd_action(PmdAction::StartEcg);
                let start = sim.handle_pmd_write(&acc_start(rate, range));
                assert_eq!(
                    start.indicate.as_ref().unwrap()[3],
                    gatt_spec::PMD_STATUS_SUCCESS
                );
                assert_ne!(start.action, PmdAction::None);
                assert_eq!(
                    sim.snapshot()["accStreaming"],
                    false,
                    "decision is not streaming"
                );
                assert!(sim.snapshot()["accSettings"].is_null());
                assert!(sim.acc_started_at.is_none());
                let before_commit = Instant::now();
                sim.apply_pmd_action(start.action);
                assert!(sim.acc_started_at.unwrap() >= before_commit);
                assert_eq!(sim.snapshot()["accStreaming"], true);
                assert_eq!(
                    sim.snapshot()["accSettings"],
                    serde_json::json!({
                        "sampleRateHz": rate, "resolutionBits":16, "rangeG":range, "channels":3,
                    })
                );
                assert!(sim.ecg_streaming);
                let repeated = sim.handle_pmd_write(&acc_start(rate, range));
                assert_eq!(repeated.action, PmdAction::None);
                assert_eq!(
                    repeated.indicate.as_ref().unwrap()[3],
                    gatt_spec::PMD_STATUS_ALREADY_IN_STATE
                );
                let ecg_stop =
                    sim.handle_pmd_write(&[gatt_spec::PMD_OP_STOP, gatt_spec::PMD_MEASUREMENT_ECG]);
                sim.apply_pmd_action(ecg_stop.action);
                assert!(!sim.ecg_streaming);
                assert_eq!(sim.snapshot()["accStreaming"], true);
                sim.apply_pmd_action(PmdAction::StartEcg);
                let acc_stop =
                    sim.handle_pmd_write(&[gatt_spec::PMD_OP_STOP, gatt_spec::PMD_MEASUREMENT_ACC]);
                assert_eq!(
                    acc_stop.indicate.as_ref().unwrap()[3],
                    gatt_spec::PMD_STATUS_SUCCESS
                );
                assert_eq!(
                    sim.snapshot()["accStreaming"],
                    true,
                    "STOP also waits for indication"
                );
                sim.apply_pmd_action(acc_stop.action);
                assert!(sim.ecg_streaming);
                assert_eq!(sim.snapshot()["accStreaming"], false);
                assert!(sim.snapshot()["accSettings"].is_null());
                assert!(sim.acc_started_at.is_none());
                let idle_stop =
                    sim.handle_pmd_write(&[gatt_spec::PMD_OP_STOP, gatt_spec::PMD_MEASUREMENT_ACC]);
                assert_eq!(idle_stop.action, PmdAction::None);
                assert_eq!(
                    idle_stop.indicate.as_ref().unwrap()[3],
                    gatt_spec::PMD_STATUS_ALREADY_IN_STATE
                );
            }
        }
    }

    #[test]
    fn incomplete_command_headers_and_acc_tlvs_never_panic_or_start_streaming() {
        let command = acc_start(200, 8);
        let mut sim = state();
        for bytes in [
            vec![],
            vec![gatt_spec::PMD_OP_START],
            vec![gatt_spec::PMD_OP_STOP],
            vec![gatt_spec::PMD_OP_GET_SETTINGS],
        ] {
            let outcome = sim.handle_pmd_write(&bytes);
            assert_eq!(
                outcome.indicate, None,
                "missing type cannot be fabricated as ECG"
            );
            assert_eq!(outcome.action, PmdAction::None);
        }
        for end in 2..command.len() {
            let outcome = sim.handle_pmd_write(&command[..end]);
            assert_eq!(outcome.action, PmdAction::None);
            assert_ne!(
                outcome.indicate.as_ref().unwrap()[3],
                gatt_spec::PMD_STATUS_SUCCESS
            );
        }
        assert!(!sim.ecg_streaming);
    }

    #[test]
    fn incomplete_ecg_settings_and_extra_non_start_parameters_are_refused() {
        let mut sim = state();
        let command = [2, 0, 0, 1, 130, 0, 1, 1, 14, 0];
        for end in 2..command.len() {
            let outcome = sim.handle_pmd_write(&command[..end]);
            assert_eq!(outcome.action, PmdAction::None, "ECG prefix length {end}");
            assert_ne!(outcome.indicate.unwrap()[3], gatt_spec::PMD_STATUS_SUCCESS);
        }
        for command in [vec![1, 0, 0], vec![1, 2, 0], vec![3, 0, 0], vec![3, 2, 0]] {
            let outcome = sim.handle_pmd_write(&command);
            assert_eq!(outcome.action, PmdAction::None);
            assert_eq!(
                outcome.indicate.unwrap()[3],
                gatt_spec::PMD_STATUS_INVALID_LENGTH
            );
        }
        sim.reject_next_status = Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED);
        assert!(sim.handle_pmd_write(&[2]).indicate.is_none());
        assert_eq!(
            sim.reject_next_status,
            Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED)
        );
    }

    #[test]
    fn unsupported_pmd_mode_bits_are_refused_not_masked_into_online_streams() {
        let mut sim = state();
        for measurement in [
            gatt_spec::PMD_MEASUREMENT_ECG,
            gatt_spec::PMD_MEASUREMENT_ACC,
        ] {
            for flags in [0x40, 0x80, 0xc0] {
                for op in [
                    gatt_spec::PMD_OP_GET_SETTINGS,
                    gatt_spec::PMD_OP_START,
                    gatt_spec::PMD_OP_STOP,
                ] {
                    let outcome = sim.handle_pmd_write(&[op, measurement | flags]);
                    assert_eq!(outcome.action, PmdAction::None);
                    assert_eq!(
                        outcome.indicate.as_ref().unwrap()[3],
                        gatt_spec::PMD_STATUS_NOT_SUPPORTED
                    );
                }
            }
        }
    }

    #[test]
    fn pending_actions_reserve_stream_state_without_starting_a_clock() {
        let mut sim = state();
        let due = Instant::now() + std::time::Duration::from_secs(1);
        let start = sim.handle_pmd_write(&acc_start(25, 2));
        assert_ne!(start.action, PmdAction::None);
        sim.pending_indications.push(PendingIndication {
            due,
            response: start.indicate.unwrap(),
            action: start.action,
        });
        let duplicate = sim.handle_pmd_write(&acc_start(200, 8));
        assert_eq!(duplicate.action, PmdAction::None);
        assert_eq!(
            duplicate.indicate.unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
        assert!(sim.acc_started_at.is_none());
        assert!(sim.acc_settings.is_none());
        let ecg_start = sim.handle_pmd_write(&[2, 0, 0, 1, 130, 0, 1, 1, 14, 0]);
        assert_eq!(
            ecg_start.action,
            PmdAction::StartEcg,
            "ACC reservation does not block ECG"
        );
        sim.pending_indications.push(PendingIndication {
            due,
            response: ecg_start.indicate.unwrap(),
            action: ecg_start.action,
        });
        assert_eq!(
            sim.handle_pmd_write(&[2, 0, 0, 1, 130, 0, 1, 1, 14, 0])
                .indicate
                .unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
        let stop = sim.handle_pmd_write(&[3, 2]);
        assert_ne!(stop.action, PmdAction::None, "STOP follows a queued START");
        sim.pending_indications.push(PendingIndication {
            due,
            response: stop.indicate.unwrap(),
            action: stop.action,
        });
        assert_eq!(
            sim.handle_pmd_write(&[3, 2]).indicate.unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
        for pending in std::mem::take(&mut sim.pending_indications) {
            sim.apply_pmd_action(pending.action);
        }
        assert!(sim.ecg_streaming);
        assert!(sim.acc_settings.is_none());
        assert!(sim.acc_started_at.is_none());
    }

    #[test]
    fn session_reset_drops_both_streams_and_uncommitted_indications() {
        let mut sim = state();
        sim.apply_pmd_action(PmdAction::StartEcg);
        let acc = sim.handle_pmd_write(&acc_start(50, 4));
        sim.apply_pmd_action(acc.action);
        sim.pending_indications.push(PendingIndication {
            due: Instant::now(),
            response: acc.indicate.unwrap(),
            action: acc.action,
        });
        sim.last_pmd_response = Some(vec![0xf0, 2, 2, 0, 0]);
        sim.reset_pmd_session();
        assert!(!sim.ecg_streaming);
        assert!(sim.acc_settings.is_none());
        assert!(sim.acc_started_at.is_none());
        assert!(sim.pending_indications.is_empty());
        assert!(sim.last_pmd_response.is_none());
        assert_eq!(
            sim.handle_pmd_write(&acc_start(200, 8)).indicate.unwrap()[3],
            gatt_spec::PMD_STATUS_SUCCESS
        );
    }

    #[test]
    fn control_point_unsubscribe_preserves_streams_and_cancels_pending_actions() {
        let mut sim = state();
        sim.apply_pmd_action(PmdAction::StartEcg);
        let acc = acc_start(50, 4);
        let acc_start = sim.handle_pmd_write(&acc);
        sim.apply_pmd_action(acc_start.action);
        let ecg_started_at = sim.ecg_started_at;
        let acc_started_at = sim.acc_started_at;
        sim.pending_indications.push(PendingIndication {
            due: Instant::now(),
            response: vec![0xf0, 3, 0, 0, 0],
            action: PmdAction::StopEcg,
        });
        sim.queue_pmd_action(42, PmdAction::StopAcc);
        sim.last_pmd_response = Some(vec![0xf0, 2, 0, 0, 0]);

        sim.observe_subscription(gatt_spec::pmd::CONTROL_POINT, false);

        assert!(sim.ecg_streaming);
        assert!(sim.acc_settings.is_some());
        assert_eq!(sim.ecg_started_at, ecg_started_at);
        assert_eq!(sim.acc_started_at, acc_started_at);
        assert!(sim.pending_indications.is_empty());
        assert_eq!(sim.settle_pmd_action(42, true), None);
        assert!(sim.last_pmd_response.is_none());
    }

    #[test]
    fn canceled_pending_start_cannot_activate_after_control_point_unsubscribe() {
        let mut sim = state();
        let start = sim.handle_pmd_write(&acc_start(25, 2));
        sim.queue_pmd_action(7, start.action);

        sim.observe_subscription(gatt_spec::pmd::CONTROL_POINT, false);

        assert_eq!(sim.settle_pmd_action(7, true), None);
        assert!(sim.acc_settings.is_none());
        assert!(sim.acc_started_at.is_none());
        assert!(!sim.ecg_streaming);
    }

    #[test]
    fn reversed_response_deadlines_cannot_commit_stop_before_start() {
        let mut sim = state();
        let now = Instant::now();
        let later = now + std::time::Duration::from_secs(1);
        let start = sim.handle_pmd_write(&acc_start(100, 8));
        sim.pending_indications.push(PendingIndication {
            due: later,
            response: start.indicate.unwrap(),
            action: start.action,
        });
        let stop = sim.handle_pmd_write(&[3, 2]);
        sim.pending_indications.push(PendingIndication {
            due: now,
            response: stop.indicate.unwrap(),
            action: stop.action,
        });
        assert!(
            sim.take_due_indication(now).is_none(),
            "due STOP cannot jump over START"
        );
        assert_eq!(sim.pending_indications.len(), 2);
        let first = sim.take_due_indication(later).unwrap();
        assert!(matches!(first.action, PmdAction::StartAcc(_)));
        assert!(
            sim.acc_settings.is_none(),
            "dequeue does not commit a radio effect"
        );
        sim.apply_pmd_action(first.action);
        let second = sim.take_due_indication(later).unwrap();
        assert_eq!(second.action, PmdAction::StopAcc);
        sim.apply_pmd_action(second.action);
        assert!(sim.acc_settings.is_none());
        assert!(sim.acc_started_at.is_none());
        assert!(sim.take_due_indication(later).is_none());
    }

    #[test]
    fn queued_transport_is_not_start_acceptance_and_failed_settlement_does_not_start() {
        let mut sim = state();
        let start = sim.handle_pmd_write(&acc_start(25, 2));
        sim.queue_pmd_action(10, start.action);
        assert!(sim.acc_settings.is_none());
        assert!(sim.acc_started_at.is_none());
        assert_eq!(
            sim.handle_pmd_write(&acc_start(50, 4)).indicate.unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
        assert_eq!(sim.settle_pmd_action(10, false), None);
        assert!(sim.acc_settings.is_none());
        let retry = sim.handle_pmd_write(&acc_start(50, 4));
        assert_ne!(retry.action, PmdAction::None);
        sim.queue_pmd_action(11, retry.action);
        let accepted = sim.settle_pmd_action(11, true).unwrap();
        assert_eq!(accepted, retry.action);
        assert!(
            sim.acc_settings.is_none(),
            "root commits and logs exactly once after settlement"
        );
        sim.apply_pmd_action(accepted);
        assert_eq!(sim.acc_settings.unwrap().sample_rate_hz, 50);
        assert_eq!(
            sim.settle_pmd_action(11, true),
            None,
            "duplicate completion cannot restart sensor clock"
        );
    }

    #[test]
    fn inflight_actions_precede_deferred_admission_and_reset_ignores_old_completions() {
        let mut sim = state();
        let start = sim.handle_pmd_write(&acc_start(25, 2));
        sim.queue_pmd_action(20, start.action);
        let stop = sim.handle_pmd_write(&[3, 2]);
        assert_eq!(stop.action, PmdAction::StopAcc);
        sim.pending_indications.push(PendingIndication {
            due: Instant::now(),
            response: stop.indicate.unwrap(),
            action: stop.action,
        });
        assert_ne!(
            sim.handle_pmd_write(&acc_start(200, 8)).action,
            PmdAction::None,
            "inflight START followed by deferred STOP projects idle"
        );
        sim.reset_pmd_session();
        assert_eq!(
            sim.settle_pmd_action(20, true),
            None,
            "old-generation acceptance cannot resurrect a stream"
        );
        assert!(sim.pending_indications.is_empty());
        assert!(sim.acc_settings.is_none());
        let current = sim.handle_pmd_write(&acc_start(200, 8));
        sim.queue_pmd_action(21, current.action);
        assert_eq!(sim.settle_pmd_action(20, false), None);
        assert_eq!(sim.settle_pmd_action(21, true), Some(current.action));
    }

    #[test]
    fn start_ecg_with_sdk_bytes_streams() {
        let mut sim = state();
        let outcome =
            sim.handle_pmd_write(&[0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00]);
        assert_eq!(outcome.action, PmdAction::StartEcg);
        // Two-phase commit: the decision alone starts nothing.
        assert!(!sim.ecg_streaming);
        sim.apply_pmd_action(outcome.action);
        assert_eq!(
            outcome.indicate,
            Some(gatt_spec::encode_pmd_response(
                0x02,
                0x00,
                gatt_spec::PMD_STATUS_SUCCESS,
                false,
                &[]
            ))
        );
        assert!(sim.ecg_streaming);
    }

    #[test]
    fn pmd_streaming_commits_at_indication_not_at_write() {
        // Two-phase commit: the write only decides (bytes + action); the
        // stream starts when the indication actually goes out. Frames must
        // never precede the START response the SDK waits for.
        let mut sim = state();
        let outcome =
            sim.handle_pmd_write(&[0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00]);
        assert_eq!(outcome.action, PmdAction::StartEcg);
        assert!(
            !sim.ecg_streaming,
            "the write decides; the indication commits"
        );
        sim.apply_pmd_action(outcome.action);
        assert!(sim.ecg_streaming, "committed when indicated");
        let stop = sim.handle_pmd_write(&[0x03, 0x00]);
        assert_eq!(stop.action, PmdAction::StopEcg);
        assert!(sim.ecg_streaming, "stop also commits at indication");
        sim.apply_pmd_action(stop.action);
        assert!(!sim.ecg_streaming);
    }

    #[test]
    fn start_ecg_rejects_wrong_sample_rate_and_resolution() {
        let mut sim = state();
        let rate = sim.handle_pmd_write(&[0x02, 0x00, 0x00, 0x01, 0x64, 0x00]);
        assert_eq!(rate.action, PmdAction::None);
        assert_eq!(
            rate.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_SAMPLE_RATE
        );
        assert!(!sim.ecg_streaming);
        let resolution = sim.handle_pmd_write(&[0x02, 0x00, 0x01, 0x01, 0x08, 0x00]);
        assert_eq!(
            resolution.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_RESOLUTION
        );
    }

    #[test]
    fn stop_ecg_halts_the_stream() {
        let mut sim = state();
        sim.ecg_streaming = true;
        let outcome = sim.handle_pmd_write(&[0x03, 0x00]);
        assert_eq!(outcome.action, PmdAction::StopEcg);
        assert_eq!(
            outcome.indicate,
            Some(gatt_spec::encode_pmd_response(
                0x03,
                0x00,
                gatt_spec::PMD_STATUS_SUCCESS,
                false,
                &[]
            ))
        );
    }

    #[test]
    fn vendor_read_serves_only_the_readable_characteristic() {
        use std::str::FromStr;
        let sim = state();
        let read = uuid::Uuid::from_str(crate::gatt_spec::vendor::READ).expect("valid UUID");
        assert_eq!(sim.vendor_read(&read), None);
        let indicate =
            uuid::Uuid::from_str(crate::gatt_spec::vendor::WRITE_INDICATE).expect("valid UUID");
        assert_eq!(sim.vendor_read(&indicate), None);
        assert!(sim.vendor_writable(&indicate));
        assert!(!sim.vendor_writable(&read));
        let char53 = uuid::Uuid::from_str(crate::gatt_spec::feee::CHAR_53).expect("valid UUID");
        assert!(sim.vendor_writable(&char53));
        assert_eq!(sim.vendor_read(&char53), None);
    }

    #[test]
    fn repeated_start_and_idle_stop_report_already_in_state() {
        let mut sim = state();
        let start = [0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0E, 0x00];
        let first = sim.handle_pmd_write(&start);
        assert_eq!(first.action, PmdAction::StartEcg);
        assert!(!sim.ecg_streaming, "uncommitted decision starts nothing");
        sim.apply_pmd_action(first.action);
        assert!(sim.ecg_streaming);
        let repeated = sim.handle_pmd_write(&start);
        assert_eq!(repeated.action, PmdAction::None, "no second start emitted");
        assert_eq!(
            repeated.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
        assert!(sim.ecg_streaming, "the stream keeps running");
        let stop = sim.handle_pmd_write(&[0x03, 0x00]);
        assert_eq!(stop.action, PmdAction::StopEcg);
        assert!(sim.ecg_streaming, "stop commits at indication");
        sim.apply_pmd_action(stop.action);
        assert!(!sim.ecg_streaming);
        let idle_stop = sim.handle_pmd_write(&[0x03, 0x00]);
        assert_eq!(idle_stop.action, PmdAction::None);
        assert_eq!(
            idle_stop.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_ALREADY_IN_STATE
        );
    }

    #[test]
    fn pending_pmd_capacity_bounds_combined_queues_without_consuming_faults() {
        for inflight in [0, 64, crate::radio::SEND_QUEUE_CAPACITY] {
            let mut sim = state();
            for id in 0..inflight {
                sim.queue_pmd_action(id as u64, PmdAction::StartEcg);
            }
            for _ in inflight..crate::radio::SEND_QUEUE_CAPACITY {
                sim.pending_indications.push(PendingIndication {
                    due: Instant::now() + std::time::Duration::from_secs(60),
                    response: vec![0xf0, 1, 2, 0],
                    action: PmdAction::None,
                });
            }
            sim.reject_next_status = Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED);
            for _ in 0..1000 {
                assert!(!sim.can_admit_pmd_command());
                let refused = sim.handle_pmd_write(&[1, 2]);
                assert!(refused.indicate.is_none());
                assert_eq!(refused.action, PmdAction::None);
            }
            assert_eq!(
                sim.reject_next_status,
                Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED)
            );
            assert!(!sim.ecg_streaming);
            assert!(sim.acc_settings.is_none());
            if inflight > 0 {
                sim.settle_pmd_action(0, false);
            } else {
                sim.pending_indications.remove(0);
            }
            assert!(sim.can_admit_pmd_command());
            assert_eq!(
                sim.handle_pmd_write(&[1, 2]).indicate.unwrap()[3],
                gatt_spec::PMD_STATUS_NOT_SUPPORTED
            );
            sim.reset_pmd_session();
            assert!(sim.can_admit_pmd_command());
        }
    }

    #[test]
    fn reject_next_fault_fires_once_with_given_status() {
        let mut sim = state();
        sim.reject_next_status = Some(gatt_spec::PMD_STATUS_NOT_SUPPORTED);
        let first = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(
            first.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_NOT_SUPPORTED
        );
        assert_eq!(first.action, PmdAction::None);
        assert_eq!(
            sim.reject_next_status, None,
            "fault must clear after firing"
        );
        let second = sim.handle_pmd_write(&[0x01, 0x00]);
        assert_eq!(
            second.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_SUCCESS
        );
    }

    #[test]
    fn unknown_op_and_type_are_reported_not_dropped() {
        let mut sim = state();
        let unknown_op = sim.handle_pmd_write(&[0x09, 0x00]);
        assert_eq!(
            unknown_op.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_OP
        );
        let unsupported = sim.handle_pmd_write(&[0x01, 0x01]);
        assert_eq!(
            unsupported.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_NOT_SUPPORTED,
            "PPG is valid but not on an H10"
        );
        let invalid = sim.handle_pmd_write(&[0x01, 0x3F]);
        assert_eq!(
            invalid.indicate.as_ref().unwrap()[3],
            gatt_spec::PMD_STATUS_INVALID_MEASUREMENT_TYPE
        );
        assert_eq!(
            sim.handle_pmd_write(&[]).indicate,
            None,
            "empty write has nothing to echo"
        );
    }
}
